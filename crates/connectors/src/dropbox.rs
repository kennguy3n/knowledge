//! Dropbox connector — Dropbox API v2.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`] against `https://api.dropboxapi.com/oauth2/token`
//!   (production: real `OAuth2Client`; tests: `MockHttpTransport`).
//! * `initial_sync` POSTs `/2/files/list_folder` (recursive) and walks
//!   `/2/files/list_folder/continue` until `has_more` is `false`,
//!   emitting one [`ConnectorEvent`] per file / deleted entry. The
//!   terminal `cursor` becomes the substrate-side watermark.
//! * `incremental_sync` POSTs `/2/files/list_folder/continue` with the
//!   stored cursor — Dropbox's contract is that the cursor carries the
//!   entire server-state delta, so a single continue chain returns
//!   exactly the entries that changed since the prior run.
//! * `fetch_content` POSTs `/2/files/download` on the content host with
//!   the file id in the `Dropbox-API-Arg` header and returns the raw
//!   bytes (Dropbox returns the file metadata in the
//!   `Dropbox-API-Result` response header).
//! * `subscribe_webhook` does **not** make an HTTP call — Dropbox
//!   webhooks are registered out-of-band in the App Console, so the
//!   connector just describes the callback + the app-secret used to
//!   verify the `X-Dropbox-Signature` HMAC.
//! * `handle_webhook_event` parses Dropbox's account-level change
//!   notification. Dropbox notifications carry **no per-file detail**
//!   (only the set of accounts that changed), so a valid notification
//!   yields an empty event vec — the runtime reacts by scheduling an
//!   `incremental_sync`. An unparseable body is rejected.
//!
//! Wiring contract (mirror of the OneDrive / Google Drive / Jira
//! connectors): the constructor takes an `Arc<dyn HttpTransport>` and
//! an `Arc<dyn OAuth2CodeExchange>`; production wires
//! `BlockingHttpTransport` + `OAuth2Client`, tests wire
//! `MockHttpTransport` + a fixed-token exchange.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_post_json, classify_failure, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

use crate::content::{response_header, strip_charset};

/// Default Dropbox RPC base URL (list / metadata endpoints). Override
/// via `auth_config_json.api_base_url` for sandboxes / proxies.
pub const DEFAULT_API_BASE_URL: &str = "https://api.dropboxapi.com";

/// Default Dropbox content host (file download / upload). Override via
/// `auth_config_json.content_base_url`.
pub const DEFAULT_CONTENT_BASE_URL: &str = "https://content.dropboxapi.com";

/// Safety ceiling on number of `list_folder/continue` pages a single
/// sync will walk — catches mis-shaped responses that keep
/// `has_more = true` forever.
pub const MAX_LIST_PAGES: usize = 10_000;

/// One entry in a `list_folder` response. Dropbox tags the entry
/// shape via the `.tag` discriminator (`file`, `folder`, `deleted`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DropboxEntry {
    /// Entry kind discriminator (`file`, `folder`, `deleted`).
    #[serde(default, rename = ".tag")]
    pub tag: String,
    /// File id (`id:...`). Present on `file` / `folder`, absent on
    /// `deleted` entries (Dropbox only echoes the path on a delete).
    #[serde(default)]
    pub id: String,
    /// Display name (final path component).
    #[serde(default)]
    pub name: String,
    /// Lower-cased full path — the stable key for `deleted` entries
    /// that carry no id.
    #[serde(default, rename = "path_lower")]
    pub path_lower: String,
    /// Server-side last-modified timestamp (files only).
    #[serde(default, rename = "server_modified")]
    pub server_modified: Option<DateTime<Utc>>,
    /// Client-reported last-modified timestamp (files only).
    #[serde(default, rename = "client_modified")]
    pub client_modified: Option<DateTime<Utc>>,
}

/// A `/2/files/list_folder` (or `/continue`) response page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListFolderResponse {
    /// Entries on this page.
    #[serde(default)]
    pub entries: Vec<DropboxEntry>,
    /// Opaque cursor to pass to `/continue` for the next page (and,
    /// once `has_more` is false, the watermark for the next
    /// incremental sync).
    #[serde(default)]
    pub cursor: String,
    /// Whether more pages remain.
    #[serde(default, rename = "has_more")]
    pub has_more: bool,
}

/// Subset of the `Dropbox-API-Result` header Dropbox echoes on a
/// successful `/2/files/download`.
#[derive(Debug, Clone, Default, Deserialize)]
struct DownloadResultMeta {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "path_lower")]
    path_lower: String,
}

/// Dropbox account-level change notification (the body Dropbox POSTs
/// to a registered webhook URL).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DropboxNotification {
    /// Accounts whose file tree changed (delta-style notification).
    #[serde(default)]
    pub list_folder: Option<DropboxListFolderAccounts>,
    /// Legacy `delta` notification (older apps).
    #[serde(default)]
    pub delta: Option<DropboxDeltaUsers>,
}

/// `list_folder.accounts` sub-object of a Dropbox notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DropboxListFolderAccounts {
    /// Dropbox account ids (`dbid:...`) with pending changes.
    #[serde(default)]
    pub accounts: Vec<String>,
}

/// `delta.users` sub-object of a legacy Dropbox notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DropboxDeltaUsers {
    /// Numeric Dropbox user ids with pending changes.
    #[serde(default)]
    pub users: Vec<i64>,
}

/// Dropbox connector.
///
/// Holds the wired [`HttpTransport`] + [`OAuth2CodeExchange`] used to
/// drive every Dropbox RPC (token exchange, folder listing, download).
pub struct DropboxConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    content_base_url: String,
}

impl std::fmt::Debug for DropboxConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropboxConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("content_base_url", &self.content_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl DropboxConnector {
    /// Construct a Dropbox connector.
    ///
    /// `transport` carries every RPC; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://api.dropboxapi.com/oauth2/token`.
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
            content_base_url: DEFAULT_CONTENT_BASE_URL.to_string(),
        }
    }

    /// Override the Dropbox RPC base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the Dropbox content host base URL.
    #[must_use]
    pub fn with_content_base_url(mut self, url: impl Into<String>) -> Self {
        self.content_base_url = url.into();
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

    fn resolved_content_base_url(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("content_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.content_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    /// Root path the connector lists from. Defaults to `""` (the whole
    /// drive). Override via `auth_config_json.root_path`.
    fn resolved_root_path(config: &ConnectorConfig) -> &str {
        config
            .auth_config_json
            .get("root_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    }

    /// Walk a `list_folder` chain from `first` until `has_more` is
    /// false, following `/continue` with each returned cursor. Returns
    /// the merged entries + the terminal cursor.
    fn paginate(
        &self,
        first: ListFolderResponse,
        base_url: &str,
        token: &OAuth2Token,
    ) -> Result<(Vec<DropboxEntry>, String)> {
        let mut entries = first.entries;
        let mut cursor = first.cursor;
        let mut has_more = first.has_more;
        let continue_url = format!("{base_url}/2/files/list_folder/continue");
        for _ in 0..MAX_LIST_PAGES {
            if !has_more {
                return Ok((entries, cursor));
            }
            let page: ListFolderResponse = bearer_post_json(
                &self.transport,
                "dropbox",
                "/2/files/list_folder/continue",
                &continue_url,
                token,
                &[],
                &serde_json::json!({ "cursor": cursor }),
            )?;
            entries.extend(page.entries);
            cursor = page.cursor;
            has_more = page.has_more;
        }
        Err(ConnectorError::Sync(format!(
            "dropbox /2/files/list_folder/continue exceeded {MAX_LIST_PAGES} pages without clearing has_more"
        )))
    }
}

/// Which sync pass produced an entry — controls whether a file entry
/// maps to `DocumentCreated` (initial walk) or `DocumentUpdated`
/// (incremental delta). Mirrors the `SyncMode` enum in the OneDrive /
/// Notion connectors.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

/// Map one Dropbox entry to a substrate event. Returns `None` for
/// `folder` entries (folders are not ingestible documents).
fn entry_to_event(entry: &DropboxEntry, mode: SyncMode) -> Option<ConnectorEvent> {
    match entry.tag.as_str() {
        "deleted" => {
            // Deleted entries carry no id — key off path_lower (the
            // stable Dropbox identifier across a file's lifetime).
            let key = if entry.path_lower.is_empty() {
                entry.name.clone()
            } else {
                entry.path_lower.clone()
            };
            Some(ConnectorEvent::DocumentDeleted {
                document_id: SourceDocumentId::new(key),
                occurred_at: Utc::now(),
            })
        }
        "file" => {
            let occurred_at = entry
                .server_modified
                .or(entry.client_modified)
                .unwrap_or_else(Utc::now);
            let key = if entry.id.is_empty() {
                entry.path_lower.clone()
            } else {
                entry.id.clone()
            };
            let document_id = SourceDocumentId::new(key);
            Some(match mode {
                SyncMode::Initial => ConnectorEvent::DocumentCreated {
                    document_id,
                    occurred_at,
                },
                SyncMode::Incremental => ConnectorEvent::DocumentUpdated {
                    document_id,
                    occurred_at,
                },
            })
        }
        // Folders (and any unknown future tag) are not documents.
        _ => None,
    }
}

impl Connector for DropboxConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "dropbox authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let root = Self::resolved_root_path(config);
        let url = format!("{base_url}/2/files/list_folder");
        let first: ListFolderResponse = bearer_post_json(
            &self.transport,
            "dropbox",
            "/2/files/list_folder",
            &url,
            token,
            &[],
            &serde_json::json!({
                "path": root,
                "recursive": true,
                "include_deleted": false,
            }),
        )?;
        let (entries, cursor) = self.paginate(first, &base_url, token)?;
        let events: Vec<ConnectorEvent> = entries
            .iter()
            .filter_map(|e| entry_to_event(e, SyncMode::Initial))
            .collect();
        Ok(SyncRunResult {
            events,
            // Empty cursor (a degenerate/empty account) is surfaced as
            // "no further pages" rather than persisting a blank cursor
            // the next continue would reject.
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
        // Dropbox's continue cursor carries the full server-state
        // delta — without it we cannot incrementally fetch; surface
        // the gap so the substrate reschedules with the seed populated.
        let cursor = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "dropbox incremental_sync: missing cursor; \
                 initial_sync must populate the list_folder cursor first"
                    .into(),
            )
        })?;
        let continue_url = format!("{base_url}/2/files/list_folder/continue");
        let first: ListFolderResponse = bearer_post_json(
            &self.transport,
            "dropbox",
            "/2/files/list_folder/continue",
            &continue_url,
            token,
            &[],
            &serde_json::json!({ "cursor": cursor }),
        )?;
        let (entries, new_cursor) = self.paginate(first, &base_url, token)?;
        let events: Vec<ConnectorEvent> = entries
            .iter()
            .filter_map(|e| entry_to_event(e, SyncMode::Incremental))
            .collect();
        // Dropbox always returns a fresh cursor; fall back to the
        // existing one if the response somehow omitted it so we never
        // lose our place.
        let next_cursor = if new_cursor.is_empty() {
            Some(cursor.to_string())
        } else {
            Some(new_cursor)
        };
        Ok(SyncRunResult {
            events,
            next_cursor,
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let content_base = self.resolved_content_base_url(config);
        let id = document_id.as_str();
        let url = format!("{content_base}/2/files/download");
        // Dropbox's download contract: the file selector goes in the
        // `Dropbox-API-Arg` header as JSON, the request body is empty,
        // and the bytes come back as the response body. The selector
        // accepts an `id:...`, a `rev:...`, or a `/path`.
        let api_arg = serde_json::json!({ "path": id }).to_string();
        let req = HttpRequest::post(&url, Vec::new())
            .with_bearer(token.access_token.expose())
            .with_header("Dropbox-API-Arg", &api_arg);
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("dropbox", "/2/files/download", &resp));
        }

        // Metadata rides back in the `Dropbox-API-Result` header.
        let meta: DownloadResultMeta = response_header(&resp, "dropbox-api-result")
            .and_then(|h| serde_json::from_str(h).ok())
            .unwrap_or_default();
        let title = if meta.name.is_empty() {
            // Fall back to the final path component of the id/path.
            id.rsplit('/').next().unwrap_or(id).to_string()
        } else {
            meta.name
        };
        let mime = response_header(&resp, "content-type")
            .map(strip_charset)
            .filter(|m| !m.is_empty())
            .map_or_else(
                || "application/octet-stream".to_string(),
                std::string::ToString::to_string,
            );
        let fc = FetchedContent::binary(resp.body, mime)
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "dropbox",
                "file_id": id,
                "path_lower": meta.path_lower,
            }));
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Dropbox webhook URLs are registered out-of-band in the App
        // Console (there is no REST endpoint to create one), so this
        // does not hit the network. We surface the configured app
        // secret — Dropbox signs every notification with
        // `X-Dropbox-Signature = HMAC-SHA256(app_secret, body)`, which
        // the substrate's webhook server verifies.
        let secret = config
            .auth_config_json
            .get("app_secret")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("dropbox-webhook-secret");
        let subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            // Dropbox webhooks do not expire.
            None,
        );
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Dropbox notifications are intentionally content-free: they
        // name the *accounts* that changed, never the files. There is
        // no per-document delta to emit here — the substrate reacts to
        // a valid notification by scheduling an `incremental_sync`,
        // which walks the `list_folder/continue` cursor for the real
        // changes. We therefore validate the shape and return an empty
        // event vec (NOT an error — an empty vec is the correct,
        // non-dropping representation for this provider).
        let notification: DropboxNotification = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!("dropbox webhook: malformed notification body: {e}"))
        })?;
        if notification.list_folder.is_none() && notification.delta.is_none() {
            return Err(ConnectorError::Webhook(
                "dropbox webhook: notification carried neither list_folder nor delta".into(),
            ));
        }
        Ok(Vec::new())
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
                "dropbox-access",
                "dropbox-refresh",
                Utc::now() + Duration::hours(1),
                "files.metadata.read files.content.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Dropbox, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/dbx",
                "content_base_url": "https://content.test/dbx",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn file_entry(id: &str, name: &str, modified: DateTime<Utc>) -> serde_json::Value {
        serde_json::json!({
            ".tag": "file",
            "id": id,
            "name": name,
            "path_lower": format!("/{name}"),
            "server_modified": modified,
        })
    }

    const LIST_URL: &str = "https://api.test/dbx/2/files/list_folder";
    const CONTINUE_URL: &str = "https://api.test/dbx/2/files/list_folder/continue";

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "dropbox-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::Dropbox, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_events_and_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            LIST_URL,
            ok_json(&serde_json::json!({
                "entries": [file_entry("id:a", "a.txt", now)],
                "cursor": "cur-1",
                "has_more": false,
            })),
        );
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("cur-1"));
    }

    #[test]
    fn initial_sync_paginates_via_continue() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            LIST_URL,
            ok_json(&serde_json::json!({
                "entries": [file_entry("id:a", "a.txt", now)],
                "cursor": "cur-1",
                "has_more": true,
            })),
        );
        transport.expect(
            HttpMethod::Post,
            CONTINUE_URL,
            ok_json(&serde_json::json!({
                "entries": [file_entry("id:b", "b.txt", now)],
                "cursor": "cur-2",
                "has_more": false,
            })),
        );
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(res.next_cursor.as_deref(), Some("cur-2"));
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn initial_sync_skips_folder_entries() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            LIST_URL,
            ok_json(&serde_json::json!({
                "entries": [
                    { ".tag": "folder", "id": "id:dir", "name": "dir", "path_lower": "/dir" },
                    file_entry("id:a", "a.txt", now),
                ],
                "cursor": "cur-1",
                "has_more": false,
            })),
        );
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1, "folder entry must be skipped");
        assert_eq!(res.events[0].document_id().as_str(), "id:a");
    }

    #[test]
    fn incremental_sync_uses_cursor_and_emits_updates_and_deletes() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            CONTINUE_URL,
            ok_json(&serde_json::json!({
                "entries": [
                    file_entry("id:a", "a.txt", now),
                    { ".tag": "deleted", "name": "b.txt", "path_lower": "/b.txt" },
                ],
                "cursor": "cur-3",
                "has_more": false,
            })),
        );
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("cur-prev".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        match &res.events[1] {
            ConnectorEvent::DocumentDeleted { document_id, .. } => {
                assert_eq!(document_id.as_str(), "/b.txt");
            }
            other => panic!("expected delete, got {other:?}"),
        }
        assert_eq!(res.next_cursor.as_deref(), Some("cur-3"));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            LIST_URL,
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn fetch_content_downloads_bytes_and_metadata() {
        let transport = Arc::new(MockHttpTransport::new());
        let mut resp = MockResponse::status(200, b"hello bytes".to_vec());
        resp.headers
            .push(("content-type".into(), "text/plain".into()));
        resp.headers.push((
            "dropbox-api-result".into(),
            serde_json::json!({ "name": "a.txt", "path_lower": "/a.txt" }).to_string(),
        ));
        transport.expect(
            HttpMethod::Post,
            "https://content.test/dbx/2/files/download",
            resp,
        );
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("id:a"))
            .unwrap();
        assert_eq!(fc.body, b"hello bytes");
        assert_eq!(fc.mime_type, "text/plain");
        assert_eq!(fc.title.as_deref(), Some("a.txt"));
        assert_eq!(fc.metadata["file_id"], serde_json::json!("id:a"));
    }

    #[test]
    fn fetch_content_maps_409_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://content.test/dbx/2/files/download",
            MockResponse::status(409, br#"{"error":"path/not_found"}"#.to_vec()),
        );
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("id:nope"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn subscribe_webhook_makes_no_http_call_and_carries_secret() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let cfg = ConnectorConfig::new(ConnectorKind::Dropbox, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "app_secret": "s3cr3t" }));
        let sub = c
            .subscribe_webhook(&cfg, &tok, "https://hook.example/dropbox")
            .unwrap();
        assert_eq!(sub.connector, c.instance);
        assert_eq!(sub.secret.expose(), "s3cr3t");
        assert!(
            transport.recorded().is_empty(),
            "Dropbox subscribe must not hit the network"
        );
    }

    #[test]
    fn webhook_valid_notification_yields_empty_events() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "list_folder": { "accounts": ["dbid:AAA"] }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_legacy_delta_notification_is_accepted() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "delta": { "users": [12345] } });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_empty_notification_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "unrelated": true });
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_malformed_body_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DropboxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
