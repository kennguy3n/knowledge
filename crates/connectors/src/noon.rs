//! Noon connector — Noon Seller Center API (UAE/Saudi e-commerce).
//!
//! * `initial_sync` pages `GET /seller/v1/orders?per_page=100&page=N`,
//!   stopping on a short page.
//! * `incremental_sync` adds the `updated_since` filter keyed off the
//!   stored RFC-3339 watermark; the filter is inclusive, so the
//!   boundary row is deduped client-side.
//! * `fetch_content` GETs `/seller/v1/orders/{id}` and renders a
//!   Markdown summary.
//! * Noon Seller Center has no API to register webhooks (they are
//!   configured in the seller portal), so `subscribe_webhook` records a
//!   polling-only subscription with no provider id.
//! * `handle_webhook_event` parses the portal-delivered payload (single
//!   object or batched array).
//!
//! Noon authenticates every request with the seller `api_key` plus an
//! HMAC-SHA256 signature over a canonical `METHOD\npath\ntimestamp`
//! string keyed by the seller `secret_key`. The signature, key id and
//! timestamp ride in `X-Noon-*` headers, so the connector issues
//! requests through the injected [`HttpTransport`] directly.

use crate::signing::{hex_lower, hmac_sha256};
use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Default Noon Seller Center API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.noon.partners";

/// Page size for order listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "orders.read";

/// One Noon order (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoonOrder {
    /// Order id.
    #[serde(default)]
    pub id: String,
    /// Customer-facing order code.
    #[serde(default)]
    pub order_nr: Option<String>,
    /// Order status (e.g. `shipped`, `delivered`).
    #[serde(default)]
    pub status: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Noon order list response (`{ "orders": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoonOrdersResponse {
    /// Page of orders.
    #[serde(default)]
    pub orders: Vec<NoonOrder>,
}

/// Noon single-order response (`{ "order": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoonOrderResponse {
    /// The order.
    #[serde(default)]
    pub order: NoonOrder,
}

/// Noon webhook delivery payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoonWebhookEvent {
    /// Affected order id (string or number).
    #[serde(default)]
    pub order_id: serde_json::Value,
    /// Event label, e.g. `order_created`, `order_cancelled`.
    #[serde(default)]
    pub event: String,
}

/// Noon connector.
pub struct NoonConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for NoonConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoonConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .finish()
    }
}

impl NoonConnector {
    /// Construct a Noon connector.
    pub fn new(
        instance: ConnectorInstanceId,
        transport: Arc<dyn HttpTransport>,
        _oauth: Arc<dyn OAuth2CodeExchange>,
    ) -> Self {
        Self {
            instance,
            transport,
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Override the Noon base URL.
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

    fn secret_key(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("secret_key")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Auth("noon: auth_config_json.secret_key is required".into())
            })
    }

    /// GET a signed JSON endpoint. `path_and_query` is the portion
    /// after the host (e.g. `/seller/v1/orders?page=1`); it is both the
    /// request target and the value bound into the HMAC signature so a
    /// tampered path invalidates the signature.
    fn signed_get<R: DeserializeOwned>(
        &self,
        base_url: &str,
        path_and_query: &str,
        api_key: &str,
        secret_key: &str,
        endpoint: &str,
    ) -> Result<R> {
        let timestamp = Utc::now().timestamp().to_string();
        let canonical = format!("GET\n{path_and_query}\n{timestamp}");
        let signature = hex_lower(&hmac_sha256(secret_key.as_bytes(), canonical.as_bytes()));
        let url = format!("{base_url}{path_and_query}");
        let req = HttpRequest::get(&url)
            .with_header("Accept", "application/json")
            .with_header("X-Noon-Api-Key", api_key)
            .with_header("X-Noon-Timestamp", &timestamp)
            .with_header("X-Noon-Signature", &signature);
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("noon", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "noon {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk the order list page-by-page, stopping on a short page.
    fn paginate_orders(
        &self,
        base_url: &str,
        api_key: &str,
        secret_key: &str,
        updated_since: Option<&str>,
    ) -> Result<Vec<NoonOrder>> {
        let mut out = Vec::<NoonOrder>::new();
        for page in 1..=MAX_PAGES {
            let mut path = format!("/seller/v1/orders?per_page={}&page={page}", self.page_size);
            if let Some(since) = updated_since {
                path.push_str("&updated_since=");
                path.push_str(&percent_encode_path_component(since));
            }
            let resp: NoonOrdersResponse =
                self.signed_get(base_url, &path, api_key, secret_key, "/seller/v1/orders")?;
            let count = resp.orders.len();
            out.extend(resp.orders);
            if count < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "noon /seller/v1/orders exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn order_watermark(o: &NoonOrder) -> Option<DateTime<Utc>> {
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

impl Connector for NoonConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let api_key = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "noon authenticate: auth_config_json.api_key is required".into(),
                )
            })?;
        // Require the secret key up front so a misconfigured connector
        // fails at authenticate rather than mid-sync.
        Self::secret_key(config)?;
        Ok(OAuth2Token::new_without_refresh(
            api_key,
            Utc::now() + chrono::Duration::days(3650),
            DEFAULT_SCOPE,
        ))
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let secret = Self::secret_key(config)?;
        let api_key = token.access_token.expose();
        let orders = self.paginate_orders(&base_url, api_key, &secret, None)?;
        let mut events = Vec::with_capacity(orders.len());
        let mut cursor = WatermarkCursor::empty();
        for o in &orders {
            let occurred_at = order_watermark(o).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(o.id.clone()),
                occurred_at,
            });
            if let Some(t) = order_watermark(o) {
                cursor.observe(t, &o.id);
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
        let secret = Self::secret_key(config)?;
        let api_key = token.access_token.expose();
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let since = prior.query_since();
        let orders = self.paginate_orders(&base_url, api_key, &secret, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for o in &orders {
            let Some(updated) = order_watermark(o) else {
                continue;
            };
            if !prior.should_emit(updated, &o.id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(o.id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &o.id);
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
        let secret = Self::secret_key(config)?;
        let api_key = token.access_token.expose();
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let path = format!("/seller/v1/orders/{id_enc}");
        let resp: NoonOrderResponse =
            self.signed_get(&base_url, &path, api_key, &secret, "/seller/v1/orders/{id}")?;
        let order = resp.order;
        let title = order
            .order_nr
            .clone()
            .unwrap_or_else(|| format!("Order {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(status) = order.status.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Status:** ");
            md.push_str(status);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "noon",
                "order_id": id,
                "status": order.status,
                "updated_at": order.updated_at,
            }))
            .with_source_url(format!("{base_url}/seller/orders/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Noon Seller Center has no API to register webhooks — they are
        // configured in the seller portal. Record a polling-only
        // subscription so the runtime falls back to incremental_sync.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("noon-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<NoonWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<NoonWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<NoonWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty noon webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.order_id).ok_or_else(|| {
                ConnectorError::Webhook("noon webhook event missing order_id".into())
            })?;
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(id_str);
            let event = if delivery.event.contains("create") {
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
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new_without_refresh(
                "unused",
                Utc::now() + chrono::Duration::hours(1),
                "x",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Noon, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "noon_key",
                "secret_key": "noon_secret",
                "api_base_url": "https://api.test/noon",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn small(c: NoonConnector) -> NoonConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_requires_api_key_and_secret() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NoonConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "noon_key"
        );
        let no_secret =
            ConnectorConfig::new(ConnectorKind::Noon, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "api_key": "noon_key" }));
        assert!(matches!(
            c.authenticate(&no_secret),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates_and_signs() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/noon/seller/v1/orders?per_page=2&page=1".to_string(),
            ok_json(&serde_json::json!({"orders": [
                {"id": "1", "updated_at": "2024-01-01T00:00:00Z"},
                {"id": "2", "updated_at": "2024-01-02T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/noon/seller/v1/orders?per_page=2&page=2".to_string(),
            ok_json(&serde_json::json!({"orders": [
                {"id": "3", "updated_at": "2024-01-03T00:00:00Z"}
            ]})),
        );
        let c = small(NoonConnector::new(
            ConnectorInstanceId::new_v4(),
            transport.clone(),
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|3")
        );
        let recorded = transport.recorded();
        let headers = &recorded[0].headers;
        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Noon-Api-Key" && v == "noon_key"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Noon-Signature" && !v.is_empty()));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/noon/seller/v1/orders?per_page=2&page=1&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"orders": [
                {"id": "10", "updated_at": "2024-03-01T00:00:00Z"},
                {"id": "13", "updated_at": "2024-03-01T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/noon/seller/v1/orders?per_page=2&page=2&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"orders": [ {"id": "11", "updated_at": "2024-06-01T00:00:00Z"} ]})),
        );
        let c = small(NoonConnector::new(
            ConnectorInstanceId::new_v4(),
            transport,
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must
        // NOT re-emit `10`, still surface the brand-new `13` at the same
        // second, and advance past the later row.
        state.cursor = Some(format!("{since}|10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["13", "11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|11")
        );
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/noon/seller/v1/orders/55".to_string(),
            ok_json(&serde_json::json!({"order": {
                "id": "55",
                "order_nr": "NOON-55",
                "status": "delivered"
            }})),
        );
        let c = NoonConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# NOON-55"));
        assert!(body.contains("**Status:** delivered"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NoonConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/noon")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NoonConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(br#"{"order_id": "7", "event": "order_created"}"#)
            .unwrap();
        assert!(matches!(single[0], ConnectorEvent::DocumentCreated { .. }));
        let batch = c
            .handle_webhook_event(
                br#"[{"order_id": 8, "event": "order_status_changed"}, {"order_id": "9", "event": "order_cancelled"}]"#,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
