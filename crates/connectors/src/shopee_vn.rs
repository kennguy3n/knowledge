//! Shopee (Vietnam) connector — Shopee Open Platform API.
//!
//! Shopee is a leading Southeast-Asian marketplace. The Open Platform
//! API authenticates with an OAuth2 `access_token` *and* requires each
//! request to carry an HMAC-SHA256 `sign` computed over
//! `partner_id + path + timestamp + access_token + shop_id`, keyed by
//! the app `partner_key`. The auth material (`partner_id`,
//! `partner_key`, `shop_id`) is supplied in `auth_config_json`; the
//! `access_token` is either configured directly or obtained by
//! exchanging an authorization code through the injected
//! [`OAuth2CodeExchange`].
//!
//! * `initial_sync` walks `GET /api/v2/order/get_order_list`, paging
//!   via the 1-based `page` cursor until a short page is returned.
//! * `incremental_sync` adds an `update_time_from` filter built from
//!   the stored RFC 3339 watermark.
//! * `fetch_content` reads `GET /api/v2/order/get_order_detail` and
//!   renders a Markdown summary.
//! * `subscribe_webhook` is configuration-based — Shopee push is
//!   registered once in the App console, so no HTTP call is issued;
//!   the `partner_key` is surfaced so the substrate can validate the
//!   delivery signature.
//! * `handle_webhook_event` parses a push payload keyed by
//!   `ordersn`.

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

/// Default Shopee Open Platform API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://partner.shopeemobile.com";

/// Default order list page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured access token.
const DEFAULT_SCOPE: &str = "order.read";

/// Resolved Shopee auth material pulled from `auth_config_json`.
struct ShopeeAuth {
    partner_id: String,
    partner_key: String,
    shop_id: String,
}

/// One Shopee order (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopeeOrder {
    /// Order serial number.
    #[serde(rename = "ordersn", alias = "order_sn")]
    pub ordersn: String,
    /// Order status (`READY_TO_SHIP`, `COMPLETED`, …).
    #[serde(default, rename = "order_status")]
    pub order_status: Option<String>,
    /// Total amount in VND.
    #[serde(default, rename = "total_amount")]
    pub total_amount: Option<i64>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "update_time")]
    pub update_time: Option<DateTime<Utc>>,
}

/// Envelope for the order list endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopeeOrderListResponse {
    /// `response.order_list` is flattened here via the wrapper below.
    #[serde(default)]
    pub response: ShopeeOrderListBody,
}

/// Inner `response` body for the order list endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopeeOrderListBody {
    /// Orders on this page.
    #[serde(default)]
    pub order_list: Vec<ShopeeOrder>,
}

/// Envelope for the order detail endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopeeOrderDetailResponse {
    /// Inner detail body.
    #[serde(default)]
    pub response: ShopeeOrderDetailBody,
}

/// Inner `response` body for the order detail endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopeeOrderDetailBody {
    /// Returned order list (Shopee returns a single-element list).
    #[serde(default)]
    pub order_list: Vec<ShopeeOrder>,
}

/// Push webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopeeWebhookPayload {
    /// Push code (Shopee `code`; `3` = order status push).
    #[serde(default)]
    pub code: Option<i64>,
    /// Affected order serial number.
    #[serde(default)]
    pub data: ShopeeWebhookData,
}

/// Inner `data` block for a Shopee push.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShopeeWebhookData {
    /// Order serial number.
    #[serde(default)]
    pub ordersn: String,
    /// New status, when present.
    #[serde(default)]
    pub status: Option<String>,
}

/// Shopee (Vietnam) connector.
pub struct ShopeeVNConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ShopeeVNConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShopeeVNConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ShopeeVNConnector {
    /// Construct a Shopee connector.
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

    fn resolve_auth(config: &ConnectorConfig) -> Result<ShopeeAuth> {
        let get = |k: &str| {
            config
                .auth_config_json
                .get(k)
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string)
        };
        let partner_id = get("partner_id").ok_or_else(|| {
            ConnectorError::Auth("shopee_vn: auth_config_json.partner_id is required".into())
        })?;
        let partner_key = get("partner_key").ok_or_else(|| {
            ConnectorError::Auth("shopee_vn: auth_config_json.partner_key is required".into())
        })?;
        let shop_id = get("shop_id").ok_or_else(|| {
            ConnectorError::Auth("shopee_vn: auth_config_json.shop_id is required".into())
        })?;
        Ok(ShopeeAuth {
            partner_id,
            partner_key,
            shop_id,
        })
    }

    /// Build the HMAC-SHA256 request signature over the Shopee base
    /// string `partner_id + path + timestamp + access_token + shop_id`.
    fn sign_request(
        partner_key: &str,
        partner_id: &str,
        path: &str,
        timestamp: i64,
        access_token: &str,
        shop_id: &str,
    ) -> String {
        let base = format!("{partner_id}{path}{timestamp}{access_token}{shop_id}");
        hmac_sha256_hex(partner_key.as_bytes(), base.as_bytes())
    }

    /// Build the common signed query string shared by every call.
    fn signed_common(auth: &ShopeeAuth, path: &str, token: &OAuth2Token) -> String {
        let ts = Utc::now().timestamp();
        let access = token.access_token.expose();
        let sign = Self::sign_request(
            &auth.partner_key,
            &auth.partner_id,
            path,
            ts,
            access,
            &auth.shop_id,
        );
        format!(
            "partner_id={}&timestamp={ts}&access_token={}&shop_id={}&sign={sign}",
            auth.partner_id,
            percent_encode_path_component(access),
            auth.shop_id,
        )
    }

    fn api_get<R: DeserializeOwned>(&self, endpoint: &str, url: &str) -> Result<R> {
        let req = HttpRequest::get(url).with_header("Accept", "application/json");
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("shopee_vn", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "shopee_vn {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every order page until a short page is returned.
    fn paginate_orders(
        &self,
        base_url: &str,
        auth: &ShopeeAuth,
        token: &OAuth2Token,
        updated_from: Option<&str>,
    ) -> Result<Vec<ShopeeOrder>> {
        let path = "/api/v2/order/get_order_list";
        let mut out = Vec::<ShopeeOrder>::new();
        let filter = updated_from.map_or_else(String::new, |ts| {
            format!("&update_time_from={}", percent_encode_path_component(ts))
        });
        for page in 1..=MAX_LIST_PAGES {
            let common = Self::signed_common(auth, path, token);
            let url = format!(
                "{base_url}{path}?{common}&page={page}&page_size={}{filter}",
                self.page_size
            );
            let resp: ShopeeOrderListResponse = self.api_get(path, &url)?;
            let returned = resp.response.order_list.len();
            out.extend(resp.response.order_list);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "shopee_vn {path} exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn order_to_event(o: &ShopeeOrder, created: bool) -> ConnectorEvent {
    let occurred_at = o.update_time.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(o.ordersn.clone());
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

impl Connector for ShopeeVNConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        // The signing material must be present for any request.
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
                    "shopee_vn authenticate: auth_config_json.access_token or .authorization_code is required"
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
            if let Some(ts) = o.update_time {
                cursor.observe(ts, &o.ordersn);
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
            match o.update_time {
                Some(ts) => {
                    if !prior.should_emit(ts, &o.ordersn) {
                        continue;
                    }
                    events.push(order_to_event(o, false));
                    cursor.observe(ts, &o.ordersn);
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
        let path = "/api/v2/order/get_order_detail";
        let common = Self::signed_common(&auth, path, token);
        let ordersn = document_id.as_str();
        let url = format!(
            "{base_url}{path}?{common}&order_sn_list={}",
            percent_encode_path_component(ordersn)
        );
        let resp: ShopeeOrderDetailResponse = self.api_get(path, &url)?;
        let order = resp.response.order_list.into_iter().next().ok_or_else(|| {
            ConnectorError::Sync(format!(
                "shopee_vn order {ordersn} not found in detail response"
            ))
        })?;

        let title = format!("Order {}", order.ordersn);
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(status) = order.order_status.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Status:** {status}\n");
        }
        if let Some(total) = order.total_amount {
            let _ = writeln!(md, "**Total:** {total} VND\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "shopee_vn",
                "ordersn": order.ordersn,
                "order_status": order.order_status,
                "total_amount": order.total_amount,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Shopee push is registered once in the App console (no
        // per-shop create-webhook endpoint), so no HTTP call is issued
        // here. Surface the partner_key so the substrate can validate
        // the delivery signature.
        let _ = token;
        let auth = Self::resolve_auth(config)?;
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(auth.partner_key),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: ShopeeWebhookPayload = serde_json::from_slice(body)?;
        if payload.data.ordersn.is_empty() {
            return Err(ConnectorError::Webhook(
                "shopee_vn push payload missing data.ordersn".into(),
            ));
        }
        Ok(vec![ConnectorEvent::DocumentUpdated {
            document_id: SourceDocumentId::new(payload.data.ordersn),
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
                "shopee-access",
                "shopee-refresh",
                Utc::now() + Duration::hours(1),
                "order.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::ShopeeVN, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "shopee_tok_123",
                "partner_id": "20001",
                "partner_key": "shopee-partner-key",
                "shop_id": "9001",
                "api_base_url": "https://api.test/shopee",
            }))
    }

    fn order(sn: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "ordersn": sn,
            "order_status": "COMPLETED",
            "total_amount": 199_000,
            "update_time": updated,
        })
    }

    fn list_resp(orders: &[serde_json::Value]) -> MockResponse {
        MockResponse::ok_json(
            serde_json::to_vec(&serde_json::json!({ "response": { "order_list": orders } }))
                .unwrap(),
        )
    }

    #[test]
    fn sign_request_is_deterministic_and_keyed() {
        let a = ShopeeVNConnector::sign_request("key", "20001", "/p", 1_700_000_000, "tok", "9001");
        let b = ShopeeVNConnector::sign_request("key", "20001", "/p", 1_700_000_000, "tok", "9001");
        let c =
            ShopeeVNConnector::sign_request("key2", "20001", "/p", 1_700_000_000, "tok", "9001");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn authenticate_requires_signing_material() {
        let c = ShopeeVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "shopee_tok_123"
        );
        let missing =
            ConnectorConfig::new(ConnectorKind::ShopeeVN, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "access_token": "t" }));
        assert!(matches!(
            c.authenticate(&missing).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn authenticate_falls_back_to_oauth_exchange() {
        let c = ShopeeVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let cfg =
            ConnectorConfig::new(ConnectorKind::ShopeeVN, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "abc",
                    "partner_id": "20001",
                    "partner_key": "k",
                    "shop_id": "9001",
                }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "shopee-access"
        );
    }

    #[test]
    fn initial_sync_signs_request_and_emits_created() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(list_resp(&[order("SN1", "2024-01-01T00:00:00Z")]));
        let c = ShopeeVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        let url = &transport.recorded()[0].url;
        assert!(url.contains("partner_id=20001"));
        assert!(url.contains("&sign="));
        assert!(url.contains("access_token=shopee_tok_123"));
    }

    #[test]
    fn incremental_sync_dedups_boundary_but_surfaces_new_same_second() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(list_resp(&[
            order("SN1", "2024-01-01T00:00:00Z"),
            order("SN3", "2024-01-01T00:00:00Z"),
            order("SN2", "2024-06-01T00:00:00Z"),
        ]));
        let c = ShopeeVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00|SN1".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["SN3", "SN2"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|SN2")
        );
        assert!(transport.recorded()[0].url.contains("update_time_from="));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.with_default_response(MockResponse::status(401, b"bad".to_vec()));
        let c = ShopeeVNConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            serde_json::to_vec(&serde_json::json!({
                "response": { "order_list": [order("SN1", "2024-01-01T00:00:00Z")] }
            }))
            .unwrap(),
        ));
        let c = ShopeeVNConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("SN1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Order SN1"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Status:** COMPLETED"));
    }

    #[test]
    fn subscribe_webhook_uses_partner_key_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ShopeeVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/shopee")
            .unwrap();
        assert_eq!(sub.secret.expose(), "shopee-partner-key");
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn handle_webhook_event_maps_to_updated() {
        let c = ShopeeVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body =
            serde_json::json!({ "code": 3, "data": { "ordersn": "SN9", "status": "SHIPPED" } });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].document_id().as_str(), "SN9");
    }

    #[test]
    fn handle_webhook_event_missing_ordersn_errors() {
        let c = ShopeeVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "code": 3, "data": { "status": "SHIPPED" } });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
