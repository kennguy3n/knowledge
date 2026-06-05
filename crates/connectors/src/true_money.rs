//! TrueMoney connector — TrueMoney Business API
//! (`https://api.truemoney.com`).
//!
//! TrueMoney is a Thai e-wallet operated by the CP Group. The
//! business API authenticates with an API key **plus** a per-request
//! HMAC-SHA256 signature over `METHOD\nURL\nTIMESTAMP` keyed by the
//! merchant secret, carried in the `X-API-Key`, `X-Timestamp` and
//! `X-Signature` headers. [`TrueMoneyConnector::authenticate`] reads
//! the API key out of `auth_config_json` (falling back to the
//! injected [`OAuth2CodeExchange`]); the signing secret is read per
//! request.
//!
//! * `initial_sync` / `incremental_sync` page `/v1/transactions`
//!   (`limit` / `offset`), tracking the maximum `created_at` as an
//!   RFC-3339 watermark; incremental runs add `since` and dedup the
//!   inclusive boundary row.
//! * `fetch_content` GETs a single transaction
//!   (`/v1/transactions/{id}`).
//! * TrueMoney webhooks are configured in the merchant console, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payload.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Default TrueMoney Business API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.truemoney.com";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "transactions";
/// Page size for transaction listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct TrueMoneyTransactionsPage {
    #[serde(default)]
    transactions: Vec<TrueMoneyTransaction>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrueMoneyTransaction {
    #[serde(default)]
    transaction_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    amount: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TrueMoneyWebhookEvent {
    #[serde(default)]
    transaction_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Hex-encode a byte slice (lowercase).
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// TrueMoney Business connector.
pub struct TrueMoneyConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for TrueMoneyConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrueMoneyConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl TrueMoneyConnector {
    /// Construct a TrueMoney connector.
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

    /// Override the TrueMoney API base URL.
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

    fn signing_secret(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("signing_secret")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "true_money: auth_config_json.signing_secret is required for HMAC signing"
                        .into(),
                )
            })
    }

    /// Compute the lowercase-hex HMAC-SHA256 signature over
    /// `METHOD\nURL\nTIMESTAMP`.
    fn sign(secret: &str, method: &str, url: &str, timestamp: i64) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .expect("HMAC-SHA256 accepts a key of any length");
        mac.update(format!("{method}\n{url}\n{timestamp}").as_bytes());
        to_hex(&mac.finalize().into_bytes())
    }

    fn signed_get<R: DeserializeOwned>(
        &self,
        config: &ConnectorConfig,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let secret = Self::signing_secret(config)?;
        let timestamp = Utc::now().timestamp();
        let signature = Self::sign(&secret, "GET", url, timestamp);
        let req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("X-API-Key", token.access_token.expose())
            .with_header("X-Timestamp", timestamp.to_string())
            .with_header("X-Signature", signature);
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("true_money", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "true_money {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    fn paginate_transactions(
        &self,
        config: &ConnectorConfig,
        base_url: &str,
        token: &OAuth2Token,
        since: Option<&str>,
    ) -> Result<Vec<TrueMoneyTransaction>> {
        let mut txns = Vec::<TrueMoneyTransaction>::new();
        for page in 0..MAX_PAGES {
            let offset = page * self.page_size as usize;
            let mut url = format!(
                "{base_url}/v1/transactions?limit={}&offset={offset}",
                self.page_size
            );
            if let Some(s) = since {
                url.push_str("&since=");
                url.push_str(&percent_encode_path_component(s));
            }
            let resp: TrueMoneyTransactionsPage =
                self.signed_get(config, "/v1/transactions", &url, token)?;
            let count = resp.transactions.len();
            txns.extend(resp.transactions);
            if count < self.page_size as usize {
                return Ok(txns);
            }
        }
        Err(ConnectorError::Sync(format!(
            "true_money /v1/transactions exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn txn_watermark(t: &TrueMoneyTransaction) -> Option<DateTime<Utc>> {
    t.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| t.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for TrueMoneyConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(api_key) = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                api_key,
                Utc::now() + chrono::Duration::days(365),
                DEFAULT_SCOPE,
            ));
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "true_money authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let txns = self.paginate_transactions(config, &base_url, token, None)?;
        let mut events = Vec::with_capacity(txns.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for txn in &txns {
            let occurred_at = txn_watermark(txn).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(txn.transaction_id.clone()),
                occurred_at,
            });
            if let Some(t) = txn_watermark(txn) {
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
        let txns = self.paginate_transactions(config, &base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for txn in &txns {
            let Some(updated) = txn_watermark(txn) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(txn.transaction_id.clone()),
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
        let url = format!("{base_url}/v1/transactions/{id_enc}");
        let txn: TrueMoneyTransaction =
            self.signed_get(config, "/v1/transactions/{id}", &url, token)?;
        let status = txn.status.as_deref().unwrap_or("unknown");
        let amount = txn.amount.unwrap_or(0.0);
        let currency = txn.currency.as_deref().unwrap_or("THB");
        let body = format!(
            "# TrueMoney transaction {id}\n\nStatus: {status}\nAmount: {amount} {currency}\n"
        );
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("TrueMoney transaction {id}"))
            .with_metadata(serde_json::json!({
                "provider": "true_money",
                "transaction_id": txn.transaction_id,
                "status": txn.status,
                "created_at": txn.created_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // TrueMoney webhooks are configured in the merchant console;
        // record a polling-only subscription so the runtime falls
        // back to incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("truemoney-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<TrueMoneyWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<TrueMoneyWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<TrueMoneyWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("true_money webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty true_money webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.transaction_id).ok_or_else(|| {
                ConnectorError::Webhook("true_money webhook event missing transaction_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            // Payment transactions are immutable ledger entries; a
            // refund/void is modelled as an update rather than a
            // delete (the original record is never removed).
            let event = if delivery.event.contains("create") || delivery.event.contains("success") {
                ConnectorEvent::DocumentCreated {
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
                "unused",
                "unused",
                Utc::now() + Duration::hours(1),
                "transactions",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::TrueMoney,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "tm-key",
            "signing_secret": "tm-secret",
            "api_base_url": "https://api.test/tm",
            "webhook_secret": "tm-webhook-secret",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrueMoneyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "tm-key");
    }

    #[test]
    fn sign_is_deterministic_and_hex() {
        let sig = TrueMoneyConnector::sign(
            "secret",
            "GET",
            "https://api.test/tm/v1/transactions",
            1_700_000_000,
        );
        // 32-byte HMAC-SHA256 → 64 lowercase hex chars.
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        let again = TrueMoneyConnector::sign(
            "secret",
            "GET",
            "https://api.test/tm/v1/transactions",
            1_700_000_000,
        );
        assert_eq!(sig, again);
    }

    #[test]
    fn initial_sync_signs_and_paginates() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/tm/v1/transactions?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "transactions": [
                    {"transaction_id": "t-1", "created_at": "2024-01-01T00:00:00Z"},
                    {"transaction_id": "t-2", "created_at": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/tm/v1/transactions?limit=2&offset=2",
            ok_json(&serde_json::json!({ "transactions": [ {"transaction_id": "t-3", "created_at": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = TrueMoneyConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00")
        );
        let recorded = transport.recorded();
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-api-key") && v == "tm-key"));
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-signature")));
    }

    #[test]
    fn signed_get_requires_signing_secret() {
        // signing_secret is validated before any HTTP call, so the
        // transport is never exercised here.
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrueMoneyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let no_secret = ConnectorConfig::new(
            ConnectorKind::TrueMoney,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "tm-key",
            "api_base_url": "https://api.test/tm",
        }));
        let tok = c.authenticate(&no_secret).unwrap();
        assert!(matches!(
            c.initial_sync(&no_secret, &tok),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/tm/v1/transactions?limit=2&offset=0&since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "transactions": [
                    {"transaction_id": "t-10", "created_at": "2024-03-01T00:00:00Z"},
                    {"transaction_id": "t-11", "created_at": "2024-06-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/tm/v1/transactions?limit=2&offset=2&since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "transactions": [] })),
        );
        let c = TrueMoneyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
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
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/tm/v1/transactions/t-1",
            ok_json(&serde_json::json!({
                "transaction_id": "t-1",
                "status": "SUCCESS",
                "amount": 250.0,
                "currency": "THB"
            })),
        );
        let c = TrueMoneyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("t-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# TrueMoney transaction t-1"));
        assert!(body.contains("SUCCESS"));
    }

    #[test]
    fn handle_webhook_event_treats_refund_as_update() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TrueMoneyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!([
            { "transaction_id": "t-1", "event": "payment.success" },
            { "transaction_id": "t-2", "event": "payment.refunded" }
        ]))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(events[1], ConnectorEvent::DocumentUpdated { .. }));
    }
}
