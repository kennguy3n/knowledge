//! Viettel Post connector — shipment tracking + orders.
//!
//! Viettel Post is Vietnam's largest logistics carrier. The partner
//! API exposes shipment orders and their tracking state behind an
//! API-key (`Token`) credential.
//!
//! * `initial_sync` walks `GET /v2/order/list`, paging via the 1-based
//!   `page` cursor until a short page is returned.
//! * `incremental_sync` adds an `updatedFrom` filter built from the
//!   stored RFC 3339 watermark.
//! * `fetch_content` reads `GET /v2/order/detail/{tracking}` and
//!   renders a Markdown tracking summary.
//! * `subscribe_webhook` is configuration-based — Viettel Post status
//!   callbacks are registered once with partner support, so no HTTP
//!   call is issued; the configured secret is surfaced for delivery
//!   validation.
//! * `handle_webhook_event` parses a status-push payload keyed by the
//!   `ORDER_NUMBER` tracking code.
//!
//! Viettel Post authenticates with a bespoke `Token` header (not a
//! bearer `Authorization`), so requests go through the injected
//! [`HttpTransport`] directly.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_form_component, percent_encode_path_component, Connector,
    ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent,
    HttpRequest, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default Viettel Post partner API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://partner.viettelpost.vn/v2";

/// Default order list page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "order.read";

/// One Viettel Post shipment order (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViettelOrder {
    /// Tracking code / order number.
    #[serde(rename = "ORDER_NUMBER", alias = "orderNumber")]
    pub order_number: String,
    /// Current status text.
    #[serde(default, rename = "STATUS_NAME", alias = "statusName")]
    pub status_name: Option<String>,
    /// Numeric status code.
    #[serde(default, rename = "ORDER_STATUS", alias = "orderStatus")]
    pub order_status: Option<i64>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "updatedAt")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Envelope for the order list endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViettelOrdersResponse {
    /// Orders on this page.
    #[serde(default, alias = "orders")]
    pub data: Vec<ViettelOrder>,
}

/// Status-push webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViettelWebhookPayload {
    /// Tracking code / order number.
    #[serde(default, rename = "ORDER_NUMBER", alias = "orderNumber")]
    pub order_number: String,
    /// New numeric status code.
    #[serde(default, rename = "ORDER_STATUS", alias = "orderStatus")]
    pub order_status: Option<i64>,
}

/// Viettel Post connector.
pub struct ViettelPostConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ViettelPostConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViettelPostConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ViettelPostConnector {
    /// Construct a Viettel Post connector.
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

    /// Override the partner API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 200]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 200);
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
            .with_header("Token", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("viettel_post", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "viettel_post {endpoint} JSON parse failed: {e} (body prefix: {})",
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
    ) -> Result<Vec<ViettelOrder>> {
        let mut out = Vec::<ViettelOrder>::new();
        let filter = updated_from.map_or_else(String::new, |ts| {
            format!("&updatedFrom={}", percent_encode_form_component(ts))
        });
        for page in 1..=MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/order/list?page={page}&size={}{filter}",
                self.page_size
            );
            let resp: ViettelOrdersResponse = self.api_get("/order/list", &url, token)?;
            let returned = resp.data.len();
            out.extend(resp.data);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "viettel_post /order/list exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn order_to_event(o: &ViettelOrder, created: bool) -> ConnectorEvent {
    let occurred_at = o.updated_at.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(o.order_number.clone());
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

impl Connector for ViettelPostConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("api_key")
            .or_else(|| config.auth_config_json.get("token"))
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
                    "viettel_post authenticate: auth_config_json.api_key or .authorization_code is required"
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
        let url = format!("{base_url}/order/detail/{id_enc}");
        let order: ViettelOrder = self.api_get("/order/detail/{id}", &url, token)?;

        let title = format!("Shipment {}", order.order_number);
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(status) = order.status_name.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Status:** {status}\n");
        }
        if let Some(code) = order.order_status {
            let _ = writeln!(md, "**Status code:** {code}\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "viettel_post",
                "order_number": order.order_number,
                "order_status": order.order_status,
                "status_name": order.status_name,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Viettel Post status callbacks are registered once with
        // partner support (no self-serve create-webhook endpoint), so
        // we do not issue an HTTP call here. Surface the configured
        // secret for delivery validation.
        let _ = token;
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "viettel_post subscribe_webhook: auth_config_json.webhook_secret is required"
                        .into(),
                )
            })?;
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: ViettelWebhookPayload = serde_json::from_slice(body)?;
        if payload.order_number.is_empty() {
            return Err(ConnectorError::Webhook(
                "viettel_post push payload missing ORDER_NUMBER".into(),
            ));
        }
        Ok(vec![ConnectorEvent::DocumentUpdated {
            document_id: SourceDocumentId::new(payload.order_number),
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
        ConnectorConfig::new(
            ConnectorKind::ViettelPost,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "vtp_token_123",
            "api_base_url": "https://api.test/vtp",
            "webhook_secret": "vtp-secret",
        }))
    }

    fn order(num: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "ORDER_NUMBER": num,
            "STATUS_NAME": "Delivered",
            "ORDER_STATUS": 500,
            "updatedAt": updated,
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_api_key() {
        let c = ViettelPostConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "vtp_token_123"
        );
    }

    #[test]
    fn initial_sync_emits_created_with_token_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vtp/order/list?page=1&size=50",
            ok_json(&serde_json::json!({ "data": [order("VTP1", "2024-01-01T00:00:00Z")] })),
        );
        let c =
            ViettelPostConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(transport.recorded()[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Token" && v == "vtp_token_123"));
    }

    #[test]
    fn incremental_sync_filters_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vtp/order/list?page=1&size=50&updatedFrom=2024-01-01T00%3A00%3A00%2B00%3A00",
            ok_json(&serde_json::json!({ "data": [
                order("VTP1", "2024-01-01T00:00:00Z"),
                order("VTP2", "2024-04-01T00:00:00Z"),
            ] })),
        );
        let c = ViettelPostConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "VTP2");
    }

    #[test]
    fn initial_sync_maps_500_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vtp/order/list?page=1&size=50",
            MockResponse::status(500, b"server error".to_vec()),
        );
        let c = ViettelPostConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn fetch_content_renders_tracking() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vtp/order/detail/VTP1",
            ok_json(&order("VTP1", "2024-01-01T00:00:00Z")),
        );
        let c = ViettelPostConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("VTP1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Shipment VTP1"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Status:** Delivered"));
    }

    #[test]
    fn subscribe_webhook_uses_secret_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c =
            ViettelPostConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/vtp")
            .unwrap();
        assert_eq!(sub.secret.expose(), "vtp-secret");
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn handle_webhook_event_maps_to_updated() {
        let c = ViettelPostConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "ORDER_NUMBER": "VTP9", "ORDER_STATUS": 200 });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].document_id().as_str(), "VTP9");
    }

    #[test]
    fn handle_webhook_event_missing_number_errors() {
        let c = ViettelPostConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "ORDER_STATUS": 200 });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
