//! Miro connector — Miro REST API v2 + board-subscription webhooks.
//!
//! * `initial_sync` walks `GET /v2/boards` and pages via Miro's
//!   opaque `cursor` token.
//! * `incremental_sync` walks the same endpoint and filters
//!   client-side on `modifiedAt` against the prior watermark (the
//!   boards list has no server-side "modified since" filter).
//! * `fetch_content` reads a single board and renders a Markdown
//!   summary.
//! * `subscribe_webhook` POSTs `/v2/webhooks/board_subscriptions`.
//! * `handle_webhook_event` parses a batched event payload
//!   (`{events:[{type,item:{id},…}]}`) and emits **every** event.
//!
//! Miro authenticates with OAuth2 bearer tokens, so the bearer
//! helpers apply directly. `authenticate` accepts a configured
//! `access_token` or an OAuth2 `authorization_code`.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Miro REST base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.miro.com";

/// Page size for list endpoints. Miro's documented max is 50.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured token.
const DEFAULT_SCOPE: &str = "boards:read";

/// One Miro board (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiroBoard {
    /// Board id.
    pub id: String,
    /// Board name.
    #[serde(default)]
    pub name: Option<String>,
    /// Board description.
    #[serde(default)]
    pub description: Option<String>,
    /// Creation timestamp.
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Last-modification timestamp.
    #[serde(rename = "modifiedAt", default)]
    pub modified_at: Option<DateTime<Utc>>,
    /// Canonical view link.
    #[serde(rename = "viewLink", default)]
    pub view_link: Option<String>,
}

/// One page of `GET /v2/boards`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiroBoardsResponse {
    /// Boards on this page.
    #[serde(default)]
    pub data: Vec<MiroBoard>,
    /// Opaque pagination cursor for the next page, when present.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Response from `POST /v2/webhooks/board_subscriptions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiroSubscriptionResponse {
    /// Subscription id.
    #[serde(default)]
    pub id: String,
}

/// Batched webhook payload (`{events:[…]}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiroWebhookPayload {
    /// Ordered batch of board events.
    #[serde(default)]
    pub events: Vec<MiroEvent>,
}

/// One Miro board event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiroEvent {
    /// Event type (`create`, `update`, `delete`).
    #[serde(rename = "type", default)]
    pub event_type: Option<String>,
    /// Event timestamp.
    #[serde(rename = "occurredAt", default)]
    pub occurred_at: Option<DateTime<Utc>>,
    /// The item the event concerns.
    #[serde(default)]
    pub item: Option<MiroItemRef>,
}

/// Minimal item reference inside an event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiroItemRef {
    /// Item id.
    #[serde(default)]
    pub id: String,
}

/// Miro connector.
pub struct MiroConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for MiroConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiroConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl MiroConnector {
    /// Construct a Miro connector.
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

    /// Override the Miro REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 50]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 50);
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

    /// Walk every boards page until no `cursor` is returned.
    fn paginate_boards(&self, base_url: &str, token: &OAuth2Token) -> Result<Vec<MiroBoard>> {
        let mut out = Vec::<MiroBoard>::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut url = format!("{base_url}/v2/boards?limit={}", self.page_size);
            if let Some(ref cur) = cursor {
                let _ = write!(url, "&cursor={}", percent_encode_path_component(cur));
            }
            let resp: MiroBoardsResponse =
                bearer_get_json(&self.transport, "miro", "/v2/boards", &url, token, &[])?;
            out.extend(resp.data);
            match resp.cursor {
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
        Err(ConnectorError::Sync(format!(
            "miro /v2/boards exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn board_to_event(b: &MiroBoard, kind: &str) -> ConnectorEvent {
    let occurred_at = b.modified_at.or(b.created_at).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(b.id.clone());
    match kind {
        "create" => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for MiroConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(tok) = config
            .auth_config_json
            .get("access_token")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                tok,
                Utc::now() + chrono::Duration::days(3650),
                DEFAULT_SCOPE,
            ));
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "miro authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let boards = self.paginate_boards(&base_url, token)?;
        let mut events = Vec::with_capacity(boards.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for b in &boards {
            events.push(board_to_event(b, "create"));
            if let Some(t) = b.modified_at.or(b.created_at) {
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
        let prior = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let boards = self.paginate_boards(&base_url, token)?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for b in &boards {
            let when = b.modified_at.or(b.created_at);
            // Miro's board list has no server-side modified filter, so
            // emit only boards touched strictly after the watermark.
            if let (Some(prev), Some(t)) = (prior, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(board_to_event(b, "update"));
            if let Some(t) = when {
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
        let id_enc = percent_encode_path_component(document_id.as_str());
        let url = format!("{base_url}/v2/boards/{id_enc}");
        let board: MiroBoard =
            bearer_get_json(&self.transport, "miro", "/v2/boards/{id}", &url, token, &[])?;

        let title = board
            .name
            .clone()
            .unwrap_or_else(|| format!("Board {}", document_id.as_str()));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(desc) = board.description.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(desc);
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "miro",
                "board_id": board.id,
            }))
            .with_source_url(
                board.view_link.unwrap_or_else(|| {
                    format!("https://miro.com/app/board/{}", document_id.as_str())
                }),
            ))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let board_id = config
            .auth_config_json
            .get("board_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "miro subscribe_webhook: auth_config_json.board_id is required".into(),
                )
            })?;
        let url = format!("{base_url}/v2/webhooks/board_subscriptions");
        let body = serde_json::json!({
            "boardId": board_id,
            "callbackUrl": callback_url,
            "status": "enabled",
        });
        let resp: MiroSubscriptionResponse = bearer_post_json(
            &self.transport,
            "miro",
            "/v2/webhooks/board_subscriptions",
            &url,
            token,
            &[],
            &body,
        )?;
        if resp.id.is_empty() {
            return Err(ConnectorError::Webhook(
                "miro /v2/webhooks/board_subscriptions returned no id".into(),
            ));
        }
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("miro-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: MiroWebhookPayload = serde_json::from_slice(body)?;
        let mut events = Vec::with_capacity(payload.events.len());
        for ev in &payload.events {
            let Some(item) = ev.item.as_ref() else {
                continue;
            };
            if item.id.is_empty() {
                continue;
            }
            let occurred_at = ev.occurred_at.unwrap_or_else(Utc::now);
            let id = SourceDocumentId::new(item.id.clone());
            let mapped = match ev.event_type.as_deref() {
                Some("create") => ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                },
                Some("delete") => ConnectorEvent::DocumentDeleted {
                    document_id: id,
                    occurred_at,
                },
                _ => ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                },
            };
            events.push(mapped);
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
                "miro-access",
                "miro-refresh",
                Utc::now() + Duration::hours(1),
                "boards:read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Miro, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "miro-tok",
                "board_id": "board-xyz",
                "api_base_url": "https://api.test/miro",
            }))
    }

    fn board(id: &str, modified: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": format!("Board {id}"), "modifiedAt": modified,
            "viewLink": format!("https://miro.com/app/board/{id}")
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "miro-tok"
        );
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Miro, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "authorization_code": "abc" }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "miro-access"
        );
    }

    #[test]
    fn initial_sync_paginates_via_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/miro/v2/boards?limit=50",
            ok_json(&serde_json::json!({
                "data": [board("b1", "2024-01-01T00:00:00Z")], "cursor": "cur2"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/miro/v2/boards?limit=50&cursor=cur2",
            ok_json(&serde_json::json!({ "data": [board("b2", "2024-01-02T00:00:00Z")] })),
        );
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_client_side() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/miro/v2/boards?limit=50",
            ok_json(&serde_json::json!({ "data": [
                board("old", "2024-01-01T00:00:00Z"),
                board("new", "2024-02-01T00:00:00Z"),
            ] })),
        );
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "new");
    }

    #[test]
    fn subscribe_webhook_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/miro/v2/webhooks/board_subscriptions",
            ok_json(&serde_json::json!({ "id": "sub42" })),
        );
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/miro")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("sub42"));
    }

    #[test]
    fn subscribe_webhook_requires_board_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Miro, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "access_token": "t" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.subscribe_webhook(&cfg, &tok, "https://hook").unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }

    #[test]
    fn webhook_emits_every_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "events": [
                { "type": "create", "item": { "id": "i1" }, "occurredAt": "2024-03-01T00:00:00Z" },
                { "type": "update", "item": { "id": "i2" } },
                { "type": "delete", "item": { "id": "i3" } }
            ]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_skips_events_without_item() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "events": [ { "type": "update" } ] });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/miro/v2/boards/board7",
            ok_json(&serde_json::json!({
                "id": "board7", "name": "Roadmap", "description": "Q3 plans"
            })),
        );
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("board7"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Roadmap"));
        assert!(body.contains("Q3 plans"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/miro/v2/boards/none",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = MiroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("none"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
