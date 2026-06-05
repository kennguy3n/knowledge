//! Tiki connector — Tiki Seller Center API (orders + products).
//!
//! Tiki is a top Vietnamese e-commerce marketplace. The Seller Center
//! integration API authenticates with a seller API key and requires
//! each request to carry an HMAC-SHA256 `sign` computed over the
//! request path and a millisecond `timestamp`, keyed by the seller
//! secret. This mirrors the signing scheme used by the other
//! marketplace connectors (Shopee, Lazada).
//!
//! * `initial_sync` walks `GET /integration/v2/orders`, paging via the
//!   1-based `page` cursor until a short page is returned.
//! * `incremental_sync` adds an `updated_from_date` filter built from
//!   the stored RFC 3339 watermark.
//! * `fetch_content` reads `GET /integration/v2/orders/{id}` and
//!   renders a Markdown summary.
//! * `subscribe_webhook` registers the callback through
//!   `POST /integration/v2/webhooks` (Tiki exposes a create-webhook
//!   endpoint) and stores the returned subscription id.
//! * `handle_webhook_event` parses an order-event payload keyed by
//!   `order_code`.

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

use crate::sign::hmac_sha256_hex;

/// Default Tiki Seller Center API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.tiki.vn";

/// Default orders page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "seller.orders.read";

/// One Tiki order (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TikiOrder {
    /// Human-facing order code.
    #[serde(rename = "code", alias = "order_code")]
    pub code: String,
    /// Order status (`processing`, `complete`, …).
    #[serde(default)]
    pub status: Option<String>,
    /// Order total in VND.
    #[serde(default, rename = "total_price")]
    pub total_price: Option<i64>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "updated_at")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Envelope for the order list endpoint (`{ "data": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TikiOrdersResponse {
    /// Orders on this page.
    #[serde(default)]
    pub data: Vec<TikiOrder>,
}

/// Response from the create-webhook endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TikiWebhookResponse {
    /// Provider subscription id.
    #[serde(default)]
    pub id: Option<String>,
}

/// Order-event webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TikiWebhookPayload {
    /// Event name (`order_created`, `order_updated`, …).
    #[serde(default)]
    pub event: Option<String>,
    /// Affected order code.
    #[serde(default, rename = "order_code", alias = "code")]
    pub order_code: String,
}

/// Tiki Seller Center connector.
pub struct TikiConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for TikiConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TikiConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl TikiConnector {
    /// Construct a Tiki connector.
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

    /// Override the Seller Center API base URL.
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

    /// Read the seller signing secret from the config.
    fn signing_secret(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("api_secret")
            .or_else(|| config.auth_config_json.get("secret_key"))
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Auth("tiki: auth_config_json.api_secret is required".into())
            })
    }

    /// Build the HMAC-SHA256 request signature: hex digest of
    /// `path + timestamp` keyed by the seller secret.
    fn sign_request(secret: &str, path: &str, timestamp: i64) -> String {
        let base = format!("{path}{timestamp}");
        hmac_sha256_hex(secret.as_bytes(), base.as_bytes())
    }

    /// Append the `timestamp` + `sign` auth pair to a query string.
    fn signed_suffix(secret: &str, path: &str) -> String {
        let ts = Utc::now().timestamp_millis();
        let sign = Self::sign_request(secret, path, ts);
        format!("&timestamp={ts}&sign={sign}")
    }

    /// GET a JSON endpoint with the seller API key header and parse it.
    fn api_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("tiki-api-key", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("tiki", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "tiki {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every order page until a short page is returned.
    fn paginate_orders(
        &self,
        base_url: &str,
        secret: &str,
        token: &OAuth2Token,
        updated_from: Option<&str>,
    ) -> Result<Vec<TikiOrder>> {
        let path = "/integration/v2/orders";
        let mut out = Vec::<TikiOrder>::new();
        let filter = updated_from.map_or_else(String::new, |ts| {
            format!("&updated_from_date={}", percent_encode_path_component(ts))
        });
        for page in 1..=MAX_LIST_PAGES {
            let suffix = Self::signed_suffix(secret, path);
            let url = format!(
                "{base_url}{path}?page={page}&limit={}{filter}{suffix}",
                self.page_size
            );
            let resp: TikiOrdersResponse = self.api_get(path, &url, token)?;
            let returned = resp.data.len();
            out.extend(resp.data);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "tiki {path} exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn order_to_event(o: &TikiOrder, created: bool) -> ConnectorEvent {
    let occurred_at = o.updated_at.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(o.code.clone());
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

impl Connector for TikiConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("api_key")
            .or_else(|| config.auth_config_json.get("access_token"))
            .and_then(serde_json::Value::as_str)
        {
            // Fail fast if the signing secret is absent — every sync /
            // fetch call needs it, so surface the misconfiguration at
            // authenticate time rather than on the first request.
            Self::signing_secret(config)?;
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
                    "tiki authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let secret = Self::signing_secret(config)?;
        let orders = self.paginate_orders(&base_url, &secret, token, None)?;
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
        let secret = Self::signing_secret(config)?;
        let prior: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let orders = self.paginate_orders(&base_url, &secret, token, state.cursor.as_deref())?;
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
        let secret = Self::signing_secret(config)?;
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        // Sign the *actual* request path (including the id segment); Tiki
        // recomputes the HMAC over the path it receives, so signing the
        // bare collection path would fail signature validation.
        let path = format!("/integration/v2/orders/{id_enc}");
        let suffix = Self::signed_suffix(&secret, &path);
        let url = format!("{base_url}{path}?{}", suffix.trim_start_matches('&'));
        let order: TikiOrder = self.api_get("/integration/v2/orders/{id}", &url, token)?;

        let title = format!("Order {}", order.code);
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(status) = order.status.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Status:** {status}\n");
        }
        if let Some(total) = order.total_price {
            let _ = writeln!(md, "**Total:** {total} VND\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "tiki",
                "order_code": order.code,
                "status": order.status,
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
        let secret = Self::signing_secret(config)?;
        let path = "/integration/v2/webhooks";
        let suffix = Self::signed_suffix(&secret, path);
        let url = format!("{base_url}{path}?{}", suffix.trim_start_matches('&'));
        let body = serde_json::to_vec(&serde_json::json!({
            "url": callback_url,
            "events": ["order_created", "order_updated"],
        }))
        .map_err(|e| ConnectorError::Webhook(format!("tiki webhook body serialise: {e}")))?;
        let req = HttpRequest::post(url, body)
            .with_header("Accept", "application/json")
            .with_header("Content-Type", "application/json")
            .with_header("tiki-api-key", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("tiki", path, &resp));
        }
        let parsed: TikiWebhookResponse = serde_json::from_slice(&resp.body).unwrap_or_default();
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = parsed.id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: TikiWebhookPayload = serde_json::from_slice(body)?;
        if payload.order_code.is_empty() {
            return Err(ConnectorError::Webhook(
                "tiki webhook payload missing order_code".into(),
            ));
        }
        let id = SourceDocumentId::new(payload.order_code);
        let event = match payload.event.as_deref() {
            Some("order_created") => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at: Utc::now(),
            },
            _ => ConnectorEvent::DocumentUpdated {
                document_id: id,
                occurred_at: Utc::now(),
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
        ConnectorConfig::new(ConnectorKind::Tiki, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "tiki_key_123",
                "api_secret": "tiki-secret",
                "api_base_url": "https://api.test/tiki",
            }))
    }

    fn order(code: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "code": code,
            "status": "complete",
            "total_price": 250_000,
            "updated_at": updated,
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn sign_request_is_deterministic_and_keyed() {
        let a = TikiConnector::sign_request("secret", "/integration/v2/orders", 1_700_000_000_000);
        let b = TikiConnector::sign_request("secret", "/integration/v2/orders", 1_700_000_000_000);
        let c = TikiConnector::sign_request("other", "/integration/v2/orders", 1_700_000_000_000);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn authenticate_requires_key_and_secret() {
        let c = TikiConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "tiki_key_123"
        );
        let no_secret =
            ConnectorConfig::new(ConnectorKind::Tiki, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "api_key": "k" }));
        assert!(matches!(
            c.authenticate(&no_secret).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn initial_sync_signs_request_and_emits_created() {
        let transport = Arc::new(MockHttpTransport::new());
        // The signed URL carries a non-deterministic timestamp, so use
        // a default response and assert on the recorded request shape.
        transport.with_default_response(ok_json(
            &serde_json::json!({ "data": [order("OD1", "2024-01-01T00:00:00Z")] }),
        ));
        let c = TikiConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        let rec = &transport.recorded()[0];
        assert!(rec.url.contains("/integration/v2/orders?page=1"));
        assert!(rec.url.contains("&sign="));
        assert!(rec.url.contains("&timestamp="));
        assert!(rec
            .headers
            .iter()
            .any(|(k, v)| k == "tiki-api-key" && v == "tiki_key_123"));
    }

    #[test]
    fn incremental_sync_filters_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(ok_json(&serde_json::json!({ "data": [
            order("OD1", "2024-01-01T00:00:00Z"),
            order("OD2", "2024-05-01T00:00:00Z"),
        ] })));
        let c = TikiConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "OD2");
        assert!(transport.recorded()[0].url.contains("updated_from_date="));
    }

    #[test]
    fn initial_sync_maps_403_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(MockResponse::status(403, b"forbidden".to_vec()));
        let c = TikiConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(ok_json(&order("OD1", "2024-01-01T00:00:00Z")));
        let c = TikiConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("OD1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Order OD1"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Status:** complete"));
        // The signature must be computed over the *actual* request path,
        // which includes the order-id segment.
        let rec = &transport.recorded()[0];
        assert!(rec.url.contains("/integration/v2/orders/OD1?"));
        assert!(rec.url.contains("&sign="));
        let expected_path = "/integration/v2/orders/OD1";
        let ts: i64 = rec
            .url
            .split("timestamp=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.parse().ok())
            .expect("timestamp present");
        let expected_sign = TikiConnector::sign_request("tiki-secret", expected_path, ts);
        assert!(rec.url.contains(&format!("&sign={expected_sign}")));
    }

    #[test]
    fn subscribe_webhook_posts_and_stores_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(ok_json(&serde_json::json!({ "id": "wh_1" })));
        let c = TikiConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/tiki")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh_1"));
        assert_eq!(sub.secret.expose(), "tiki-secret");
        assert_eq!(transport.recorded()[0].method, HttpMethod::Post);
    }

    #[test]
    fn handle_webhook_event_maps_created() {
        let c = TikiConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "event": "order_created", "order_code": "OD9" });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "OD9");
    }

    #[test]
    fn handle_webhook_event_missing_code_errors() {
        let c = TikiConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "event": "order_updated" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
