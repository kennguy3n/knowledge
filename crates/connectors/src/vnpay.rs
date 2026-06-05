//! VNPay connector — VNPay merchant transaction API + IPN webhook.
//!
//! VNPay is Vietnam's #1 payment gateway. The merchant API exposes
//! settled transaction history and merchant metadata behind an
//! API-key credential; settlement notifications arrive on the IPN
//! (Instant Payment Notification) callback configured in the merchant
//! portal.
//!
//! * `initial_sync` walks `GET /merchant/v1/transactions`, paging via
//!   the 1-based `page` cursor until a short page is returned.
//! * `incremental_sync` adds an `updatedFrom` filter built from the
//!   stored RFC 3339 watermark so only transactions settled since the
//!   last run are re-emitted.
//! * `fetch_content` reads `GET /merchant/v1/transactions/{id}` and
//!   renders a Markdown receipt.
//! * `subscribe_webhook` surfaces the operator-provided IPN
//!   `hash_secret` — VNPay IPN endpoints are registered once in the
//!   merchant portal (no create-webhook REST endpoint), so no HTTP
//!   call is issued; the secret lets the substrate validate the
//!   `vnp_SecureHash` on delivery.
//! * `handle_webhook_event` parses an IPN payload keyed by
//!   `vnp_TxnRef`.
//!
//! VNPay authenticates merchant API calls with an `X-Api-Key` header
//! when a static API key is configured, falling back to the injected
//! [`OAuth2CodeExchange`] when only an authorization-code grant is
//! available. Requests pick their auth header from the token's
//! provenance (recorded in [`OAuth2Token::token_type`], following the
//! same convention as the Discord connector): a static API key is
//! sent in the provider-native `X-Api-Key` header, while an
//! OAuth-issued access token is sent as `Authorization: Bearer`.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    apply_auth_by_provenance, classify_failure, percent_encode_path_component, Connector,
    ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent,
    HttpRequest, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default VNPay merchant API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://merchant.vnpay.vn/api";

/// Default transactions page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "merchant.transactions.read";
/// `OAuth2Token::token_type` marker for a static API-key credential.
/// Distinguishes the API-key auth path (provider-native `X-Api-Key`
/// header) from an OAuth-issued bearer token.
pub const API_KEY_TOKEN_TYPE: &str = "ApiKey";

/// One VNPay merchant transaction (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VNPayTransaction {
    /// Transaction reference (`vnp_TxnRef`).
    #[serde(rename = "txnRef", alias = "vnp_TxnRef")]
    pub txn_ref: String,
    /// Amount in minor units (VND has no minor unit; VNPay multiplies
    /// by 100 on the wire).
    #[serde(default)]
    pub amount: Option<i64>,
    /// Response/settlement code (`00` = success).
    #[serde(default, rename = "responseCode")]
    pub response_code: Option<String>,
    /// Order description.
    #[serde(default, rename = "orderInfo")]
    pub order_info: Option<String>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "updatedAt")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Envelope for the transaction list endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VNPayTransactionsResponse {
    /// Transactions on this page.
    #[serde(default, alias = "transactions")]
    pub data: Vec<VNPayTransaction>,
}

/// IPN (Instant Payment Notification) webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VNPayIpnPayload {
    /// Transaction reference.
    #[serde(default, rename = "vnp_TxnRef", alias = "txnRef")]
    pub txn_ref: String,
    /// Response code (`00` = success).
    #[serde(default, rename = "vnp_ResponseCode")]
    pub response_code: Option<String>,
    /// Pay date in `yyyyMMddHHmmss` (VNPay local time).
    #[serde(default, rename = "vnp_PayDate")]
    pub pay_date: Option<String>,
}

/// VNPay merchant connector.
pub struct VNPayConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for VNPayConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VNPayConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl VNPayConnector {
    /// Construct a VNPay connector.
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

    /// Override the merchant API base URL.
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

    /// GET a JSON endpoint with the `X-Api-Key` header and parse it.
    fn api_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = apply_auth(
            HttpRequest::get(url).with_header("Accept", "application/json"),
            token,
        );
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("vnpay", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "vnpay {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every transaction page until a short page is returned.
    /// `updated_from` is an optional RFC 3339 lower bound applied
    /// server-side for incremental runs.
    fn paginate_transactions(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_from: Option<&str>,
    ) -> Result<Vec<VNPayTransaction>> {
        let mut out = Vec::<VNPayTransaction>::new();
        let filter = updated_from.map_or_else(String::new, |ts| {
            format!("&updatedFrom={}", percent_encode_path_component(ts))
        });
        for page in 1..=MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/merchant/v1/transactions?page={page}&limit={}{filter}",
                self.page_size
            );
            let resp: VNPayTransactionsResponse =
                self.api_get("/merchant/v1/transactions", &url, token)?;
            let returned = resp.data.len();
            out.extend(resp.data);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "vnpay /merchant/v1/transactions exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

/// Attach the auth header matching the token's provenance: a static
/// API-key token (tagged [`API_KEY_TOKEN_TYPE`] in `authenticate`)
/// goes in the provider-native `X-Api-Key` header, while an
/// OAuth-issued token is sent as `Authorization: <scheme> <token>`
/// (scheme from `token_type`, defaulting to `Bearer`).
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    apply_auth_by_provenance(req, token, "X-Api-Key", API_KEY_TOKEN_TYPE)
}

fn txn_to_event(t: &VNPayTransaction, created: bool) -> ConnectorEvent {
    let occurred_at = t.updated_at.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(t.txn_ref.clone());
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

impl Connector for VNPayConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("api_key")
            .or_else(|| config.auth_config_json.get("access_token"))
            .and_then(serde_json::Value::as_str)
        {
            let mut token = OAuth2Token::new_without_refresh(
                key,
                Utc::now() + chrono::Duration::days(3650),
                DEFAULT_SCOPE,
            );
            token.token_type = API_KEY_TOKEN_TYPE.to_string();
            return Ok(token);
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "vnpay authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let txns = self.paginate_transactions(&base_url, token, None)?;
        let mut events = Vec::with_capacity(txns.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for t in &txns {
            events.push(txn_to_event(t, true));
            if let Some(ts) = t.updated_at {
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
        let txns = self.paginate_transactions(&base_url, token, state.cursor.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for t in &txns {
            // Guard the boundary: the server-side filter is inclusive,
            // so drop anything at or before the prior watermark.
            if let (Some(prev), Some(ts)) = (prior, t.updated_at) {
                if ts <= prev {
                    continue;
                }
            }
            events.push(txn_to_event(t, false));
            if let Some(ts) = t.updated_at {
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
        let url = format!("{base_url}/merchant/v1/transactions/{id_enc}");
        let txn: VNPayTransaction = self.api_get("/merchant/v1/transactions/{id}", &url, token)?;

        let title = format!("Transaction {}", txn.txn_ref);
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(amount) = txn.amount {
            let _ = writeln!(md, "**Amount:** {amount}\n");
        }
        if let Some(code) = txn.response_code.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Response code:** {code}\n");
        }
        if let Some(info) = txn.order_info.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Order info:** {info}\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "vnpay",
                "txn_ref": txn.txn_ref,
                "response_code": txn.response_code,
                "amount": txn.amount,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // VNPay IPN endpoints are registered once in the merchant
        // portal (there is no create-webhook REST endpoint), so we do
        // not issue an HTTP call here. Surface the IPN hash secret so
        // the substrate can validate the `vnp_SecureHash` on delivery.
        let _ = token;
        let secret = config
            .auth_config_json
            .get("hash_secret")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "vnpay subscribe_webhook: auth_config_json.hash_secret is required".into(),
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
        let payload: VNPayIpnPayload = serde_json::from_slice(body)?;
        if payload.txn_ref.is_empty() {
            return Err(ConnectorError::Webhook(
                "vnpay IPN payload missing vnp_TxnRef".into(),
            ));
        }
        Ok(vec![ConnectorEvent::DocumentUpdated {
            document_id: SourceDocumentId::new(payload.txn_ref),
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
        ConnectorConfig::new(ConnectorKind::VNPay, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "vnp_key_123",
                "api_base_url": "https://api.test/vnpay",
                "hash_secret": "vnp-hash-secret",
            }))
    }

    fn txn(id: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "txnRef": id,
            "amount": 100_000,
            "responseCode": "00",
            "orderInfo": format!("Order {id}"),
            "updatedAt": updated,
        })
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::VNPay, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "auth-code",
                "api_base_url": "https://api.test/vnpay",
                "hash_secret": "vnp-hash-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_api_key() {
        let c = VNPayConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "vnp_key_123");
        assert_eq!(tok.token_type, API_KEY_TOKEN_TYPE);
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let c = VNPayConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let tok = c.authenticate(&cfg_oauth()).unwrap();
        // OAuth-issued token keeps the bearer token_type, not the
        // API-key marker, so requests use `Authorization: Bearer`.
        assert_eq!(tok.access_token.expose(), "unused");
        assert_eq!(tok.token_type, "Bearer");
    }

    #[test]
    fn oauth_token_is_sent_as_bearer_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vnpay/merchant/v1/transactions?page=1&limit=50",
            ok_json(&serde_json::json!({ "data": [txn("T1", "2024-01-01T00:00:00Z")] })),
        );
        let c = VNPayConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg_oauth()).unwrap();
        let res = c.initial_sync(&cfg_oauth(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        let recorded = transport.recorded();
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer unused"));
        assert!(!recorded[0]
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-api-key")));
    }

    #[test]
    fn authenticate_requires_api_key() {
        let c = VNPayConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let bare = ConnectorConfig::new(ConnectorKind::VNPay, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn initial_sync_emits_created_with_api_key_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vnpay/merchant/v1/transactions?page=1&limit=50",
            ok_json(&serde_json::json!({ "data": [txn("T1", "2024-01-01T00:00:00Z")] })),
        );
        let c = VNPayConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-01T00:00:00+00:00")
        );
        assert!(transport.recorded()[0]
            .headers
            .iter()
            .any(|(k, v)| k == "X-Api-Key" && v == "vnp_key_123"));
    }

    #[test]
    fn incremental_sync_applies_filter_and_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vnpay/merchant/v1/transactions?page=1&limit=50&updatedFrom=2024-01-01T00%3A00%3A00%2B00%3A00",
            ok_json(&serde_json::json!({ "data": [
                txn("T1", "2024-01-01T00:00:00Z"),
                txn("T2", "2024-02-01T00:00:00Z"),
            ] })),
        );
        let c = VNPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "T2");
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vnpay/merchant/v1/transactions?page=1&limit=50",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = VNPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn fetch_content_renders_receipt() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vnpay/merchant/v1/transactions/T1",
            ok_json(&txn("T1", "2024-01-01T00:00:00Z")),
        );
        let c = VNPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("T1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Transaction T1"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Response code:** 00"));
    }

    #[test]
    fn subscribe_webhook_uses_hash_secret_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = VNPayConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/vnpay")
            .unwrap();
        assert_eq!(sub.secret.expose(), "vnp-hash-secret");
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn handle_webhook_event_maps_ipn_to_updated() {
        let c = VNPayConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "vnp_TxnRef": "T9", "vnp_ResponseCode": "00" });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "T9");
    }

    #[test]
    fn handle_webhook_event_missing_ref_errors() {
        let c = VNPayConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "vnp_ResponseCode": "00" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
