//! Notion connector — Notion REST API + polling-only delta.
//!
//! Notion does **not** ship native webhooks (as of 2026-05). Steady
//! state is therefore polled — `incremental_sync` re-queries `/v1/search`
//! and filters by `last_edited_time >= cursor`. `subscribe_webhook`
//! and `handle_webhook_event` exist for trait completeness but return
//! a [`ConnectorError::Webhook`] tagged as polling-only so the
//! substrate routes Notion to its scheduled-poll path.
//!
//! Production wiring runs over [`HttpTransport`] — the substrate
//! constructs a [`NotionConnector`] with a real
//! `connector_framework::BlockingHttpTransport` (under the
//! `http-client` feature) and a real `OAuth2Client` for the
//! `https://api.notion.com/v1/oauth/token` exchange. Unit tests
//! pass `MockHttpTransport` + a fixture OAuth2 exchange so the
//! pagination + classification logic runs against real wire-format
//! JSON without hitting `api.notion.com`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Notion REST base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.notion.com/v1";

/// Page size cap for `/v1/search`. Notion accepts up to 100; we
/// default to the API maximum to minimise round-trips.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on the number of `/v1/search` pages a single sync
/// will walk. Notion workspaces with more than 1 000 000 pages
/// (10 000 pages × 100/page) need a redesigned ingestion strategy.
pub const MAX_SEARCH_PAGES: usize = 10_000;

/// Notion `object` enum (subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotionObjectKind {
    /// `"page"`.
    Page,
    /// `"database"`.
    Database,
    /// `"block"`.
    Block,
}

/// One page or database returned by `/v1/search` or
/// `/v1/databases/{id}/query`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionObject {
    /// Object id (UUID v4 string).
    pub id: String,
    /// Object kind.
    pub object: NotionObjectKind,
    /// Wall-clock created time.
    #[serde(default)]
    pub created_time: Option<DateTime<Utc>>,
    /// Wall-clock last-edited time.
    #[serde(default)]
    pub last_edited_time: Option<DateTime<Utc>>,
    /// Notion's `archived = true` is the deletion signal.
    #[serde(default)]
    pub archived: bool,
}

/// `/v1/search` (or `/v1/databases/{id}/query`) response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotionSearchResponse {
    /// Page of results.
    #[serde(default)]
    pub results: Vec<NotionObject>,
    /// `next_cursor` for the following page; `None` when exhausted.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// `has_more` flag for explicit pagination control.
    #[serde(default)]
    pub has_more: bool,
}

/// Safety ceiling on `/v1/blocks/{id}/children` pages walked per
/// block while reconstructing a page body.
const MAX_BLOCK_PAGES: usize = 1_000;

/// Maximum block-tree recursion depth while reconstructing Markdown.
/// Notion's own nesting limit is far below this; the cap just stops a
/// pathological / cyclic response from recursing without bound.
const MAX_BLOCK_DEPTH: usize = 32;

/// One block from `GET /v1/blocks/{id}/children`. The typed payload
/// lives under a key equal to [`Self::block_type`] (e.g. a `paragraph`
/// block carries its rich text under `"paragraph"`), so we flatten the
/// remainder into a JSON map and index it by type at render time.
#[derive(Debug, Clone, Deserialize)]
struct NotionBlock {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "type")]
    block_type: String,
    #[serde(default)]
    has_children: bool,
    #[serde(flatten)]
    payload: serde_json::Map<String, serde_json::Value>,
}

/// One page of `GET /v1/blocks/{id}/children`.
#[derive(Debug, Clone, Default, Deserialize)]
struct NotionBlockChildren {
    #[serde(default)]
    results: Vec<NotionBlock>,
    #[serde(default)]
    next_cursor: Option<String>,
    #[serde(default)]
    has_more: bool,
}

/// Notion connector. Pure poll-based — no webhook surface.
pub struct NotionConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for NotionConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl NotionConnector {
    /// Construct a Notion connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://api.notion.com/v1/oauth/token`. The production
    /// substrate wires these to `BlockingHttpTransport` +
    /// `OAuth2Client`; tests use `MockHttpTransport`.
    pub fn new(
        instance: ConnectorInstanceId,
        transport: Arc<dyn HttpTransport>,
        oauth: Arc<dyn OAuth2CodeExchange>,
    ) -> Self {
        Self {
            instance,
            transport,
            oauth,
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Override the Notion REST base URL. Only useful when proxying
    /// the API through a gateway during local development.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the per-page size used by `/v1/search`. Clamped to
    /// `[1, 100]` per Notion's documented maximum.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 100);
        self
    }

    /// Resolve the base URL from `auth_config_json` if set,
    /// otherwise fall back to the constructor-time default.
    fn resolved_base_url(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    /// Notion mandates a `Notion-Version` header — pin the
    /// `2022-06-28` stable schema since that's what `NotionObject`
    /// is modelled against. `bearer_post_json` already sets
    /// `Authorization: Bearer <token>` / `Content-Type` / `Accept`.
    const NOTION_VERSION_HEADER: (&'static str, &'static str) = ("Notion-Version", "2022-06-28");

    /// Run `/v1/search` with the supplied filter payload and walk
    /// every page (capped at [`MAX_SEARCH_PAGES`]).
    ///
    /// `filter_payload` is the JSON body that Notion accepts on
    /// `POST /v1/search` — `initial_sync` posts a `{}` body which
    /// matches everything; `incremental_sync` posts a sort over
    /// `last_edited_time` descending and a `cutoff` watermark so we
    /// can short-circuit pagination at the first object older than
    /// the prior watermark.
    ///
    /// `cutoff` is honoured only when the caller has sorted the
    /// search descending by `last_edited_time` (i.e. the incremental
    /// path). Pass `None` to walk every page (the initial path).
    /// When set and the response page contains an object with
    /// `last_edited_time <= cutoff`, the iteration stops mid-page
    /// — every subsequent object is guaranteed older under the
    /// descending sort, so fetching further pages would be wasted
    /// I/O.
    fn paginate_search(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        filter_payload: &serde_json::Value,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<Vec<NotionObject>> {
        let mut results = Vec::<NotionObject>::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_SEARCH_PAGES {
            let mut body = filter_payload.clone();
            let body_map = body.as_object_mut().ok_or_else(|| {
                ConnectorError::Sync("notion /v1/search body must be a JSON object".into())
            })?;
            body_map.insert(
                "page_size".to_string(),
                serde_json::Value::from(self.page_size),
            );
            if let Some(c) = cursor.as_ref() {
                body_map.insert(
                    "start_cursor".to_string(),
                    serde_json::Value::String(c.clone()),
                );
            }
            let url = format!("{base_url}/search");
            let resp: NotionSearchResponse = bearer_post_json(
                &self.transport,
                "notion",
                "/v1/search",
                &url,
                token,
                &[Self::NOTION_VERSION_HEADER],
                &body,
            )?;
            if let Some(cut) = cutoff {
                // Descending sort by `last_edited_time` — find the
                // first object at or below the cutoff and stop
                // there. Truncate this page (and skip every later
                // page) since they are guaranteed older.
                if let Some(stop_at) = resp
                    .results
                    .iter()
                    .position(|o| o.last_edited_time.is_some_and(|t| t <= cut))
                {
                    results.extend(resp.results.into_iter().take(stop_at));
                    return Ok(results);
                }
            }
            results.extend(resp.results);
            let next = resp.next_cursor;
            if !resp.has_more || next.is_none() {
                return Ok(results);
            }
            if next == cursor {
                // Defence-in-depth: Notion claims has_more=true but
                // hands back the same cursor — abort instead of
                // looping forever.
                return Err(ConnectorError::Sync("notion /v1/search returned the same cursor twice; aborting to avoid infinite loop"
                        .into(),
                ));
            }
            cursor = next;
        }
        Err(ConnectorError::Sync(format!(
            "notion /v1/search exceeded {MAX_SEARCH_PAGES} pages without a terminating cursor"
        )))
    }

    /// Walk every `GET /v1/blocks/{block_id}/children` page for one
    /// parent block (or page) and collect the child blocks.
    fn fetch_block_children(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        block_id: &str,
    ) -> Result<Vec<NotionBlock>> {
        let mut blocks = Vec::<NotionBlock>::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_BLOCK_PAGES {
            let mut url = format!(
                "{base_url}/blocks/{}/children?page_size={}",
                percent_encode_path_component(block_id),
                self.page_size,
            );
            if let Some(c) = cursor.as_deref() {
                url.push_str("&start_cursor=");
                url.push_str(&percent_encode_path_component(c));
            }
            let resp: NotionBlockChildren = bearer_get_json(
                &self.transport,
                "notion",
                "/v1/blocks/{id}/children",
                &url,
                token,
                &[Self::NOTION_VERSION_HEADER],
            )?;
            blocks.extend(resp.results);
            let next = resp.next_cursor;
            if !resp.has_more || next.is_none() {
                return Ok(blocks);
            }
            if next == cursor {
                return Err(ConnectorError::Sync("notion /v1/blocks children returned the same cursor twice; aborting to avoid infinite loop"
                        .into(),
                ));
            }
            cursor = next;
        }
        Err(ConnectorError::Sync(format!(
            "notion /v1/blocks children exceeded {MAX_BLOCK_PAGES} pages without a terminating cursor"
        )))
    }

    /// Best-effort fetch of a page's title via `GET /v1/pages/{id}`.
    ///
    /// The block-children endpoint returns body content but not the
    /// page's own properties, so the title — which lives in the single
    /// `title`-typed property — needs this extra call. Returns an empty
    /// string when the page has no title property, the title is blank,
    /// or the request fails; the body has already been reconstructed by
    /// the caller, so a title lookup error must not fail the whole fetch.
    fn fetch_page_title(&self, base_url: &str, token: &OAuth2Token, page_id: &str) -> String {
        let url = format!(
            "{base_url}/pages/{}",
            percent_encode_path_component(page_id)
        );
        let page: serde_json::Value = match bearer_get_json(
            &self.transport,
            "notion",
            "/v1/pages/{id}",
            &url,
            token,
            &[Self::NOTION_VERSION_HEADER],
        ) {
            Ok(page) => page,
            Err(_) => return String::new(),
        };
        notion_page_title(&page)
    }

    /// Reconstruct Markdown for a list of blocks, recursing into
    /// children (capped at [`MAX_BLOCK_DEPTH`]). `depth` drives the
    /// indentation of nested list / toggle content.
    fn render_blocks(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        blocks: &[NotionBlock],
        depth: usize,
        out: &mut String,
    ) -> Result<()> {
        let indent = "  ".repeat(depth);
        for block in blocks {
            render_block_line(block, &indent, out);
            // Recurse into nested children (lists, toggles, callouts,
            // table rows live as child blocks of their parent).
            if block.has_children && depth < MAX_BLOCK_DEPTH && !block.id.is_empty() {
                let children = self.fetch_block_children(base_url, token, &block.id)?;
                self.render_blocks(base_url, token, &children, depth + 1, out)?;
            }
        }
        Ok(())
    }
}

/// Extract a Notion page title from its `properties` map.
///
/// The title lives in the single property whose `type` is `"title"`,
/// carried as a `plain_text` rich-text array. Returns an empty string
/// when no such property exists or it is blank.
fn notion_page_title(page: &serde_json::Value) -> String {
    page.get("properties")
        .and_then(serde_json::Value::as_object)
        .and_then(|props| {
            props
                .values()
                .find(|p| p.get("type").and_then(serde_json::Value::as_str) == Some("title"))
        })
        .and_then(|p| p.get("title"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|rt| rt.get("plain_text").and_then(serde_json::Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Join the `plain_text` of a Notion rich-text array.
fn rich_text_of(payload: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    payload
        .get(key)
        .and_then(|v| v.get("rich_text"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|rt| rt.get("plain_text").and_then(serde_json::Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Render one block's own line(s) of Markdown (excluding children),
/// appending to `out`. Covers the block types the spec calls out:
/// paragraph, heading_1..3, bulleted / numbered list items, to_do,
/// code, quote, callout, toggle, divider, and table rows.
fn render_block_line(block: &NotionBlock, indent: &str, out: &mut String) {
    let ty = block.block_type.as_str();
    let text = rich_text_of(&block.payload, ty);
    match ty {
        "paragraph" => {
            out.push_str(indent);
            out.push_str(&text);
            out.push('\n');
        }
        "heading_1" => push_line(out, indent, &format!("# {text}")),
        "heading_2" => push_line(out, indent, &format!("## {text}")),
        "heading_3" => push_line(out, indent, &format!("### {text}")),
        // `toggle` has no native Markdown form; render its summary
        // line like a bullet (its children recurse underneath).
        "bulleted_list_item" | "toggle" => push_line(out, indent, &format!("- {text}")),
        "numbered_list_item" => push_line(out, indent, &format!("1. {text}")),
        "to_do" => {
            let checked = block
                .payload
                .get("to_do")
                .and_then(|v| v.get("checked"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let mark = if checked { "x" } else { " " };
            push_line(out, indent, &format!("- [{mark}] {text}"));
        }
        "quote" => push_line(out, indent, &format!("> {text}")),
        "callout" => {
            let icon = block
                .payload
                .get("callout")
                .and_then(|v| v.get("icon"))
                .and_then(|v| v.get("emoji"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let prefix = if icon.is_empty() {
                "> ".to_string()
            } else {
                format!("> {icon} ")
            };
            push_line(out, indent, &format!("{prefix}{text}"));
        }
        "code" => {
            let lang = block
                .payload
                .get("code")
                .and_then(|v| v.get("language"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            push_line(out, indent, &format!("```{lang}"));
            for line in text.split('\n') {
                push_line(out, indent, line);
            }
            push_line(out, indent, "```");
        }
        "divider" => push_line(out, indent, "---"),
        "table_row" => {
            let cells = block
                .payload
                .get("table_row")
                .and_then(|v| v.get("cells"))
                .and_then(serde_json::Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|cell| {
                            cell.as_array()
                                .map(|spans| {
                                    spans
                                        .iter()
                                        .filter_map(|rt| {
                                            rt.get("plain_text").and_then(serde_json::Value::as_str)
                                        })
                                        .collect::<String>()
                                })
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .unwrap_or_default();
            push_line(out, indent, &format!("| {cells} |"));
        }
        // `table`, `column_list`, `column`, `synced_block`, … are pure
        // containers — their rendered content is their children, which
        // `render_blocks` recurses into. Emit nothing for the wrapper
        // itself. Unknown types fall through with any rich text we
        // could extract so no content is silently dropped.
        "table" | "column_list" | "column" | "synced_block" => {}
        _ if !text.is_empty() => {
            out.push_str(indent);
            out.push_str(&text);
            out.push('\n');
        }
        _ => {}
    }
}

/// Push `line` prefixed with `indent` and a trailing newline.
fn push_line(out: &mut String, indent: &str, line: &str) {
    out.push_str(indent);
    out.push_str(line);
    out.push('\n');
}

/// Which sync pass produced this object — we use this instead of
/// comparing `created_time == last_edited_time` because Notion may
/// stamp the two fields at slightly different millisecond instants
/// even on creation, which would silently misclassify an event as
/// `DocumentUpdated`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

fn object_to_event(obj: &NotionObject, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = obj
        .last_edited_time
        .or(obj.created_time)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(obj.id.clone());
    if obj.archived {
        return ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        };
    }
    match mode {
        SyncMode::Initial => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        SyncMode::Incremental => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for NotionConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "notion authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        // Initial sync — `/v1/search` with an empty body matches
        // every page and database the integration can see.
        let objects = self.paginate_search(&base_url, token, &serde_json::json!({}), None)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(objects.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for obj in &objects {
            events.push(object_to_event(obj, SyncMode::Initial));
            if let Some(t) = obj.last_edited_time {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        // Incremental — sort `last_edited_time` descending so we can
        // short-circuit pagination once we cross the prior watermark.
        let search_payload = serde_json::json!({
            "sort": {
                "direction": "descending",
                "timestamp": "last_edited_time"
            }
        });
        let cursor_watermark: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        // `paginate_search` already truncates at the first object
        // older than `cursor_watermark` under the descending sort,
        // so every object here has `last_edited_time > cursor_watermark`
        // (or is missing the field, in which case we still emit it
        // — Notion shouldn't normally omit `last_edited_time`, but
        // a missing value is safer to surface than to silently
        // drop).
        let objects = self.paginate_search(&base_url, token, &search_payload, cursor_watermark)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(objects.len());
        let mut watermark: Option<DateTime<Utc>> = cursor_watermark;
        for obj in &objects {
            events.push(object_to_event(obj, SyncMode::Incremental));
            if let Some(t) = obj.last_edited_time {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let base_url = self.resolved_base_url(config);
        let page_id = document_id.as_str();
        // Reconstruct the page body from its block tree. Notion has no
        // "give me the whole page as Markdown" endpoint — the body is
        // the (paginated, nestable) children of the page block.
        let top = self.fetch_block_children(&base_url, token, page_id)?;
        let mut markdown = String::new();
        self.render_blocks(&base_url, token, &top, 0, &mut markdown)?;
        // Trim the trailing newline run for a tidy body.
        let body = markdown.trim_end().to_string();
        // The body endpoint omits the page's own title, so fetch it
        // separately (best-effort) from the page object's properties.
        let title = self.fetch_page_title(&base_url, token, page_id);
        // Notion page URLs are the dash-stripped id under notion.so.
        let source_url = format!("https://www.notion.so/{}", page_id.replace('-', ""));
        // `with_title` normalises a blank title to `None`, so an
        // untitled page passes through unconditionally.
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "notion",
                "page_id": page_id,
                "top_level_block_count": top.len(),
            }))
            .with_source_url(source_url))
    }

    fn subscribe_webhook(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        _callback_url: &str,
    ) -> Result<WebhookSubscription> {
        Err(ConnectorError::Webhook(
            "polling-only mode: Notion has no native webhook surface".to_string(),
        ))
    }

    fn handle_webhook_event(&self, _body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        Err(ConnectorError::Webhook(
            "polling-only mode: Notion does not deliver webhooks; use incremental_sync".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    /// Stub OAuth2 exchange that returns a deterministic token so the
    /// tests can focus on the HTTP / pagination logic.
    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "notion-access",
                "notion-refresh",
                Utc::now() + Duration::days(180),
                "read_content read_user_with_email",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/notion/v1",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "notion-access");
        assert!(tok.scope.contains("read_content"));
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_events_for_each_object() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [
                    {
                        "id": "page-1",
                        "object": "page",
                        "created_time": now,
                        "last_edited_time": now,
                        "archived": false,
                    },
                    {
                        "id": "db-1",
                        "object": "database",
                        "created_time": now - Duration::hours(2),
                        "last_edited_time": now - Duration::hours(1),
                        "archived": false,
                    },
                ],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(res.next_cursor.is_some());
        // Verify the request was POST /search with page_size=100.
        let last = transport.recorded().last().cloned().unwrap();
        assert_eq!(last.method, HttpMethod::Post);
        let body: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
        assert_eq!(body["page_size"], serde_json::json!(100));
    }

    #[test]
    fn initial_sync_paginates_via_next_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-1", "object": "page",
                    "created_time": now, "last_edited_time": now, "archived": false,
                }],
                "next_cursor": "cur-2",
                "has_more": true,
            })),
        );
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-2", "object": "page",
                    "created_time": now, "last_edited_time": now, "archived": false,
                }],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        // Second call must carry start_cursor=cur-2.
        let recorded = transport.recorded();
        assert_eq!(recorded.len(), 2);
        let second: serde_json::Value = serde_json::from_slice(&recorded[1].body).unwrap();
        assert_eq!(second["start_cursor"], serde_json::json!("cur-2"));
    }

    #[test]
    fn initial_sync_classifies_objects_as_created_regardless_of_timestamps() {
        // Regression test: earlier revisions used `created_time ==
        // last_edited_time` to decide DocumentCreated vs
        // DocumentUpdated, which silently misclassified real-world
        // payloads where Notion stamps the two values a few
        // milliseconds apart even on first creation.
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-1",
                    "object": "page",
                    "created_time": now,
                    "last_edited_time": now + Duration::milliseconds(7),
                    "archived": false,
                }],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn incremental_sync_skips_objects_before_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        // Cursor = now - 1 hour; only the page edited at `now`
        // should slip through. The older page must be filtered.
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [
                    {
                        "id": "new-page",
                        "object": "page",
                        "created_time": now - Duration::days(1),
                        "last_edited_time": now,
                        "archived": false,
                    },
                    {
                        "id": "old-page",
                        "object": "page",
                        "created_time": now - Duration::days(2),
                        "last_edited_time": now - Duration::hours(2),
                        "archived": false,
                    },
                ],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some((now - Duration::hours(1)).to_rfc3339());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn incremental_sync_short_circuits_pagination_at_watermark() {
        // First page contains one fresh object then one already-seen
        // object — the descending sort means every subsequent page
        // is guaranteed older, so paginate_search must NOT fetch
        // page 2 even though the response says `has_more=true`.
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [
                    {
                        "id": "fresh",
                        "object": "page",
                        "created_time": now - Duration::days(1),
                        "last_edited_time": now,
                        "archived": false,
                    },
                    {
                        "id": "already-seen",
                        "object": "page",
                        "created_time": now - Duration::days(2),
                        "last_edited_time": now - Duration::hours(2),
                        "archived": false,
                    },
                ],
                "next_cursor": "page-2",
                "has_more": true,
            })),
        );
        // Page 2 must never be requested — assert this by recording
        // only one expected response. A second call would 404 on
        // the mock and fail the test loudly.
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some((now - Duration::hours(1)).to_rfc3339());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            transport
                .recorded()
                .iter()
                .filter(|r| r.method == HttpMethod::Post)
                .count(),
            1,
            "paginate_search must short-circuit at the watermark and not fetch page 2"
        );
    }

    #[test]
    fn incremental_sync_emits_archived_as_deleted() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-2",
                    "object": "page",
                    "created_time": now - Duration::days(1),
                    "last_edited_time": now,
                    "archived": true,
                }],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            MockResponse::status(401, b"{\"error\":\"unauthorized\"}".to_vec()),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_maps_500_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            MockResponse::status(500, b"{\"error\":\"server\"}".to_vec()),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn webhook_subscribe_is_unsupported() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://example/webhook")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_event_is_unsupported() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"{}").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn paginate_search_aborts_on_repeated_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let page = serde_json::json!({
            "results": [{
                "id": "page-1", "object": "page",
                "created_time": now, "last_edited_time": now, "archived": false,
            }],
            "next_cursor": "stuck",
            "has_more": true,
        });
        // Register the same response twice — connector receives the
        // same cursor on both pages and must error out.
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&page),
        );
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&page),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        // We need two requests to be issued (first sets cursor=stuck,
        // second uses start_cursor=stuck and gets the same cursor
        // back). Manually exercise paginate_search.
        let base_url = c.resolved_base_url(&cfg());
        let err = c
            .paginate_search(&base_url, &tok, &serde_json::json!({}), None)
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    // ───────────── fetch_content ─────────────

    #[test]
    fn fetch_content_reconstructs_markdown_from_blocks() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/page-1/children?page_size=100",
            ok_json(&serde_json::json!({
                "results": [
                    { "id": "b1", "type": "heading_1", "has_children": false,
                      "heading_1": { "rich_text": [{ "plain_text": "Title" }] } },
                    { "id": "b2", "type": "paragraph", "has_children": false,
                      "paragraph": { "rich_text": [{ "plain_text": "Hello " }, { "plain_text": "world" }] } },
                    { "id": "b3", "type": "bulleted_list_item", "has_children": false,
                      "bulleted_list_item": { "rich_text": [{ "plain_text": "first" }] } },
                    { "id": "b4", "type": "to_do", "has_children": false,
                      "to_do": { "checked": true, "rich_text": [{ "plain_text": "done" }] } },
                    { "id": "b5", "type": "code", "has_children": false,
                      "code": { "language": "rust", "rich_text": [{ "plain_text": "fn main() {}" }] } },
                    { "id": "b6", "type": "divider", "has_children": false, "divider": {} },
                ],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("page-1"))
            .unwrap();
        assert_eq!(fc.mime_type, "text/markdown");
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Title"), "body: {body}");
        assert!(body.contains("Hello world"), "body: {body}");
        assert!(body.contains("- first"), "body: {body}");
        assert!(body.contains("- [x] done"), "body: {body}");
        assert!(body.contains("```rust"), "body: {body}");
        assert!(body.contains("fn main() {}"), "body: {body}");
        assert!(body.contains("---"), "body: {body}");
        // Notion page URLs strip the dashes from the id.
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://www.notion.so/page1")
        );
        // Bearer token + Notion-Version header are set on the GET.
        let req = transport.recorded().last().cloned().unwrap();
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer notion-access"));
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Notion-Version" && v == "2022-06-28"));
    }

    #[test]
    fn fetch_content_populates_title_from_page_object() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/page-1/children?page_size=100",
            ok_json(&serde_json::json!({
                "results": [
                    { "id": "b1", "type": "paragraph", "has_children": false,
                      "paragraph": { "rich_text": [{ "plain_text": "body text" }] } },
                ],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        // The page object carries the title under its `title`-typed
        // property — the key name is arbitrary (here "Name").
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/pages/page-1",
            ok_json(&serde_json::json!({
                "object": "page",
                "id": "page-1",
                "properties": {
                    "Name": {
                        "id": "title",
                        "type": "title",
                        "title": [
                            { "plain_text": "Quarterly " },
                            { "plain_text": "Report" },
                        ],
                    },
                    "Status": { "type": "select", "select": { "name": "Done" } },
                },
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("page-1"))
            .unwrap();
        assert_eq!(fc.title.as_deref(), Some("Quarterly Report"));
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("body text"), "body: {body}");
    }

    #[test]
    fn fetch_content_omits_title_when_page_lookup_fails() {
        // Only the body endpoint is configured; the title lookup 404s
        // and must be swallowed so the body is still returned.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/page-1/children?page_size=100",
            ok_json(&serde_json::json!({
                "results": [
                    { "id": "b1", "type": "paragraph", "has_children": false,
                      "paragraph": { "rich_text": [{ "plain_text": "body only" }] } },
                ],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("page-1"))
            .unwrap();
        assert_eq!(fc.title, None);
        assert!(String::from_utf8(fc.body).unwrap().contains("body only"));
    }

    #[test]
    fn fetch_content_recurses_into_child_blocks() {
        let transport = Arc::new(MockHttpTransport::new());
        // Top-level: one toggle with children.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/root/children?page_size=100",
            ok_json(&serde_json::json!({
                "results": [
                    { "id": "toggle-1", "type": "toggle", "has_children": true,
                      "toggle": { "rich_text": [{ "plain_text": "Parent" }] } },
                ],
                "next_cursor": null, "has_more": false,
            })),
        );
        // toggle-1's children.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/toggle-1/children?page_size=100",
            ok_json(&serde_json::json!({
                "results": [
                    { "id": "child-1", "type": "paragraph", "has_children": false,
                      "paragraph": { "rich_text": [{ "plain_text": "Nested line" }] } },
                ],
                "next_cursor": null, "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("root"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("- Parent"), "body: {body}");
        // Child is indented two spaces under the toggle.
        assert!(body.contains("  Nested line"), "body: {body}");
    }

    #[test]
    fn fetch_content_paginates_block_children() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/page-1/children?page_size=100",
            ok_json(&serde_json::json!({
                "results": [
                    { "id": "b1", "type": "paragraph", "has_children": false,
                      "paragraph": { "rich_text": [{ "plain_text": "page one" }] } },
                ],
                "next_cursor": "cur-2", "has_more": true,
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/page-1/children?page_size=100&start_cursor=cur-2",
            ok_json(&serde_json::json!({
                "results": [
                    { "id": "b2", "type": "paragraph", "has_children": false,
                      "paragraph": { "rich_text": [{ "plain_text": "page two" }] } },
                ],
                "next_cursor": null, "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("page-1"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("page one"), "body: {body}");
        assert!(body.contains("page two"), "body: {body}");
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/missing/children?page_size=100",
            MockResponse::status(404, br#"{"object":"error","status":404}"#.to_vec()),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("missing"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_429_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/notion/v1/blocks/busy/children?page_size=100",
            MockResponse::too_many_requests(),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("busy"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }
}
