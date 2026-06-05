//! MoMo connector — MoMo Business API (transactions + analytics).
//!
//! MoMo is Vietnam's leading e-wallet. The Business API exposes a
//! merchant's settled transaction records behind an OAuth2 bearer
//! credential.
//!
//! * `initial_sync` walks `GET /v2/business/transactions`, paging via
//!   the 1-based `page` cursor until a short page is returned.
//! * `incremental_sync` adds a `fromUpdatedAt` filter built from the
//!   stored RFC 3339 watermark.
//! * `fetch_content` reads `GET /v2/business/transactions/{id}` and
//!   renders a Markdown receipt.
//! * `subscribe_webhook` surfaces the operator-provided IPN secret —
//!   MoMo notify endpoints are registered once in the Business portal
//!   (no create-webhook REST endpoint), so no HTTP call is issued; the
//!   secret lets the substrate validate the delivery `signature`.
//! * `handle_webhook_event` parses an IPN payload keyed by `orderId`.
//!
//! MoMo uses a standard bearer `Authorization` header, so the
//! connector routes JSON reads through the framework's bearer helpers.
//! `authenticate` accepts a configured `access_token` or exchanges an
//! OAuth2 `authorization_code` through the injected
//! [`OAuth2CodeExchange`].

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default MoMo Business API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://business.momo.vn/api";

/// Default transactions page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured access token.
const DEFAULT_SCOPE: &str = "business.transactions.read";

/// One MoMo Business transaction (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MoMoTransaction {
    /// Merchant order id.
    #[serde(rename = "orderId", alias = "order_id")]
    pub order_id: String,
    /// MoMo transaction id.
    #[serde(default, rename = "transId")]
    pub trans_id: Option<String>,
    /// Amount in VND.
    #[serde(default)]
    pub amount: Option<i64>,
    /// Result code (`0` = success).
    #[serde(default, rename = "resultCode")]
    pub result_code: Option<i64>,
    /// Order description.
    #[serde(default, rename = "orderInfo")]
    pub order_info: Option<String>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "updatedAt")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Envelope for the transaction list endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MoMoTransactionsResponse {
    /// Transactions on this page.
    #[serde(default, alias = "transactions")]
    pub data: Vec<MoMoTransaction>,
}

/// IPN webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MoMoIpnPayload {
    /// Merchant order id.
    #[serde(default, rename = "orderId")]
    pub order_id: String,
    /// Result code (`0` = success).
    #[serde(default, rename = "resultCode")]
    pub result_code: Option<i64>,
    /// MoMo transaction id.
    #[serde(default, rename = "transId")]
    pub trans_id: Option<String>,
}

/// MoMo Business connector.
pub struct MoMoConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for MoMoConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoMoConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl MoMoConnector {
    /// Construct a MoMo connector.
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

    /// Override the Business API base URL.
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

    /// Walk every transaction page until a short page is returned.
    fn paginate_transactions(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_from: Option<&str>,
    ) -> Result<Vec<MoMoTransaction>> {
        let mut out = Vec::<MoMoTransaction>::new();
        let filter = updated_from.map_or_else(String::new, |ts| {
            format!("&fromUpdatedAt={}", percent_encode_path_component(ts))
        });
        for page in 1..=MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/v2/business/transactions?page={page}&limit={}{filter}",
                self.page_size
            );
            let resp: MoMoTransactionsResponse = bearer_get_json(
                &self.transport,
                "momo",
                "/v2/business/transactions",
                &url,
                token,
                &[],
            )?;
            let returned = resp.data.len();
            out.extend(resp.data);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "momo /v2/business/transactions exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn txn_to_event(t: &MoMoTransaction, created: bool) -> ConnectorEvent {
    let occurred_at = t.updated_at.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(t.order_id.clone());
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

impl Connector for MoMoConnector {
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
                    "momo authenticate: auth_config_json.access_token or .authorization_code is required"
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
        let url = format!("{base_url}/v2/business/transactions/{id_enc}");
        let txn: MoMoTransaction = bearer_get_json(
            &self.transport,
            "momo",
            "/v2/business/transactions/{id}",
            &url,
            token,
            &[],
        )?;

        let title = format!("Transaction {}", txn.order_id);
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(trans_id) = txn.trans_id.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**MoMo transId:** {trans_id}\n");
        }
        if let Some(amount) = txn.amount {
            let _ = writeln!(md, "**Amount:** {amount} VND\n");
        }
        if let Some(code) = txn.result_code {
            let _ = writeln!(md, "**Result code:** {code}\n");
        }
        if let Some(info) = txn.order_info.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Order info:** {info}\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "momo",
                "order_id": txn.order_id,
                "trans_id": txn.trans_id,
                "result_code": txn.result_code,
                "amount": txn.amount,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // MoMo notify (IPN) endpoints are registered once in the
        // Business portal (no create-webhook REST endpoint), so we do
        // not issue an HTTP call here. Surface the IPN secret so the
        // substrate can validate the delivery `signature`.
        let _ = token;
        let secret = config
            .auth_config_json
            .get("ipn_secret")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "momo subscribe_webhook: auth_config_json.ipn_secret is required".into(),
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
        let payload: MoMoIpnPayload = serde_json::from_slice(body)?;
        if payload.order_id.is_empty() {
            return Err(ConnectorError::Webhook(
                "momo IPN payload missing orderId".into(),
            ));
        }
        Ok(vec![ConnectorEvent::DocumentUpdated {
            document_id: SourceDocumentId::new(payload.order_id),
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
                "momo-access",
                "momo-refresh",
                Utc::now() + Duration::hours(1),
                "business.transactions.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::MoMo, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "momo_tok_123",
                "api_base_url": "https://api.test/momo",
                "ipn_secret": "momo-ipn-secret",
            }))
    }

    fn txn(id: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "orderId": id,
            "transId": format!("MM{id}"),
            "amount": 50_000,
            "resultCode": 0,
            "orderInfo": format!("Order {id}"),
            "updatedAt": updated,
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let c = MoMoConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "momo_tok_123");
    }

    #[test]
    fn authenticate_falls_back_to_oauth_exchange() {
        let c = MoMoConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let cfg = ConnectorConfig::new(ConnectorKind::MoMo, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "authorization_code": "abc" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert_eq!(tok.access_token.expose(), "momo-access");
    }

    #[test]
    fn initial_sync_emits_created_with_bearer() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/momo/v2/business/transactions?page=1&limit=50",
            ok_json(&serde_json::json!({ "data": [txn("O1", "2024-01-01T00:00:00Z")] })),
        );
        let c = MoMoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(transport.recorded()[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer momo_tok_123"));
    }

    #[test]
    fn incremental_sync_applies_filter_and_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/momo/v2/business/transactions?page=1&limit=50&fromUpdatedAt=2024-01-01T00%3A00%3A00%2B00%3A00",
            ok_json(&serde_json::json!({ "data": [
                txn("O1", "2024-01-01T00:00:00Z"),
                txn("O2", "2024-03-01T00:00:00Z"),
            ] })),
        );
        let c = MoMoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "O2");
    }

    #[test]
    fn initial_sync_maps_403_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/momo/v2/business/transactions?page=1&limit=50",
            MockResponse::status(403, b"forbidden".to_vec()),
        );
        let c = MoMoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            "https://api.test/momo/v2/business/transactions/O1",
            ok_json(&txn("O1", "2024-01-01T00:00:00Z")),
        );
        let c = MoMoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("O1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Transaction O1"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Result code:** 0"));
    }

    #[test]
    fn subscribe_webhook_uses_ipn_secret_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MoMoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/momo")
            .unwrap();
        assert_eq!(sub.secret.expose(), "momo-ipn-secret");
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn handle_webhook_event_maps_to_updated() {
        let c = MoMoConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "orderId": "O9", "resultCode": 0 });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].document_id().as_str(), "O9");
    }

    #[test]
    fn handle_webhook_event_missing_order_id_errors() {
        let c = MoMoConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "resultCode": 0 });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
