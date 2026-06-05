//! Grab connector — Grab for Business partner API
//! (`https://partner-api.grab.com`).
//!
//! Grab is the Singapore-headquartered ride-hailing / food-delivery
//! super-app. The partner API is OAuth2 (authorization-code), so
//! [`GrabConnector::authenticate`] delegates to the injected
//! [`OAuth2CodeExchange`].
//!
//! * `initial_sync` / `incremental_sync` page `/v1/orders`
//!   (`page_index` / `page_size`), tracking the maximum `updated_at`
//!   as an RFC-3339 watermark; incremental runs add `updated_since`
//!   and dedup the inclusive boundary row.
//! * `fetch_content` GETs a single order (`/v1/orders/{id}`) and
//!   renders Markdown from its status / merchant / total.
//! * `subscribe_webhook` registers a push subscription
//!   (`POST /v1/webhooks`) and records the returned id.
//! * `handle_webhook_event` parses the delivered payload (single or
//!   batched) into connector events.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Grab partner API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://partner-api.grab.com";
/// Page size for order listing (`page_size`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct GrabOrdersPage {
    #[serde(default)]
    orders: Vec<GrabOrder>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GrabOrder {
    #[serde(default)]
    order_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    merchant_name: Option<String>,
    #[serde(default)]
    total: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrabWebhookCreateResponse {
    #[serde(default)]
    webhook_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GrabWebhookEvent {
    #[serde(default)]
    order_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Grab for Business connector.
pub struct GrabConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GrabConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrabConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GrabConnector {
    /// Construct a Grab connector.
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

    /// Override the Grab API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size.
    #[must_use]
    pub fn with_page_size(mut self, page_size: u32) -> Self {
        self.page_size = page_size.max(1);
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

    fn paginate_orders(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_since: Option<&str>,
    ) -> Result<Vec<GrabOrder>> {
        let mut orders = Vec::<GrabOrder>::new();
        for page in 0..MAX_PAGES {
            let mut url = format!(
                "{base_url}/v1/orders?page_size={}&page_index={page}",
                self.page_size
            );
            if let Some(since) = updated_since {
                url.push_str("&updated_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: GrabOrdersPage =
                bearer_get_json(&self.transport, "grab", "/v1/orders", &url, token, &[])?;
            let count = resp.orders.len();
            orders.extend(resp.orders);
            if count < self.page_size as usize {
                return Ok(orders);
            }
        }
        Err(ConnectorError::Sync(format!(
            "grab /v1/orders exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn order_watermark(o: &GrabOrder) -> Option<DateTime<Utc>> {
    o.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| o.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for GrabConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "grab authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let orders = self.paginate_orders(&base_url, token, None)?;
        let mut events = Vec::with_capacity(orders.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for order in &orders {
            let occurred_at = order_watermark(order).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(order.order_id.clone()),
                occurred_at,
            });
            if let Some(t) = order_watermark(order) {
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
        let prior: Option<DateTime<Utc>> = state.cursor.as_deref().and_then(parse_rfc3339);
        let since = prior.map(|t| t.to_rfc3339());
        let orders = self.paginate_orders(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for order in &orders {
            let Some(updated) = order_watermark(order) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(order.order_id.clone()),
                occurred_at: updated,
            });
            watermark = Some(watermark.map_or(updated, |w| w.max(updated)));
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
        let url = format!("{base_url}/v1/orders/{id_enc}");
        let order: GrabOrder =
            bearer_get_json(&self.transport, "grab", "/v1/orders/{id}", &url, token, &[])?;
        let status = order.status.as_deref().unwrap_or("unknown");
        let merchant = order
            .merchant_name
            .as_deref()
            .unwrap_or("(unknown merchant)");
        let total = order.total.unwrap_or(0.0);
        let currency = order.currency.as_deref().unwrap_or("");
        let body = format!(
            "# Grab order {id}\n\nMerchant: {merchant}\nStatus: {status}\nTotal: {total} {currency}\n"
        );
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("Grab order {id}"))
            .with_metadata(serde_json::json!({
                "provider": "grab",
                "order_id": order.order_id,
                "status": order.status,
                "updated_at": order.updated_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/v1/webhooks");
        let body = serde_json::json!({
            "callback_url": callback_url,
            "event_types": ["order.created", "order.updated", "order.delivered"],
        });
        let resp: GrabWebhookCreateResponse = bearer_post_json(
            &self.transport,
            "grab",
            "/v1/webhooks",
            &url,
            token,
            &[],
            &body,
        )?;
        let provider_id = resp.webhook_id.ok_or_else(|| {
            ConnectorError::Webhook("grab /v1/webhooks returned no webhook_id".into())
        })?;
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("grab-webhook-secret");
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(provider_id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<GrabWebhookEvent> = if let Ok(batch) =
            serde_json::from_slice::<Vec<GrabWebhookEvent>>(body)
        {
            batch
        } else {
            vec![serde_json::from_slice::<GrabWebhookEvent>(body)
                .map_err(|e| ConnectorError::Webhook(format!("grab webhook parse failed: {e}")))?]
        };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty grab webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.order_id).ok_or_else(|| {
                ConnectorError::Webhook("grab webhook event missing order_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("delete") || delivery.event.contains("cancel") {
                ConnectorEvent::DocumentDeleted {
                    document_id: id,
                    occurred_at,
                }
            } else {
                ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                }
            };
            events.push(event);
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
                "grab-access",
                "grab-refresh",
                Utc::now() + Duration::hours(1),
                "orders",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Grab, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/grab",
                "webhook_secret": "grab-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn small(c: GrabConnector) -> GrabConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GrabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Grab, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates_until_short_page() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/grab/v1/orders?page_size=2&page_index=0",
            ok_json(&serde_json::json!({
                "orders": [
                    {"order_id": "o-1", "updated_at": "2024-01-01T00:00:00Z"},
                    {"order_id": "o-2", "updated_at": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/grab/v1/orders?page_size=2&page_index=1",
            ok_json(&serde_json::json!({
                "orders": [ {"order_id": "o-3", "updated_at": "2024-01-03T00:00:00Z"} ]
            })),
        );
        let c = small(GrabConnector::new(
            ConnectorInstanceId::new_v4(),
            transport.clone(),
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00")
        );
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/grab/v1/orders?page_size=2&page_index=0&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "orders": [
                    {"order_id": "o-10", "updated_at": "2024-03-01T00:00:00Z"},
                    {"order_id": "o-11", "updated_at": "2024-06-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/grab/v1/orders?page_size=2&page_index=1&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "orders": [] })),
        );
        let c = small(GrabConnector::new(
            ConnectorInstanceId::new_v4(),
            transport,
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(since.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00")
        );
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/grab/v1/orders/o-1",
            ok_json(&serde_json::json!({
                "order_id": "o-1",
                "status": "DELIVERED",
                "merchant_name": "Nasi Lemak House",
                "total": 18.5,
                "currency": "SGD"
            })),
        );
        let c = GrabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("o-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Grab order o-1"));
        assert!(body.contains("Nasi Lemak House"));
        assert!(body.contains("DELIVERED"));
    }

    #[test]
    fn subscribe_webhook_registers_and_records_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/grab/v1/webhooks",
            ok_json(&serde_json::json!({ "webhook_id": "wh-99" })),
        );
        let c = GrabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/grab")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh-99"));
        assert_eq!(sub.secret.expose(), "grab-secret");
    }

    #[test]
    fn handle_webhook_event_maps_event_kinds() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GrabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!([
            { "order_id": "o-1", "event": "order.created" },
            { "order_id": "o-2", "event": "order.cancelled" }
        ]))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(events[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
