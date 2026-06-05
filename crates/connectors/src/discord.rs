//! Discord connector — Discord REST API v10.
//!
//! A Discord connector instance ingests one text channel's message
//! history (`/channels/{channel-id}/messages`).
//!
//! * `authenticate` supports two credential models. When
//!   `auth_config_json.authorization_code` is present it runs the
//!   wired [`OAuth2CodeExchange`] (user OAuth2, `Bearer` scheme). When
//!   only `auth_config_json.bot_token` is present it wraps the bot
//!   token in an [`OAuth2Token`] tagged `token_type = "Bot"` so the
//!   REST calls authenticate with Discord's `Authorization: Bot …`
//!   scheme (the scheme bot tokens require — `Bearer` is rejected).
//! * `initial_sync` walks the channel history backwards via the
//!   `before` cursor (Discord returns newest-first, ≤100 per page) and
//!   emits one [`ConnectorEvent::DocumentCreated`] per message. The
//!   highest message snowflake seen becomes the cursor.
//! * `incremental_sync` polls forward with `after=<cursor>` and emits
//!   the new messages, advancing the cursor to the newest snowflake.
//! * `fetch_content` GETs a single message and returns its text body.
//! * `subscribe_webhook` makes no REST call — Discord pushes events
//!   over the Gateway / Interactions endpoint configured out-of-band in
//!   the Developer Portal. The returned [`WebhookSubscription`] carries
//!   the application's Ed25519 public key so the substrate can verify
//!   interaction signatures.
//! * `handle_webhook_event` parses a Gateway dispatch payload
//!   (`MESSAGE_CREATE` / `MESSAGE_UPDATE` / `MESSAGE_DELETE`), draining
//!   a batched array if Discord delivers one, and treats an
//!   interaction `PING` (no dispatch type) as an empty result rather
//!   than an error.
//!
//! Wiring contract mirrors the other connectors: the constructor takes
//! an `Arc<dyn HttpTransport>` and an `Arc<dyn OAuth2CodeExchange>`.

use std::cmp::Ordering;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpResponse, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Discord REST base URL. Override via
/// `auth_config_json.api_base_url`.
pub const DEFAULT_API_BASE_URL: &str = "https://discord.com/api/v10";

/// Messages requested per history page (Discord's documented max).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of history pages a single sync will walk.
pub const MAX_HISTORY_PAGES: usize = 100_000;

/// One Discord message (subset relevant to substrate ingestion).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscordMessage {
    /// Message snowflake id.
    #[serde(default)]
    pub id: String,
    /// Message text content.
    #[serde(default)]
    pub content: String,
    /// ISO-8601 send timestamp.
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    /// ISO-8601 last-edit timestamp (null if never edited).
    #[serde(default)]
    pub edited_timestamp: Option<DateTime<Utc>>,
}

/// Discord connector. Holds the wired transport + OAuth exchange.
pub struct DiscordConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for DiscordConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl DiscordConnector {
    /// Construct a Discord connector.
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

    /// Override the Discord REST base URL.
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

    fn resolved_channel(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("channel_id")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync(
                    "discord: auth_config_json.channel_id is required (the channel to ingest)"
                        .into(),
                )
            })
    }

    /// Issue an authenticated GET, picking the `Authorization` scheme
    /// from the token's `token_type` (`Bot` for bot tokens, `Bearer`
    /// for user OAuth2 tokens), and parse the JSON response.
    fn get_json<R: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let scheme = if token.token_type.is_empty() {
            "Bearer"
        } else {
            token.token_type.as_str()
        };
        let auth = format!("{scheme} {}", token.access_token.expose());
        let req = HttpRequest::get(url)
            .with_header("Authorization", auth)
            .with_header("Accept", "application/json");
        let resp: HttpResponse = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("discord", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body)
            .map_err(|e| ConnectorError::Sync(format!("discord {endpoint} JSON parse failed: {e}")))
    }
}

/// Order two Discord snowflake id strings chronologically.
///
/// Snowflakes are 64-bit, monotonically increasing with time. Compare
/// numerically when both parse, and fall back to length-then-
/// lexicographic order for any non-numeric id so ordering stays total
/// and never regresses. A plain `str::cmp` would misorder ids of
/// differing digit-length (e.g. `"9"` vs `"10"`).
fn snowflake_cmp(a: &str, b: &str) -> Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(na), Ok(nb)) => na.cmp(&nb),
        _ => (a.len(), a).cmp(&(b.len(), b)),
    }
}

/// Return the larger (newer) of two snowflakes per [`snowflake_cmp`].
fn max_snowflake(a: String, b: &str) -> String {
    if snowflake_cmp(b, &a) == Ordering::Greater {
        b.to_string()
    } else {
        a
    }
}

fn message_event(msg: &DiscordMessage) -> ConnectorEvent {
    let occurred_at = msg
        .edited_timestamp
        .or(msg.timestamp)
        .unwrap_or_else(Utc::now);
    ConnectorEvent::DocumentCreated {
        document_id: SourceDocumentId::new(msg.id.clone()),
        occurred_at,
    }
}

/// A Gateway dispatch envelope (`{ "t": "MESSAGE_CREATE", "d": {…} }`).
/// `t` is optional so an Interactions `PING` (which has no dispatch
/// type) deserialises cleanly and is treated as a no-op.
#[derive(Debug, Clone, Default, Deserialize)]
struct GatewayDispatch {
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    d: serde_json::Value,
}

/// Discord webhook bodies are a single dispatch object; accept an
/// array too so a batched delivery is fully drained.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum DiscordWebhookBody {
    Batch(Vec<GatewayDispatch>),
    Single(GatewayDispatch),
}

fn dispatch_to_event(dispatch: &GatewayDispatch) -> Option<ConnectorEvent> {
    let t = dispatch.t.as_deref()?;
    let id = dispatch.d.get("id").and_then(serde_json::Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    let occurred_at = Utc::now();
    let document_id = SourceDocumentId::new(id.to_string());
    match t {
        "MESSAGE_CREATE" => Some(ConnectorEvent::DocumentCreated {
            document_id,
            occurred_at,
        }),
        "MESSAGE_UPDATE" => Some(ConnectorEvent::DocumentUpdated {
            document_id,
            occurred_at,
        }),
        "MESSAGE_DELETE" => Some(ConnectorEvent::DocumentDeleted {
            document_id,
            occurred_at,
        }),
        _ => None,
    }
}

impl Connector for DiscordConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        // User OAuth2 flow takes precedence when an authorization code
        // is supplied; otherwise fall back to a long-lived bot token.
        if let Some(code) = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
        {
            return self.oauth.exchange_code(config, code);
        }
        let bot_token = config
            .auth_config_json
            .get("bot_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "discord authenticate: auth_config_json.bot_token or authorization_code is required"
                        .into(),
                )
            })?;
        // Bot tokens are long-lived; set a far-future expiry so the
        // vault never schedules a (pointless) refresh.
        let mut token =
            OAuth2Token::new_without_refresh(bot_token, Utc::now() + Duration::days(3650), "bot");
        token.token_type = "Bot".to_string();
        Ok(token)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base = self.resolved_base_url(config);
        let channel = percent_encode_path_component(&Self::resolved_channel(config)?);
        let limit = self.page_size;
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut before: Option<String> = None;
        let mut max_id: Option<String> = None;
        for _ in 0..MAX_HISTORY_PAGES {
            let url = match &before {
                Some(b) => format!(
                    "{base}/channels/{channel}/messages?limit={limit}&before={}",
                    percent_encode_path_component(b)
                ),
                None => format!("{base}/channels/{channel}/messages?limit={limit}"),
            };
            let page: Vec<DiscordMessage> =
                self.get_json("/channels/{id}/messages", &url, token)?;
            let got = page.len();
            for msg in &page {
                events.push(message_event(msg));
                max_id = Some(match max_id.take() {
                    Some(cur) => max_snowflake(cur, &msg.id),
                    None => msg.id.clone(),
                });
            }
            // Discord returns newest-first; the last entry is the
            // oldest on this page — page backwards from it.
            match page.last() {
                Some(last) if got == limit as usize => before = Some(last.id.clone()),
                _ => {
                    return Ok(SyncRunResult {
                        events,
                        next_cursor: max_id,
                    })
                }
            }
        }
        Err(ConnectorError::Sync(format!(
            "discord channel history exceeded {MAX_HISTORY_PAGES} pages"
        )))
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base = self.resolved_base_url(config);
        let channel = percent_encode_path_component(&Self::resolved_channel(config)?);
        let limit = self.page_size;
        let start = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "discord incremental_sync: missing cursor; initial_sync must seed \
                 the latest message snowflake first"
                    .into(),
            )
        })?;
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut after = start.to_string();
        let mut max_id = start.to_string();
        for _ in 0..MAX_HISTORY_PAGES {
            let url = format!(
                "{base}/channels/{channel}/messages?limit={limit}&after={}",
                percent_encode_path_component(&after)
            );
            let mut page: Vec<DiscordMessage> =
                self.get_json("/channels/{id}/messages", &url, token)?;
            if page.is_empty() {
                break;
            }
            // With `after`, Discord still sorts newest-first; sort
            // ascending (numerically, by snowflake) so we page forward
            // deterministically and `page.last()` is the newest id.
            page.sort_by(|a, b| snowflake_cmp(&a.id, &b.id));
            let got = page.len();
            for msg in &page {
                events.push(message_event(msg));
                max_id = max_snowflake(max_id, &msg.id);
            }
            if got < limit as usize {
                break;
            }
            // Advance past the newest id we just saw.
            after = page.last().map_or_else(|| max_id.clone(), |m| m.id.clone());
        }
        Ok(SyncRunResult {
            events,
            next_cursor: Some(max_id),
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let base = self.resolved_base_url(config);
        let channel = percent_encode_path_component(&Self::resolved_channel(config)?);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base}/channels/{channel}/messages/{id_enc}");
        let msg: DiscordMessage = self.get_json("/channels/{id}/messages/{id}", &url, token)?;
        let fc = FetchedContent::text(msg.content, "text/plain")
            .with_title(format!("Discord message {id}"))
            .with_metadata(serde_json::json!({
                "provider": "discord",
                "message_id": id,
            }));
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Discord does not expose a REST "subscribe to channel events"
        // call — events arrive over the Gateway, and interaction
        // callbacks are delivered to an Interactions Endpoint URL
        // registered out-of-band in the Developer Portal. We therefore
        // make no HTTP call and surface the application's Ed25519
        // public key so the substrate can verify interaction
        // signatures on inbound deliveries.
        let _ = (token, &self.transport);
        let public_key = config
            .auth_config_json
            .get("public_key")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("discord-ed25519-public-key");
        let subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(public_key),
            WebhookEventTypes::all(),
            // Gateway / interaction registrations do not expire.
            None,
        );
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let parsed: DiscordWebhookBody = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!("discord webhook: malformed dispatch body: {e}"))
        })?;
        let dispatches = match parsed {
            DiscordWebhookBody::Batch(v) => v,
            DiscordWebhookBody::Single(d) => vec![d],
        };
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(dispatches.len());
        for d in &dispatches {
            if let Some(e) = dispatch_to_event(d) {
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
                "discord-user-access",
                "discord-refresh",
                Utc::now() + Duration::hours(1),
                "messages.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Discord, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "bot_token": "bot-xyz",
                "api_base_url": "https://api.test/discord",
                "channel_id": "C1",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_uses_bot_token_with_bot_scheme() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "bot-xyz");
        assert_eq!(tok.token_type, "Bot");
    }

    #[test]
    fn authenticate_prefers_oauth_when_code_present() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_oauth =
            ConnectorConfig::new(ConnectorKind::Discord, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "the-code",
                    "channel_id": "C1",
                }));
        let tok = c.authenticate(&cfg_oauth).unwrap();
        assert_eq!(tok.access_token.expose(), "discord-user-access");
        assert_eq!(tok.token_type, "Bearer");
    }

    #[test]
    fn authenticate_requires_a_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_none =
            ConnectorConfig::new(ConnectorKind::Discord, AuthKind::ApiKey, ScopeId::new_v4());
        let err = c.authenticate(&cfg_none).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_and_tracks_max_snowflake() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        // One short page (3 < 100) ends the walk immediately.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/discord/channels/C1/messages?limit=100",
            ok_json(&serde_json::json!([
                { "id": "30", "content": "c", "timestamp": now },
                { "id": "20", "content": "b", "timestamp": now },
                { "id": "10", "content": "a", "timestamp": now },
            ])),
        );
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(res.next_cursor.as_deref(), Some("30"));
    }

    #[test]
    fn initial_sync_pages_backwards_via_before() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let mut first = Vec::new();
        // 100 messages, ids 200 down to 101 (newest-first).
        for i in (101..=200).rev() {
            first
                .push(serde_json::json!({ "id": i.to_string(), "content": "x", "timestamp": now }));
        }
        transport.expect(
            HttpMethod::Get,
            "https://api.test/discord/channels/C1/messages?limit=100",
            ok_json(&serde_json::json!(first)),
        );
        // Next page: before=101, returns a short page (2 messages).
        transport.expect(
            HttpMethod::Get,
            "https://api.test/discord/channels/C1/messages?limit=100&before=101",
            ok_json(&serde_json::json!([
                { "id": "100", "content": "y", "timestamp": now },
                { "id": "99", "content": "z", "timestamp": now },
            ])),
        );
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 102);
        assert_eq!(res.next_cursor.as_deref(), Some("200"));
    }

    #[test]
    fn initial_sync_requires_channel() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_chan =
            ConnectorConfig::new(ConnectorKind::Discord, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "bot_token": "bot-xyz" }));
        let tok = c.authenticate(&cfg_no_chan).unwrap();
        let err = c.initial_sync(&cfg_no_chan, &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn incremental_sync_polls_after_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/discord/channels/C1/messages?limit=100&after=30",
            ok_json(&serde_json::json!([
                { "id": "40", "content": "new", "timestamp": now },
            ])),
        );
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("30".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.next_cursor.as_deref(), Some("40"));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            "https://api.test/discord/channels/C1/messages?limit=100",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn fetch_content_returns_message_text() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/discord/channels/C1/messages/40",
            ok_json(&serde_json::json!({ "id": "40", "content": "hello there" })),
        );
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("40"))
            .unwrap();
        assert_eq!(fc.body, b"hello there");
        assert_eq!(fc.mime_type, "text/plain");
    }

    #[test]
    fn subscribe_webhook_makes_no_call_and_carries_public_key() {
        // No transport expectations registered → a stray HTTP call
        // would panic the mock.
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let cfg_key =
            ConnectorConfig::new(ConnectorKind::Discord, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "bot_token": "bot-xyz",
                    "channel_id": "C1",
                    "public_key": "ed25519-pub",
                }));
        let sub = c
            .subscribe_webhook(&cfg_key, &tok, "https://hook.example/discord")
            .unwrap();
        assert_eq!(sub.secret.expose(), "ed25519-pub");
        assert_eq!(sub.connector, c.instance);
    }

    #[test]
    fn webhook_single_dispatch_emits_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "t": "MESSAGE_CREATE",
            "d": { "id": "55", "content": "hi" }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "55");
    }

    #[test]
    fn webhook_batch_emits_every_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!([
            { "t": "MESSAGE_CREATE", "d": { "id": "1" } },
            { "t": "MESSAGE_UPDATE", "d": { "id": "2" } },
            { "t": "MESSAGE_DELETE", "d": { "id": "3" } },
        ]);
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_interaction_ping_is_noop() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        // Interaction PING: type 1, no dispatch `t`.
        let evs = c.handle_webhook_event(br#"{"type":1}"#).unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_malformed_body_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DiscordConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn max_snowflake_picks_numerically_larger() {
        assert_eq!(max_snowflake("100".into(), "99"), "100");
        assert_eq!(max_snowflake("99".into(), "100"), "100");
        assert_eq!(max_snowflake("100".into(), "100"), "100");
    }

    #[test]
    fn snowflake_cmp_orders_numerically_not_lexically() {
        // "9" < "10" numerically, even though "9" > "10" lexically.
        assert_eq!(snowflake_cmp("9", "10"), Ordering::Less);
        assert_eq!(snowflake_cmp("10", "9"), Ordering::Greater);
        assert_eq!(snowflake_cmp("100", "100"), Ordering::Equal);

        // A page sorted by `snowflake_cmp` puts the newest id last, so
        // the `after` cursor advances correctly across digit-lengths.
        let mut ids = ["10", "9", "100", "11"];
        ids.sort_by(|a, b| snowflake_cmp(a, b));
        assert_eq!(ids, ["9", "10", "11", "100"]);
    }
}
