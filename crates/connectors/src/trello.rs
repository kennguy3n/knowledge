//! Trello connector — Trello REST API + webhooks.
//!
//! * `initial_sync` walks `GET /1/boards/{id}/cards` and pages via
//!   Trello's `before=<card id>` cursor (cards are returned in
//!   descending id order, so we page backwards in time).
//! * `incremental_sync` walks the same endpoint and filters
//!   client-side on `dateLastActivity` against the prior watermark
//!   (Trello's card list has no server-side "since" filter).
//! * `fetch_content` reads a single card and renders its description.
//! * `subscribe_webhook` POSTs `/1/webhooks`.
//! * `handle_webhook_event` parses an action payload
//!   (`{action:{type,data:{card:{id}}}}`).
//!
//! Trello authenticates with an API key plus a user token passed as
//! `key` / `token` query parameters (not a bearer header), so the
//! connector issues requests through the injected [`HttpTransport`]
//! directly. `authenticate` wraps the configured `token`; the `key`
//! is read from config on each request.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default Trello REST base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.trello.com";

/// Page size for the cards endpoint. Trello's documented max is 1000.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on the synthesised token.
const DEFAULT_SCOPE: &str = "read";

/// One Trello card (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrelloCard {
    /// Card id (24-hex string).
    pub id: String,
    /// Card name.
    #[serde(default)]
    pub name: Option<String>,
    /// Card description (Markdown).
    #[serde(default)]
    pub desc: Option<String>,
    /// Canonical short URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Timestamp of the last activity on the card.
    #[serde(rename = "dateLastActivity", default)]
    pub date_last_activity: Option<DateTime<Utc>>,
}

/// Response from `POST /1/webhooks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrelloWebhookResponse {
    /// Webhook id.
    #[serde(default)]
    pub id: String,
}

/// Webhook action payload (`{action:{…}}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrelloWebhookPayload {
    /// The action that triggered the webhook.
    #[serde(default)]
    pub action: Option<TrelloAction>,
}

/// One Trello action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrelloAction {
    /// Action type (`createCard`, `updateCard`, `deleteCard`, …).
    #[serde(rename = "type", default)]
    pub action_type: Option<String>,
    /// Action timestamp.
    #[serde(default)]
    pub date: Option<DateTime<Utc>>,
    /// Action data block.
    #[serde(default)]
    pub data: Option<TrelloActionData>,
}

/// `data` block of a Trello action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrelloActionData {
    /// The card the action concerns.
    #[serde(default)]
    pub card: Option<TrelloCardRef>,
}

/// Minimal card reference inside an action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrelloCardRef {
    /// Card id.
    #[serde(default)]
    pub id: String,
}

/// Trello connector.
pub struct TrelloConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for TrelloConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrelloConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl TrelloConnector {
    /// Construct a Trello connector.
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

    /// Override the Trello REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the cards page size. Clamped to `[1, 1000]`.
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

    fn api_key(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("key")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| ConnectorError::Auth("trello: auth_config_json.key is required".into()))
    }

    fn board_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("board_id")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("trello: auth_config_json.board_id is required".into())
            })
    }

    /// Build the `key=…&token=…` credential suffix appended to every
    /// request URL.
    fn auth_query(key: &str, token: &OAuth2Token) -> String {
        format!(
            "key={}&token={}",
            percent_encode_path_component(key),
            percent_encode_path_component(token.access_token.expose())
        )
    }

    /// GET a JSON endpoint and parse the response.
    fn trello_get<R: DeserializeOwned>(&self, endpoint: &str, url: &str) -> Result<R> {
        let req = HttpRequest::get(url).with_header("Accept", "application/json");
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("trello", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body)
            .map_err(|e| ConnectorError::Sync(format!("trello {endpoint} JSON parse failed: {e}")))
    }

    /// Walk every cards page, paging by `before=<oldest card id>`.
    fn paginate_cards(
        &self,
        base_url: &str,
        board_enc: &str,
        auth: &str,
    ) -> Result<Vec<TrelloCard>> {
        let mut out = Vec::<TrelloCard>::new();
        let mut before: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut url = format!(
                "{base_url}/1/boards/{board_enc}/cards?{auth}&fields=name,desc,url,dateLastActivity&limit={}",
                self.page_size
            );
            if let Some(ref b) = before {
                let _ = write!(url, "&before={}", percent_encode_path_component(b));
            }
            let cards: Vec<TrelloCard> = self.trello_get("/1/boards/{id}/cards", &url)?;
            let returned = cards.len();
            // Trello returns cards newest-first; the last element is
            // the oldest on the page, so we page further back with it.
            let oldest = cards.last().map(|c| c.id.clone());
            out.extend(cards);
            if returned < self.page_size as usize {
                return Ok(out);
            }
            match oldest {
                Some(id) => before = Some(id),
                None => return Ok(out),
            }
        }
        Err(ConnectorError::Sync(format!(
            "trello cards exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn card_to_event(c: &TrelloCard, kind: &str) -> ConnectorEvent {
    let occurred_at = c.date_last_activity.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(c.id.clone());
    match kind {
        "create" => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        "delete" => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for TrelloConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(token) = config
            .auth_config_json
            .get("token")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                token,
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
                    "trello authenticate: auth_config_json.token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let key = Self::api_key(config)?;
        let board = Self::board_id(config)?;
        let board_enc = percent_encode_path_component(&board);
        let auth = Self::auth_query(&key, token);
        let cards = self.paginate_cards(&base_url, &board_enc, &auth)?;
        let mut events = Vec::with_capacity(cards.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for c in &cards {
            events.push(card_to_event(c, "create"));
            if let Some(t) = c.date_last_activity {
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
        let key = Self::api_key(config)?;
        let board = Self::board_id(config)?;
        let board_enc = percent_encode_path_component(&board);
        let auth = Self::auth_query(&key, token);
        let prior = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let cards = self.paginate_cards(&base_url, &board_enc, &auth)?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for c in &cards {
            // Trello has no server-side "modified since" filter, so we
            // fetch all cards and emit only those touched strictly
            // after the prior watermark.
            if let (Some(prev), Some(t)) = (prior, c.date_last_activity) {
                if t <= prev {
                    continue;
                }
            }
            events.push(card_to_event(c, "update"));
            if let Some(t) = c.date_last_activity {
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
        let key = Self::api_key(config)?;
        let auth = Self::auth_query(&key, token);
        let id_enc = percent_encode_path_component(document_id.as_str());
        let url =
            format!("{base_url}/1/cards/{id_enc}?{auth}&fields=name,desc,url,dateLastActivity");
        let card: TrelloCard = self.trello_get("/1/cards/{id}", &url)?;

        let title = card
            .name
            .clone()
            .unwrap_or_else(|| format!("Card {}", document_id.as_str()));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(desc) = card.desc.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(desc);
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "trello",
                "card_id": card.id,
            }))
            .with_source_url(
                card.url
                    .unwrap_or_else(|| format!("https://trello.com/c/{}", document_id.as_str())),
            ))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let key = Self::api_key(config)?;
        let board = Self::board_id(config)?;
        let auth = Self::auth_query(&key, token);
        let url = format!("{base_url}/1/webhooks?{auth}");
        let body = serde_json::json!({
            "callbackURL": callback_url,
            "idModel": board,
            "description": "knowledge-substrate",
        });
        let body_bytes = serde_json::to_vec(&body).map_err(|e| {
            ConnectorError::Webhook(format!("trello webhook serialise failed: {e}"))
        })?;
        let req = HttpRequest::post(&url, body_bytes)
            .with_header("Accept", "application/json")
            .with_header("Content-Type", "application/json");
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("trello", "/1/webhooks", &resp));
        }
        let parsed: TrelloWebhookResponse = serde_json::from_slice(&resp.body).map_err(|e| {
            ConnectorError::Webhook(format!("trello /1/webhooks JSON parse failed: {e}"))
        })?;
        if parsed.id.is_empty() {
            return Err(ConnectorError::Webhook(
                "trello /1/webhooks returned no id".into(),
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
                    .unwrap_or("trello-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(parsed.id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: TrelloWebhookPayload = serde_json::from_slice(body)?;
        let action = payload
            .action
            .ok_or_else(|| ConnectorError::Webhook("trello webhook missing action".into()))?;
        let card_id = action
            .data
            .and_then(|d| d.card)
            .map(|c| c.id)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ConnectorError::Webhook("trello webhook missing card id".into()))?;
        let occurred_at = action.date.unwrap_or_else(Utc::now);
        let id = SourceDocumentId::new(card_id);
        let event = match action.action_type.as_deref() {
            Some("createCard") => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at,
            },
            Some("deleteCard") => ConnectorEvent::DocumentDeleted {
                document_id: id,
                occurred_at,
            },
            _ => ConnectorEvent::DocumentUpdated {
                document_id: id,
                occurred_at,
            },
        };
        Ok(vec![event])
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
                "trello-access",
                "trello-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Trello, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "key": "K1",
                "token": "T1",
                "board_id": "board9",
                "api_base_url": "https://api.test/trello",
            }))
    }

    fn card(id: &str, activity: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": format!("Card {id}"), "desc": "",
            "url": format!("https://trello.com/c/{id}"), "dateLastActivity": activity
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(c.authenticate(&cfg()).unwrap().access_token.expose(), "T1");
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Trello, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "authorization_code": "abc" }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "trello-access"
        );
    }

    #[test]
    fn initial_sync_emits_cards_with_key_and_token() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/trello/1/boards/board9/cards?key=K1&token=T1&fields=name,desc,url,dateLastActivity&limit=100",
            ok_json(&serde_json::json!([card("c1", "2024-01-01T00:00:00Z")])),
        );
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn initial_sync_paginates_via_before() {
        let transport = Arc::new(MockHttpTransport::new());
        let full: Vec<serde_json::Value> = (0..100)
            .map(|i| card(&format!("c{i:03}"), "2024-01-01T00:00:00Z"))
            .collect();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/trello/1/boards/board9/cards?key=K1&token=T1&fields=name,desc,url,dateLastActivity&limit=100",
            ok_json(&serde_json::json!(full)),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/trello/1/boards/board9/cards?key=K1&token=T1&fields=name,desc,url,dateLastActivity&limit=100&before=c099",
            ok_json(&serde_json::json!([card("c100", "2024-01-02T00:00:00Z")])),
        );
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 101);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_client_side() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/trello/1/boards/board9/cards?key=K1&token=T1&fields=name,desc,url,dateLastActivity&limit=100",
            ok_json(&serde_json::json!([
                card("old", "2024-01-01T00:00:00Z"),
                card("new", "2024-02-01T00:00:00Z"),
            ])),
        );
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "new");
    }

    #[test]
    fn initial_sync_requires_board_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Trello, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "key": "K", "token": "T" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn subscribe_webhook_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/trello/1/webhooks?key=K1&token=T1",
            ok_json(&serde_json::json!({ "id": "wh99" })),
        );
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/trello")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh99"));
    }

    #[test]
    fn webhook_create_update_delete_map_correctly() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let mk = |t: &str| serde_json::json!({ "action": { "type": t, "data": { "card": { "id": "card1" } } } });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&mk("createCard")).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&mk("updateCard")).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&mk("deleteCard")).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn webhook_missing_card_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "action": { "type": "updateCard" } });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }

    #[test]
    fn fetch_content_renders_description() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/trello/1/cards/card7?key=K1&token=T1&fields=name,desc,url,dateLastActivity",
            ok_json(&serde_json::json!({
                "id": "card7", "name": "Do thing", "desc": "Details here"
            })),
        );
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("card7"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Do thing"));
        assert!(body.contains("Details here"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/trello/1/cards/cardX?key=K1&token=T1&fields=name,desc,url,dateLastActivity",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = TrelloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("cardX"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
