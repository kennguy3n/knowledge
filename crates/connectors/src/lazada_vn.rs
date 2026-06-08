//! Lazada (Vietnam) connector — Lazada Open Platform API.
//!
//! Lazada is a major Southeast-Asian marketplace. The Open Platform
//! API authenticates with an OAuth2 `access_token` and signs every
//! request using Lazada's canonical scheme: sort all request
//! parameters by key, concatenate `key + value` pairs, prefix the API
//! path, and take the upper-case hex HMAC-SHA256 keyed by the
//! `app_secret`. The auth material (`app_key`, `app_secret`) lives in
//! `auth_config_json`; the `access_token` is either configured
//! directly or obtained by exchanging an authorization code through
//! the injected [`OAuth2CodeExchange`].
//!
//! * `initial_sync` walks `GET /orders/get`, paging via the `offset`
//!   cursor until a short page is returned.
//! * `incremental_sync` adds an `update_after` filter built from the
//!   stored RFC 3339 watermark.
//! * `fetch_content` reads `GET /order/get` and renders a Markdown
//!   summary.
//! * `subscribe_webhook` is configuration-based — Lazada push is
//!   registered once in the App console, so no HTTP call is issued;
//!   the `app_secret` is surfaced so the substrate can validate the
//!   delivery signature.
//! * `handle_webhook_event` parses a push payload keyed by
//!   `order_id`.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::sign::hmac_sha256_hex;

/// Default Lazada Open Platform API base URL (Vietnam gateway).
pub const DEFAULT_API_BASE_URL: &str = "https://api.lazada.vn/rest";

/// Default order list page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured access token.
const DEFAULT_SCOPE: &str = "order.read";

/// Resolved Lazada auth material pulled from `auth_config_json`.
struct LazadaAuth {
    app_key: String,
    app_secret: String,
}

/// One Lazada order (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LazadaOrder {
    /// Order id (Lazada returns it as a number or string).
    #[serde(rename = "order_id", alias = "order_number")]
    pub order_id: serde_json::Value,
    /// Order status.
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    /// Order total in VND (string on the wire).
    #[serde(default)]
    pub price: Option<String>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "updated_at")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl LazadaOrder {
    /// Normalise the order id to a string regardless of wire shape.
    fn id_string(&self) -> String {
        match &self.order_id {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    }
}

/// Envelope for the order list endpoint (`{ "data": { "orders": [] } }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LazadaOrdersResponse {
    /// Inner data block.
    #[serde(default)]
    pub data: LazadaOrdersData,
}

/// Inner `data` block for the order list endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LazadaOrdersData {
    /// Orders on this page.
    #[serde(default)]
    pub orders: Vec<LazadaOrder>,
}

/// Envelope for the single-order endpoint (`{ "data": {order} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LazadaOrderResponse {
    /// The order.
    #[serde(default)]
    pub data: LazadaOrder,
}

/// Push webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LazadaWebhookPayload {
    /// Message type (Lazada `message_type`).
    #[serde(default)]
    pub message_type: Option<i64>,
    /// Affected order id.
    #[serde(default)]
    pub data: LazadaWebhookData,
}

/// Inner `data` block for a Lazada push.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LazadaWebhookData {
    /// Order id (number or string).
    #[serde(default, rename = "trade_order_id", alias = "order_id")]
    pub order_id: serde_json::Value,
}

impl LazadaWebhookData {
    fn id_string(&self) -> String {
        match &self.order_id {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    }
}

/// Lazada (Vietnam) connector.
pub struct LazadaVNConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for LazadaVNConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazadaVNConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl LazadaVNConnector {
    /// Construct a Lazada connector.
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

    /// Override the Open Platform API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 100]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 100);
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

    fn resolve_auth(config: &ConnectorConfig) -> Result<LazadaAuth> {
        let get = |k: &str| {
            config
                .auth_config_json
                .get(k)
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string)
        };
        let app_key = get("app_key").ok_or_else(|| {
            ConnectorError::Auth("lazada_vn: auth_config_json.app_key is required".into())
        })?;
        let app_secret = get("app_secret").ok_or_else(|| {
            ConnectorError::Auth("lazada_vn: auth_config_json.app_secret is required".into())
        })?;
        Ok(LazadaAuth {
            app_key,
            app_secret,
        })
    }

    /// Lazada canonical signature: sort params by key, concatenate
    /// `key + value`, prefix the API path, then take the upper-case
    /// hex HMAC-SHA256 keyed by the `app_secret`.
    fn sign_request(app_secret: &str, path: &str, params: &[(&str, &str)]) -> String {
        let mut sorted: Vec<(&str, &str)> = params.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut base = String::from(path);
        for (k, v) in sorted {
            base.push_str(k);
            base.push_str(v);
        }
        hmac_sha256_hex(app_secret.as_bytes(), base.as_bytes()).to_uppercase()
    }

    /// Build the full signed query string for a call. `extra` carries
    /// the API-specific params (already string-valued); the common
    /// `app_key`, `timestamp`, `sign_method` and `access_token`
    /// params are added here, and the resulting `sign` is appended.
    fn signed_query(
        auth: &LazadaAuth,
        path: &str,
        token: &OAuth2Token,
        extra: &[(&str, String)],
    ) -> String {
        let ts = Utc::now().timestamp_millis().to_string();
        let access = token.access_token.expose().to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("app_key", auth.app_key.as_str()),
            ("timestamp", ts.as_str()),
            ("sign_method", "sha256"),
            ("access_token", access.as_str()),
        ];
        for (k, v) in extra {
            params.push((k, v.as_str()));
        }
        let sign = Self::sign_request(&auth.app_secret, path, &params);
        let mut query = String::new();
        for (k, v) in &params {
            let _ = write!(query, "{k}={}&", percent_encode_path_component(v));
        }
        let _ = write!(query, "sign={sign}");
        query
    }

    fn api_get<R: DeserializeOwned>(&self, endpoint: &str, url: &str) -> Result<R> {
        let req = HttpRequest::get(url).with_header("Accept", "application/json");
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("lazada_vn", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "lazada_vn {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every order page until a short page is returned.
    fn paginate_orders(
        &self,
        base_url: &str,
        auth: &LazadaAuth,
        token: &OAuth2Token,
        update_after: Option<&str>,
    ) -> Result<Vec<LazadaOrder>> {
        let path = "/orders/get";
        let mut out = Vec::<LazadaOrder>::new();
        for page in 0..MAX_LIST_PAGES {
            let offset = page * self.page_size as usize;
            let mut extra: Vec<(&str, String)> = vec![
                ("limit", self.page_size.to_string()),
                ("offset", offset.to_string()),
                ("sort_direction", "ASC".to_string()),
            ];
            if let Some(ts) = update_after {
                extra.push(("update_after", ts.to_string()));
            }
            let query = Self::signed_query(auth, path, token, &extra);
            let url = format!("{base_url}{path}?{query}");
            let resp: LazadaOrdersResponse = self.api_get(path, &url)?;
            let returned = resp.data.orders.len();
            out.extend(resp.data.orders);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "lazada_vn {path} exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn order_to_event(o: &LazadaOrder, created: bool) -> ConnectorEvent {
    let occurred_at = o.updated_at.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(o.id_string());
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

impl Connector for LazadaVNConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        Self::resolve_auth(config)?;
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
                    "lazada_vn authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let auth = Self::resolve_auth(config)?;
        let orders = self.paginate_orders(&base_url, &auth, token, None)?;
        let mut events = Vec::with_capacity(orders.len());
        let mut cursor = WatermarkCursor::empty();
        for o in &orders {
            events.push(order_to_event(o, true));
            if let Some(ts) = o.updated_at {
                cursor.observe(ts, &o.id_string());
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
        let auth = Self::resolve_auth(config)?;
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let orders =
            self.paginate_orders(&base_url, &auth, token, prior.query_since().as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for o in &orders {
            match o.updated_at {
                Some(ts) => {
                    if !prior.should_emit(ts, &o.id_string()) {
                        continue;
                    }
                    events.push(order_to_event(o, false));
                    cursor.observe(ts, &o.id_string());
                }
                None => events.push(order_to_event(o, false)),
            }
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
        let auth = Self::resolve_auth(config)?;
        let path = "/order/get";
        let id = document_id.as_str();
        let extra = vec![("order_id", id.to_string())];
        let query = Self::signed_query(&auth, path, token, &extra);
        let url = format!("{base_url}{path}?{query}");
        let resp: LazadaOrderResponse = self.api_get(path, &url)?;
        let order = resp.data;

        let title = format!("Order {}", order.id_string());
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(statuses) = order.statuses.as_ref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Status:** {}\n", statuses.join(", "));
        }
        if let Some(price) = order.price.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Total:** {price} VND\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "lazada_vn",
                "order_id": order.id_string(),
                "statuses": order.statuses,
                "price": order.price,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Lazada push is registered once in the App console, so no
        // HTTP call is issued here. Surface the app_secret so the
        // substrate can validate the delivery signature.
        let _ = token;
        let auth = Self::resolve_auth(config)?;
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(auth.app_secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: LazadaWebhookPayload = serde_json::from_slice(body)?;
        let id = payload.data.id_string();
        if id.is_empty() {
            return Err(ConnectorError::Webhook(
                "lazada_vn push payload missing data.trade_order_id".into(),
            ));
        }
        Ok(vec![ConnectorEvent::DocumentUpdated {
            document_id: SourceDocumentId::new(id),
            occurred_at: Utc::now(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use connector_framework::{AuthKind, ConnectorKind, MockHttpTransport, MockResponse};
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "lazada-access",
                "lazada-refresh",
                Utc::now() + Duration::hours(1),
                "order.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::LazadaVN, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "lzd_tok_123",
                "app_key": "100001",
                "app_secret": "lazada-app-secret",
                "api_base_url": "https://api.test/lazada",
            }))
    }

    fn order(id: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "order_id": id,
            "statuses": ["delivered"],
            "price": "350000",
            "updated_at": updated,
        })
    }

    fn list_resp(orders: &[serde_json::Value]) -> MockResponse {
        MockResponse::ok_json(
            serde_json::to_vec(&serde_json::json!({ "data": { "orders": orders } })).unwrap(),
        )
    }

    #[test]
    fn sign_request_sorts_and_is_uppercase_hex() {
        let s = LazadaVNConnector::sign_request("secret", "/orders/get", &[("b", "2"), ("a", "1")]);
        // Equivalent to signing the canonical "/orders/geta1b2".
        let direct = hmac_sha256_hex(b"secret", b"/orders/geta1b2").to_uppercase();
        assert_eq!(s, direct);
        assert_eq!(s.len(), 64);
        assert_eq!(s, s.to_uppercase());
    }

    #[test]
    fn authenticate_requires_app_credentials() {
        let c = LazadaVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "lzd_tok_123"
        );
        let missing =
            ConnectorConfig::new(ConnectorKind::LazadaVN, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "access_token": "t" }));
        assert!(matches!(
            c.authenticate(&missing).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn initial_sync_signs_request_and_emits_created() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(list_resp(&[order("700001", "2024-01-01T00:00:00Z")]));
        let c = LazadaVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "700001");
        let url = &transport.recorded()[0].url;
        assert!(url.contains("app_key=100001"));
        assert!(url.contains("sign_method=sha256"));
        assert!(url.contains("&sign="));
    }

    #[test]
    fn incremental_sync_dedups_boundary_but_surfaces_new_same_second() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(list_resp(&[
            order("700001", "2024-01-01T00:00:00Z"),
            order("700003", "2024-01-01T00:00:00Z"),
            order("700002", "2024-07-01T00:00:00Z"),
        ]));
        let c = LazadaVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00|700001".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["700003", "700002"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-07-01T00:00:00+00:00|700002")
        );
        assert!(transport.recorded()[0].url.contains("update_after="));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(MockResponse::status(401, b"bad".to_vec()));
        let c = LazadaVNConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(MockResponse::ok_json(
            serde_json::to_vec(
                &serde_json::json!({ "data": order("700001", "2024-01-01T00:00:00Z") }),
            )
            .unwrap(),
        ));
        let c = LazadaVNConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("700001"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Order 700001"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Status:** delivered"));
    }

    #[test]
    fn subscribe_webhook_uses_app_secret_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = LazadaVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/lazada")
            .unwrap();
        assert_eq!(sub.secret.expose(), "lazada-app-secret");
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn handle_webhook_event_maps_numeric_id() {
        let c = LazadaVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "message_type": 4, "data": { "trade_order_id": 700009 } });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].document_id().as_str(), "700009");
    }

    #[test]
    fn handle_webhook_event_missing_id_errors() {
        let c = LazadaVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "message_type": 4, "data": {} });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
