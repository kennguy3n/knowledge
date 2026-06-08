//! Tokopedia connector — Tokopedia Seller API
//! (`https://fs.tokopedia.net`).
//!
//! Tokopedia is Indonesia's largest marketplace (part of the GoTo
//! group). The Seller API is OAuth2 (authorization-code), so
//! [`TokopediaConnector::authenticate`] delegates to the injected
//! [`OAuth2CodeExchange`].
//!
//! * `initial_sync` / `incremental_sync` page `/v1/orders`
//!   (`per_page` / `page`), tracking the maximum `update_time` as an
//!   RFC-3339 watermark; incremental runs add `updated_since` and
//!   dedup the inclusive boundary row.
//! * `fetch_content` GETs a single order (`/v1/orders/{id}`).
//! * Tokopedia webhooks are registered in the developer console, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payload.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WatermarkCursor,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Tokopedia Seller API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://fs.tokopedia.net";
/// Page size for order listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct TokopediaOrdersPage {
    #[serde(default)]
    data: Vec<TokopediaOrder>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TokopediaOrder {
    #[serde(default)]
    order_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    buyer_name: Option<String>,
    #[serde(default)]
    update_time: Option<String>,
    #[serde(default)]
    create_time: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TokopediaWebhookEvent {
    #[serde(default)]
    order_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Tokopedia Seller connector.
pub struct TokopediaConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for TokopediaConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokopediaConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl TokopediaConnector {
    /// Construct a Tokopedia connector.
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

    /// Override the Tokopedia API base URL.
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
    ) -> Result<Vec<TokopediaOrder>> {
        let mut orders = Vec::<TokopediaOrder>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!(
                "{base_url}/v1/orders?per_page={}&page={page}",
                self.page_size
            );
            if let Some(since) = updated_since {
                url.push_str("&updated_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: TokopediaOrdersPage =
                bearer_get_json(&self.transport, "tokopedia", "/v1/orders", &url, token, &[])?;
            let count = resp.data.len();
            orders.extend(resp.data);
            if count < self.page_size as usize {
                return Ok(orders);
            }
        }
        Err(ConnectorError::Sync(format!(
            "tokopedia /v1/orders exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn order_watermark(o: &TokopediaOrder) -> Option<DateTime<Utc>> {
    o.update_time
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| o.create_time.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for TokopediaConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "tokopedia authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let orders = self.paginate_orders(&base_url, token, None)?;
        let mut events = Vec::with_capacity(orders.len());
        let mut cursor = WatermarkCursor::empty();
        for order in &orders {
            let occurred_at = order_watermark(order).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(order.order_id.clone()),
                occurred_at,
            });
            if let Some(t) = order_watermark(order) {
                cursor.observe(t, &order.order_id);
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: cursor.to_cursor_string(),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let since = prior.query_since();
        let orders = self.paginate_orders(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for order in &orders {
            let Some(updated) = order_watermark(order) else {
                continue;
            };
            if !prior.should_emit(updated, &order.order_id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(order.order_id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &order.order_id);
        }
        Ok(SyncRunResult {
            events,
            next_cursor: cursor.to_cursor_string(),
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
        let order: TokopediaOrder = bearer_get_json(
            &self.transport,
            "tokopedia",
            "/v1/orders/{id}",
            &url,
            token,
            &[],
        )?;
        let status = order.status.as_deref().unwrap_or("unknown");
        let buyer = order.buyer_name.as_deref().unwrap_or("(unknown buyer)");
        let body = format!("# Tokopedia order {id}\n\nBuyer: {buyer}\nStatus: {status}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("Tokopedia order {id}"))
            .with_metadata(serde_json::json!({
                "provider": "tokopedia",
                "order_id": order.order_id,
                "status": order.status,
                "update_time": order.update_time,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Tokopedia webhooks are registered in the developer console;
        // record a polling-only subscription so the runtime falls
        // back to incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("tokopedia-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<TokopediaWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<TokopediaWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<TokopediaWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("tokopedia webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty tokopedia webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.order_id).ok_or_else(|| {
                ConnectorError::Webhook("tokopedia webhook event missing order_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if delivery.event.contains("create") || delivery.event.contains("new") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("cancel") || delivery.event.contains("delete") {
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
                "toko-access",
                "toko-refresh",
                Utc::now() + Duration::hours(1),
                "orders",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Tokopedia,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/toko",
            "webhook_secret": "toko-secret",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TokopediaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(
            ConnectorKind::Tokopedia,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/toko/v1/orders?per_page=2&page=1",
            ok_json(&serde_json::json!({
                "data": [
                    {"order_id": "o-1", "update_time": "2024-01-01T00:00:00Z"},
                    {"order_id": "o-2", "update_time": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/toko/v1/orders?per_page=2&page=2",
            ok_json(&serde_json::json!({ "data": [ {"order_id": "o-3", "update_time": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = TokopediaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|o-3")
        );
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/toko/v1/orders?per_page=2&page=1&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "data": [
                    {"order_id": "o-10", "update_time": "2024-03-01T00:00:00Z"},
                    {"order_id": "o-13", "update_time": "2024-03-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/toko/v1/orders?per_page=2&page=2&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "data": [ {"order_id": "o-11", "update_time": "2024-06-01T00:00:00Z"} ] })),
        );
        let c = TokopediaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `o-10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must
        // NOT re-emit `o-10`, still surface the brand-new `o-13` at the same
        // second, and advance past the later row.
        state.cursor = Some(format!("{since}|o-10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["o-13", "o-11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|o-11")
        );
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/toko/v1/orders/o-1",
            ok_json(&serde_json::json!({
                "order_id": "o-1",
                "status": "SHIPPED",
                "buyer_name": "Dewi"
            })),
        );
        let c = TokopediaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("o-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Tokopedia order o-1"));
        assert!(body.contains("Dewi"));
    }

    #[test]
    fn handle_webhook_event_maps_kinds() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TokopediaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!([
            { "order_id": "o-1", "event": "order.new" },
            { "order_id": "o-2", "event": "order.cancelled" }
        ]))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(events[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
