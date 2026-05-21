//! OneDrive connector — Microsoft Graph `/me/drive/root/delta`.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`] against
//!   `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token`
//!   (production: `OAuth2Client<BlockingHttpTransport>` driven by
//!   `default_oauth_client`; tests: `MockHttpTransport`).
//! * `initial_sync` walks `/me/drive/root/delta` from scratch and
//!   emits one [`ConnectorEvent`] per `DriveItem`. Graph paginates
//!   via `@odata.nextLink` (a fully-qualified URL we follow verbatim)
//!   and surfaces the final cursor as `@odata.deltaLink`.
//! * `incremental_sync` polls the previously-seen `@odata.deltaLink`
//!   verbatim — Graph's contract is that the delta link carries the
//!   server-state cursor and accepts no other parameters.
//! * `subscribe_webhook` POSTs `/subscriptions` to install a Graph
//!   change-notification subscription targeting `callback_url`
//!   (Microsoft Graph drive subscriptions max out at ~3-day TTL; the
//!   substrate-side renewal job ticks well before then). The
//!   subscription id Graph returns is stashed in
//!   `provider_subscription_id` so the substrate's revoke path has
//!   what it needs.
//! * `handle_webhook_event` parses Graph's
//!   [`changeNotificationCollection`](https://learn.microsoft.com/en-us/graph/api/resources/changenotification)
//!   payload — every notification in the batch is materialised
//!   into a substrate event; unknown `changeType` strings are
//!   skipped (defence-in-depth so a new Graph lifecycle value can't
//!   silently discard valid events queued behind it).
//!
//! Wiring contract (mirror of the Google Drive / Jira / Confluence
//! connectors): the constructor takes an `Arc<dyn HttpTransport>` and
//! an `Arc<dyn OAuth2CodeExchange>`; production wires
//! `BlockingHttpTransport` + `OAuth2Client`, tests wire
//! `MockHttpTransport` + a fixed-token exchange.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Graph base URL. Override via
/// `auth_config_json.api_base_url` for sandboxes / sovereign clouds.
pub const DEFAULT_API_BASE_URL: &str = "https://graph.microsoft.com";

/// Default Graph API version path.
pub const DEFAULT_API_VERSION: &str = "/v1.0";

/// Default substrate-side subscription TTL — Graph caps drive
/// subscriptions at ~4230 minutes (≈3 days); we sit one minute under
/// the limit to leave breathing room for clock skew.
pub const DEFAULT_SUBSCRIPTION_TTL_MINUTES: i64 = 4_229;

/// Safety ceiling on number of pages a single sync will walk —
/// catches mis-shaped server responses that return a non-empty page
/// without ever clearing `@odata.nextLink`.
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

/// One page of `/drive/root/delta` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeltaResponse {
    /// Items that changed in this page.
    #[serde(default)]
    pub value: Vec<DriveItem>,
    /// Forward link to the next page (mid-walk). Graph emits a
    /// fully-qualified URL here; we follow it verbatim.
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    /// Final-state cursor — present on the last page only. The next
    /// `incremental_sync` POSTs this verbatim.
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// `/subscriptions` create response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphSubscriptionResponse {
    /// Subscription id Graph assigned (needed for revoke).
    #[serde(default)]
    pub id: Option<String>,
    /// Echoed expiry (RFC-3339) — Graph caps drive subs at ~3 days.
    #[serde(default, rename = "expirationDateTime")]
    pub expiration_date_time: Option<DateTime<Utc>>,
}

/// One `changeNotification` in a Graph subscription batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeNotification {
    /// Substrate-side resource id (`drive/items/{id}`).
    pub resource: String,
    /// Lifecycle string: `created`, `updated`, `deleted`,
    /// `shared` (== permission change).
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

/// `changeNotificationCollection` payload — what Graph POSTs to the
/// `notificationUrl` of an active subscription.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeNotificationCollection {
    /// Notification batch.
    #[serde(default)]
    pub value: Vec<ChangeNotification>,
}

/// OneDrive connector.
///
/// Holds the wired [`HttpTransport`] + [`OAuth2CodeExchange`] used to
/// drive every Graph REST call (token exchange, delta walking,
/// subscription create).
pub struct OneDriveConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    api_version: String,
}

impl std::fmt::Debug for OneDriveConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneDriveConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("api_version", &self.api_version)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl OneDriveConnector {
    /// Construct a OneDrive connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token`.
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

    /// Where the substrate Graph drive lives. Defaults to
    /// `/me/drive` — override via `auth_config_json.drive_path` to
    /// target a different drive (e.g. `/drives/{drive-id}` for an
    /// app-scoped tenant install).
    fn resolved_drive_path(config: &ConnectorConfig) -> &str {
        config
            .auth_config_json
            .get("drive_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("/me/drive")
    }

    /// Walk every `@odata.nextLink` page until either the link is
    /// absent, the server returns an empty page with no link, or
    /// [`MAX_DELTA_PAGES`] is hit. Returns the merged page list +
    /// the final `@odata.deltaLink`.
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
                "onedrive",
                "/drive/root/delta",
                &url,
                token,
                &[],
            )?;
            let returned = resp.value.len();
            items.extend(resp.value);
            // Graph's contract: `@odata.deltaLink` is set only on
            // the final page; persist it for the substrate
            // watermark.
            if resp.delta_link.is_some() {
                delta_link = resp.delta_link;
            }
            let Some(next) = resp.next_link else {
                return Ok((items, delta_link));
            };
            // Loop guard — a misbehaving server that echoes the same
            // nextLink on every page would otherwise spin forever.
            if prev_url.as_deref() == Some(next.as_str()) {
                return Ok((items, delta_link));
            }
            // Empty page mid-stream → end-of-list defensively.
            if returned == 0 {
                return Ok((items, delta_link));
            }
            prev_url = Some(next.clone());
            url = next;
        }
        Err(ConnectorError::Sync(format!(
            "onedrive /drive/root/delta exceeded {MAX_DELTA_PAGES} pages without exhausting cursor"
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

/// Which sync pass produced this item — we use this instead of
/// comparing `createdDateTime == lastModifiedDateTime` because
/// during `initial_sync` the substrate is seeing every non-deleted
/// item for the first time and must classify it as
/// `DocumentCreated` regardless of whether the file has been edited
/// upstream. Mirror of the `SyncMode` enum in the Notion and
/// HubSpot connectors.
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

impl Connector for OneDriveConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "onedrive authenticate: auth_config_json.authorization_code is required".into(),
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
        // Graph's `@odata.deltaLink` is a fully-qualified URL with
        // the server-state cursor baked into the query string — we
        // POST/GET it verbatim. Without a cursor we cannot
        // incrementally fetch; surface the gap so the substrate
        // reschedules with the seed populated.
        let delta_url = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "onedrive incremental_sync: missing cursor; \
                 initial_sync must populate @odata.deltaLink first"
                    .into(),
            )
        })?;
        let (items, new_delta_link) = self.paginate_delta(delta_url, token)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(items.len());
        for item in &items {
            events.push(item_to_event(item, SyncMode::Incremental));
        }
        // Graph returns `@odata.deltaLink` on the final page. If for
        // any reason the server omitted it, fall back to the existing
        // cursor so we don't lose our place.
        let next_cursor = new_delta_link.or_else(|| Some(delta_url.to_string()));
        Ok(SyncRunResult {
            events,
            next_cursor,
        })
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
            "onedrive",
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
        // Microsoft Graph delivers batched `changeNotification`
        // payloads under a top-level `value` array. Every recognised
        // entry must be materialised — returning only the first one
        // would silently drop concurrent file changes.
        //
        // Unknown `changeType`s are skipped rather than aborting the
        // whole batch — when Graph adds a new lifecycle string we
        // cannot retroactively discard every well-formed event that
        // happened to be queued behind it. Mirrors the HubSpot
        // handler's policy on unknown subscription types.
        let batch: ChangeNotificationCollection = serde_json::from_slice(body)?;
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
                "Files.Read.All Sites.Read.All",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::OneDrive, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/graph",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const DELTA_URL: &str = "https://api.test/graph/v1.0/me/drive/root/delta";

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = MockHttpTransport::new();
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("Files.Read.All"));
        assert_eq!(tok.access_token.expose(), "graph-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = MockHttpTransport::new();
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let bad_cfg =
            ConnectorConfig::new(ConnectorKind::OneDrive, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&bad_cfg).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_walks_nextlink_and_advances_watermark() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let next_link = "https://api.test/graph/v1.0/me/drive/root/delta?$skiptoken=ABC";
        transport.expect(
            HttpMethod::Get,
            DELTA_URL,
            ok_json(&serde_json::json!({
                "value": [{
                    "id": "f1",
                    "name": "A.docx",
                    "lastModifiedDateTime": now,
                }],
                "@odata.nextLink": next_link,
            })),
        );
        transport.expect(
            HttpMethod::Get,
            next_link,
            ok_json(&serde_json::json!({
                "value": [{
                    "id": "f2",
                    "name": "B.docx",
                    "lastModifiedDateTime": now,
                }],
                "@odata.deltaLink": "https://api.test/graph/v1.0/me/drive/root/delta?token=tok-1",
            })),
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("https://api.test/graph/v1.0/me/drive/root/delta?token=tok-1")
        );
    }

    #[test]
    fn initial_sync_emits_deleted_for_deleted_facet() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            DELTA_URL,
            ok_json(&serde_json::json!({
                "value": [{
                    "id": "f1",
                    "name": "X.docx",
                    "deleted": { "state": "deleted" },
                }],
                "@odata.deltaLink": "tok-z",
            })),
        );
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn paginate_delta_loop_guard_stops_on_repeated_link() {
        let transport = MockHttpTransport::new();
        // Same URL echoed as nextLink — pathological.
        transport.expect(
            HttpMethod::Get,
            DELTA_URL,
            ok_json(&serde_json::json!({
                "value": [{"id":"a", "name":"A"}],
                "@odata.nextLink": "https://api.test/graph/v1.0/me/drive/root/delta?stuck=1",
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/graph/v1.0/me/drive/root/delta?stuck=1",
            ok_json(&serde_json::json!({
                "value": [{"id":"b", "name":"B"}],
                "@odata.nextLink": "https://api.test/graph/v1.0/me/drive/root/delta?stuck=1",
            })),
        );
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
    }

    #[test]
    fn incremental_sync_walks_delta_link_verbatim() {
        let transport = MockHttpTransport::new();
        let cursor = "https://api.test/graph/v1.0/me/drive/root/delta?token=prev";
        transport.expect(
            HttpMethod::Get,
            cursor,
            ok_json(&serde_json::json!({
                "value": [
                    {"id": "f1", "name": "A.docx"},
                    {"id": "f2", "name": "B.docx", "deleted": {"state":"deleted"}},
                ],
                "@odata.deltaLink": "https://api.test/graph/v1.0/me/drive/root/delta?token=next",
            })),
        );
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor.into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 2);
        let updates = res
            .events
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentUpdated { .. }))
            .count();
        let deletes = res
            .events
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }))
            .count();
        assert_eq!(updates, 1);
        assert_eq!(deletes, 1);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("https://api.test/graph/v1.0/me/drive/root/delta?token=next")
        );
    }

    #[test]
    fn incremental_sync_falls_back_to_existing_cursor_when_no_new_delta_link() {
        let transport = MockHttpTransport::new();
        let cursor = "https://api.test/graph/v1.0/me/drive/root/delta?token=hold";
        transport.expect(
            HttpMethod::Get,
            cursor,
            ok_json(&serde_json::json!({"value": []})),
        );
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor.into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 0);
        assert_eq!(res.next_cursor.as_deref(), Some(cursor));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        match err {
            ConnectorError::Sync(msg) => assert!(msg.contains("missing cursor")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn subscribe_webhook_posts_subscription_and_captures_id() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/graph/v1.0/subscriptions",
            ok_json(&serde_json::json!({
                "id": "sub-abc",
                "expirationDateTime": (Utc::now() + Duration::days(2)).to_rfc3339(),
            })),
        );
        let transport_arc: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport_arc, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/onedrive")
            .unwrap();
        assert_eq!(sub.connector, c.instance);
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("sub-abc"));
        assert!(sub.expires_at.is_some());
    }

    #[test]
    fn subscribe_webhook_falls_back_to_local_ttl_when_server_omits_expiration() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/graph/v1.0/subscriptions",
            ok_json(&serde_json::json!({"id": "sub-xyz"})),
        );
        let transport_arc: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport_arc, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/onedrive")
            .unwrap();
        assert!(sub.expires_at.is_some());
    }

    #[test]
    fn unauthorized_status_maps_to_auth_error() {
        let transport = MockHttpTransport::new();
        transport.with_default_response(MockResponse::status(
            401,
            br#"{"error":"unauthorized"}"#.to_vec(),
        ));
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn webhook_parses_shared_as_permission_change() {
        let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
        let body = serde_json::json!({
            "value": [{
                "resource": "drive/items/item-7",
                "changeType": "shared",
                "subscriptionId": "sub-1",
                "eventTime": Utc::now(),
                "user_id": "u-3",
                "new_role": "write",
            }]
        });
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged {
                document_id,
                new_level,
                ..
            } => {
                assert_eq!(document_id.as_str(), "item-7");
                assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_empty_batch_errors() {
        let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
        let body = serde_json::json!({"value": []});
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_unknown_change_type_is_skipped_not_errored() {
        let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
        let body = serde_json::json!({
            "value": [
                {
                    "resource": "drive/items/file-a",
                    "changeType": "created",
                    "subscriptionId": "sub-1",
                    "eventTime": Utc::now(),
                },
                {
                    "resource": "drive/items/file-b",
                    "changeType": "undocumented_future_event",
                    "subscriptionId": "sub-1",
                    "eventTime": Utc::now(),
                },
                {
                    "resource": "drive/items/file-c",
                    "changeType": "deleted",
                    "subscriptionId": "sub-1",
                    "eventTime": Utc::now(),
                }
            ]
        });
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn initial_sync_classifies_items_as_created_regardless_of_timestamps() {
        let transport = MockHttpTransport::new();
        let created = Utc::now() - Duration::days(7);
        let modified = Utc::now();
        transport.expect(
            HttpMethod::Get,
            DELTA_URL,
            ok_json(&serde_json::json!({
                "value": [
                    {
                        "id": "item-edited",
                        "name": "Edited.docx",
                        "createdDateTime": created,
                        "lastModifiedDateTime": modified,
                    },
                    {
                        "id": "item-untouched",
                        "name": "Untouched.docx",
                        "createdDateTime": created,
                        "lastModifiedDateTime": created,
                    },
                ],
                "@odata.deltaLink": "tok-delta",
            })),
        );
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        for ev in &res.events {
            assert!(
                matches!(ev, ConnectorEvent::DocumentCreated { .. }),
                "initial_sync must emit DocumentCreated for every non-deleted item, got {ev:?}"
            );
        }
    }
}
