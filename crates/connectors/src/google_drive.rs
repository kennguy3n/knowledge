//! Google Drive connector — Drive API v3.
//!
//! * `authenticate` POSTs the authorization code to
//!   `https://oauth2.googleapis.com/token` via the wired
//!   [`OAuth2CodeExchange`] (production: real `OAuth2Client` against
//!   Google's IdP; tests: `MockHttpTransport`).
//! * `initial_sync` walks `GET /drive/v3/files` keyed off
//!   `nextPageToken` and seeds the substrate-side cursor with the
//!   `startPageToken` Drive returns from
//!   `GET /drive/v3/changes/startPageToken`.
//! * `incremental_sync` walks `GET /drive/v3/changes?pageToken=<cursor>`,
//!   surfacing each change as the appropriate
//!   [`ConnectorEvent`] (created / updated / deleted). The
//!   `newStartPageToken` on the final page becomes the next cursor.
//! * `subscribe_webhook` POSTs
//!   `https://www.googleapis.com/drive/v3/changes/watch?pageToken=<token>`
//!   to install a push channel; Drive returns the channel id and
//!   `resourceId`, both of which we stash on the
//!   [`WebhookSubscription`] (`provider_subscription_id` carries the
//!   channel id, the resource id is recorded in the secret blob via
//!   the channel-state payload that Drive POSTs back to the
//!   substrate).
//! * `handle_webhook_event` parses Drive's resource-state push body
//!   into a [`ConnectorEvent`] (`add` → created, `update` → updated,
//!   `remove`/`trash` → deleted, `permission_change` →
//!   permission-changed).
//!
//! Wiring contract (mirror of the Jira / Confluence / HubSpot
//! connectors): the constructor takes an `Arc<dyn HttpTransport>` and
//! an `Arc<dyn OAuth2CodeExchange>`; production wires
//! `BlockingHttpTransport` + `OAuth2Client`, tests wire
//! `MockHttpTransport` + a fixed-token exchange.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SourcePermissionLevel, SourceUserId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

use crate::content::{bearer_get_raw, response_header, strip_charset};

/// Default Drive REST base URL. Override via
/// `auth_config_json.api_base_url` for sandboxes / proxies.
pub const DEFAULT_API_BASE_URL: &str = "https://www.googleapis.com";

/// Default page size for `files.list` / `changes.list`. Drive's
/// documented maximum is 1000 — we stay at 100 to match the other
/// connectors' batching cadence and keep error-path body capture
/// readable in logs.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk —
/// catches mis-shaped server responses that return a non-empty page
/// without ever clearing `nextPageToken`.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Default `fields` mask requested from `files.list` / `changes.list`.
/// Drive returns the full file body if you ask for it; we request
/// only the subset the substrate parses to keep responses small.
pub const FILE_FIELDS_MASK: &str = "id,name,mimeType,trashed,modifiedTime,createdTime";

/// `fields` mask used on the wrapping `files.list` page.
const FILE_LIST_FIELDS_MASK: &str =
    "nextPageToken,files(id,name,mimeType,trashed,modifiedTime,createdTime)";

/// `fields` mask used on the wrapping `changes.list` page.
const CHANGE_LIST_FIELDS_MASK: &str = "nextPageToken,newStartPageToken,\
     changes(fileId,kind,removed,time,file(id,name,mimeType,trashed,modifiedTime,createdTime))";

/// One file as returned by Drive `files.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleDriveFile {
    /// Drive file id.
    pub id: String,
    /// File name (display).
    #[serde(default)]
    pub name: String,
    /// MIME type (e.g. `application/pdf`).
    #[serde(default, rename = "mimeType")]
    pub mime_type: String,
    /// Trashed flag — when `true` the file is in the user's trash.
    #[serde(default)]
    pub trashed: bool,
    /// RFC-3339 timestamp Drive last modified the file.
    #[serde(default, rename = "modifiedTime")]
    pub modified_time: Option<DateTime<Utc>>,
    /// RFC-3339 timestamp Drive created the file.
    #[serde(default, rename = "createdTime")]
    pub created_time: Option<DateTime<Utc>>,
}

/// One page of `files.list` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleDriveFileList {
    /// File records on this page.
    #[serde(default)]
    pub files: Vec<GoogleDriveFile>,
    /// Token for the next page; absent on the final page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    /// `files.list` does NOT return `newStartPageToken`; the substrate
    /// fetches it separately via `changes.getStartPageToken`. The
    /// field is retained on the wire type so existing fixtures /
    /// integration tests that bundle the start token into the final
    /// page continue to deserialize cleanly.
    #[serde(default, rename = "newStartPageToken")]
    pub new_start_page_token: Option<String>,
}

/// One change record from Drive's Changes API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleDriveChange {
    /// File id of the changed resource.
    #[serde(rename = "fileId")]
    pub file_id: String,
    /// Change kind: `"file"` or `"drive"`.
    #[serde(default)]
    pub kind: String,
    /// `removed = true` is Drive's deletion signal.
    #[serde(default)]
    pub removed: bool,
    /// File body — present unless `removed`.
    #[serde(default)]
    pub file: Option<GoogleDriveFile>,
    /// RFC-3339 timestamp the change was recorded.
    #[serde(default, rename = "time")]
    pub time: Option<DateTime<Utc>>,
}

/// One page of `changes.list` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleDriveChangeList {
    /// Change records on this page, in source order.
    #[serde(default)]
    pub changes: Vec<GoogleDriveChange>,
    /// Token for the next page; absent on the final page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    /// Token to seed the next incremental run with.
    #[serde(default, rename = "newStartPageToken")]
    pub new_start_page_token: Option<String>,
}

/// `GET /drive/v3/changes/startPageToken` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleDriveStartPageToken {
    /// Cursor to seed the next `changes.list` call with.
    #[serde(default, rename = "startPageToken")]
    pub start_page_token: Option<String>,
}

/// `POST /drive/v3/changes/watch` response — Drive returns the
/// channel id + the resourceId we'll see on inbound push pings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleDriveWatchResponse {
    /// Channel id Drive assigned (echoes what we sent in the
    /// request, unless we omitted it — we always send a UUID).
    #[serde(default)]
    pub id: Option<String>,
    /// Resource id Drive will surface on push pings.
    #[serde(default, rename = "resourceId")]
    pub resource_id: Option<String>,
    /// Channel expiry, milliseconds since the Unix epoch.
    #[serde(default)]
    pub expiration: Option<i64>,
}

/// One Drive push-notification body. The relevant headers
/// (`X-Goog-Resource-State`, `X-Goog-Resource-Id`) are inlined into
/// the JSON body for substrate-side parity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleDrivePushNotification {
    /// Resource id the change concerns (Drive file id).
    #[serde(rename = "resourceId")]
    pub resource_id: String,
    /// `add`, `update`, `remove`, or `permission_change`.
    #[serde(rename = "resourceState")]
    pub resource_state: String,
    /// `targetUserEmail` or similar — only set for permission events.
    #[serde(default, rename = "userId")]
    pub user_id: Option<String>,
    /// New permission role (`reader`, `writer`, `owner`); `None` on
    /// revocation.
    #[serde(default, rename = "newRole")]
    pub new_role: Option<String>,
    /// RFC-3339 wall-clock time Drive emitted the notification.
    #[serde(default, rename = "occurredAt")]
    pub occurred_at: Option<DateTime<Utc>>,
}

/// Google Drive connector.
///
/// Holds the wired [`HttpTransport`] + [`OAuth2CodeExchange`] used to
/// drive every Drive REST call (token exchange, `files.list`,
/// `changes.list`, `changes.watch`).
pub struct GoogleDriveConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GoogleDriveConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleDriveConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GoogleDriveConnector {
    /// Construct a Drive connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://oauth2.googleapis.com/token`. Production wires these
    /// to `BlockingHttpTransport` + `OAuth2Client`; tests use
    /// `MockHttpTransport`.
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

    /// Override the Drive REST base URL (production wires the
    /// default; tests redirect to a fake host).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size used by `files.list` / `changes.list`.
    /// Clamped to `[1, 1000]` per Drive's documented maximum.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 1000);
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

    /// Read the optional `q` filter from `auth_config_json.q` (Drive
    /// search query) — defaults to "trashed=false" so deleted files
    /// don't pollute the initial sync.
    fn resolved_query(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("q")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("trashed = false")
            .to_string()
    }

    /// Walk every `files.list` page until either `nextPageToken` is
    /// absent, the server returns an empty page with no token, or
    /// [`MAX_LIST_PAGES`] is hit.
    fn paginate_files(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        q: &str,
    ) -> Result<Vec<GoogleDriveFile>> {
        let mut files = Vec::<GoogleDriveFile>::new();
        let mut page_token: Option<String> = None;
        let mut prev_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut url = format!(
                "{base_url}/drive/v3/files?pageSize={}&q={}&fields={}",
                self.page_size,
                percent_encode_path_component(q),
                percent_encode_path_component(FILE_LIST_FIELDS_MASK),
            );
            if let Some(tok) = page_token.as_deref() {
                url.push_str("&pageToken=");
                url.push_str(&percent_encode_path_component(tok));
            }
            let resp: GoogleDriveFileList = bearer_get_json(
                &self.transport,
                "google_drive",
                "/drive/v3/files",
                &url,
                token,
                &[],
            )?;
            let returned = resp.files.len();
            files.extend(resp.files);
            let Some(next) = resp.next_page_token else {
                return Ok(files);
            };
            // Loop guard — a misbehaving server that echoes the same
            // token on every page would otherwise spin forever.
            if prev_token.as_deref() == Some(next.as_str()) {
                return Ok(files);
            }
            // Empty page mid-stream — treat as end-of-list defensively
            // even if a token was returned.
            if returned == 0 {
                return Ok(files);
            }
            prev_token = Some(next.clone());
            page_token = Some(next);
        }
        Err(ConnectorError::Sync(format!("google_drive /drive/v3/files exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }

    /// Fetch the cursor to seed `incremental_sync` with —
    /// `GET /drive/v3/changes/startPageToken`. Drive's docs are
    /// explicit: callers MUST call this once after the initial sync
    /// to anchor the changes feed, otherwise the first
    /// `changes.list` skips every change between the sync window and
    /// the watch installation.
    fn fetch_start_page_token(
        &self,
        base_url: &str,
        token: &OAuth2Token,
    ) -> Result<Option<String>> {
        let url = format!("{base_url}/drive/v3/changes/startPageToken");
        let resp: GoogleDriveStartPageToken = bearer_get_json(
            &self.transport,
            "google_drive",
            "/drive/v3/changes/startPageToken",
            &url,
            token,
            &[],
        )?;
        Ok(resp.start_page_token)
    }

    /// Walk `changes.list` pages until either `nextPageToken` is
    /// absent (Drive sets `newStartPageToken` on the final page so
    /// callers can advance the watermark) or [`MAX_LIST_PAGES`] is
    /// hit.
    fn paginate_changes(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        start_token: &str,
    ) -> Result<(Vec<GoogleDriveChange>, Option<String>)> {
        let mut changes = Vec::<GoogleDriveChange>::new();
        let mut page_token = start_token.to_string();
        let mut prev_token: Option<String> = None;
        let mut new_start_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let url = format!("{base_url}/drive/v3/changes?pageToken={}&pageSize={}&includeRemoved=true&fields={}",
                percent_encode_path_component(&page_token),
                self.page_size,
                percent_encode_path_component(CHANGE_LIST_FIELDS_MASK),
            );
            let resp: GoogleDriveChangeList = bearer_get_json(
                &self.transport,
                "google_drive",
                "/drive/v3/changes",
                &url,
                token,
                &[],
            )?;
            let returned = resp.changes.len();
            changes.extend(resp.changes);
            // Drive returns `newStartPageToken` on the final page —
            // hold onto it as the substrate watermark, regardless of
            // whether `nextPageToken` is also set (mid-stream the
            // server may include both).
            if resp.new_start_page_token.is_some() {
                new_start_token = resp.new_start_page_token;
            }
            let Some(next) = resp.next_page_token else {
                return Ok((changes, new_start_token));
            };
            if prev_token.as_deref() == Some(next.as_str()) {
                return Ok((changes, new_start_token));
            }
            if returned == 0 {
                return Ok((changes, new_start_token));
            }
            prev_token = Some(next.clone());
            page_token = next;
        }
        Err(ConnectorError::Sync(format!("google_drive /drive/v3/changes exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "reader" | "viewer" => Some(SourcePermissionLevel::Read),
        "writer" | "commenter" => Some(SourcePermissionLevel::Write),
        "owner" | "organizer" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// Map a single Google Drive push notification into connector events.
///
/// Shared by every Drive-backed connector (Drive, Docs, Sheets) so the
/// `X-Goog-Resource-State` handling stays in one place. Notably:
///
/// * The `sync` state is the verification handshake Google sends right
///   after `changes.watch` creates a channel. It carries no change and
///   MUST be acknowledged with a 2xx — rejecting it (HTTP 400) signals a
///   broken endpoint and can make Google deactivate the channel — so it
///   yields an empty event list rather than an error.
/// * `permission_change` maps to [`ConnectorEvent::PermissionChanged`].
/// * Any genuinely unknown state is a [`ConnectorError::Webhook`] (400),
///   telling Google to stop redelivering a body we cannot interpret.
pub fn drive_push_notification_to_events(
    push: GoogleDrivePushNotification,
) -> Result<Vec<ConnectorEvent>> {
    let occurred_at = push.occurred_at.unwrap_or_else(Utc::now);
    let document_id = SourceDocumentId::new(push.resource_id);
    let event = match push.resource_state.as_str() {
        // Channel-creation handshake: no change to report, just ACK.
        "sync" => return Ok(Vec::new()),
        "add" | "create" => ConnectorEvent::DocumentCreated {
            document_id,
            occurred_at,
        },
        "update" | "change" => ConnectorEvent::DocumentUpdated {
            document_id,
            occurred_at,
        },
        "remove" | "trash" => ConnectorEvent::DocumentDeleted {
            document_id,
            occurred_at,
        },
        "permission_change" => ConnectorEvent::PermissionChanged {
            document_id,
            user_id: SourceUserId::new(push.user_id.unwrap_or_default()),
            new_level: push.new_role.as_deref().and_then(parse_role),
            occurred_at,
        },
        other => {
            return Err(ConnectorError::Webhook(format!(
                "unknown drive resource state: {other}"
            )))
        }
    };
    Ok(vec![event])
}

fn file_to_created_event(f: &GoogleDriveFile) -> ConnectorEvent {
    let occurred_at = f.created_time.or(f.modified_time).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(f.id.clone());
    if f.trashed {
        ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        }
    } else {
        ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        }
    }
}

fn change_to_event(ch: &GoogleDriveChange) -> ConnectorEvent {
    let occurred_at = ch.time.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(ch.file_id.clone());
    if ch.removed || ch.file.as_ref().is_some_and(|f| f.trashed) {
        ConnectorEvent::DocumentDeleted {
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

/// MIME-type prefix for Google Workspace native documents (Docs,
/// Sheets, Slides, …). These have no byte stream to download — they
/// must be `export`ed to a concrete format.
const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

/// MIME type we export Google Workspace docs to. The spec pins
/// `text/plain` for Docs / Sheets / Slides so the substrate ingests a
/// uniform, embedding-friendly body.
const EXPORT_MIME: &str = "text/plain";

/// Minimal file metadata used by `fetch_content` to decide between the
/// `export` path (Workspace docs) and the `alt=media` path (binary
/// blobs), and to recover a title + citation URL.
#[derive(Debug, Clone, Default, Deserialize)]
struct GoogleDriveFileMeta {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "mimeType")]
    mime_type: String,
    #[serde(default, rename = "webViewLink")]
    web_view_link: Option<String>,
}

/// The set of Google Workspace types we can usefully export to
/// `text/plain`. Other `application/vnd.google-apps.*` kinds (folder,
/// form, shortcut, …) carry no exportable body — `fetch_content`
/// surfaces them as an unfetchable [`ConnectorError::Sync`].
fn is_exportable_google_doc(mime: &str) -> bool {
    matches!(
        mime,
        "application/vnd.google-apps.document"
            | "application/vnd.google-apps.spreadsheet"
            | "application/vnd.google-apps.presentation"
    )
}

impl GoogleDriveConnector {
    /// Optional byte ceiling for binary downloads, read from
    /// `auth_config_json.max_export_size` (or the camel-case
    /// `maxExportSize`). When set, `fetch_content` sends a
    /// `Range: bytes=0-(N-1)` header so a large blob is capped at the
    /// substrate's configured ingest budget rather than streamed in
    /// full.
    fn resolved_max_export_size(config: &ConnectorConfig) -> Option<u64> {
        config
            .auth_config_json
            .get("max_export_size")
            .or_else(|| config.auth_config_json.get("maxExportSize"))
            .and_then(serde_json::Value::as_u64)
            .filter(|n| *n > 0)
    }
}

impl Connector for GoogleDriveConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "google_drive authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let q = Self::resolved_query(config);
        let files = self.paginate_files(&base_url, token, &q)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(files.len());
        for f in &files {
            events.push(file_to_created_event(f));
        }
        // Anchor the changes feed at the point in time we finished
        // the file walk — every change after this point flows
        // through the incremental sync. Without this step, the first
        // incremental run would start from an undefined cursor and
        // either skip or re-deliver every change since installation.
        let next_cursor = self.fetch_start_page_token(&base_url, token)?;
        Ok(SyncRunResult {
            events,
            next_cursor,
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        // Without a cursor we cannot incrementally fetch — the
        // substrate should be running the initial sync first. We
        // surface the gap as a `Sync` error so the runtime reschedules
        // with the seed cursor populated.
        let start_token = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "google_drive incremental_sync: missing cursor; \
                 initial_sync must populate startPageToken first"
                    .into(),
            )
        })?;
        let (changes, new_start_token) = self.paginate_changes(&base_url, token, start_token)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(changes.len());
        for ch in &changes {
            events.push(change_to_event(ch));
        }
        // Drive returns `newStartPageToken` on the final page; if the
        // server omitted it (mock / older API), fall back to the
        // existing cursor so we don't lose our place.
        let next_cursor = new_start_token.or_else(|| Some(start_token.to_string()));
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
        let base_url = self.resolved_base_url(config);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);

        // 1. Metadata: decide export-vs-download and recover title /
        //    citation URL. The `fields` mask keeps the response tiny.
        let meta_url =
            format!("{base_url}/drive/v3/files/{id_enc}?fields=id,name,mimeType,webViewLink");
        let meta: GoogleDriveFileMeta = bearer_get_json(
            &self.transport,
            "google_drive",
            "/drive/v3/files/{id}",
            &meta_url,
            token,
            &[],
        )?;
        let source_url = meta
            .web_view_link
            .clone()
            .unwrap_or_else(|| format!("https://drive.google.com/file/d/{id}/view"));

        if meta.mime_type.starts_with(GOOGLE_APPS_PREFIX) {
            // 2a. Workspace native doc — must be exported, never
            //     downloaded. Unexportable kinds (folder/form/…) have
            //     no body to fetch.
            if !is_exportable_google_doc(&meta.mime_type) {
                return Err(ConnectorError::Sync(format!(
                    "google_drive fetch_content: {} ({}) has no exportable body",
                    id, meta.mime_type
                )));
            }
            let export_url = format!(
                "{base_url}/drive/v3/files/{id_enc}/export?mimeType={}",
                percent_encode_path_component(EXPORT_MIME),
            );
            let resp = bearer_get_raw(
                &self.transport,
                "google_drive",
                "/drive/v3/files/{id}/export",
                &export_url,
                token,
                &[],
            )?;
            let mime = response_header(&resp, "content-type")
                .map_or(EXPORT_MIME, strip_charset)
                .to_string();
            return Ok(FetchedContent::binary(resp.body, mime)
                .with_title(meta.name)
                .with_metadata(serde_json::json!({
                    "provider": "google_drive",
                    "file_id": id,
                    "source_mime_type": meta.mime_type,
                    "exported": true,
                }))
                .with_source_url(source_url));
        }

        // 2b. Binary blob — download the raw bytes. Honour an optional
        //     byte ceiling via a Range header.
        let download_url = format!("{base_url}/drive/v3/files/{id_enc}?alt=media");
        let range_header;
        let mut extra: Vec<(&str, &str)> = Vec::new();
        if let Some(max) = Self::resolved_max_export_size(config) {
            range_header = format!("bytes=0-{}", max.saturating_sub(1));
            extra.push(("Range", range_header.as_str()));
        }
        let resp = bearer_get_raw(
            &self.transport,
            "google_drive",
            "/drive/v3/files/{id}?alt=media",
            &download_url,
            token,
            &extra,
        )?;
        let mime = response_header(&resp, "content-type")
            .map(strip_charset)
            .filter(|m| !m.is_empty())
            .map_or_else(
                || {
                    if meta.mime_type.is_empty() {
                        "application/octet-stream".to_string()
                    } else {
                        meta.mime_type.clone()
                    }
                },
                str::to_string,
            );
        Ok(FetchedContent::binary(resp.body, mime)
            .with_title(meta.name)
            .with_metadata(serde_json::json!({
                "provider": "google_drive",
                "file_id": id,
                "source_mime_type": meta.mime_type,
                "exported": false,
            }))
            .with_source_url(source_url))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        // Drive's watch endpoint requires a `pageToken` so it can
        // bind the channel to a specific point in the changes feed.
        // Without one, Drive would return 400 — we surface that to
        // the substrate as a configuration error.
        let page_token = config
            .auth_config_json
            .get("start_page_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook("google_drive subscribe_webhook: auth_config_json.start_page_token is required \
                     (call changes.getStartPageToken first)"
                        .into(),
                )
            })?;
        let channel_id = config
            .auth_config_json
            .get("channel_id")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.instance.as_uuid().to_string(),
                std::string::ToString::to_string,
            );
        let token_secret = config
            .auth_config_json
            .get("channel_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("google-drive-channel-secret")
            .to_string();
        let url = format!(
            "{base_url}/drive/v3/changes/watch?pageToken={}",
            percent_encode_path_component(page_token),
        );
        let body = serde_json::json!({
            "id": channel_id,
            "type": "web_hook",
            "address": callback_url,
            "token": token_secret,
        });
        let resp: GoogleDriveWatchResponse = bearer_post_json(
            &self.transport,
            "google_drive",
            "/drive/v3/changes/watch",
            &url,
            token,
            &[],
            &body,
        )?;
        // Drive echoes the channel id; if for any reason it didn't,
        // fall back to the one we sent so the subscription is still
        // revocable.
        let assigned_id = resp.id.unwrap_or(channel_id);
        let expires_at = resp
            .expiration
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            // Drive channels are valid for 7 days when the server
            // doesn't echo an explicit expiration.
            .or_else(|| Some(Utc::now() + chrono::Duration::days(7)));
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(token_secret),
            WebhookEventTypes::all(),
            expires_at,
        );
        // Drive's revocation endpoint needs the channel id + resource
        // id together. We persist them as `<channel_id>:<resource_id>`
        // so the substrate's webhook-revoke path has everything it
        // needs without a second round-trip to look the resource id
        // up.
        subscription.provider_subscription_id = Some(match resp.resource_id {
            Some(rid) if !rid.is_empty() => format!("{assigned_id}:{rid}"),
            _ => assigned_id,
        });
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Google Drive push notifications carry a single change
        // per HTTP POST — the channel API does not batch.
        let push: GoogleDrivePushNotification = serde_json::from_slice(body)?;
        drive_push_notification_to_events(push)
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
                "drive-access",
                "drive-refresh",
                Utc::now() + Duration::hours(1),
                "https://www.googleapis.com/auth/drive.readonly",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::GoogleDrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/google",
            "start_page_token": "watch-start-1",
        }))
    }

    fn file(id: &str, trashed: bool, modified: DateTime<Utc>) -> GoogleDriveFile {
        GoogleDriveFile {
            id: id.into(),
            name: format!("{id}.gdoc"),
            mime_type: "application/vnd.google-apps.document".into(),
            trashed,
            modified_time: Some(modified),
            created_time: Some(modified),
        }
    }

    fn expect_files_list(
        transport: &MockHttpTransport,
        base_url: &str,
        q: &str,
        page_token: Option<&str>,
        response: &GoogleDriveFileList,
    ) {
        let mut url = format!(
            "{base_url}/drive/v3/files?pageSize={}&q={}&fields={}",
            DEFAULT_PAGE_SIZE,
            percent_encode_path_component(q),
            percent_encode_path_component(FILE_LIST_FIELDS_MASK),
        );
        if let Some(tok) = page_token {
            url.push_str("&pageToken=");
            url.push_str(&percent_encode_path_component(tok));
        }
        transport.expect(
            HttpMethod::Get,
            url,
            MockResponse::ok_json(serde_json::to_vec(response).unwrap()),
        );
    }

    fn expect_start_page_token(transport: &MockHttpTransport, base_url: &str, token: &str) {
        transport.expect(
            HttpMethod::Get,
            format!("{base_url}/drive/v3/changes/startPageToken"),
            MockResponse::ok_json(
                serde_json::to_vec(&GoogleDriveStartPageToken {
                    start_page_token: Some(token.into()),
                })
                .unwrap(),
            ),
        );
    }

    fn expect_changes_list(
        transport: &MockHttpTransport,
        base_url: &str,
        page_token: &str,
        response: &GoogleDriveChangeList,
    ) {
        let url = format!(
            "{base_url}/drive/v3/changes?pageToken={}&pageSize={}&includeRemoved=true&fields={}",
            percent_encode_path_component(page_token),
            DEFAULT_PAGE_SIZE,
            percent_encode_path_component(CHANGE_LIST_FIELDS_MASK),
        );
        transport.expect(
            HttpMethod::Get,
            url,
            MockResponse::ok_json(serde_json::to_vec(response).unwrap()),
        );
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = MockHttpTransport::new();
        let c =
            GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("drive"));
        assert_eq!(tok.access_token.expose(), "drive-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = MockHttpTransport::new();
        let c =
            GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let bad_cfg = ConnectorConfig::new(
            ConnectorKind::GoogleDrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&bad_cfg).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_walks_files_then_fetches_start_token() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/google";
        let q = "trashed = false";

        // Page 1.
        expect_files_list(
            &transport,
            base,
            q,
            None,
            &GoogleDriveFileList {
                files: vec![file("f1", false, now), file("f2", false, now)],
                next_page_token: Some("page-token-2".into()),
                new_start_page_token: None,
            },
        );
        // Page 2 (final).
        expect_files_list(
            &transport,
            base,
            q,
            Some("page-token-2"),
            &GoogleDriveFileList {
                files: vec![file("f3", false, now)],
                next_page_token: None,
                new_start_page_token: None,
            },
        );
        // Anchor the watermark.
        expect_start_page_token(&transport, base, "spt-99");

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c =
            GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(res.next_cursor.as_deref(), Some("spt-99"));
    }

    #[test]
    fn initial_sync_emits_deleted_for_trashed_files() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/google";
        let q = "trashed = false";

        expect_files_list(
            &transport,
            base,
            q,
            None,
            &GoogleDriveFileList {
                files: vec![file("f1", true, now)],
                next_page_token: None,
                new_start_page_token: None,
            },
        );
        expect_start_page_token(&transport, base, "spt-1");

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn paginate_files_loop_guard_stops_on_repeated_token() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/google";
        let q = "trashed = false";

        // Two pages with the same nextPageToken — pathological.
        expect_files_list(
            &transport,
            base,
            q,
            None,
            &GoogleDriveFileList {
                files: vec![file("f1", false, now)],
                next_page_token: Some("stuck".into()),
                new_start_page_token: None,
            },
        );
        expect_files_list(
            &transport,
            base,
            q,
            Some("stuck"),
            &GoogleDriveFileList {
                files: vec![file("f2", false, now)],
                next_page_token: Some("stuck".into()),
                new_start_page_token: None,
            },
        );
        expect_start_page_token(&transport, base, "spt-3");

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        // Two pages walked, third would be the same token → stop.
        assert_eq!(res.events.len(), 2);
        assert_eq!(res.next_cursor.as_deref(), Some("spt-3"));
    }

    #[test]
    fn incremental_sync_walks_changes_and_advances_watermark() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/google";

        expect_changes_list(
            &transport,
            base,
            "watermark-1",
            &GoogleDriveChangeList {
                changes: vec![
                    GoogleDriveChange {
                        file_id: "f1".into(),
                        kind: "file".into(),
                        removed: false,
                        file: Some(file("f1", false, now)),
                        time: Some(now),
                    },
                    GoogleDriveChange {
                        file_id: "f-deleted".into(),
                        kind: "file".into(),
                        removed: true,
                        file: None,
                        time: Some(now),
                    },
                ],
                next_page_token: Some("page-b".into()),
                new_start_page_token: None,
            },
        );
        expect_changes_list(
            &transport,
            base,
            "page-b",
            &GoogleDriveChangeList {
                changes: vec![GoogleDriveChange {
                    file_id: "f3".into(),
                    kind: "file".into(),
                    removed: false,
                    file: Some(file("f3", false, now)),
                    time: Some(now),
                }],
                next_page_token: None,
                new_start_page_token: Some("watermark-2".into()),
            },
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("watermark-1".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 3);
        let deletes = res
            .events
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }))
            .count();
        let updates = res
            .events
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentUpdated { .. }))
            .count();
        assert_eq!(deletes, 1);
        assert_eq!(updates, 2);
        assert_eq!(res.next_cursor.as_deref(), Some("watermark-2"));
    }

    #[test]
    fn incremental_sync_falls_back_to_previous_cursor_when_no_new_start_token() {
        let transport = MockHttpTransport::new();
        let base = "https://api.test/google";

        expect_changes_list(
            &transport,
            base,
            "watermark-x",
            &GoogleDriveChangeList {
                changes: vec![],
                next_page_token: None,
                new_start_page_token: None,
            },
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("watermark-x".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 0);
        // Server didn't return a new token; we hold our place.
        assert_eq!(res.next_cursor.as_deref(), Some("watermark-x"));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = MockHttpTransport::new();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        match err {
            ConnectorError::Sync(msg) => assert!(msg.contains("missing cursor")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn subscribe_webhook_posts_watch_request_and_captures_channel_id() {
        let transport = MockHttpTransport::new();
        let base = "https://api.test/google";
        let watch_url = format!(
            "{base}/drive/v3/changes/watch?pageToken={}",
            percent_encode_path_component("watch-start-1"),
        );
        transport.expect(
            HttpMethod::Post,
            watch_url,
            MockResponse::ok_json(
                serde_json::to_vec(&GoogleDriveWatchResponse {
                    id: Some("chan-42".into()),
                    resource_id: Some("res-99".into()),
                    expiration: None,
                })
                .unwrap(),
            ),
        );

        let transport_arc: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(
            ConnectorInstanceId::new_v4(),
            transport_arc.clone(),
            oauth(),
        );
        let tok = c.authenticate(&cfg()).unwrap();
        let mut config = cfg();
        // Force the channel id we want to see echoed.
        if let Some(obj) = config.auth_config_json.as_object_mut() {
            obj.insert(
                "channel_id".into(),
                serde_json::Value::String("chan-42".into()),
            );
        }
        let sub = c
            .subscribe_webhook(&config, &tok, "https://substrate.example/hooks/drive")
            .unwrap();
        assert_eq!(sub.connector, c.instance);
        assert_eq!(
            sub.provider_subscription_id.as_deref(),
            Some("chan-42:res-99")
        );
        assert!(sub.expires_at.is_some());
    }

    #[test]
    fn subscribe_webhook_requires_start_page_token() {
        let transport = MockHttpTransport::new();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let bad_cfg = ConnectorConfig::new(
            ConnectorKind::GoogleDrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/google",
        }));
        let err = c
            .subscribe_webhook(&bad_cfg, &tok, "https://substrate.example/hooks/drive")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn unauthorized_status_maps_to_auth_error() {
        let transport = MockHttpTransport::new();
        transport.with_default_response(MockResponse::status(
            401,
            br#"{"error":"unauthorized"}"#.to_vec(),
        ));
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn webhook_parses_permission_change() {
        let transport = MockHttpTransport::new();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let body = serde_json::json!({
            "resourceId": "f1",
            "resourceState": "permission_change",
            "userId": "user-7",
            "newRole": "writer",
            "occurredAt": Utc::now(),
        });
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_state_errors() {
        let transport = MockHttpTransport::new();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let body = serde_json::json!({"resourceId": "f1", "resourceState": "weird"});
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_sync_handshake_is_acked_with_no_events() {
        // Google's channel-creation `sync` notification must be accepted
        // (2xx / empty), not rejected, or Google may drop the channel.
        let transport = MockHttpTransport::new();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let body = serde_json::json!({"resourceId": "chan", "resourceState": "sync"});
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_add_state_emits_created() {
        let transport = MockHttpTransport::new();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let body = serde_json::json!({"resourceId": "f1", "resourceState": "add"});
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn webhook_remove_state_emits_deleted() {
        let transport = MockHttpTransport::new();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let body = serde_json::json!({"resourceId": "f1", "resourceState": "remove"});
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    // ───────────── fetch_content ─────────────

    fn raw_response(content_type: &str, body: impl Into<Vec<u8>>) -> MockResponse {
        MockResponse {
            status: 200,
            headers: vec![("content-type".into(), content_type.into())],
            body: body.into(),
        }
    }

    #[test]
    fn fetch_content_exports_google_doc_to_text() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/doc-1?fields=id,name,mimeType,webViewLink",
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "id": "doc-1",
                    "name": "Quarterly Plan",
                    "mimeType": "application/vnd.google-apps.document",
                    "webViewLink": "https://docs.google.com/document/d/doc-1/edit",
                }))
                .unwrap(),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/doc-1/export?mimeType=text%2Fplain",
            raw_response("text/plain; charset=UTF-8", b"Exported body text".to_vec()),
        );
        let c =
            GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("doc-1"))
            .unwrap();
        assert_eq!(fc.mime_type, "text/plain");
        assert_eq!(fc.body, b"Exported body text");
        assert_eq!(fc.title.as_deref(), Some("Quarterly Plan"));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://docs.google.com/document/d/doc-1/edit")
        );
        assert_eq!(fc.metadata["exported"], serde_json::json!(true));
        // Auth header carried on the export GET.
        let req = transport.recorded().last().cloned().unwrap();
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer drive-access"));
    }

    #[test]
    fn fetch_content_downloads_binary_via_alt_media() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/bin-1?fields=id,name,mimeType,webViewLink",
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "id": "bin-1",
                    "name": "diagram.pdf",
                    "mimeType": "application/pdf",
                }))
                .unwrap(),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/bin-1?alt=media",
            raw_response("application/pdf", vec![0x25, 0x50, 0x44, 0x46]),
        );
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("bin-1"))
            .unwrap();
        assert_eq!(fc.mime_type, "application/pdf");
        assert_eq!(fc.body, vec![0x25, 0x50, 0x44, 0x46]);
        assert_eq!(fc.title.as_deref(), Some("diagram.pdf"));
        // No webViewLink → falls back to the canonical drive URL.
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://drive.google.com/file/d/bin-1/view")
        );
        assert_eq!(fc.metadata["exported"], serde_json::json!(false));
    }

    #[test]
    fn fetch_content_applies_range_header_when_max_size_set() {
        let transport = Arc::new(MockHttpTransport::new());
        let cfg = ConnectorConfig::new(
            ConnectorKind::GoogleDrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/google",
            "max_export_size": 1024,
        }));
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/bin-2?fields=id,name,mimeType,webViewLink",
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "id": "bin-2", "name": "big.bin", "mimeType": "application/octet-stream",
                }))
                .unwrap(),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/bin-2?alt=media",
            raw_response("application/octet-stream", vec![1, 2, 3]),
        );
        let c =
            GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg).unwrap();
        let _ = c
            .fetch_content(&cfg, &tok, &SourceDocumentId::new("bin-2"))
            .unwrap();
        let req = transport.recorded().last().cloned().unwrap();
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Range" && v == "bytes=0-1023"));
    }

    #[test]
    fn fetch_content_rejects_unexportable_google_type() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/folder-1?fields=id,name,mimeType,webViewLink",
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "id": "folder-1", "name": "My Folder",
                    "mimeType": "application/vnd.google-apps.folder",
                }))
                .unwrap(),
            ),
        );
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("folder-1"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_404_metadata_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/gone?fields=id,name,mimeType,webViewLink",
            MockResponse::status(404, br#"{"error":{"code":404}}"#.to_vec()),
        );
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("gone"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/google/drive/v3/files/secret?fields=id,name,mimeType,webViewLink",
            MockResponse::status(401, br#"{"error":{"code":401}}"#.to_vec()),
        );
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("secret"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }
}
