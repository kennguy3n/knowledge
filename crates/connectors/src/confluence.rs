//! Confluence connector — Confluence Cloud REST API v2 + Atlassian
//! webhooks.
//!
//! * `initial_sync` walks `/wiki/api/v2/pages?sort=-modified-date`,
//!   following `_links.next` cursor pagination until exhausted.
//! * `incremental_sync` keys off the prior watermark
//!   (`SyncState::cursor` is the last-modified RFC-3339 timestamp
//!   from the previous run) and filters the v2 response client-side
//!   — the v2 pages endpoint sorts by modified-date but offers no
//!   server-side `lastModified > X` filter on Cloud.
//! * `subscribe_webhook` POSTs `/wiki/rest/webhooks/1.0/webhook` to
//!   register `page_created`, `page_updated`, `page_removed`, and
//!   `space_permissions_updated`; the assigned numeric id is
//!   captured in [`WebhookSubscription::provider_subscription_id`]
//!   for later revocation / re-registration.
//! * `handle_webhook_event` parses Atlassian's webhook envelope —
//!   `page_created`, `page_updated`, `page_removed` /
//!   `page_trashed`, and `space_permissions_updated`.
//!
//! Production wiring runs over [`HttpTransport`] — the substrate
//! constructs a [`ConfluenceConnector`] with a real
//! `connector_framework::BlockingHttpTransport` (under the
//! `http-client` feature) and a real `OAuth2Client` for the
//! `https://auth.atlassian.com/oauth/token` exchange. Unit tests
//! pass `MockHttpTransport` + a fixture OAuth2 exchange.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SourcePermissionLevel, SourceUserId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

use crate::content::strip_html;

/// Default Atlassian Confluence Cloud base URL. Per-instance
/// overrides go through `auth_config_json.api_base_url` (Confluence
/// Cloud sites are per-tenant: `https://your-tenant.atlassian.net`).
pub const DEFAULT_API_BASE_URL: &str = "https://your-tenant.atlassian.net";

/// Page size for `/wiki/api/v2/pages`. Confluence Cloud's documented
/// maximum is 250; we stay at 50 to balance latency vs round-trips
/// for the median workspace.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on number of cursor pages a single sync will walk
/// — protects against pathological `_links.next` loops from a
/// mis-shaped server response.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Confluence content type. v2 emits `page` / `blogpost`; webhook
/// payloads still carry the legacy `comment` / `attachment` types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    /// Wiki page.
    Page,
    /// Blog post.
    Blogpost,
    /// Comment.
    Comment,
    /// Attachment.
    Attachment,
}

/// Lifecycle status of a Confluence content row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentStatus {
    /// Live, indexable.
    Current,
    /// Soft-deleted (in trash).
    Trashed,
    /// Permanently deleted.
    Deleted,
    /// Draft, not published.
    Draft,
}

/// Confluence content metadata (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceContent {
    /// Content id.
    pub id: String,
    /// Content type — defaults to [`ContentType::Page`] on the v2
    /// `/pages` endpoint, which only returns pages and blogposts.
    #[serde(rename = "type", default = "default_content_type")]
    pub kind: ContentType,
    /// Title.
    #[serde(default)]
    pub title: String,
    /// Status.
    pub status: ContentStatus,
    /// History block — carries `createdDate` (legacy v1 / webhook
    /// payload).
    #[serde(default)]
    pub history: Option<ConfluenceHistory>,
    /// Version block — carries `when` / `createdAt` (v1 / v2) and
    /// version `number`.
    #[serde(default)]
    pub version: Option<ConfluenceVersion>,
}

fn default_content_type() -> ContentType {
    ContentType::Page
}

/// Confluence history sub-object (v1 / webhook payload).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceHistory {
    /// Created date.
    #[serde(rename = "createdDate")]
    pub created_date: DateTime<Utc>,
}

/// Confluence version sub-object.
///
/// The v1 REST + webhook payloads carry `when`; the v2 `/pages`
/// endpoint carries `createdAt`. We accept either spelling so the
/// same struct serialises both wire formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceVersion {
    /// Last-modified timestamp.
    #[serde(alias = "createdAt")]
    pub when: DateTime<Utc>,
    /// Version number.
    #[serde(default)]
    pub number: u32,
}

/// One page of `/wiki/api/v2/pages` results. Cloud's v2 API uses
/// cursor pagination — the `_links.next` URL carries the opaque
/// cursor we forward on the next round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfluenceContentList {
    /// Results on this page.
    #[serde(default)]
    pub results: Vec<ConfluenceContent>,
    /// Links block — `_links.next` is the relative URL of the next
    /// page, or absent at the end of the list.
    #[serde(default, rename = "_links")]
    pub links: ConfluenceLinks,
}

/// `GET /wiki/api/v2/pages/{id}/labels` response (subset).
#[derive(Debug, Clone, Default, Deserialize)]
struct ConfluenceLabelList {
    #[serde(default)]
    results: Vec<ConfluenceLabel>,
}

/// One Confluence label.
#[derive(Debug, Clone, Default, Deserialize)]
struct ConfluenceLabel {
    #[serde(default)]
    name: String,
}

/// `GET /wiki/api/v2/spaces/{id}` response (subset) — used to resolve
/// a page's numeric `spaceId` to its human-readable space key.
#[derive(Debug, Clone, Default, Deserialize)]
struct ConfluenceSpace {
    #[serde(default)]
    key: String,
}

/// `_links` block on a Confluence v2 list response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfluenceLinks {
    /// Relative URL of the next page, e.g.
    /// `/wiki/api/v2/pages?cursor=...&limit=50`.
    #[serde(default)]
    pub next: Option<String>,
}

/// Response from `POST /wiki/rest/webhooks/1.0/webhook` — Atlassian
/// returns the registered webhook with its assigned id and self URL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfluenceWebhookCreateResponse {
    /// Numeric webhook id Atlassian assigned. The substrate persists
    /// this into [`WebhookSubscription::provider_subscription_id`].
    #[serde(default)]
    pub id: Option<i64>,
    /// Echoed callback URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// One Confluence webhook envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceWebhookPayload {
    /// `page_created`, `page_updated`, `page_removed`,
    /// `space_permissions_updated`.
    #[serde(rename = "webhookEvent")]
    pub webhook_event: String,
    /// Affected content.
    #[serde(default)]
    pub page: Option<ConfluenceContent>,
    /// Wall-clock event time (millis since epoch).
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// User account id whose permission changed.
    #[serde(default, rename = "accountId")]
    pub account_id: Option<String>,
    /// New permission level.
    #[serde(default)]
    pub new_role: Option<String>,
    /// Affected content id (used on permission events).
    #[serde(default, rename = "contentId")]
    pub content_id: Option<String>,
}

/// Confluence connector.
///
/// Every REST round-trip is routed through `transport`; the
/// `authorization_code` exchange runs through `oauth`. Production
/// wires `BlockingHttpTransport` + `OAuth2Client`; tests use
/// `MockHttpTransport`.
#[derive(Clone)]
pub struct ConfluenceConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ConfluenceConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfluenceConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ConfluenceConnector {
    /// Construct a Confluence connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://auth.atlassian.com/oauth/token`. The production
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

    /// Override the Confluence Cloud base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size used by `/wiki/api/v2/pages`. Clamped
    /// to `[1, 250]` per Confluence Cloud's documented maximum.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 250);
        self
    }

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

    /// Walk every `/wiki/api/v2/pages` cursor page until either
    /// `_links.next` is absent, the server returns an empty page, or
    /// [`MAX_LIST_PAGES`] is hit.
    ///
    /// Returns the merged page list in the order the server emitted
    /// them — Confluence v2 sorts by `-modified-date` (most recent
    /// first) for our query.
    ///
    /// `cutoff` enables **server-bounded incremental loading**:
    /// because the v2 endpoint sorts by `-modified-date` (newest
    /// first), once the response contains an object whose
    /// [`modified_at`] is `<= cutoff`, every subsequent object on
    /// this page and every later page is guaranteed to be at or
    /// before the watermark and is dropped. The current page is
    /// truncated to the strictly-newer prefix and iteration stops
    /// — saving the substrate from fetching the rest of the
    /// workspace's history on every incremental run. Pass `None`
    /// to walk every page (the `initial_sync` path).
    ///
    /// The early-exit only fires when the *server's* sort order is
    /// honoured; if the response is out-of-order (a transient
    /// Atlassian cache anomaly), the function falls through and
    /// walks the rest of the page — the per-row `t <= prev`
    /// defence-in-depth filter in [`Self::incremental_sync`] still
    /// drops stale rows correctly.
    fn paginate_pages(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<Vec<ConfluenceContent>> {
        let mut pages = Vec::<ConfluenceContent>::new();
        // First page — explicit query string. Subsequent pages
        // follow `_links.next` verbatim (it already carries the
        // cursor).
        let mut next_path = Some(format!(
            "/wiki/api/v2/pages?limit={}&sort=-modified-date",
            self.page_size
        ));
        let mut prev_path: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let Some(path) = next_path.take() else {
                return Ok(pages);
            };
            // Loop guard — Confluence sometimes echoes the same
            // cursor on the final page; we treat that as the end of
            // the list.
            if prev_path.as_deref() == Some(path.as_str()) {
                return Ok(pages);
            }
            prev_path = Some(path.clone());
            let url = format!("{base_url}{path}");
            let resp: ConfluenceContentList = bearer_get_json(
                &self.transport,
                "confluence",
                "/wiki/api/v2/pages",
                &url,
                token,
                &[],
            )?;
            // Empty page mid-stream — treat as end-of-list
            // defensively even if `_links.next` was set.
            if resp.results.is_empty() {
                return Ok(pages);
            }
            // Watermark-aware short-circuit — descending sort means
            // the first `<= cutoff` row is the boundary between
            // strictly-newer (keep) and at-or-older (drop). Stop
            // here without following `_links.next`; every later
            // page is guaranteed to be at or below the cutoff.
            if let Some(cut) = cutoff {
                if let Some(stop_at) = resp
                    .results
                    .iter()
                    .position(|c| modified_at(c).is_some_and(|t| t <= cut))
                {
                    pages.extend(resp.results.into_iter().take(stop_at));
                    return Ok(pages);
                }
            }
            pages.extend(resp.results);
            next_path = resp.links.next;
        }
        Err(ConnectorError::Sync(format!("confluence /wiki/api/v2/pages exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "view" | "read" | "viewer" => Some(SourcePermissionLevel::Read),
        "edit" | "write" | "contributor" => Some(SourcePermissionLevel::Write),
        "admin" | "administrator" | "owner" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// Pull the most reliable "last activity" timestamp off a content
/// row — the version's `when` if present, else the history's
/// `createdDate`.
fn modified_at(c: &ConfluenceContent) -> Option<DateTime<Utc>> {
    c.version
        .as_ref()
        .map(|v| v.when)
        .or_else(|| c.history.as_ref().map(|h| h.created_date))
}

fn content_to_event(c: &ConfluenceContent) -> ConnectorEvent {
    let occurred_at = modified_at(c).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(c.id.clone());
    match c.status {
        ContentStatus::Trashed | ContentStatus::Deleted => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
        ContentStatus::Current | ContentStatus::Draft => {
            // First version → created; otherwise updated.
            let version_number = c.version.as_ref().map_or(1, |v| v.number);
            if version_number <= 1 {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else {
                ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                }
            }
        }
    }
}

impl Connector for ConfluenceConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "confluence authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let pages = self.paginate_pages(&base_url, token, None)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(pages.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for c in &pages {
            events.push(content_to_event(c));
            if let Some(t) = modified_at(c) {
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
        let prior_watermark: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        // `paginate_pages` short-circuits on the first row
        // at-or-below `prior_watermark` (it relies on the
        // server's `-modified-date` sort). The per-row filter
        // below is kept as defence-in-depth for the rare case
        // where Atlassian's cache returns rows out of order on
        // a single page — in that scenario the short-circuit
        // truncates at the first stale row, but the prefix may
        // still contain stragglers we want to drop.
        let pages = self.paginate_pages(&base_url, token, prior_watermark)?;
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = prior_watermark;
        for c in &pages {
            // Defence-in-depth filter for out-of-order rows from
            // Atlassian cache thrash — the server-side sort + the
            // `paginate_pages` short-circuit already drop the
            // majority of stale rows before we ever see them, so
            // this loop body normally never fires `continue`.
            if let (Some(prev), Some(t)) = (prior_watermark, modified_at(c)) {
                if t <= prev {
                    continue;
                }
            }
            events.push(content_to_event(c));
            if let Some(t) = modified_at(c) {
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
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);

        // 1. Page + storage-format (XHTML) body.
        let page_url = format!("{base_url}/wiki/api/v2/pages/{id_enc}?body-format=storage");
        let page: serde_json::Value = bearer_get_json(
            &self.transport,
            "confluence",
            "/wiki/api/v2/pages/{id}",
            &page_url,
            token,
            &[],
        )?;
        let title = page
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let storage = page
            .get("body")
            .and_then(|b| b.get("storage"))
            .and_then(|s| s.get("value"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let text = strip_html(storage);
        // Confluence Cloud v2 `_links.webui` is relative to the `/wiki`
        // context root (e.g. `/spaces/OPS/pages/123/Runbook`), NOT the
        // site root, so it must be joined onto the `/wiki` prefix. Prefer
        // the API-provided `_links.base` (which already includes `/wiki`)
        // and fall back to appending `/wiki` to the configured base URL;
        // an already-absolute `webui` is used verbatim.
        let source_url = page
            .get("_links")
            .and_then(|l| l.get("webui"))
            .and_then(serde_json::Value::as_str)
            .filter(|rel| !rel.is_empty())
            .map(|rel| {
                if rel.starts_with("http://") || rel.starts_with("https://") {
                    return rel.to_string();
                }
                let base = page
                    .get("_links")
                    .and_then(|l| l.get("base"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|b| !b.is_empty())
                    .map_or_else(|| format!("{base_url}/wiki"), str::to_string);
                format!("{}{}", base.trim_end_matches('/'), rel)
            });
        let space_id = page.get("spaceId").and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        });

        // 2. Labels (best-effort metadata enrichment). The page body is
        //    already retrieved above, so a labels failure (e.g. 429 on a
        //    secondary call) must not discard it — fall back to none.
        let labels_url = format!("{base_url}/wiki/api/v2/pages/{id_enc}/labels");
        let labels: Result<ConfluenceLabelList> = bearer_get_json(
            &self.transport,
            "confluence",
            "/wiki/api/v2/pages/{id}/labels",
            &labels_url,
            token,
            &[],
        );
        let label_names: Vec<String> = labels
            .map(|l| {
                l.results
                    .into_iter()
                    .map(|x| x.name)
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        // 3. Space key — resolve the numeric spaceId when present, also
        //    best-effort (used only for metadata, not the body/url).
        let space_key = match &space_id {
            Some(sid) if !sid.is_empty() => {
                let space_url = format!(
                    "{base_url}/wiki/api/v2/spaces/{}",
                    percent_encode_path_component(sid)
                );
                let space: Result<ConfluenceSpace> = bearer_get_json(
                    &self.transport,
                    "confluence",
                    "/wiki/api/v2/spaces/{id}",
                    &space_url,
                    token,
                    &[],
                );
                space.ok().map(|s| s.key)
            }
            _ => None,
        };

        // Assemble Markdown body: title heading, body text, labels.
        let mut md = String::new();
        if !title.is_empty() {
            md.push_str("# ");
            md.push_str(&title);
            md.push_str("\n\n");
        }
        if !text.is_empty() {
            md.push_str(&text);
            md.push_str("\n\n");
        }
        if !label_names.is_empty() {
            md.push_str("Labels: ");
            md.push_str(&label_names.join(", "));
            md.push('\n');
        }
        let body = md.trim_end().to_string();

        let mut fc = FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "confluence",
                "page_id": id,
                "space_key": space_key,
                "labels": label_names,
            }));
        if let Some(url) = source_url {
            fc = fc.with_source_url(url);
        }
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/wiki/rest/webhooks/1.0/webhook");
        // Per Atlassian's webhook docs the body is
        // `{name, url, events: [...], excludeBody: false}`.
        let body = serde_json::json!({
            "name": "knowledge-substrate-sync",
            "url": callback_url,
            "events": [
                "page_created",
                "page_updated",
                "page_removed",
                "page_trashed",
                "space_permissions_updated"
            ],
            "excludeBody": false,
        });
        let resp: ConfluenceWebhookCreateResponse = bearer_post_json(
            &self.transport,
            "confluence",
            "/wiki/rest/webhooks/1.0/webhook",
            &url,
            token,
            &[],
            &body,
        )?;
        let webhook_id = resp.id.ok_or_else(|| {
            ConnectorError::Webhook(
                "confluence /wiki/rest/webhooks/1.0/webhook returned no id".into(),
            )
        })?;
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            // Atlassian generates the webhook signing secret
            // out-of-band; surface the configured secret if present,
            // else record a placeholder so the substrate can sign
            // incoming requests once the operator fills it in.
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("confluence-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            // Confluence webhooks have no provider-side TTL; we
            // refresh at most monthly.
            Some(Utc::now() + chrono::Duration::days(30)),
        );
        subscription.provider_subscription_id = Some(webhook_id.to_string());
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Confluence posts one event per HTTP request.
        let p: ConfluenceWebhookPayload = serde_json::from_slice(body)?;
        let occurred_at = p
            .timestamp
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .unwrap_or_else(Utc::now);
        let event = match p.webhook_event.as_str() {
            "page_created" => {
                let c = p
                    .page
                    .ok_or_else(|| ConnectorError::Webhook("missing page body".into()))?;
                ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new(c.id),
                    occurred_at,
                }
            }
            "page_updated" => {
                let c = p
                    .page
                    .ok_or_else(|| ConnectorError::Webhook("missing page body".into()))?;
                ConnectorEvent::DocumentUpdated {
                    document_id: SourceDocumentId::new(c.id),
                    occurred_at,
                }
            }
            "page_removed" | "page_trashed" => {
                let c = p
                    .page
                    .ok_or_else(|| ConnectorError::Webhook("missing page body".into()))?;
                ConnectorEvent::DocumentDeleted {
                    document_id: SourceDocumentId::new(c.id),
                    occurred_at,
                }
            }
            "space_permissions_updated" => {
                let id = p
                    .content_id
                    .or_else(|| p.page.as_ref().map(|c| c.id.clone()))
                    .ok_or_else(|| {
                        ConnectorError::Webhook(
                            "permission event missing contentId / page.id".into(),
                        )
                    })?;
                ConnectorEvent::PermissionChanged {
                    document_id: SourceDocumentId::new(id),
                    user_id: SourceUserId::new(p.account_id.unwrap_or_default()),
                    new_level: p.new_role.as_deref().and_then(parse_role),
                    occurred_at,
                }
            }
            other => {
                return Err(ConnectorError::Webhook(format!(
                    "unknown Confluence webhookEvent: {other}"
                )))
            }
        };
        Ok(vec![event])
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

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "confluence-access",
                "confluence-refresh",
                Utc::now() + Duration::hours(1),
                "read:confluence-content.all read:confluence-space.summary \
                 read:confluence-content.permission",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Confluence,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/confluence",
            "webhook_secret": "tenant-webhook-secret",
        }))
    }

    fn page(id: &str, version: u32, when: DateTime<Utc>) -> ConfluenceContent {
        ConfluenceContent {
            id: id.into(),
            kind: ContentType::Page,
            title: "Doc".into(),
            status: ContentStatus::Current,
            history: Some(ConfluenceHistory { created_date: when }),
            version: Some(ConfluenceVersion {
                when,
                number: version,
            }),
        }
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("confluence-content"));
        assert_eq!(tok.access_token.expose(), "confluence-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::Confluence,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_for_v1_and_advances_watermark() {
        let now = Utc::now();
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?limit=50&sort=-modified-date",
            ok_json(&serde_json::json!({
                "results": [page("c1", 1, now)],
                "_links": {}
            })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn initial_sync_follows_links_next_cursor() {
        let now = Utc::now();
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?limit=50&sort=-modified-date",
            ok_json(&serde_json::json!({
                "results": [page("c1", 1, now)],
                "_links": {
                    "next": "/wiki/api/v2/pages?cursor=abc&limit=50&sort=-modified-date"
                }
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?cursor=abc&limit=50&sort=-modified-date",
            ok_json(&serde_json::json!({
                "results": [page("c2", 2, now - Duration::minutes(5))],
                "_links": {}
            })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(matches!(
            res.events[1],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn incremental_sync_filters_against_prior_watermark() {
        let now = Utc::now();
        // Cursor (prior watermark) is now-1m; we expect only the
        // newer row (now) to be emitted.
        let watermark = (now - Duration::minutes(1)).to_rfc3339();
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?limit=50&sort=-modified-date",
            ok_json(&serde_json::json!({
                "results": [
                    page("c-new", 2, now),
                    page("c-old", 3, now - Duration::hours(2)),
                ],
                "_links": {}
            })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(watermark);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        match &res.events[0] {
            ConnectorEvent::DocumentUpdated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "c-new");
            }
            other => panic!("expected DocumentUpdated for c-new, got {other:?}"),
        }
    }

    #[test]
    fn incremental_sync_short_circuits_pagination_at_first_stale_page() {
        // Pins the server-bounded incremental loading invariant:
        // because the v2 endpoint sorts by `-modified-date`
        // (newest first), once the response contains a row at or
        // below the prior watermark, `paginate_pages` must stop
        // immediately without following `_links.next`.
        //
        // We register a SINGLE canned response for page 1 — page 1
        // contains 1 fresh row + 1 stale row + a `_links.next`
        // pointing at a hypothetical page 2. We do NOT register a
        // response for page 2. If the short-circuit ever
        // regresses, the connector will issue a GET for page 2,
        // the mock will fall through to `mock_not_configured`
        // (HTTP 404), and `incremental_sync` will return
        // `Err(ConnectorError::Sync(...))`. The `.unwrap()` below
        // makes that failure mode loud.
        let now = Utc::now();
        let watermark = (now - Duration::minutes(1)).to_rfc3339();
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?limit=50&sort=-modified-date",
            ok_json(&serde_json::json!({
                "results": [
                    // Strictly newer than the watermark — keep.
                    page("c-new", 2, now),
                    // At or below the watermark — drop, and skip
                    // page 2 entirely.
                    page("c-old", 3, now - Duration::hours(2)),
                ],
                "_links": {
                    // If this URL ever gets fetched, the test
                    // fails — no expectation is registered for it.
                    "next": "/wiki/api/v2/pages?cursor=should-not-fetch&limit=50&sort=-modified-date"
                }
            })),
        );
        let recorder = Arc::clone(&transport);
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(watermark);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // Only the fresh row was emitted.
        assert_eq!(res.events.len(), 1);
        // Explicit positive assertion: exactly ONE GET landed on
        // the transport — page 2 was never fetched.
        let requests = recorder.recorded();
        assert_eq!(
            requests.len(),
            1,
            "expected short-circuit to issue only one GET, got {}: {:?}",
            requests.len(),
            requests.iter().map(|r| &r.url).collect::<Vec<_>>()
        );
        assert!(
            !requests[0].url.contains("should-not-fetch"),
            "first request should be the initial page, not the next-cursor URL"
        );
    }

    #[test]
    fn pagination_aborts_on_repeated_cursor() {
        let now = Utc::now();
        // Mis-shaped server response — every page echoes the same
        // `_links.next` path. Each `expect` call registers a single
        // canned response; the second hit on the same URL will fall
        // through to "mock_not_configured" if our loop guard didn't
        // catch the duplicate. The guard short-circuits on the
        // second observation of the same cursor.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?limit=50&sort=-modified-date",
            ok_json(&serde_json::json!({
                "results": [page("c1", 1, now)],
                "_links": {
                    "next": "/wiki/api/v2/pages?limit=50&sort=-modified-date"
                }
            })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
    }

    #[test]
    fn list_500_propagates_as_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?limit=50&sort=-modified-date",
            MockResponse::status(500, b"upstream boom".to_vec()),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn list_401_propagates_as_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages?limit=50&sort=-modified-date",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn subscribe_webhook_posts_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/confluence/wiki/rest/webhooks/1.0/webhook",
            ok_json(&serde_json::json!({
                "id": 4242,
                "url": "https://demo.example/webhooks/confluence",
            })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://demo.example/webhooks/confluence")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("4242"));
        // Body must carry every event we care about — assert via
        // recorded request rather than re-parsing the URL.
        let recorded = transport.recorded();
        let webhook_req = recorded
            .iter()
            .find(|r| r.url.ends_with("/wiki/rest/webhooks/1.0/webhook"))
            .expect("webhook POST recorded");
        let body: serde_json::Value = serde_json::from_slice(&webhook_req.body).unwrap();
        let events: Vec<&str> = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for needed in [
            "page_created",
            "page_updated",
            "page_removed",
            "page_trashed",
            "space_permissions_updated",
        ] {
            assert!(
                events.contains(&needed),
                "missing {needed} from webhook subscription body"
            );
        }
    }

    #[test]
    fn subscribe_webhook_errors_when_id_missing() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/confluence/wiki/rest/webhooks/1.0/webhook",
            ok_json(&serde_json::json!({})),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://demo.example/webhooks/confluence")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_parses_page_removed() {
        let body = serde_json::json!({
            "webhookEvent": "page_removed",
            "timestamp": Utc::now().timestamp_millis(),
            "page": page("c9", 5, Utc::now()),
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_parses_space_permission_change() {
        let body = serde_json::json!({
            "webhookEvent": "space_permissions_updated",
            "timestamp": Utc::now().timestamp_millis(),
            "contentId": "c-12",
            "accountId": "acc-7",
            "new_role": "edit",
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_event_errors() {
        let body = serde_json::json!({"webhookEvent": "weird"});
        let transport = Arc::new(MockHttpTransport::new());
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    // ───────────── fetch_content ─────────────

    #[test]
    fn fetch_content_strips_storage_xhtml_and_includes_labels_and_space() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/123?body-format=storage",
            ok_json(&serde_json::json!({
                "id": "123",
                "title": "Runbook",
                "spaceId": 4567,
                "body": { "storage": {
                    "representation": "storage",
                    "value": "<h1>Intro</h1><p>Step <strong>one</strong> &amp; two.</p>"
                }},
                // Real Confluence Cloud v2 `webui` links are relative to
                // the `/wiki` context root (no `/wiki` prefix of their own).
                "_links": { "webui": "/spaces/OPS/pages/123/Runbook" }
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/123/labels",
            ok_json(&serde_json::json!({
                "results": [ { "name": "runbook" }, { "name": "oncall" } ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/spaces/4567",
            ok_json(&serde_json::json!({ "id": "4567", "key": "OPS" })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("123"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Runbook"));
        assert!(body.contains("Step one & two."));
        assert!(body.contains("Labels: runbook, oncall"));
        assert_eq!(fc.mime_type, "text/markdown");
        assert_eq!(fc.title.as_deref(), Some("Runbook"));
        // The relative `webui` is joined onto the `/wiki` context root.
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://api.test/confluence/wiki/spaces/OPS/pages/123/Runbook")
        );
        assert_eq!(fc.metadata["space_key"], serde_json::json!("OPS"));
        assert_eq!(
            fc.metadata["labels"],
            serde_json::json!(["runbook", "oncall"])
        );
    }

    #[test]
    fn fetch_content_handles_page_without_space_or_labels() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/9?body-format=storage",
            ok_json(&serde_json::json!({
                "id": "9",
                "title": "Bare",
                "body": { "storage": { "value": "<p>Hi</p>" } }
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/9/labels",
            ok_json(&serde_json::json!({ "results": [] })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("9"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert_eq!(body, "# Bare\n\nHi");
        assert_eq!(fc.metadata["space_key"], serde_json::Value::Null);
        assert!(fc.source_url.is_none());
    }

    #[test]
    fn fetch_content_returns_body_when_label_and_space_lookups_fail() {
        // The labels and space-key calls are best-effort enrichment: a
        // 429 on labels and a 500 on the space lookup must not discard
        // the already-fetched page body.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/123?body-format=storage",
            ok_json(&serde_json::json!({
                "id": "123",
                "title": "Runbook",
                "spaceId": 4567,
                "body": { "storage": { "value": "<p>Body survives</p>" } }
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/123/labels",
            MockResponse::status(429, b"slow down".to_vec()),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/spaces/4567",
            MockResponse::status(500, b"boom".to_vec()),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("123"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("Body survives"));
        // Enrichment degraded gracefully to empty/none.
        assert_eq!(fc.metadata["labels"], serde_json::json!([]));
        assert_eq!(fc.metadata["space_key"], serde_json::Value::Null);
    }

    #[test]
    fn fetch_content_prefers_links_base_for_source_url() {
        // When the API returns an absolute `_links.base` (which already
        // includes the `/wiki` context root), it is used verbatim in
        // preference to the configured connector base URL.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/5?body-format=storage",
            ok_json(&serde_json::json!({
                "id": "5",
                "title": "Linked",
                "body": { "storage": { "value": "<p>x</p>" } },
                "_links": {
                    "base": "https://acme.atlassian.net/wiki",
                    "webui": "/spaces/ENG/pages/5/Linked"
                }
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/5/labels",
            ok_json(&serde_json::json!({ "results": [] })),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("5"))
            .unwrap();
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://acme.atlassian.net/wiki/spaces/ENG/pages/5/Linked")
        );
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/404?body-format=storage",
            MockResponse::status(404, br#"{"errors":[{"status":404}]}"#.to_vec()),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("404"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_429_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/confluence/wiki/api/v2/pages/7?body-format=storage",
            MockResponse::status(429, b"slow down".to_vec()),
        );
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("7"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }
}
