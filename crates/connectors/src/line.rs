//! LINE connector — LINE Messaging API (`https://api.line.me`).
//!
//! LINE is the dominant messaging platform in Thailand, Japan and
//! Taiwan. The Messaging API authenticates with a long-lived
//! *channel access token* presented as a bearer credential — there is
//! no authorization-code exchange, so [`LineConnector::authenticate`]
//! reads the token straight out of `auth_config_json`.
//!
//! * `initial_sync` / `incremental_sync` list the bot's rich menus
//!   (`GET /v2/bot/richmenu/list`). Rich menus carry no server-side
//!   timestamps and the list is small, so the connector re-walks the
//!   full set each run (initial emits `DocumentCreated`, incremental
//!   emits `DocumentUpdated`) and never stores a cursor.
//! * `fetch_content` GETs a single rich menu
//!   (`/v2/bot/richmenu/{id}`) and renders Markdown from its name,
//!   chat-bar text and tappable-area count.
//! * LINE webhooks are configured in the LINE Developers console (no
//!   REST endpoint creates them), so `subscribe_webhook` records a
//!   polling-only subscription with no provider id.
//! * `handle_webhook_event` parses the console-delivered payload
//!   (`{ "events": [ { "type": "message", … } ] }`) and emits one
//!   `DocumentCreated` per inbound message event.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::Deserialize;

/// Default LINE Messaging API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.line.me";
/// Scope label recorded on the synthesised channel-access token.
pub const DEFAULT_SCOPE: &str = "messaging";

#[derive(Debug, Deserialize)]
struct RichMenuListResponse {
    #[serde(default)]
    richmenus: Vec<RichMenu>,
}

#[derive(Debug, Deserialize)]
struct RichMenu {
    #[serde(rename = "richMenuId")]
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "chatBarText")]
    chat_bar_text: Option<String>,
    #[serde(default)]
    areas: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct LineWebhookBody {
    #[serde(default)]
    events: Vec<LineWebhookEvent>,
}

#[derive(Debug, Deserialize)]
struct LineWebhookEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    message: Option<LineMessage>,
}

#[derive(Debug, Deserialize)]
struct LineMessage {
    id: String,
}

/// LINE Messaging API connector.
pub struct LineConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
}

impl std::fmt::Debug for LineConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl LineConnector {
    /// Construct a LINE connector.
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
        }
    }

    /// Override the LINE API base URL.
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

    fn list_rich_menus(&self, base_url: &str, token: &OAuth2Token) -> Result<Vec<RichMenu>> {
        let url = format!("{base_url}/v2/bot/richmenu/list");
        let resp: RichMenuListResponse = bearer_get_json(
            &self.transport,
            "line",
            "/v2/bot/richmenu/list",
            &url,
            token,
            &[],
        )?;
        Ok(resp.richmenus)
    }
}

impl Connector for LineConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(token) = config
            .auth_config_json
            .get("channel_access_token")
            .and_then(serde_json::Value::as_str)
        {
            // Channel access tokens are long-lived (30-day or stateless
            // JWT). Record a far-future expiry so the vault treats the
            // token as durable rather than attempting a refresh grant
            // the Messaging API does not support.
            return Ok(OAuth2Token::new_without_refresh(
                token,
                Utc::now() + chrono::Duration::days(30),
                DEFAULT_SCOPE,
            ));
        }
        // Channel access tokens can also be issued via LINE Login's
        // authorization-code grant; fall back to the injected exchange
        // when no static token is configured.
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "line authenticate: auth_config_json.channel_access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let menus = self.list_rich_menus(&base_url, token)?;
        let now = Utc::now();
        let events = menus
            .into_iter()
            .map(|m| ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(m.id),
                occurred_at: now,
            })
            .collect();
        Ok(SyncRunResult {
            events,
            next_cursor: None,
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        _state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let menus = self.list_rich_menus(&base_url, token)?;
        let now = Utc::now();
        let events = menus
            .into_iter()
            .map(|m| ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(m.id),
                occurred_at: now,
            })
            .collect();
        Ok(SyncRunResult {
            events,
            next_cursor: None,
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
        let url = format!("{base_url}/v2/bot/richmenu/{id_enc}");
        let menu: RichMenu = bearer_get_json(
            &self.transport,
            "line",
            "/v2/bot/richmenu/{id}",
            &url,
            token,
            &[],
        )?;
        let name = menu.name.as_deref().unwrap_or("(untitled rich menu)");
        let chat_bar = menu.chat_bar_text.as_deref().unwrap_or("");
        let body = format!(
            "# {name}\n\nChat bar: {chat_bar}\n\nTappable areas: {}\n",
            menu.areas.len()
        );
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(name.to_string())
            .with_metadata(serde_json::json!({
                "rich_menu_id": menu.id,
                "areas": menu.areas.len(),
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // LINE webhooks are bound to the channel through the LINE
        // Developers console; there is no REST endpoint to create
        // them. Record a polling-only subscription keyed off the
        // channel secret so the substrate can still verify inbound
        // signatures.
        let secret = config
            .auth_config_json
            .get("channel_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("line-channel-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let parsed: LineWebhookBody = serde_json::from_slice(body)
            .map_err(|e| ConnectorError::Webhook(format!("line webhook parse failed: {e}")))?;
        let mut events = Vec::new();
        for ev in parsed.events {
            if ev.event_type != "message" {
                continue;
            }
            let Some(message) = ev.message else { continue };
            let occurred_at = ev
                .timestamp
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(message.id),
                occurred_at,
            });
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
                "unused",
                "unused",
                Utc::now() + Duration::hours(1),
                "messaging",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Line, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "channel_access_token": "channel-token",
                "channel_secret": "chsecret",
                "api_base_url": "https://api.test/line",
            }))
    }

    fn json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_channel_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = LineConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "channel-token");
        assert!(token.refresh_token.is_none());
    }

    #[test]
    fn authenticate_requires_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = LineConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bad = ConnectorConfig::new(ConnectorKind::Line, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(c.authenticate(&bad).is_err());
    }

    #[test]
    fn initial_sync_lists_rich_menus() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/line/v2/bot/richmenu/list",
            json(&serde_json::json!({
                "richmenus": [
                    { "richMenuId": "rm-1", "name": "Main" },
                    { "richMenuId": "rm-2", "name": "Promo" }
                ]
            })),
        );
        let c = LineConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        let out = c.initial_sync(&cfg(), &token).unwrap();
        assert_eq!(out.events.len(), 2);
        assert!(out.next_cursor.is_none());
        assert!(matches!(
            out.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn incremental_sync_emits_updates() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/line/v2/bot/richmenu/list",
            json(&serde_json::json!({ "richmenus": [ { "richMenuId": "rm-1" } ] })),
        );
        let c = LineConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let out = c.incremental_sync(&cfg(), &token, &state).unwrap();
        assert_eq!(out.events.len(), 1);
        assert!(matches!(
            out.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/line/v2/bot/richmenu/rm-1",
            json(&serde_json::json!({
                "richMenuId": "rm-1",
                "name": "Main",
                "chatBarText": "Tap here",
                "areas": [ {}, {} ]
            })),
        );
        let c = LineConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &token, &SourceDocumentId::new("rm-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Main"));
        assert!(body.contains("Tappable areas: 2"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = LineConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &token, "https://substrate.example/webhooks/line")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "chsecret");
    }

    #[test]
    fn handle_webhook_event_parses_messages() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = LineConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "events": [
                { "type": "message", "timestamp": 1_700_000_000_000_i64, "message": { "id": "m-1" } },
                { "type": "follow", "timestamp": 1_700_000_000_001_i64 }
            ]
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "m-1");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
