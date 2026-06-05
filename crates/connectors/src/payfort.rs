//! PayFort connector — Amazon Payment Services / PayFort.
//!
//! * `initial_sync` pages `GET /api/v1/transactions?per_page=100&page=N`,
//!   stopping on a short page.
//! * `incremental_sync` adds the `updated_since` filter keyed off the
//!   stored RFC-3339 watermark; the filter is inclusive, so the
//!   boundary row is deduped client-side.
//! * `fetch_content` GETs `/api/v1/transactions/{id}` and renders a
//!   Markdown summary.
//! * Amazon Payment Services delivers transaction feedback to a single
//!   merchant-configured URL set in the back office; there is no API to
//!   register webhooks, so `subscribe_webhook` records a polling-only
//!   subscription with no provider id.
//! * `handle_webhook_event` parses the back-office notification payload
//!   (single object or batched array).
//!
//! PayFort authenticates with a merchant `access_code` plus a SHA-256
//! signature computed over the SHA-request-phrase-wrapped canonical
//! string (`{phrase}GET{path}{timestamp}{phrase}`), mirroring the
//! signature scheme documented for its REST endpoints. The signature,
//! access code and timestamp ride in `X-Payfort-*` headers, so the
//! connector issues requests through the injected [`HttpTransport`]
//! directly.

use crate::signing::sha256_hex;
use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Default Amazon Payment Services API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://paymentservices.payfort.com";

/// Page size for transaction listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// Scope recorded on a token synthesised from a configured access code.
const DEFAULT_SCOPE: &str = "transactions.read";

/// One PayFort transaction (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PayfortTransaction {
    /// Transaction id (Fort id).
    #[serde(default)]
    pub id: String,
    /// Merchant reference.
    #[serde(default)]
    pub merchant_reference: Option<String>,
    /// Transaction amount (minor units, string-encoded).
    #[serde(default)]
    pub amount: Option<String>,
    /// ISO-4217 currency code.
    #[serde(default)]
    pub currency: Option<String>,
    /// Transaction status text.
    #[serde(default)]
    pub status: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// PayFort transaction list response (`{ "transactions": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PayfortTransactionsResponse {
    /// Page of transactions.
    #[serde(default)]
    pub transactions: Vec<PayfortTransaction>,
}

/// PayFort single-transaction response (`{ "transaction": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PayfortTransactionResponse {
    /// The transaction.
    #[serde(default)]
    pub transaction: PayfortTransaction,
}

/// PayFort webhook / back-office notification payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PayfortWebhookEvent {
    /// Fort transaction id (string or number).
    #[serde(default)]
    pub fort_id: serde_json::Value,
    /// Status carried with the notification.
    #[serde(default)]
    pub status: String,
}

/// PayFort connector.
pub struct PayfortConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for PayfortConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayfortConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .finish()
    }
}

impl PayfortConnector {
    /// Construct a PayFort connector.
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

    /// Override the PayFort base URL.
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

    fn sha_request_phrase(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("sha_request_phrase")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "payfort: auth_config_json.sha_request_phrase is required".into(),
                )
            })
    }

    /// GET a signed JSON endpoint. `path_and_query` is bound into the
    /// SHA-256 signature (wrapped by the SHA request phrase) so a
    /// tampered path invalidates the signature.
    fn signed_get<R: DeserializeOwned>(
        &self,
        base_url: &str,
        path_and_query: &str,
        access_code: &str,
        sha_request_phrase: &str,
        endpoint: &str,
    ) -> Result<R> {
        let timestamp = Utc::now().timestamp().to_string();
        let canonical =
            format!("{sha_request_phrase}GET{path_and_query}{timestamp}{sha_request_phrase}");
        let signature = sha256_hex(canonical.as_bytes());
        let url = format!("{base_url}{path_and_query}");
        let req = HttpRequest::get(&url)
            .with_header("Accept", "application/json")
            .with_header("X-Payfort-Access-Code", access_code)
            .with_header("X-Payfort-Timestamp", &timestamp)
            .with_header("X-Payfort-Signature", &signature);
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("payfort", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "payfort {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk the transaction list page-by-page, stopping on a short page.
    fn paginate_transactions(
        &self,
        base_url: &str,
        access_code: &str,
        sha_request_phrase: &str,
        updated_since: Option<&str>,
    ) -> Result<Vec<PayfortTransaction>> {
        let mut out = Vec::<PayfortTransaction>::new();
        for page in 1..=MAX_PAGES {
            let mut path = format!(
                "/api/v1/transactions?per_page={}&page={page}",
                self.page_size
            );
            if let Some(since) = updated_since {
                path.push_str("&updated_since=");
                path.push_str(&percent_encode_path_component(since));
            }
            let resp: PayfortTransactionsResponse = self.signed_get(
                base_url,
                &path,
                access_code,
                sha_request_phrase,
                "/api/v1/transactions",
            )?;
            let count = resp.transactions.len();
            out.extend(resp.transactions);
            if count < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "payfort /api/v1/transactions exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn transaction_watermark(t: &PayfortTransaction) -> Option<DateTime<Utc>> {
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

impl Connector for PayfortConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let access_code = config
            .auth_config_json
            .get("access_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "payfort authenticate: auth_config_json.access_code is required".into(),
                )
            })?;
        // Require the SHA request phrase up front so a misconfigured
        // connector fails at authenticate rather than mid-sync.
        Self::sha_request_phrase(config)?;
        Ok(OAuth2Token::new_without_refresh(
            access_code,
            Utc::now() + chrono::Duration::days(3650),
            DEFAULT_SCOPE,
        ))
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let phrase = Self::sha_request_phrase(config)?;
        let access_code = token.access_token.expose();
        let txns = self.paginate_transactions(&base_url, access_code, &phrase, None)?;
        let mut events = Vec::with_capacity(txns.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for t in &txns {
            let occurred_at = transaction_watermark(t).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(t.id.clone()),
                occurred_at,
            });
            if let Some(ts) = transaction_watermark(t) {
                watermark = Some(watermark.map_or(ts, |w| w.max(ts)));
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
        let phrase = Self::sha_request_phrase(config)?;
        let access_code = token.access_token.expose();
        let prior: Option<DateTime<Utc>> = state.cursor.as_deref().and_then(parse_rfc3339);
        let since = prior.map(|t| t.to_rfc3339());
        let txns = self.paginate_transactions(&base_url, access_code, &phrase, since.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for t in &txns {
            let Some(updated) = transaction_watermark(t) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(t.id.clone()),
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
        let phrase = Self::sha_request_phrase(config)?;
        let access_code = token.access_token.expose();
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let path = format!("/api/v1/transactions/{id_enc}");
        let resp: PayfortTransactionResponse = self.signed_get(
            &base_url,
            &path,
            access_code,
            &phrase,
            "/api/v1/transactions/{id}",
        )?;
        let txn = resp.transaction;
        let title = txn
            .merchant_reference
            .clone()
            .unwrap_or_else(|| format!("Transaction {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(status) = txn.status.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Status:** ");
            md.push_str(status);
            md.push_str("\n\n");
        }
        if let Some(amount) = txn.amount.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Amount:** ");
            md.push_str(amount);
            if let Some(currency) = txn.currency.as_deref().filter(|s| !s.is_empty()) {
                md.push(' ');
                md.push_str(currency);
            }
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "payfort",
                "transaction_id": id,
                "status": txn.status,
                "updated_at": txn.updated_at,
            }))
            .with_source_url(format!("{base_url}/transactions/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Amazon Payment Services posts transaction feedback to a single
        // merchant-configured URL set in the back office — there is no
        // API to register webhooks. Record a polling-only subscription
        // so the runtime falls back to incremental_sync.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("payfort-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<PayfortWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<PayfortWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<PayfortWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty payfort webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.fort_id).ok_or_else(|| {
                ConnectorError::Webhook("payfort webhook event missing fort_id".into())
            })?;
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(id_str);
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: id,
                occurred_at,
            });
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
        ConnectorConfig::new(ConnectorKind::Payfort, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_code": "ac_123",
                "sha_request_phrase": "phrase",
                "api_base_url": "https://api.test/payfort",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn small(c: PayfortConnector) -> PayfortConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_requires_access_code_and_phrase() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayfortConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "ac_123"
        );
        let no_phrase =
            ConnectorConfig::new(ConnectorKind::Payfort, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "access_code": "ac_123" }));
        assert!(matches!(
            c.authenticate(&no_phrase),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates_and_signs() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/payfort/api/v1/transactions?per_page=2&page=1".to_string(),
            ok_json(&serde_json::json!({"transactions": [
                {"id": "1", "updated_at": "2024-01-01T00:00:00Z"},
                {"id": "2", "updated_at": "2024-01-02T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/payfort/api/v1/transactions?per_page=2&page=2".to_string(),
            ok_json(&serde_json::json!({"transactions": [
                {"id": "3", "updated_at": "2024-01-03T00:00:00Z"}
            ]})),
        );
        let c = small(PayfortConnector::new(
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
        let recorded = transport.recorded();
        let headers = &recorded[0].headers;
        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Payfort-Access-Code" && v == "ac_123"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Payfort-Signature" && !v.is_empty()));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/payfort/api/v1/transactions?per_page=2&page=1&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"transactions": [
                {"id": "10", "updated_at": "2024-03-01T00:00:00Z"},
                {"id": "11", "updated_at": "2024-06-01T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/payfort/api/v1/transactions?per_page=2&page=2&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"transactions": []})),
        );
        let c = small(PayfortConnector::new(
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
            "https://api.test/payfort/api/v1/transactions/55".to_string(),
            ok_json(&serde_json::json!({"transaction": {
                "id": "55",
                "merchant_reference": "MR-55",
                "amount": "10000",
                "currency": "AED",
                "status": "Purchase Success"
            }})),
        );
        let c = PayfortConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# MR-55"));
        assert!(body.contains("**Status:** Purchase Success"));
        assert!(body.contains("**Amount:** 10000 AED"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayfortConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/payfort")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayfortConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(br#"{"fort_id": "7", "status": "14"}"#)
            .unwrap();
        assert!(matches!(single[0], ConnectorEvent::DocumentUpdated { .. }));
        let batch = c
            .handle_webhook_event(
                br#"[{"fort_id": 8, "status": "02"}, {"fort_id": "9", "status": "14"}]"#,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[1], ConnectorEvent::DocumentUpdated { .. }));
    }
}
