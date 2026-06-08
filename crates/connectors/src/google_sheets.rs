//! Google Sheets connector — Sheets API v4 + Drive change feed.
//!
//! Google Sheets are Drive files of MIME type
//! `application/vnd.google-apps.spreadsheet`, so discovery and change
//! tracking reuse Drive's `files.list` / `changes.list` feed (the same
//! wire types as the Drive connector), while content is reconstructed
//! through the Sheets API's grid data.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`].
//! * `initial_sync` walks `files.list` filtered to spreadsheets, then
//!   anchors the changes feed via `changes.getStartPageToken`.
//! * `incremental_sync` walks `changes.list?pageToken=<cursor>`,
//!   keeping only spreadsheets (or removals), and advances to
//!   `newStartPageToken`.
//! * `fetch_content` GETs `…/v4/spreadsheets/{id}?includeGridData=true`
//!   and flattens every sheet's grid into tab-separated text.
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

/// Default Sheets API base URL (content reconstruction).
pub const DEFAULT_SHEETS_API_BASE_URL: &str = "https://sheets.googleapis.com";

/// MIME type identifying Google Sheets.
pub const SHEET_MIME_TYPE: &str = "application/vnd.google-apps.spreadsheet";

/// Default page size for `files.list` / `changes.list`.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Field mask requesting just enough grid data to render text.
const GRID_FIELDS_MASK: &str =
    "properties.title,sheets(properties(title),data(rowData(values(formattedValue))))";

/// One cell within a Sheets grid.
#[derive(Debug, Clone, Default, Deserialize)]
struct CellData {
    #[serde(default, rename = "formattedValue")]
    formatted_value: Option<String>,
}

/// One row within a Sheets grid.
#[derive(Debug, Clone, Default, Deserialize)]
struct RowData {
    #[serde(default)]
    values: Vec<CellData>,
}

/// A block of grid data within a sheet.
#[derive(Debug, Clone, Default, Deserialize)]
struct GridData {
    #[serde(default, rename = "rowData")]
    row_data: Vec<RowData>,
}

/// Properties of a single sheet (tab).
#[derive(Debug, Clone, Default, Deserialize)]
struct SheetProperties {
    #[serde(default)]
    title: String,
}

/// One sheet (tab) within a spreadsheet.
#[derive(Debug, Clone, Default, Deserialize)]
struct Sheet {
    #[serde(default)]
    properties: SheetProperties,
    #[serde(default)]
    data: Vec<GridData>,
}

/// Top-level spreadsheet properties.
#[derive(Debug, Clone, Default, Deserialize)]
struct SpreadsheetProperties {
    #[serde(default)]
    title: String,
}

/// A Sheets `spreadsheets.get` response (subset).
#[derive(Debug, Clone, Default, Deserialize)]
struct Spreadsheet {
    #[serde(default)]
    properties: SpreadsheetProperties,
    #[serde(default)]
    sheets: Vec<Sheet>,
}

/// Flatten a spreadsheet into tab-separated text, one block per sheet.
fn spreadsheet_to_text(sheet: &Spreadsheet) -> String {
    let mut out = String::new();
    for tab in &sheet.sheets {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("# ");
        out.push_str(&tab.properties.title);
        out.push('\n');
        for grid in &tab.data {
            for row in &grid.row_data {
                let cells: Vec<&str> = row
                    .values
                    .iter()
                    .map(|c| c.formatted_value.as_deref().unwrap_or(""))
                    .collect();
                out.push_str(&cells.join("\t"));
                out.push('\n');
            }
        }
    }
    out
}

/// Google Sheets connector. Holds the wired transport + OAuth exchange.
pub struct GoogleSheetsConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    sheets_api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GoogleSheetsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleSheetsConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("sheets_api_base_url", &self.sheets_api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GoogleSheetsConnector {
    /// Construct a Google Sheets connector.
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
            sheets_api_base_url: DEFAULT_SHEETS_API_BASE_URL.to_string(),
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Override the Drive REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the Sheets API base URL.
    #[must_use]
    pub fn with_sheets_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.sheets_api_base_url = url.into();
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

    fn resolved_sheets_base_url(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("sheets_api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.sheets_api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    /// Drive `q` filter restricting discovery to Google Sheets.
    fn resolved_query(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("q")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || format!("mimeType='{SHEET_MIME_TYPE}' and trashed = false"),
                std::string::ToString::to_string,
            )
    }
}

/// Keep only changes that concern a Google Sheet (or a removal).
fn change_concerns_sheet(ch: &GoogleDriveChange) -> bool {
    if ch.removed {
        return true;
    }
    ch.file
        .as_ref()
        .is_some_and(|f| f.mime_type == SHEET_MIME_TYPE)
}

impl Connector for GoogleSheetsConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "google_sheets authenticate: auth_config_json.authorization_code is required"
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
            "google_sheets",
            &base_url,
            self.page_size,
            token,
            &q,
        )?;
        let events: Vec<ConnectorEvent> = files.iter().map(file_to_created_event).collect();
        let next_cursor =
            drive_fetch_start_page_token(&self.transport, "google_sheets", &base_url, token)?;
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
                "google_sheets incremental_sync: missing cursor; \
                 initial_sync must seed startPageToken first"
                    .into(),
            )
        })?;
        let (changes, new_start) = drive_paginate_changes(
            &self.transport,
            "google_sheets",
            &base_url,
            self.page_size,
            token,
            start_token,
        )?;
        let events: Vec<ConnectorEvent> = changes
            .iter()
            .filter(|c| change_concerns_sheet(c))
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
        let sheets_base = self.resolved_sheets_base_url(config);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!(
            "{sheets_base}/v4/spreadsheets/{id_enc}?includeGridData=true&fields={}",
            percent_encode_path_component(GRID_FIELDS_MASK),
        );
        let sheet: Spreadsheet = bearer_get_json(
            &self.transport,
            "google_sheets",
            "/v4/spreadsheets/{id}",
            &url,
            token,
            &[],
        )?;
        let body = spreadsheet_to_text(&sheet);
        let title = if sheet.properties.title.is_empty() {
            format!("Google Sheet {id}")
        } else {
            sheet.properties.title.clone()
        };
        let fc = FetchedContent::text(body, "text/plain")
            .with_title(title)
            .with_source_url(format!("https://docs.google.com/spreadsheets/d/{id}/edit"))
            .with_metadata(serde_json::json!({
                "provider": "google_sheets",
                "spreadsheet_id": id,
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
                    "google_sheets subscribe_webhook: auth_config_json.start_page_token is \
                     required (call changes.getStartPageToken first)"
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
            .unwrap_or("google-sheets-channel-secret")
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
            "google_sheets",
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
        // Sheets reuse Drive's `changes.watch` push channel, so the
        // notification envelope and its `X-Goog-Resource-State` semantics
        // (including the `sync` handshake and `permission_change`) are
        // shared with the Drive connector.
        let push: GoogleDrivePushNotification = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!(
                "google_sheets webhook: malformed notification body: {e}"
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
                "gsheets-access",
                "gsheets-refresh",
                Utc::now() + Duration::hours(1),
                "https://www.googleapis.com/auth/spreadsheets.readonly",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::GoogleSheets,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/g",
            "sheets_api_base_url": "https://api.test/sheets",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn files_url() -> String {
        format!(
            "https://api.test/g/drive/v3/files?pageSize=100&q={}&fields={}",
            percent_encode_path_component(&format!(
                "mimeType='{SHEET_MIME_TYPE}' and trashed = false"
            )),
            percent_encode_path_component(FILE_LIST_FIELDS_MASK),
        )
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "gsheets-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::GoogleSheets,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_lists_sheets_and_seeds_start_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            files_url(),
            ok_json(&serde_json::json!({
                "files": [
                    { "id": "s1", "name": "Sheet 1", "mimeType": SHEET_MIME_TYPE, "createdTime": now },
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/g/drive/v3/changes/startPageToken",
            ok_json(&serde_json::json!({ "startPageToken": "ST1" })),
        );
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("ST1"));
    }

    #[test]
    fn incremental_sync_filters_non_sheets() {
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
                    { "fileId": "s1", "time": now, "file": { "id": "s1", "mimeType": SHEET_MIME_TYPE } },
                    { "fileId": "d2", "time": now, "file": { "id": "d2", "mimeType": "application/vnd.google-apps.document" } },
                ],
                "newStartPageToken": "ST2"
            })),
        );
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("ST1".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("ST2"));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_flattens_grid() {
        let transport = Arc::new(MockHttpTransport::new());
        let url = format!(
            "https://api.test/sheets/v4/spreadsheets/s1?includeGridData=true&fields={}",
            percent_encode_path_component(GRID_FIELDS_MASK)
        );
        transport.expect(
            HttpMethod::Get,
            url,
            ok_json(&serde_json::json!({
                "properties": { "title": "Budget" },
                "sheets": [
                    { "properties": { "title": "Q1" }, "data": [ { "rowData": [
                        { "values": [ { "formattedValue": "Item" }, { "formattedValue": "Cost" } ] },
                        { "values": [ { "formattedValue": "Pizza" }, { "formattedValue": "10" } ] }
                    ] } ] }
                ]
            })),
        );
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("s1"))
            .unwrap();
        assert_eq!(fc.title.as_deref(), Some("Budget"));
        let text = String::from_utf8(fc.body.clone()).unwrap();
        assert!(text.contains("# Q1"));
        assert!(text.contains("Item\tCost"));
        assert!(text.contains("Pizza\t10"));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            files_url(),
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn webhook_maps_resource_state() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(br#"{"resourceId":"s1","resourceState":"remove"}"#)
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_sync_handshake_is_acked_with_no_events() {
        // Google's channel-creation `sync` notification must be accepted
        // (2xx / empty), not rejected, or Google may drop the channel.
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(br#"{"resourceId":"chan","resourceState":"sync"}"#)
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_maps_permission_change() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(br#"{"resourceId":"s1","resourceState":"permission_change"}"#)
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::PermissionChanged { .. }));
    }

    #[test]
    fn webhook_unknown_state_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(br#"{"resourceId":"s1","resourceState":"bogus"}"#)
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_malformed_body_is_webhook_error() {
        // A body we cannot parse is a 400 (stop redelivering), not a 502.
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleSheetsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
