//! SCB Easy connector — Siam Commercial Bank Open Banking API
//! (`https://api.partners.scb`).
//!
//! SCB is one of Thailand's largest banks; the partner / Open Banking
//! API is OAuth2 (authorization-code), so
//! [`ScbEasyConnector::authenticate`] delegates to the injected
//! [`OAuth2CodeExchange`].
//!
//! * `initial_sync` / `incremental_sync` page
//!   `/v1/me/account/transactions` (`limit` / `offset`), tracking the
//!   maximum `date_time` as an RFC-3339 watermark; incremental runs
//!   add `from_date` and dedup the inclusive boundary row.
//! * `fetch_content` GETs a single transaction
//!   (`/v1/me/account/transactions/{id}`).
//! * SCB delivers Open Banking events but exposes no self-serve
//!   webhook-creation endpoint, so `subscribe_webhook` records a
//!   polling-only subscription.
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

/// Default SCB Open Banking API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.partners.scb";
/// Page size for transaction listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct ScbTransactionsPage {
    #[serde(default)]
    transactions: Vec<ScbTransaction>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScbTransaction {
    #[serde(default, rename = "transactionId")]
    transaction_id: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    amount: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default, rename = "dateTime")]
    date_time: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ScbWebhookEvent {
    #[serde(default, rename = "transactionId")]
    transaction_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// SCB Easy connector.
pub struct ScbEasyConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ScbEasyConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScbEasyConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ScbEasyConnector {
    /// Construct an SCB Easy connector.
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

    /// Override the SCB API base URL.
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

    fn paginate_transactions(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        from_date: Option<&str>,
    ) -> Result<Vec<ScbTransaction>> {
        let mut txns = Vec::<ScbTransaction>::new();
        for page in 0..MAX_PAGES {
            let offset = page * self.page_size as usize;
            let mut url = format!(
                "{base_url}/v1/me/account/transactions?limit={}&offset={offset}",
                self.page_size
            );
            if let Some(since) = from_date {
                url.push_str("&from_date=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: ScbTransactionsPage = bearer_get_json(
                &self.transport,
                "scb_easy",
                "/v1/me/account/transactions",
                &url,
                token,
                &[],
            )?;
            let count = resp.transactions.len();
            txns.extend(resp.transactions);
            if count < self.page_size as usize {
                return Ok(txns);
            }
        }
        Err(ConnectorError::Sync(format!(
            "scb_easy /v1/me/account/transactions exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn txn_watermark(t: &ScbTransaction) -> Option<DateTime<Utc>> {
    t.date_time.as_deref().and_then(parse_rfc3339)
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for ScbEasyConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "scb_easy authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let txns = self.paginate_transactions(&base_url, token, None)?;
        let mut events = Vec::with_capacity(txns.len());
        let mut cursor = WatermarkCursor::empty();
        for txn in &txns {
            let occurred_at = txn_watermark(txn).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(txn.transaction_id.clone()),
                occurred_at,
            });
            if let Some(t) = txn_watermark(txn) {
                cursor.observe(t, &txn.transaction_id);
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
        let txns = self.paginate_transactions(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for txn in &txns {
            let Some(updated) = txn_watermark(txn) else {
                continue;
            };
            if !prior.should_emit(updated, &txn.transaction_id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(txn.transaction_id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &txn.transaction_id);
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
        let url = format!("{base_url}/v1/me/account/transactions/{id_enc}");
        let txn: ScbTransaction = bearer_get_json(
            &self.transport,
            "scb_easy",
            "/v1/me/account/transactions/{id}",
            &url,
            token,
            &[],
        )?;
        let description = txn.description.as_deref().unwrap_or("(no description)");
        let amount = txn.amount.unwrap_or(0.0);
        let currency = txn.currency.as_deref().unwrap_or("THB");
        let body =
            format!("# SCB transaction {id}\n\n{description}\nAmount: {amount} {currency}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("SCB transaction {id}"))
            .with_metadata(serde_json::json!({
                "provider": "scb_easy",
                "transaction_id": txn.transaction_id,
                "date_time": txn.date_time,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // SCB delivers Open Banking events but exposes no self-serve
        // webhook-creation endpoint; record a polling-only
        // subscription so the runtime falls back to incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("scb-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<ScbWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<ScbWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<ScbWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("scb_easy webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty scb_easy webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.transaction_id).ok_or_else(|| {
                ConnectorError::Webhook("scb_easy webhook event missing transactionId".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            // Bank transactions are immutable ledger entries; treat a
            // posted/settled event as a creation and anything else as
            // an update (records are never deleted).
            let event = if delivery.event.contains("created") || delivery.event.contains("posted") {
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
                "scb-access",
                "scb-refresh",
                Utc::now() + Duration::hours(1),
                "transactions",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::ScbEasy, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/scb",
                "webhook_secret": "scb-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ScbEasyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::ScbEasy, AuthKind::OAuth2, ScopeId::new_v4());
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
            "https://api.test/scb/v1/me/account/transactions?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "transactions": [
                    {"transactionId": "x-1", "dateTime": "2024-01-01T00:00:00Z"},
                    {"transactionId": "x-2", "dateTime": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/scb/v1/me/account/transactions?limit=2&offset=2",
            ok_json(&serde_json::json!({ "transactions": [ {"transactionId": "x-3", "dateTime": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = ScbEasyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|x-3")
        );
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/scb/v1/me/account/transactions?limit=2&offset=0&from_date={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "transactions": [
                    {"transactionId": "x-10", "dateTime": "2024-03-01T00:00:00Z"},
                    {"transactionId": "x-13", "dateTime": "2024-03-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/scb/v1/me/account/transactions?limit=2&offset=2&from_date={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "transactions": [ {"transactionId": "x-11", "dateTime": "2024-06-01T00:00:00Z"} ] })),
        );
        let c = ScbEasyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `x-10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must NOT
        // re-emit `x-10`, still surface the brand-new `x-13` at the same second,
        // and advance past the later row.
        state.cursor = Some(format!("{since}|x-10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["x-13", "x-11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|x-11")
        );
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/scb/v1/me/account/transactions/x-1",
            ok_json(&serde_json::json!({
                "transactionId": "x-1",
                "description": "Coffee",
                "amount": 95.0,
                "currency": "THB"
            })),
        );
        let c = ScbEasyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("x-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# SCB transaction x-1"));
        assert!(body.contains("Coffee"));
    }

    #[test]
    fn handle_webhook_event_immutable_ledger() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ScbEasyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!([
            { "transactionId": "x-1", "event": "txn.posted" },
            { "transactionId": "x-2", "event": "txn.reversed" }
        ]))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(events[1], ConnectorEvent::DocumentUpdated { .. }));
    }
}
