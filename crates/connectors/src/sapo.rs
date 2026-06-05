//! Sapo connector — Sapo Open API (orders + products + customers).
//!
//! Sapo is a Vietnamese e-commerce / POS platform. The Open API
//! authenticates with a store API access token and is scoped to a
//! single store subdomain.
//!
//! This connector ingests **orders** as the primary document stream.
//!
//! * `initial_sync` walks `GET /admin/orders.json`, paging via the
//!   1-based `page` cursor until a short page is returned.
//! * `incremental_sync` adds an `updated_at_min` filter built from the
//!   stored RFC 3339 watermark.
//! * `fetch_content` reads `GET /admin/orders/{id}.json` and renders a
//!   Markdown summary.
//! * `subscribe_webhook` registers the callback through
//!   `POST /admin/webhooks.json` (Sapo exposes a create-webhook
//!   endpoint) and stores the returned subscription id.
//! * `handle_webhook_event` parses an order webhook keyed by order id.
//!
//! Sapo authenticates with an `X-Sapo-Access-Token` header, so
//! requests go through the injected [`HttpTransport`] directly.

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

/// Default Sapo API base URL (override with the store subdomain).
pub const DEFAULT_API_BASE_URL: &str = "https://store.mysapo.net";

/// Default order list page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "orders.read";

/// One Sapo order (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SapoOrder {
    /// Order id.
    #[serde(default)]
    pub id: i64,
    /// Human-facing order name (`#1001`).
    #[serde(default)]
    pub name: Option<String>,
    /// Financial status.
    #[serde(default, rename = "financial_status")]
    pub financial_status: Option<String>,
    /// Order total (string on the wire).
    #[serde(default, rename = "total_price")]
    pub total_price: Option<String>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "updated_at")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Envelope for the order list endpoint (`{ "orders": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SapoOrdersResponse {
    /// Orders on this page.
    #[serde(default)]
    pub orders: Vec<SapoOrder>,
}

/// Envelope for the single-order endpoint (`{ "order": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SapoOrderResponse {
    /// The order.
    #[serde(default)]
    pub order: SapoOrder,
}

/// Envelope for the create-webhook endpoint (`{ "webhook": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SapoWebhookResponse {
    /// The created webhook.
    #[serde(default)]
    pub webhook: SapoWebhook,
}

/// A Sapo webhook record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SapoWebhook {
    /// Provider subscription id.
    #[serde(default)]
    pub id: Option<i64>,
}

/// Order webhook payload (`{ "id": .., "name": ".." }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SapoWebhookPayload {
    /// Affected order id.
    #[serde(default)]
    pub id: Option<i64>,
}

/// Sapo connector.
pub struct SapoConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for SapoConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SapoConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl SapoConnector {
    /// Construct a Sapo connector.
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

    /// Override the store API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 250]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 250);
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

    fn api_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("X-Sapo-Access-Token", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("sapo", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "sapo {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every order page until a short page is returned.
    fn paginate_orders(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_from: Option<&str>,
    ) -> Result<Vec<SapoOrder>> {
        let mut out = Vec::<SapoOrder>::new();
        let filter = updated_from.map_or_else(String::new, |ts| {
            format!("&updated_at_min={}", percent_encode_path_component(ts))
        });
        for page in 1..=MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/admin/orders.json?page={page}&limit={}{filter}",
                self.page_size
            );
            let resp: SapoOrdersResponse = self.api_get("/admin/orders.json", &url, token)?;
            let returned = resp.orders.len();
            out.extend(resp.orders);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "sapo /admin/orders.json exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn order_to_event(o: &SapoOrder, created: bool) -> ConnectorEvent {
    let occurred_at = o.updated_at.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(o.id.to_string());
    if created {
        ConnectorEvent::DocumentCreated {
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

impl Connector for SapoConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("api_key")
            .or_else(|| config.auth_config_json.get("access_token"))
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                key,
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
                    "sapo authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let orders = self.paginate_orders(&base_url, token, None)?;
        let mut events = Vec::with_capacity(orders.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for o in &orders {
            events.push(order_to_event(o, true));
            if let Some(ts) = o.updated_at {
                watermark = Some(watermark.map_or(ts, |w| w.max(ts)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|ts| ts.to_rfc3339()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let prior: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let orders = self.paginate_orders(&base_url, token, state.cursor.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for o in &orders {
            if let (Some(prev), Some(ts)) = (prior, o.updated_at) {
                if ts <= prev {
                    continue;
                }
            }
            events.push(order_to_event(o, false));
            if let Some(ts) = o.updated_at {
                watermark = Some(watermark.map_or(ts, |w| w.max(ts)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|ts| ts.to_rfc3339()),
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
        let url = format!("{base_url}/admin/orders/{id_enc}.json");
        let resp: SapoOrderResponse = self.api_get("/admin/orders/{id}.json", &url, token)?;
        let order = resp.order;

        let name = order
            .name
            .clone()
            .unwrap_or_else(|| format!("#{}", order.id));
        let title = format!("Order {name}");
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(status) = order.financial_status.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Financial status:** {status}\n");
        }
        if let Some(total) = order.total_price.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Total:** {total} VND\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "sapo",
                "order_id": order.id,
                "name": order.name,
                "financial_status": order.financial_status,
                "total_price": order.total_price,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/admin/webhooks.json");
        let body = serde_json::to_vec(&serde_json::json!({
            "webhook": {
                "topic": "orders/updated",
                "address": callback_url,
                "format": "json",
            }
        }))
        .map_err(|e| ConnectorError::Webhook(format!("sapo webhook body serialise: {e}")))?;
        let req = HttpRequest::post(url, body)
            .with_header("Accept", "application/json")
            .with_header("Content-Type", "application/json")
            .with_header("X-Sapo-Access-Token", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("sapo", "/admin/webhooks.json", &resp));
        }
        let parsed: SapoWebhookResponse = serde_json::from_slice(&resp.body).unwrap_or_default();
        // Sapo signs deliveries with the app's shared secret, supplied
        // out of band; fall back to the access token when unset.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| token.access_token.expose());
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = parsed.webhook.id.map(|id| id.to_string());
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: SapoWebhookPayload = serde_json::from_slice(body)?;
        let id = payload
            .id
            .ok_or_else(|| ConnectorError::Webhook("sapo webhook payload missing id".into()))?;
        Ok(vec![ConnectorEvent::DocumentUpdated {
            document_id: SourceDocumentId::new(id.to_string()),
            occurred_at: Utc::now(),
        }])
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
                "x",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Sapo, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "sapo_tok_123",
                "api_base_url": "https://api.test/sapo",
                "webhook_secret": "sapo-secret",
            }))
    }

    fn order(id: i64, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": format!("#{id}"),
            "financial_status": "paid",
            "total_price": "500000",
            "updated_at": updated,
        })
    }

    fn list_resp(orders: &[serde_json::Value]) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(&serde_json::json!({ "orders": orders })).unwrap())
    }

    #[test]
    fn authenticate_wraps_api_key() {
        let c = SapoConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "sapo_tok_123"
        );
    }

    #[test]
    fn initial_sync_emits_created_with_token_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/sapo/admin/orders.json?page=1&limit=50",
            list_resp(&[order(1001, "2024-01-01T00:00:00Z")]),
        );
        let c = SapoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "1001");
        assert!(transport.recorded()[0]
            .headers
            .iter()
            .any(|(k, v)| k == "X-Sapo-Access-Token" && v == "sapo_tok_123"));
    }

    #[test]
    fn incremental_sync_filters_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/sapo/admin/orders.json?page=1&limit=50&updated_at_min=2024-01-01T00%3A00%3A00%2B00%3A00",
            list_resp(&[
                order(1001, "2024-01-01T00:00:00Z"),
                order(1002, "2024-09-01T00:00:00Z"),
            ]),
        );
        let c = SapoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "1002");
    }

    #[test]
    fn initial_sync_maps_403_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/sapo/admin/orders.json?page=1&limit=50",
            MockResponse::status(403, b"bad".to_vec()),
        );
        let c = SapoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/sapo/admin/orders/1001.json",
            MockResponse::ok_json(
                serde_json::to_vec(
                    &serde_json::json!({ "order": order(1001, "2024-01-01T00:00:00Z") }),
                )
                .unwrap(),
            ),
        );
        let c = SapoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("1001"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Order #1001"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Financial status:** paid"));
    }

    #[test]
    fn subscribe_webhook_posts_and_stores_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/sapo/admin/webhooks.json",
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({ "webhook": { "id": 555 } })).unwrap(),
            ),
        );
        let c = SapoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/sapo")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("555"));
        assert_eq!(sub.secret.expose(), "sapo-secret");
    }

    #[test]
    fn handle_webhook_event_maps_to_updated() {
        let c = SapoConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "id": 1009, "name": "#1009" });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].document_id().as_str(), "1009");
    }

    #[test]
    fn handle_webhook_event_missing_id_errors() {
        let c = SapoConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "name": "#1009" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
