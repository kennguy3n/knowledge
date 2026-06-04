//! SharePoint connector — Microsoft Graph `/sites/{id}/drive/root/delta`.
//!
//! SharePoint document libraries are Graph drives, so this connector is
//! a close sibling of the OneDrive connector — the difference is the
//! drive path (`/sites/{site-id}/drive` rather than `/me/drive`) and
//! the `provider` tag used in error classification.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`] against
//!   `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token`.
//! * `initial_sync` walks `/sites/{id}/drive/root/delta`, following
//!   `@odata.nextLink` pages and surfacing the final `@odata.deltaLink`
//!   as the cursor; emits one [`ConnectorEvent`] per `DriveItem`.
//! * `incremental_sync` GETs the stored `@odata.deltaLink` verbatim.
//! * `fetch_content` reads item metadata then downloads the bytes via
//!   the pre-authenticated `@microsoft.graph.downloadUrl` (falling back
//!   to the authenticated `/content` endpoint).
//! * `subscribe_webhook` POSTs `/subscriptions` for a Graph change
//!   notification targeting the site drive root.
//! * `handle_webhook_event` drains the entire
//!   `changeNotificationCollection` batch, skipping unknown
//!   `changeType`s.
//!
//! Wiring contract mirrors the other connectors: the constructor takes
//! an `Arc<dyn HttpTransport>` and an `Arc<dyn OAuth2CodeExchange>`.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SourcePermissionLevel, SourceUserId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

use crate::content::{bearer_get_raw, get_raw, response_header, strip_charset};

/// Default Graph base URL. Override via
/// `auth_config_json.api_base_url` for sovereign clouds.
pub const DEFAULT_API_BASE_URL: &str = "https://graph.microsoft.com";

/// Default Graph API version path.
pub const DEFAULT_API_VERSION: &str = "/v1.0";

/// Default substrate-side subscription TTL — Graph caps drive
/// subscriptions at ~4230 minutes (≈3 days); sit one minute under.
pub const DEFAULT_SUBSCRIPTION_TTL_MINUTES: i64 = 4_229;

/// Safety ceiling on number of delta pages a single sync will walk.
pub const MAX_DELTA_PAGES: usize = 10_000;

/// One Graph `DriveItem` (subset relevant to substrate ingestion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveItem {
    /// Item id.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Wall-clock created date.
    #[serde(default, rename = "createdDateTime")]
    pub created_date_time: Option<DateTime<Utc>>,
    /// Wall-clock last-modified date.
    #[serde(default, rename = "lastModifiedDateTime")]
    pub last_modified_date_time: Option<DateTime<Utc>>,
    /// Marker indicating Graph reports the item as soft-deleted.
    #[serde(default)]
    pub deleted: Option<DeletedFacet>,
}

/// Graph "deleted" facet — its presence means the item is gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedFacet {
    /// Reason string (e.g. `"deleted"`).
    #[serde(default)]
    pub state: String,
}

/// One page of `/sites/{id}/drive/root/delta` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeltaResponse {
    /// Items that changed in this page.
    #[serde(default)]
    pub value: Vec<DriveItem>,
    /// Forward link to the next page (mid-walk).
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    /// Final-state cursor — present on the last page only.
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// `/subscriptions` create response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphSubscriptionResponse {
    /// Subscription id Graph assigned (needed for revoke).
    #[serde(default)]
    pub id: Option<String>,
    /// Echoed expiry (RFC-3339).
    #[serde(default, rename = "expirationDateTime")]
    pub expiration_date_time: Option<DateTime<Utc>>,
}

/// One `changeNotification` in a Graph subscription batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeNotification {
    /// Substrate-side resource id (`sites/{id}/drive/items/{id}`).
    pub resource: String,
    /// Lifecycle string: `created`, `updated`, `deleted`, `shared`.
    #[serde(rename = "changeType")]
    pub change_type: String,
    /// Subscription id Graph uses for delivery routing.
    #[serde(default, rename = "subscriptionId")]
    pub subscription_id: String,
    /// Wall-clock event time.
    #[serde(default, rename = "eventTime")]
    pub event_time: Option<DateTime<Utc>>,
    /// User id whose permission changed (only on `shared`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// New role string (`read`, `write`, `owner`).
    #[serde(default)]
    pub new_role: Option<String>,
}

/// `changeNotificationCollection` payload Graph POSTs to the
/// `notificationUrl` of an active subscription.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeNotificationCollection {
    /// Notification batch.
    #[serde(default)]
    pub value: Vec<ChangeNotification>,
}

/// SharePoint connector. Holds the wired transport + OAuth exchange.
pub struct SharePointConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    api_version: String,
}

impl std::fmt::Debug for SharePointConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharePointConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("api_version", &self.api_version)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl SharePointConnector {
    /// Construct a SharePoint connector.
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
            api_version: DEFAULT_API_VERSION.to_string(),
        }
    }

    /// Override the Graph REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the Graph API version segment (`/v1.0` or `/beta`).
    #[must_use]
    pub fn with_api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
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

    fn resolved_api_version(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("api_version")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.api_version.clone(),
                std::string::ToString::to_string,
            )
    }

    /// Which SharePoint document library to walk. Defaults to
    /// `/sites/root/drive`. Override via `auth_config_json.drive_path`
    /// to target a specific site / library
    /// (e.g. `/sites/{site-id}/drive` or `/drives/{drive-id}`).
    fn resolved_drive_path(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("drive_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("/sites/root/drive")
            .to_string()
    }

    /// Walk every `@odata.nextLink` page until the link is absent, the
    /// server returns an empty page, or [`MAX_DELTA_PAGES`] is hit.
    fn paginate_delta(
        &self,
        first_url: &str,
        token: &OAuth2Token,
    ) -> Result<(Vec<DriveItem>, Option<String>)> {
        let mut items = Vec::<DriveItem>::new();
        let mut url = first_url.to_string();
        let mut prev_url: Option<String> = None;
        let mut delta_link: Option<String> = None;
        for _ in 0..MAX_DELTA_PAGES {
            let resp: DeltaResponse = bearer_get_json(
                &self.transport,
                "sharepoint",
                "/sites/{id}/drive/root/delta",
                &url,
                token,
                &[],
            )?;
            let returned = resp.value.len();
            items.extend(resp.value);
            if resp.delta_link.is_some() {
                delta_link = resp.delta_link;
            }
            let Some(next) = resp.next_link else {
                return Ok((items, delta_link));
            };
            if prev_url.as_deref() == Some(next.as_str()) {
                return Ok((items, delta_link));
            }
            if returned == 0 {
                return Ok((items, delta_link));
            }
            prev_url = Some(next.clone());
            url = next;
        }
        Err(ConnectorError::Sync(format!(
            "sharepoint delta exceeded {MAX_DELTA_PAGES} pages without exhausting cursor"
        )))
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "read" | "viewer" => Some(SourcePermissionLevel::Read),
        "write" | "editor" | "contribute" => Some(SourcePermissionLevel::Write),
        "owner" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// The `file` facet of a Graph `DriveItem`.
#[derive(Debug, Clone, Default, Deserialize)]
struct SharePointFileFacet {
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
}

/// Item metadata needed by `fetch_content`.
#[derive(Debug, Clone, Default, Deserialize)]
struct SharePointItemMeta {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "webUrl")]
    web_url: Option<String>,
    #[serde(default, rename = "@microsoft.graph.downloadUrl")]
    download_url: Option<String>,
    #[serde(default)]
    file: Option<SharePointFileFacet>,
    #[serde(default)]
    folder: Option<serde_json::Value>,
}

/// Which sync pass produced this item.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

fn item_to_event(item: &DriveItem, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = item
        .last_modified_date_time
        .or(item.created_date_time)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(item.id.clone());
    if item.deleted.is_some() {
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

impl Connector for SharePointConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "sharepoint authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base = self.resolved_base_url(config);
        let version = self.resolved_api_version(config);
        let drive_path = Self::resolved_drive_path(config);
        let url = format!("{base}{version}{drive_path}/root/delta");
        let (items, delta_link) = self.paginate_delta(&url, token)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(items.len());
        for item in &items {
            events.push(item_to_event(item, SyncMode::Initial));
        }
        Ok(SyncRunResult {
            events,
            next_cursor: delta_link,
        })
    }

    fn incremental_sync(
        &self,
        _config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let delta_url = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "sharepoint incremental_sync: missing cursor; \
                 initial_sync must populate @odata.deltaLink first"
                    .into(),
            )
        })?;
        let (items, new_delta_link) = self.paginate_delta(delta_url, token)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(items.len());
        for item in &items {
            events.push(item_to_event(item, SyncMode::Incremental));
        }
        let next_cursor = new_delta_link.or_else(|| Some(delta_url.to_string()));
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
        let base = self.resolved_base_url(config);
        let version = self.resolved_api_version(config);
        let drive_path = Self::resolved_drive_path(config);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);

        let meta_url = format!(
            "{base}{version}{drive_path}/items/{id_enc}\
             ?$select=id,name,webUrl,file,folder,@microsoft.graph.downloadUrl"
        );
        let meta: SharePointItemMeta = bearer_get_json(
            &self.transport,
            "sharepoint",
            "/sites/{id}/drive/items/{id}",
            &meta_url,
            token,
            &[],
        )?;

        if meta.folder.is_some() && meta.file.is_none() {
            return Err(ConnectorError::Sync(format!(
                "sharepoint fetch_content: item {id} is a folder with no content"
            )));
        }

        let source_mime = meta
            .file
            .as_ref()
            .and_then(|f| f.mime_type.clone())
            .unwrap_or_default();

        let resp = if let Some(download_url) = meta.download_url.as_deref() {
            get_raw(
                &self.transport,
                "sharepoint",
                "@microsoft.graph.downloadUrl",
                download_url,
                &[],
            )?
        } else {
            let content_url = format!("{base}{version}{drive_path}/items/{id_enc}/content");
            bearer_get_raw(
                &self.transport,
                "sharepoint",
                "/sites/{id}/drive/items/{id}/content",
                &content_url,
                token,
                &[],
            )?
        };

        let mime = response_header(&resp, "content-type")
            .map(strip_charset)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .or_else(|| (!source_mime.is_empty()).then(|| source_mime.clone()))
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let source_url = meta.web_url.clone();
        let mut fc = FetchedContent::binary(resp.body, mime)
            .with_title(meta.name)
            .with_metadata(serde_json::json!({
                "provider": "sharepoint",
                "item_id": id,
                "source_mime_type": source_mime,
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
        let base = self.resolved_base_url(config);
        let version = self.resolved_api_version(config);
        let drive_path = Self::resolved_drive_path(config);
        let url = format!("{base}{version}/subscriptions");
        let client_state = config
            .auth_config_json
            .get("client_state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("graph-clientstate-secret")
            .to_string();
        let expires_at = Utc::now() + Duration::minutes(DEFAULT_SUBSCRIPTION_TTL_MINUTES);
        let body = serde_json::json!({
            "changeType": "updated,deleted",
            "notificationUrl": callback_url,
            "resource": format!("{drive_path}/root"),
            "expirationDateTime": expires_at.to_rfc3339(),
            "clientState": client_state,
        });
        let resp: GraphSubscriptionResponse = bearer_post_json(
            &self.transport,
            "sharepoint",
            "/subscriptions",
            &url,
            token,
            &[],
            &body,
        )?;
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(client_state),
            WebhookEventTypes::all(),
            Some(resp.expiration_date_time.unwrap_or(expires_at)),
        );
        subscription.provider_subscription_id = resp.id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let batch: ChangeNotificationCollection = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!(
                "sharepoint webhook: malformed notification body: {e}"
            ))
        })?;
        if batch.value.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty Graph changeNotification batch".to_string(),
            ));
        }
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(batch.value.len());
        for n in batch.value {
            let occurred_at = n.event_time.unwrap_or_else(Utc::now);
            let document_id = SourceDocumentId::new(
                n.resource
                    .rsplit('/')
                    .next()
                    .unwrap_or(&n.resource)
                    .to_string(),
            );
            let event = match n.change_type.as_str() {
                "created" => ConnectorEvent::DocumentCreated {
                    document_id,
                    occurred_at,
                },
                "updated" => ConnectorEvent::DocumentUpdated {
                    document_id,
                    occurred_at,
                },
                "deleted" => ConnectorEvent::DocumentDeleted {
                    document_id,
                    occurred_at,
                },
                "shared" | "permission_changed" => ConnectorEvent::PermissionChanged {
                    document_id,
                    user_id: SourceUserId::new(n.user_id.unwrap_or_default()),
                    new_level: n.new_role.as_deref().and_then(parse_role),
                    occurred_at,
                },
                _ => continue,
            };
            events.push(event);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "graph-access",
                "graph-refresh",
                Utc::now() + Duration::hours(1),
                "Sites.Read.All",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::SharePoint,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/graph",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const DELTA_URL: &str = "https://api.test/graph/v1.0/sites/root/drive/root/delta";

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "graph-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::SharePoint,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_and_seeds_delta_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            DELTA_URL,
            ok_json(&serde_json::json!({
                "value": [
                    { "id": "i1", "name": "a.docx", "lastModifiedDateTime": now },
                    { "id": "i2", "name": "b.xlsx", "lastModifiedDateTime": now },
                ],
                "@odata.deltaLink": "https://api.test/graph/v1.0/sites/root/drive/root/delta?token=D1"
            })),
        );
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("https://api.test/graph/v1.0/sites/root/drive/root/delta?token=D1")
        );
    }

    #[test]
    fn initial_sync_follows_next_link() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            DELTA_URL,
            ok_json(&serde_json::json!({
                "value": [ { "id": "i1", "name": "a", "lastModifiedDateTime": now } ],
                "@odata.nextLink": "https://api.test/graph/v1.0/sites/root/drive/root/delta?page=2"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/graph/v1.0/sites/root/drive/root/delta?page=2",
            ok_json(&serde_json::json!({
                "value": [ { "id": "i2", "name": "b", "lastModifiedDateTime": now } ],
                "@odata.deltaLink": "https://api.test/graph/v1.0/sites/root/drive/root/delta?token=D2"
            })),
        );
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
    }

    #[test]
    fn incremental_sync_maps_deleted() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let cursor = "https://api.test/graph/v1.0/sites/root/drive/root/delta?token=D1";
        transport.expect(
            HttpMethod::Get,
            cursor,
            ok_json(&serde_json::json!({
                "value": [
                    { "id": "i9", "name": "gone", "lastModifiedDateTime": now, "deleted": { "state": "deleted" } },
                ],
                "@odata.deltaLink": "https://api.test/graph/v1.0/sites/root/drive/root/delta?token=D2"
            })),
        );
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor.into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            DELTA_URL,
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn fetch_content_uses_download_url() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/graph/v1.0/sites/root/drive/items/i1\
             ?$select=id,name,webUrl,file,folder,@microsoft.graph.downloadUrl",
            ok_json(&serde_json::json!({
                "id": "i1",
                "name": "report.docx",
                "webUrl": "https://contoso.sharepoint.com/report.docx",
                "@microsoft.graph.downloadUrl": "https://cdn.test/dl/i1",
                "file": { "mimeType": "application/vnd.openxmlformats-officedocument.wordprocessingml.document" }
            })),
        );
        let mut bytes = MockResponse::status(200, b"docx-bytes".to_vec());
        bytes
            .headers
            .push(("content-type".into(), "application/octet-stream".into()));
        transport.expect(HttpMethod::Get, "https://cdn.test/dl/i1", bytes);
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("i1"))
            .unwrap();
        assert_eq!(fc.title.as_deref(), Some("report.docx"));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://contoso.sharepoint.com/report.docx")
        );
        assert_eq!(fc.body, b"docx-bytes");
    }

    #[test]
    fn subscribe_webhook_posts_subscription_and_keeps_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/graph/v1.0/subscriptions",
            ok_json(&serde_json::json!({ "id": "sub-1" })),
        );
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/sp")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("sub-1"));
    }

    #[test]
    fn webhook_batch_emits_every_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "value": [
                { "resource": "sites/s/drive/items/1", "changeType": "created" },
                { "resource": "sites/s/drive/items/2", "changeType": "deleted" },
                { "resource": "sites/s/drive/items/3", "changeType": "bogus" },
            ]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_empty_batch_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "value": [] });
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_malformed_body_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SharePointConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
