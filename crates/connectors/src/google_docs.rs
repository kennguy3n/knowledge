//! Google Docs connector — Docs API v1 + Drive change feed.
//!
//! Google Docs are Drive files of MIME type
//! `application/vnd.google-apps.document`, so discovery and change
//! tracking reuse Drive's `files.list` / `changes.list` feed (the same
//! wire types as the Drive connector), while content is reconstructed
//! through the Docs API.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`].
//! * `initial_sync` walks `files.list` filtered to Docs, then anchors
//!   the changes feed via `changes.getStartPageToken` (the cursor).
//! * `incremental_sync` walks `changes.list?pageToken=<cursor>`,
//!   keeping only Docs (or removals), and advances to
//!   `newStartPageToken`.
//! * `fetch_content` GETs `…/v1/documents/{id}` and flattens the
//!   structural `body.content` into plain text.
//! * `subscribe_webhook` installs a Drive `changes.watch` push channel.
//! * `handle_webhook_event` maps a Drive resource-state push to a
//!   [`ConnectorEvent`].
//!
//! Wiring contract mirrors the other connectors: the constructor takes
//! an `Arc<dyn HttpTransport>` and an `Arc<dyn OAuth2CodeExchange>`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::Deserialize;

use crate::google_drive::{
    change_to_event, drive_fetch_start_page_token, drive_paginate_changes, drive_paginate_files,
    drive_push_notification_to_events, file_to_created_event, GoogleDriveChange,
    GoogleDrivePushNotification, GoogleDriveWatchResponse,
};

/// Default Drive REST base URL (discovery + change feed).
pub const DEFAULT_API_BASE_URL: &str = "https://www.googleapis.com";

/// Default Docs API base URL (content reconstruction).
pub const DEFAULT_DOCS_API_BASE_URL: &str = "https://docs.googleapis.com";

/// MIME type identifying Google Docs.
pub const DOC_MIME_TYPE: &str = "application/vnd.google-apps.document";

/// Default page size for `files.list` / `changes.list`.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// One run of text within a Docs paragraph.
#[derive(Debug, Clone, Default, Deserialize)]
struct DocsTextRun {
    #[serde(default)]
    content: String,
}

/// One element of a Docs paragraph.
#[derive(Debug, Clone, Default, Deserialize)]
struct DocsParagraphElement {
    #[serde(default, rename = "textRun")]
    text_run: Option<DocsTextRun>,
}

/// A Docs paragraph (sequence of text runs).
#[derive(Debug, Clone, Default, Deserialize)]
struct DocsParagraph {
    #[serde(default)]
    elements: Vec<DocsParagraphElement>,
}

/// One structural element of a Docs body.
#[derive(Debug, Clone, Default, Deserialize)]
struct DocsStructuralElement {
    #[serde(default)]
    paragraph: Option<DocsParagraph>,
}

/// The body of a Docs document.
#[derive(Debug, Clone, Default, Deserialize)]
struct DocsBody {
    #[serde(default)]
    content: Vec<DocsStructuralElement>,
}

/// A Docs `documents.get` response (subset).
#[derive(Debug, Clone, Default, Deserialize)]
struct DocsDocument {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: DocsBody,
}

/// Flatten a Docs document body into plain text.
fn docs_body_to_text(doc: &DocsDocument) -> String {
    let mut out = String::new();
    for element in &doc.body.content {
        if let Some(paragraph) = &element.paragraph {
            for el in &paragraph.elements {
                if let Some(run) = &el.text_run {
                    out.push_str(&run.content);
                }
            }
        }
    }
    out
}

/// Google Docs connector. Holds the wired transport + OAuth exchange.
pub struct GoogleDocsConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    docs_api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GoogleDocsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleDocsConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("docs_api_base_url", &self.docs_api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GoogleDocsConnector {
    /// Construct a Google Docs connector.
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
            docs_api_base_url: DEFAULT_DOCS_API_BASE_URL.to_string(),
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Override the Drive REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the Docs API base URL.
    #[must_use]
    pub fn with_docs_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.docs_api_base_url = url.into();
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

    fn resolved_docs_base_url(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("docs_api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.docs_api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    /// Drive `q` filter restricting discovery to Google Docs.
    fn resolved_query(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("q")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || format!("mimeType='{DOC_MIME_TYPE}' and trashed = false"),
                std::string::ToString::to_string,
            )
    }
}

/// Keep only changes that concern a Google Doc (or a removal, where
/// Drive omits the file body so the MIME type is unknown).
fn change_concerns_doc(ch: &GoogleDriveChange) -> bool {
    if ch.removed {
        return true;
    }
    ch.file
        .as_ref()
        .is_some_and(|f| f.mime_type == DOC_MIME_TYPE)
}

impl Connector for GoogleDocsConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "google_docs authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let q = Self::resolved_query(config);
        let files = drive_paginate_files(
            &self.transport,
            "google_docs",
            &base_url,
            self.page_size,
            token,
            &q,
        )?;
        let events: Vec<ConnectorEvent> = files.iter().map(file_to_created_event).collect();
        let next_cursor =
            drive_fetch_start_page_token(&self.transport, "google_docs", &base_url, token)?;
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
        let start_token = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "google_docs incremental_sync: missing cursor; \
                 initial_sync must seed startPageToken first"
                    .into(),
            )
        })?;
        let (changes, new_start) = drive_paginate_changes(
            &self.transport,
            "google_docs",
            &base_url,
            self.page_size,
            token,
            start_token,
        )?;
        let events: Vec<ConnectorEvent> = changes
            .iter()
            .filter(|c| change_concerns_doc(c))
            .map(change_to_event)
            .collect();
        let next_cursor = new_start.or_else(|| Some(start_token.to_string()));
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
        let docs_base = self.resolved_docs_base_url(config);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{docs_base}/v1/documents/{id_enc}");
        let doc: DocsDocument = bearer_get_json(
            &self.transport,
            "google_docs",
            "/v1/documents/{id}",
            &url,
            token,
            &[],
        )?;
        let body = docs_body_to_text(&doc);
        let title = if doc.title.is_empty() {
            format!("Google Doc {id}")
        } else {
            doc.title.clone()
        };
        let fc = FetchedContent::text(body, "text/plain")
            .with_title(title)
            .with_source_url(format!("https://docs.google.com/document/d/{id}/edit"))
            .with_metadata(serde_json::json!({
                "provider": "google_docs",
                "document_id": id,
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
        let page_token = config
            .auth_config_json
            .get("start_page_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "google_docs subscribe_webhook: auth_config_json.start_page_token is required \
                     (call changes.getStartPageToken first)"
                        .into(),
                )
            })?;
        let channel_id = config
            .auth_config_json
            .get("channel_id")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| self.instance.as_uuid().to_string(), str::to_string);
        let token_secret = config
            .auth_config_json
            .get("channel_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("google-docs-channel-secret")
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
            "google_docs",
            "/drive/v3/changes/watch",
            &url,
            token,
            &[],
            &body,
        )?;
        let assigned_id = resp.id.unwrap_or(channel_id);
        let expires_at = resp
            .expiration
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .or_else(|| Some(Utc::now() + chrono::Duration::days(7)));
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(token_secret),
            WebhookEventTypes::all(),
            expires_at,
        );
        subscription.provider_subscription_id = Some(match resp.resource_id {
            Some(rid) if !rid.is_empty() => format!("{assigned_id}:{rid}"),
            _ => assigned_id,
        });
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Docs reuse Drive's `changes.watch` push channel, so the
        // notification envelope and its `X-Goog-Resource-State` semantics
        // (including the `sync` handshake and `permission_change`) are
        // shared with the Drive connector.
        let push: GoogleDrivePushNotification = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!(
                "google_docs webhook: malformed notification body: {e}"
            ))
        })?;
        drive_push_notification_to_events(push)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_drive::{CHANGE_LIST_FIELDS_MASK, FILE_LIST_FIELDS_MASK};
    use chrono::Duration;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "gdocs-access",
                "gdocs-refresh",
                Utc::now() + Duration::hours(1),
                "https://www.googleapis.com/auth/documents.readonly",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::GoogleDocs,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/g",
            "docs_api_base_url": "https://api.test/docs",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn files_url() -> String {
        format!(
            "https://api.test/g/drive/v3/files?pageSize=100&q={}&fields={}",
            percent_encode_path_component(&format!(
                "mimeType='{DOC_MIME_TYPE}' and trashed = false"
            )),
            percent_encode_path_component(FILE_LIST_FIELDS_MASK),
        )
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "gdocs-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::GoogleDocs,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_lists_docs_and_seeds_start_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            files_url(),
            ok_json(&serde_json::json!({
                "files": [
                    { "id": "d1", "name": "Doc 1", "mimeType": DOC_MIME_TYPE, "createdTime": now },
                    { "id": "d2", "name": "Doc 2", "mimeType": DOC_MIME_TYPE, "createdTime": now },
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/g/drive/v3/changes/startPageToken",
            ok_json(&serde_json::json!({ "startPageToken": "ST1" })),
        );
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(res.next_cursor.as_deref(), Some("ST1"));
    }

    #[test]
    fn incremental_sync_filters_non_docs() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let url = format!(
            "https://api.test/g/drive/v3/changes?pageToken=ST1&pageSize=100&includeRemoved=true&fields={}",
            percent_encode_path_component(CHANGE_LIST_FIELDS_MASK)
        );
        transport.expect(
            HttpMethod::Get,
            url,
            ok_json(&serde_json::json!({
                "changes": [
                    { "fileId": "d1", "time": now, "file": { "id": "d1", "mimeType": DOC_MIME_TYPE } },
                    { "fileId": "x9", "time": now, "file": { "id": "x9", "mimeType": "application/pdf" } },
                    { "fileId": "d3", "time": now, "removed": true },
                ],
                "newStartPageToken": "ST2"
            })),
        );
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("ST1".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // d1 (updated) + d3 (removed); the PDF is filtered out.
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert!(matches!(
            res.events[1],
            ConnectorEvent::DocumentDeleted { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("ST2"));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            files_url(),
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn fetch_content_flattens_document_body() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/docs/v1/documents/d1",
            ok_json(&serde_json::json!({
                "title": "Design notes",
                "body": { "content": [
                    { "paragraph": { "elements": [ { "textRun": { "content": "Hello " } }, { "textRun": { "content": "world\n" } } ] } },
                    { "paragraph": { "elements": [ { "textRun": { "content": "Second line\n" } } ] } }
                ] }
            })),
        );
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("d1"))
            .unwrap();
        assert_eq!(fc.title.as_deref(), Some("Design notes"));
        let text = String::from_utf8(fc.body.clone()).unwrap();
        assert!(text.contains("Hello world"));
        assert!(text.contains("Second line"));
    }

    #[test]
    fn subscribe_webhook_posts_watch() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/g/drive/v3/changes/watch?pageToken=ST1",
            ok_json(&serde_json::json!({ "id": "chan-1", "resourceId": "res-1" })),
        );
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let cfg_watch = ConnectorConfig::new(
            ConnectorKind::GoogleDocs,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/g",
            "start_page_token": "ST1",
        }));
        let sub = c
            .subscribe_webhook(&cfg_watch, &tok, "https://hook.example/gdocs")
            .unwrap();
        assert_eq!(
            sub.provider_subscription_id.as_deref(),
            Some("chan-1:res-1")
        );
    }

    #[test]
    fn webhook_maps_resource_state() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(br#"{"resourceId":"d1","resourceState":"update"}"#)
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_unknown_state_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(br#"{"resourceId":"d1","resourceState":"bogus"}"#)
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_sync_handshake_is_acked_with_no_events() {
        // Google's channel-creation `sync` notification must be accepted
        // (2xx / empty), not rejected, or Google may drop the channel.
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(br#"{"resourceId":"chan","resourceState":"sync"}"#)
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_maps_permission_change() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(br#"{"resourceId":"d1","resourceState":"permission_change"}"#)
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::PermissionChanged { .. }));
    }

    #[test]
    fn webhook_malformed_body_is_webhook_error() {
        // A body we cannot parse is a 400 (stop redelivering), not a 502.
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleDocsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
