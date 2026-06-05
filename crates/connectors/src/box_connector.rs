//! Box connector — Box Content API v2.
//!
//! The module is named `box_connector` (and the type `BoxConnector`)
//! because `box` is a reserved Rust keyword.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`] against `https://api.box.com/oauth2/token`
//!   (production: real `OAuth2Client`; tests: `MockHttpTransport`).
//! * `initial_sync` walks `/2.0/folders/{id}/items` breadth-first from
//!   the configured root folder (default `0`), paginating each folder
//!   via `limit` / `offset`, and emits one [`ConnectorEvent`] per file.
//!   It then seeds the substrate cursor from `/2.0/events` so the next
//!   incremental run resumes from the correct stream position.
//! * `incremental_sync` walks `/2.0/events?stream_position=<cursor>`
//!   (the long-poll change feed), mapping each `event_type` to the
//!   appropriate [`ConnectorEvent`]; the `next_stream_position` becomes
//!   the next cursor.
//! * `fetch_content` GETs `/2.0/files/{id}` for metadata and
//!   `/2.0/files/{id}/content` for the bytes.
//! * `subscribe_webhook` POSTs `/2.0/webhooks` targeting the root
//!   folder; the returned webhook id is stashed in
//!   `provider_subscription_id`.
//! * `handle_webhook_event` parses Box's notification payload. Box
//!   normally delivers one trigger per POST, but the handler also
//!   accepts a top-level array so a batched delivery never silently
//!   drops events after the first.
//!
//! Wiring contract (mirror of the other connectors): the constructor
//! takes an `Arc<dyn HttpTransport>` and an `Arc<dyn OAuth2CodeExchange>`.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

use crate::content::{bearer_get_raw, response_header, strip_charset};

/// Default Box API base URL. Override via
/// `auth_config_json.api_base_url` for sandboxes / proxies.
pub const DEFAULT_API_BASE_URL: &str = "https://api.box.com";

/// Default folder-items page size. Box's documented maximum is 1000;
/// we stay at 100 to match the other connectors' batching cadence.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of folder-item pages a single walk may
/// fetch — catches mis-shaped responses that never advance the offset.
pub const MAX_LIST_PAGES: usize = 100_000;

/// Safety ceiling on number of distinct folders a single
/// `initial_sync` will descend into.
pub const MAX_FOLDERS: usize = 100_000;

/// Safety ceiling on number of `/2.0/events` pages an incremental walk
/// will fetch in one run.
pub const MAX_EVENT_PAGES: usize = 10_000;

/// `fields` mask requested for folder items — only what the substrate
/// parses, to keep responses small.
const ITEM_FIELDS_MASK: &str = "id,name,type,modified_at,created_at";

/// One entry in a `/2.0/folders/{id}/items` collection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxItem {
    /// Item id.
    #[serde(default)]
    pub id: String,
    /// Item kind (`file`, `folder`, `web_link`).
    #[serde(default, rename = "type")]
    pub item_type: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// RFC-3339 last-modified timestamp.
    #[serde(default, rename = "modified_at")]
    pub modified_at: Option<DateTime<Utc>>,
    /// RFC-3339 creation timestamp.
    #[serde(default, rename = "created_at")]
    pub created_at: Option<DateTime<Utc>>,
}

/// A page of `/2.0/folders/{id}/items`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxItemCollection {
    /// Total number of items in the folder (across all pages).
    #[serde(default)]
    pub total_count: u32,
    /// Items on this page.
    #[serde(default)]
    pub entries: Vec<BoxItem>,
}

/// The `source` object Box attaches to a change event / webhook.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxSource {
    /// Source item id.
    #[serde(default)]
    pub id: String,
    /// Source item type (`file`, `folder`).
    #[serde(default, rename = "type")]
    pub source_type: String,
}

/// One entry in a `/2.0/events` change-feed page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxEvent {
    /// Box event type (`ITEM_CREATE`, `ITEM_UPLOAD`, `ITEM_TRASH`, …).
    #[serde(default)]
    pub event_type: String,
    /// Event creation time.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// The item the event refers to.
    #[serde(default)]
    pub source: Option<BoxSource>,
}

/// A `/2.0/events` response page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxEventCollection {
    /// Number of events in this chunk.
    #[serde(default)]
    pub chunk_size: u32,
    /// Cursor to resume from on the next poll.
    #[serde(default)]
    pub next_stream_position: serde_json::Value,
    /// Events in this chunk.
    #[serde(default)]
    pub entries: Vec<BoxEvent>,
}

/// `/2.0/webhooks` create response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxWebhookResponse {
    /// Webhook id Box assigned (needed for revoke).
    #[serde(default)]
    pub id: Option<String>,
}

/// File metadata subset needed by `fetch_content`.
#[derive(Debug, Clone, Default, Deserialize)]
struct BoxFileMeta {
    #[serde(default)]
    name: String,
}

/// Box connector. Holds the wired transport + OAuth exchange.
pub struct BoxConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for BoxConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl BoxConnector {
    /// Construct a Box connector.
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

    /// Override the Box API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
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

    /// Root folder id to walk. Defaults to `0` (the Box "All Files"
    /// root). Override via `auth_config_json.root_folder_id`.
    fn resolved_root_folder(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("root_folder_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("0")
            .to_string()
    }

    /// List every item in one folder, paginating via `limit` / `offset`.
    fn list_folder_items(
        &self,
        base_url: &str,
        folder_id: &str,
        token: &OAuth2Token,
    ) -> Result<Vec<BoxItem>> {
        let mut items = Vec::<BoxItem>::new();
        let mut offset: u32 = 0;
        let limit = self.page_size;
        let folder_enc = percent_encode_path_component(folder_id);
        for _ in 0..MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/2.0/folders/{folder_enc}/items\
                 ?limit={limit}&offset={offset}&fields={ITEM_FIELDS_MASK}"
            );
            let page: BoxItemCollection = bearer_get_json(
                &self.transport,
                "box",
                "/2.0/folders/{id}/items",
                &url,
                token,
                &[],
            )?;
            let got = page.entries.len();
            let total = page.total_count;
            items.extend(page.entries);
            offset = offset.saturating_add(limit);
            // Stop when we've covered the reported total or the server
            // returned a short page (no more items).
            if got < limit as usize || offset >= total {
                return Ok(items);
            }
        }
        Err(ConnectorError::Sync(format!(
            "box /2.0/folders/{folder_id}/items exceeded {MAX_LIST_PAGES} pages"
        )))
    }

    /// Breadth-first walk from `root`, collecting every file item.
    fn walk_files(
        &self,
        base_url: &str,
        root: String,
        token: &OAuth2Token,
    ) -> Result<Vec<BoxItem>> {
        let mut files = Vec::<BoxItem>::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(root);
        let mut folders_seen = 0usize;
        while let Some(folder_id) = queue.pop_front() {
            folders_seen += 1;
            if folders_seen > MAX_FOLDERS {
                return Err(ConnectorError::Sync(format!(
                    "box initial_sync exceeded {MAX_FOLDERS} folders"
                )));
            }
            for item in self.list_folder_items(base_url, &folder_id, token)? {
                match item.item_type.as_str() {
                    "folder" => queue.push_back(item.id),
                    "file" => files.push(item),
                    _ => {}
                }
            }
        }
        Ok(files)
    }

    /// Fetch the current event stream position to seed the cursor.
    fn current_stream_position(&self, base_url: &str, token: &OAuth2Token) -> Result<String> {
        let url = format!("{base_url}/2.0/events?stream_position=now&stream_type=changes");
        let page: BoxEventCollection =
            bearer_get_json(&self.transport, "box", "/2.0/events", &url, token, &[])?;
        Ok(stream_position_to_string(&page.next_stream_position))
    }
}

/// Box returns `next_stream_position` as either a JSON number or
/// string depending on endpoint / account; normalise to a string.
fn stream_position_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Map a file discovered during the initial folder walk to a
/// `DocumentCreated` event. Incremental changes flow through the Box
/// events API instead (see [`box_event_to_event`]).
fn file_to_event(item: &BoxItem) -> ConnectorEvent {
    let occurred_at = item
        .modified_at
        .or(item.created_at)
        .unwrap_or_else(Utc::now);
    ConnectorEvent::DocumentCreated {
        document_id: SourceDocumentId::new(item.id.clone()),
        occurred_at,
    }
}

/// Map a Box `event_type` to a substrate event. Returns `None` for
/// event types that don't correspond to a document change the
/// substrate ingests (folder-only events, collaboration noise, …).
fn box_event_to_event(event: &BoxEvent) -> Option<ConnectorEvent> {
    let source = event.source.as_ref()?;
    // Only file changes are ingestible documents.
    if source.source_type != "file" || source.id.is_empty() {
        return None;
    }
    let occurred_at = event.created_at.unwrap_or_else(Utc::now);
    let document_id = SourceDocumentId::new(source.id.clone());
    match event.event_type.as_str() {
        "ITEM_CREATE" | "ITEM_UPLOAD" | "ITEM_COPY" | "ITEM_UNDELETE_VIA_TRASH" => {
            Some(ConnectorEvent::DocumentCreated {
                document_id,
                occurred_at,
            })
        }
        "ITEM_RENAME" | "ITEM_MODIFY" | "ITEM_MOVE" => Some(ConnectorEvent::DocumentUpdated {
            document_id,
            occurred_at,
        }),
        "ITEM_TRASH" => Some(ConnectorEvent::DocumentDeleted {
            document_id,
            occurred_at,
        }),
        _ => None,
    }
}

/// Map a Box webhook trigger to a substrate event.
fn box_trigger_to_event(trigger: &str, source: &BoxSource) -> Option<ConnectorEvent> {
    if source.source_type != "file" || source.id.is_empty() {
        return None;
    }
    let occurred_at = Utc::now();
    let document_id = SourceDocumentId::new(source.id.clone());
    match trigger {
        "FILE.UPLOADED" | "FILE.COPIED" | "FILE.RESTORED" => {
            Some(ConnectorEvent::DocumentCreated {
                document_id,
                occurred_at,
            })
        }
        "FILE.RENAMED" | "FILE.MOVED" | "FILE.LOCKED" | "FILE.UNLOCKED" => {
            Some(ConnectorEvent::DocumentUpdated {
                document_id,
                occurred_at,
            })
        }
        "FILE.TRASHED" | "FILE.DELETED" => Some(ConnectorEvent::DocumentDeleted {
            document_id,
            occurred_at,
        }),
        _ => None,
    }
}

/// One Box webhook notification (single-trigger delivery).
#[derive(Debug, Clone, Default, Deserialize)]
struct BoxWebhookNotification {
    #[serde(default)]
    trigger: String,
    #[serde(default)]
    source: Option<BoxSource>,
}

/// Box webhook bodies are normally a single object; accept an array
/// too so a batched delivery is fully drained.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum BoxWebhookBody {
    Batch(Vec<BoxWebhookNotification>),
    Single(BoxWebhookNotification),
}

impl Connector for BoxConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "box authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let root = Self::resolved_root_folder(config);
        // Seed the incremental cursor BEFORE the full walk so we never
        // miss a change that lands while we're paginating folders.
        let cursor = self.current_stream_position(&base_url, token)?;
        let files = self.walk_files(&base_url, root, token)?;
        let events: Vec<ConnectorEvent> = files.iter().map(file_to_event).collect();
        Ok(SyncRunResult {
            events,
            next_cursor: (!cursor.is_empty()).then_some(cursor),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let start = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "box incremental_sync: missing cursor; \
                 initial_sync must seed the event stream position first"
                    .into(),
            )
        })?;
        let mut position = start.to_string();
        let mut events: Vec<ConnectorEvent> = Vec::new();
        for _ in 0..MAX_EVENT_PAGES {
            let pos_enc = percent_encode_path_component(&position);
            let url =
                format!("{base_url}/2.0/events?stream_position={pos_enc}&stream_type=changes");
            let page: BoxEventCollection =
                bearer_get_json(&self.transport, "box", "/2.0/events", &url, token, &[])?;
            for ev in &page.entries {
                if let Some(e) = box_event_to_event(ev) {
                    events.push(e);
                }
            }
            let next = stream_position_to_string(&page.next_stream_position);
            // Box signals "caught up" by returning an empty chunk whose
            // next_stream_position equals the one we requested.
            if page.entries.is_empty() || next.is_empty() || next == position {
                position = if next.is_empty() { position } else { next };
                break;
            }
            position = next;
        }
        Ok(SyncRunResult {
            events,
            next_cursor: Some(position),
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

        // 1. Metadata for the title.
        let meta_url = format!("{base_url}/2.0/files/{id_enc}?fields=id,name");
        let meta: BoxFileMeta = bearer_get_json(
            &self.transport,
            "box",
            "/2.0/files/{id}",
            &meta_url,
            token,
            &[],
        )?;

        // 2. Content bytes (Box 302-redirects to a CDN; the transport
        //    follows the redirect, carrying the bearer through).
        let content_url = format!("{base_url}/2.0/files/{id_enc}/content");
        let resp = bearer_get_raw(
            &self.transport,
            "box",
            "/2.0/files/{id}/content",
            &content_url,
            token,
            &[],
        )?;
        let mime = response_header(&resp, "content-type")
            .map(strip_charset)
            .filter(|m| !m.is_empty())
            .map_or_else(
                || "application/octet-stream".to_string(),
                std::string::ToString::to_string,
            );
        let title = if meta.name.is_empty() {
            id.to_string()
        } else {
            meta.name
        };
        let fc = FetchedContent::binary(resp.body, mime)
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "box",
                "file_id": id,
            }));
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let root = Self::resolved_root_folder(config);
        let url = format!("{base_url}/2.0/webhooks");
        let body = serde_json::json!({
            "target": { "id": root, "type": "folder" },
            "address": callback_url,
            "triggers": [
                "FILE.UPLOADED",
                "FILE.TRASHED",
                "FILE.DELETED",
                "FILE.RENAMED",
                "FILE.MOVED",
                "FILE.COPIED",
                "FILE.RESTORED",
            ],
        });
        let resp: BoxWebhookResponse = bearer_post_json(
            &self.transport,
            "box",
            "/2.0/webhooks",
            &url,
            token,
            &[],
            &body,
        )?;
        // Box signs webhook deliveries with a configured primary key
        // (`Ok...`); the substrate verifies the `box-signature-*`
        // headers against it.
        let secret = config
            .auth_config_json
            .get("primary_signature_key")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("box-webhook-secret");
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            // Box webhooks do not expire.
            None,
        );
        subscription.provider_subscription_id = resp.id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let parsed: BoxWebhookBody = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!("box webhook: malformed notification body: {e}"))
        })?;
        let notifications = match parsed {
            BoxWebhookBody::Batch(v) => v,
            BoxWebhookBody::Single(n) => vec![n],
        };
        if notifications.is_empty() {
            return Err(ConnectorError::Webhook(
                "box webhook: empty notification batch".into(),
            ));
        }
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(notifications.len());
        for n in &notifications {
            let Some(source) = n.source.as_ref() else {
                continue;
            };
            if let Some(e) = box_trigger_to_event(&n.trigger, source) {
                events.push(e);
            }
        }
        Ok(events)
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
                "box-access",
                "box-refresh",
                Utc::now() + Duration::hours(1),
                "root_readonly",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Box, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/box",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const EVENTS_NOW: &str =
        "https://api.test/box/2.0/events?stream_position=now&stream_type=changes";

    fn root_items_url(offset: u32) -> String {
        format!(
            "https://api.test/box/2.0/folders/0/items\
             ?limit=100&offset={offset}&fields={ITEM_FIELDS_MASK}"
        )
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "box-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::Box, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_walks_folders_and_seeds_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            EVENTS_NOW,
            ok_json(&serde_json::json!({ "next_stream_position": 555, "entries": [] })),
        );
        // Root folder: one subfolder + one file.
        transport.expect(
            HttpMethod::Get,
            root_items_url(0),
            ok_json(&serde_json::json!({
                "total_count": 2,
                "entries": [
                    { "id": "f1", "type": "folder", "name": "sub" },
                    { "id": "10", "type": "file", "name": "a.txt", "modified_at": now },
                ]
            })),
        );
        // Subfolder f1: one file.
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/box/2.0/folders/f1/items\
                 ?limit=100&offset=0&fields={ITEM_FIELDS_MASK}"
            ),
            ok_json(&serde_json::json!({
                "total_count": 1,
                "entries": [
                    { "id": "11", "type": "file", "name": "b.txt", "modified_at": now },
                ]
            })),
        );
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(res.next_cursor.as_deref(), Some("555"));
    }

    #[test]
    fn list_folder_items_paginates_via_offset() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            EVENTS_NOW,
            ok_json(&serde_json::json!({ "next_stream_position": "1", "entries": [] })),
        );
        // page size is 100; emulate two full pages then a short one by
        // overriding page_size to a small value is not possible here,
        // so model total_count > returned to force a second fetch.
        let mut entries = Vec::new();
        for i in 0..100 {
            entries.push(serde_json::json!({
                "id": format!("{i}"), "type": "file", "name": format!("f{i}"), "modified_at": now
            }));
        }
        transport.expect(
            HttpMethod::Get,
            root_items_url(0),
            ok_json(&serde_json::json!({ "total_count": 101, "entries": entries })),
        );
        transport.expect(
            HttpMethod::Get,
            root_items_url(100),
            ok_json(&serde_json::json!({
                "total_count": 101,
                "entries": [ { "id": "100", "type": "file", "name": "f100", "modified_at": now } ]
            })),
        );
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 101);
    }

    #[test]
    fn incremental_sync_maps_event_types() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/box/2.0/events?stream_position=100&stream_type=changes",
            ok_json(&serde_json::json!({
                "next_stream_position": 200,
                "entries": [
                    { "event_type": "ITEM_UPLOAD", "created_at": now, "source": { "id": "1", "type": "file" } },
                    { "event_type": "ITEM_TRASH", "created_at": now, "source": { "id": "2", "type": "file" } },
                    { "event_type": "ITEM_CREATE", "created_at": now, "source": { "id": "3", "type": "folder" } },
                ]
            })),
        );
        // After advancing to 200, the connector polls again and gets a
        // caught-up (empty) chunk.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/box/2.0/events?stream_position=200&stream_type=changes",
            ok_json(&serde_json::json!({ "next_stream_position": 200, "entries": [] })),
        );
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("100".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // folder event filtered out, file create + delete kept.
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(matches!(
            res.events[1],
            ConnectorEvent::DocumentDeleted { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("200"));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn initial_sync_maps_403_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            EVENTS_NOW,
            MockResponse::status(403, b"forbidden".to_vec()),
        );
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn fetch_content_assembles_metadata_and_bytes() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/box/2.0/files/10?fields=id,name",
            ok_json(&serde_json::json!({ "id": "10", "name": "report.pdf" })),
        );
        let mut bytes = MockResponse::status(200, b"%PDF-1.7 data".to_vec());
        bytes
            .headers
            .push(("content-type".into(), "application/pdf".into()));
        transport.expect(
            HttpMethod::Get,
            "https://api.test/box/2.0/files/10/content",
            bytes,
        );
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("10"))
            .unwrap();
        assert_eq!(fc.mime_type, "application/pdf");
        assert_eq!(fc.title.as_deref(), Some("report.pdf"));
        assert_eq!(fc.body, b"%PDF-1.7 data");
    }

    #[test]
    fn subscribe_webhook_registers_and_keeps_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/box/2.0/webhooks",
            ok_json(&serde_json::json!({ "id": "wh-99" })),
        );
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/box")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh-99"));
        assert_eq!(sub.connector, c.instance);
    }

    #[test]
    fn webhook_single_notification_emits_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "trigger": "FILE.UPLOADED",
            "source": { "id": "42", "type": "file" }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "42");
    }

    #[test]
    fn webhook_batch_emits_every_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!([
            { "trigger": "FILE.UPLOADED", "source": { "id": "1", "type": "file" } },
            { "trigger": "FILE.TRASHED", "source": { "id": "2", "type": "file" } },
        ]);
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_unknown_trigger_is_skipped() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "trigger": "COLLABORATION.CREATED",
            "source": { "id": "9", "type": "file" }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_malformed_body_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
