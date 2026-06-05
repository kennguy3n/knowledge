//! Shopify connector — Shopify Admin REST API + webhooks.
//!
//! * `initial_sync` walks `GET /admin/api/2024-01/orders.json` and
//!   pages via Shopify's `since_id` cursor (ascending order id).
//! * `incremental_sync` adds `updated_at_min=<iso>` keyed off the
//!   prior watermark.
//! * `fetch_content` reads `GET /admin/api/2024-01/orders/{id}.json`
//!   and renders a Markdown summary.
//! * `subscribe_webhook` POSTs `/admin/api/2024-01/webhooks.json`.
//! * `handle_webhook_event` parses an order payload; Shopify carries
//!   the topic in the `X-Shopify-Topic` header (not the body), so an
//!   optional `topic` field is honoured when a gateway injects it and
//!   the default is `DocumentUpdated`.
//!
//! Shopify authenticates Admin API calls with an access token in the
//! `X-Shopify-Access-Token` header (not a bearer `Authorization`), so
//! the connector issues requests through the injected
//! [`HttpTransport`] directly rather than the bearer helpers.
//! `authenticate` accepts a configured `access_token` (the common
//! case) or an OAuth2 `authorization_code` exchanged through the
//! injected [`OAuth2CodeExchange`].

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

/// Default Shopify Admin REST base URL template host. Production
/// callers override this with `https://{shop}.myshopify.com`.
pub const DEFAULT_API_BASE_URL: &str = "https://shop.myshopify.com";

/// Admin API version path segment.
pub const API_VERSION: &str = "2024-01";

/// Page size for list endpoints. Shopify's documented max is 250.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured access token.
const DEFAULT_SCOPE: &str = "read_orders";

/// One Shopify order (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopifyOrder {
    /// Numeric order id.
    pub id: i64,
    /// Order name (e.g. `#1001`).
    #[serde(default)]
    pub name: Option<String>,
    /// Customer-facing email.
    #[serde(default)]
    pub email: Option<String>,
    /// Total price as a decimal string.
    #[serde(default)]
    pub total_price: Option<String>,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Envelope for the orders list endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopifyOrdersResponse {
    /// Orders on this page.
    #[serde(default)]
    pub orders: Vec<ShopifyOrder>,
}

/// Envelope for the single-order endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopifyOrderResponse {
    /// The order.
    #[serde(default)]
    pub order: ShopifyOrder,
}

/// Envelope for the webhook-create endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopifyWebhookResponse {
    /// The created webhook.
    #[serde(default)]
    pub webhook: ShopifyWebhook,
}

/// One Shopify webhook registration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopifyWebhook {
    /// Numeric webhook id.
    #[serde(default)]
    pub id: i64,
}

/// Webhook order payload. Shopify posts the order object directly;
/// `topic` is an optional convenience field (the real topic arrives
/// in the `X-Shopify-Topic` header).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopifyWebhookPayload {
    /// Order id.
    #[serde(default)]
    pub id: i64,
    /// Optional topic hint (`orders/create`, `orders/updated`,
    /// `orders/delete`).
    #[serde(default)]
    pub topic: Option<String>,
    /// Update timestamp, when present.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Shopify connector.
pub struct ShopifyConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ShopifyConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShopifyConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ShopifyConnector {
    /// Construct a Shopify connector.
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

    /// Override the shop base URL (`https://{shop}.myshopify.com`).
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

    /// GET a JSON endpoint with Shopify's `X-Shopify-Access-Token`
    /// header and parse the response.
    fn shopify_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("X-Shopify-Access-Token", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("shopify", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "shopify {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// POST a JSON body with the access-token header and parse the
    /// response.
    fn shopify_post<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
        body: &serde_json::Value,
    ) -> Result<R> {
        let body_bytes = serde_json::to_vec(body).map_err(|e| {
            ConnectorError::Sync(format!("shopify {endpoint} serialise body failed: {e}"))
        })?;
        let req = HttpRequest::post(url, body_bytes)
            .with_header("Accept", "application/json")
            .with_header("Content-Type", "application/json")
            .with_header("X-Shopify-Access-Token", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("shopify", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "shopify {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every orders page until a short page is returned, paging
    /// by ascending `since_id`.
    fn paginate_orders(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_at_min: Option<&str>,
    ) -> Result<Vec<ShopifyOrder>> {
        let mut out = Vec::<ShopifyOrder>::new();
        let mut since_id: i64 = 0;
        for _ in 0..MAX_LIST_PAGES {
            let mut url = format!(
                "{base_url}/admin/api/{API_VERSION}/orders.json?status=any&limit={}&since_id={since_id}",
                self.page_size
            );
            if let Some(min) = updated_at_min {
                let _ = write!(
                    url,
                    "&updated_at_min={}",
                    percent_encode_path_component(min)
                );
            }
            let resp: ShopifyOrdersResponse =
                self.shopify_get("/admin/api/orders.json", &url, token)?;
            let returned = resp.orders.len();
            let max_id = resp.orders.iter().map(|o| o.id).max();
            out.extend(resp.orders);
            if returned < self.page_size as usize {
                return Ok(out);
            }
            match max_id {
                // `since_id` is strictly exclusive, so advancing past
                // the largest id we saw guarantees forward progress.
                Some(id) if id > since_id => since_id = id,
                _ => return Ok(out),
            }
        }
        Err(ConnectorError::Sync(format!(
            "shopify orders.json exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn order_to_event(o: &ShopifyOrder, kind: &str) -> ConnectorEvent {
    let occurred_at = o.updated_at.or(o.created_at).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(o.id.to_string());
    match kind {
        "delete" => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
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

impl Connector for ShopifyConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("access_token")
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
                    "shopify authenticate: auth_config_json.access_token or .authorization_code is required"
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
            events.push(order_to_event(o, "create"));
            if let Some(t) = o.updated_at.or(o.created_at) {
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
        let orders = self.paginate_orders(&base_url, token, state.cursor.as_deref())?;
        let mut events = Vec::with_capacity(orders.len());
        let mut watermark = prior;
        for o in &orders {
            let when = o.updated_at.or(o.created_at);
            // `updated_at_min` is inclusive; skip the boundary order
            // that was already emitted on the prior run.
            if let (Some(prev), Some(t)) = (prior, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(order_to_event(o, "update"));
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
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base_url}/admin/api/{API_VERSION}/orders/{id_enc}.json");
        let resp: ShopifyOrderResponse =
            self.shopify_get("/admin/api/orders/{id}.json", &url, token)?;
        let order = resp.order;

        let title = order.name.clone().unwrap_or_else(|| format!("Order {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(email) = order.email.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Email:** ");
            md.push_str(email);
            md.push_str("\n\n");
        }
        if let Some(total) = order.total_price.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Total:** ");
            md.push_str(total);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "shopify",
                "order_id": id,
                "email": order.email,
            }))
            .with_source_url(format!("{base_url}/admin/orders/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/admin/api/{API_VERSION}/webhooks.json");
        let body = serde_json::json!({
            "webhook": {
                "topic": "orders/updated",
                "address": callback_url,
                "format": "json",
            }
        });
        let resp: ShopifyWebhookResponse =
            self.shopify_post("/admin/api/webhooks.json", &url, token, &body)?;
        if resp.webhook.id == 0 {
            return Err(ConnectorError::Webhook(
                "shopify /admin/api/webhooks.json returned no webhook id".into(),
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
                    .unwrap_or("shopify-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.webhook.id.to_string());
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: ShopifyWebhookPayload = serde_json::from_slice(body)?;
        if payload.id == 0 {
            return Err(ConnectorError::Webhook(
                "shopify webhook payload missing order id".into(),
            ));
        }
        let occurred_at = payload.updated_at.unwrap_or_else(Utc::now);
        let id = SourceDocumentId::new(payload.id.to_string());
        // The topic lives in the `X-Shopify-Topic` header; honour an
        // injected `topic` field when present, else treat any order
        // payload as an update.
        let event = match payload.topic.as_deref() {
            Some("orders/create") => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at,
            },
            Some("orders/delete") => ConnectorEvent::DocumentDeleted {
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
                "shopify-access",
                "shopify-refresh",
                Utc::now() + Duration::hours(1),
                "read_orders",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Shopify, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "shpat_123",
                "api_base_url": "https://api.test/shopify",
            }))
    }

    fn order(id: i64, updated: &str) -> serde_json::Value {
        serde_json::json!({ "id": id, "name": format!("#{id}"), "updated_at": updated })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "shpat_123");
    }

    #[test]
    fn authenticate_falls_back_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Shopify, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "authorization_code": "abc" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert_eq!(tok.access_token.expose(), "shopify-access");
    }

    #[test]
    fn authenticate_requires_token_or_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::Shopify, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn initial_sync_emits_created_with_token_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/shopify/admin/api/2024-01/orders.json?status=any&limit=50&since_id=0",
            ok_json(&serde_json::json!({ "orders": [order(1001, "2024-01-01T00:00:00Z")] })),
        );
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        let rec = transport.recorded();
        assert!(rec[0]
            .headers
            .iter()
            .any(|(k, v)| k == "X-Shopify-Access-Token" && v == "shpat_123"));
    }

    #[test]
    fn initial_sync_paginates_via_since_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let full: Vec<serde_json::Value> =
            (1..=50).map(|i| order(i, "2024-01-01T00:00:00Z")).collect();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/shopify/admin/api/2024-01/orders.json?status=any&limit=50&since_id=0",
            ok_json(&serde_json::json!({ "orders": full })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/shopify/admin/api/2024-01/orders.json?status=any&limit=50&since_id=50",
            ok_json(&serde_json::json!({ "orders": [order(51, "2024-01-02T00:00:00Z")] })),
        );
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 51);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_updated_at_min_and_dedupes_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let prior = "2024-01-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/shopify/admin/api/2024-01/orders.json?status=any&limit=50&since_id=0&updated_at_min={}", percent_encode_path_component(prior)),
            ok_json(&serde_json::json!({ "orders": [
                order(1, "2024-01-01T00:00:00Z"),
                order(2, "2024-02-01T00:00:00Z"),
            ] })),
        );
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(prior.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "2");
    }

    #[test]
    fn initial_sync_maps_403_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/shopify/admin/api/2024-01/orders.json?status=any&limit=50&since_id=0",
            MockResponse::status(403, b"forbidden".to_vec()),
        );
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn subscribe_webhook_registers_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/shopify/admin/api/2024-01/webhooks.json",
            ok_json(&serde_json::json!({ "webhook": { "id": 777 } })),
        );
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/shopify")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("777"));
    }

    #[test]
    fn webhook_defaults_to_update_without_topic() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "id": 42, "updated_at": "2024-03-01T00:00:00Z" });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "42");
    }

    #[test]
    fn webhook_honours_injected_topic() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let created = serde_json::json!({ "id": 7, "topic": "orders/create" });
        let deleted = serde_json::json!({ "id": 8, "topic": "orders/delete" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&created).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&deleted).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn webhook_missing_id_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "topic": "orders/create" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }

    #[test]
    fn fetch_content_assembles_order_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/shopify/admin/api/2024-01/orders/1001.json",
            ok_json(&serde_json::json!({ "order": {
                "id": 1001, "name": "#1001", "email": "buyer@test", "total_price": "42.00"
            }})),
        );
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("1001"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# #1001"));
        assert!(body.contains("buyer@test"));
        assert!(body.contains("42.00"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/shopify/admin/api/2024-01/orders/999.json",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = ShopifyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("999"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
