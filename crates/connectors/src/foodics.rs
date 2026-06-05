//! Foodics connector — Foodics API v5 (Saudi/GCC restaurant SaaS).
//!
//! * `initial_sync` pages `GET /v5/orders?per_page=100&page=N`,
//!   stopping on a short page.
//! * `incremental_sync` adds the `updated_after` filter keyed off the
//!   stored RFC-3339 watermark; the filter is inclusive, so the
//!   boundary row is deduped client-side.
//! * `fetch_content` GETs `/v5/orders/{id}` and renders a Markdown
//!   summary of the POS transaction.
//! * `subscribe_webhook` POSTs `/v5/webhooks` and records the returned
//!   provider subscription id.
//! * `handle_webhook_event` parses a single object or a batched array.
//!
//! Foodics authenticates with an OAuth2 bearer token obtained through
//! the injected [`OAuth2CodeExchange`].

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Default Foodics API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.foodics.com";

/// Page size for order listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// One Foodics order (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoodicsOrder {
    /// Order id.
    #[serde(default)]
    pub id: String,
    /// Sequential order reference / number.
    #[serde(default)]
    pub reference: Option<String>,
    /// Order total (string-encoded decimal, as Foodics returns it).
    #[serde(default)]
    pub total_price: Option<String>,
    /// Branch name the order was placed at.
    #[serde(default)]
    pub branch_name: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Foodics list response (`{ "data": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoodicsOrdersResponse {
    /// Page of orders.
    #[serde(default)]
    pub data: Vec<FoodicsOrder>,
}

/// Foodics single-resource response (`{ "data": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoodicsOrderResponse {
    /// The order.
    #[serde(default)]
    pub data: FoodicsOrder,
}

/// Foodics webhook-create response (`{ "data": { "id": ... } }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoodicsWebhookResponse {
    /// The created subscription.
    #[serde(default)]
    pub data: FoodicsWebhook,
}

/// Created webhook subscription.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoodicsWebhook {
    /// Provider subscription id.
    #[serde(default)]
    pub id: String,
}

/// Foodics webhook delivery payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoodicsWebhookEvent {
    /// Affected order id (string or number).
    #[serde(default)]
    pub order_id: serde_json::Value,
    /// Event label, e.g. `order.created`, `order.updated`.
    #[serde(default)]
    pub event: String,
}

/// Foodics connector.
pub struct FoodicsConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for FoodicsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FoodicsConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl FoodicsConnector {
    /// Construct a Foodics connector.
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

    /// Override the Foodics base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size (clamped to at least 1).
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

    /// Walk the order list page-by-page, stopping on a short page.
    fn paginate_orders(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_after: Option<&str>,
    ) -> Result<Vec<FoodicsOrder>> {
        let mut out = Vec::<FoodicsOrder>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!(
                "{base_url}/v5/orders?per_page={}&page={page}",
                self.page_size
            );
            if let Some(after) = updated_after {
                url.push_str("&updated_after=");
                url.push_str(&percent_encode_path_component(after));
            }
            let resp: FoodicsOrdersResponse =
                bearer_get_json(&self.transport, "foodics", "/v5/orders", &url, token, &[])?;
            let count = resp.data.len();
            out.extend(resp.data);
            if count < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "foodics /v5/orders exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn order_watermark(o: &FoodicsOrder) -> Option<DateTime<Utc>> {
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

impl Connector for FoodicsConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "foodics authenticate: auth_config_json.authorization_code is required".into(),
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
            let occurred_at = order_watermark(o).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(o.id.clone()),
                occurred_at,
            });
            if let Some(t) = order_watermark(o) {
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
        for o in &orders {
            let Some(updated) = order_watermark(o) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(o.id.clone()),
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
        let url = format!("{base_url}/v5/orders/{id_enc}");
        let resp: FoodicsOrderResponse = bearer_get_json(
            &self.transport,
            "foodics",
            "/v5/orders/{id}",
            &url,
            token,
            &[],
        )?;
        let order = resp.data;
        let title = order
            .reference
            .clone()
            .unwrap_or_else(|| format!("Order {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(branch) = order.branch_name.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Branch:** ");
            md.push_str(branch);
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
                "provider": "foodics",
                "order_id": id,
                "branch_name": order.branch_name,
                "updated_at": order.updated_at,
            }))
            .with_source_url(format!("{base_url}/v5/orders/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/v5/webhooks");
        let body = serde_json::json!({
            "url": callback_url,
            "events": ["order.created", "order.updated"],
        });
        let resp: FoodicsWebhookResponse = bearer_post_json(
            &self.transport,
            "foodics",
            "/v5/webhooks",
            &url,
            token,
            &[],
            &body,
        )?;
        if resp.data.id.is_empty() {
            return Err(ConnectorError::Webhook(
                "foodics /v5/webhooks returned no webhook id".into(),
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
                    .unwrap_or("foodics-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.data.id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<FoodicsWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<FoodicsWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<FoodicsWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty foodics webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.order_id).ok_or_else(|| {
                ConnectorError::Webhook("foodics webhook event missing order_id".into())
            })?;
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(id_str);
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("delete") || delivery.event.contains("void") {
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
                "foodics-access",
                "foodics-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Foodics, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/foodics",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn small(c: FoodicsConnector) -> FoodicsConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FoodicsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::Foodics, AuthKind::OAuth2, ScopeId::new_v4());
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
            "https://api.test/foodics/v5/orders?per_page=2&page=1".to_string(),
            ok_json(&serde_json::json!({"data": [
                {"id": "1", "updated_at": "2024-01-01T00:00:00Z"},
                {"id": "2", "updated_at": "2024-01-02T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/foodics/v5/orders?per_page=2&page=2".to_string(),
            ok_json(&serde_json::json!({"data": [
                {"id": "3", "updated_at": "2024-01-03T00:00:00Z"}
            ]})),
        );
        let c = small(FoodicsConnector::new(
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
                "https://api.test/foodics/v5/orders?per_page=2&page=1&updated_after={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"data": [
                {"id": "10", "updated_at": "2024-03-01T00:00:00Z"},
                {"id": "11", "updated_at": "2024-06-01T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/foodics/v5/orders?per_page=2&page=2&updated_after={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"data": []})),
        );
        let c = small(FoodicsConnector::new(
            ConnectorInstanceId::new_v4(),
            transport,
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(since.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00")
        );
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/foodics/v5/orders/55".to_string(),
            ok_json(&serde_json::json!({"data": {
                "id": "55",
                "reference": "ORD-55",
                "branch_name": "Riyadh",
                "total_price": "120.00",
                "updated_at": "2024-03-01T00:00:00Z"
            }})),
        );
        let c = FoodicsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# ORD-55"));
        assert!(body.contains("**Branch:** Riyadh"));
        assert!(body.contains("**Total:** 120.00"));
    }

    #[test]
    fn subscribe_webhook_records_provider_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/foodics/v5/webhooks".to_string(),
            ok_json(&serde_json::json!({"data": {"id": "wh_9"}})),
        );
        let c = FoodicsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/foodics")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh_9"));
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FoodicsConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(br#"{"order_id": "7", "event": "order.created"}"#)
            .unwrap();
        assert!(matches!(single[0], ConnectorEvent::DocumentCreated { .. }));
        let batch = c
            .handle_webhook_event(
                br#"[{"order_id": 8, "event": "order.updated"}, {"order_id": "9", "event": "order.void"}]"#,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
