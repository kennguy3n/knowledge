//! Tabby connector — Tabby Merchant API (UAE/Saudi BNPL).
//!
//! * `initial_sync` pages `GET /api/v2/payments?per_page=100&page=N`,
//!   stopping on a short page.
//! * `incremental_sync` adds the `updated_after` filter keyed off the
//!   stored RFC-3339 watermark; the filter is inclusive, so the
//!   boundary row is deduped client-side.
//! * `fetch_content` GETs `/api/v2/payments/{id}` and renders a
//!   Markdown summary.
//! * `subscribe_webhook` POSTs `/api/v1/webhooks` and records the
//!   returned provider subscription id.
//! * `handle_webhook_event` parses Tabby's payment payload (single
//!   object or batched array).
//!
//! Tabby authenticates with the merchant secret key as a bearer token,
//! obtained from `auth_config_json.secret_key` (the common case) or by
//! exchanging an OAuth2 `authorization_code` through the injected
//! [`OAuth2CodeExchange`].

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Default Tabby Merchant API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.tabby.ai";

/// Page size for payment listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// Scope recorded on a token synthesised from a configured secret key.
const DEFAULT_SCOPE: &str = "payments.read";

/// One Tabby payment (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabbyPayment {
    /// Payment id.
    #[serde(default)]
    pub id: String,
    /// Merchant order reference.
    #[serde(default)]
    pub order_reference: Option<String>,
    /// Payment amount (string-encoded decimal).
    #[serde(default)]
    pub amount: Option<String>,
    /// ISO-4217 currency code.
    #[serde(default)]
    pub currency: Option<String>,
    /// Payment status (e.g. `authorized`, `closed`).
    #[serde(default)]
    pub status: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Tabby payment list response (`{ "payments": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabbyPaymentsResponse {
    /// Page of payments.
    #[serde(default)]
    pub payments: Vec<TabbyPayment>,
}

/// Tabby single-payment response (`{ "payment": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabbyPaymentResponse {
    /// The payment.
    #[serde(default)]
    pub payment: TabbyPayment,
}

/// Tabby webhook-create response (`{ "id": ... }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabbyWebhookResponse {
    /// Provider subscription id.
    #[serde(default)]
    pub id: String,
}

/// Tabby webhook delivery payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabbyWebhookEvent {
    /// Affected payment id.
    #[serde(default)]
    pub id: String,
    /// Payment status carried with the notification.
    #[serde(default)]
    pub status: String,
}

/// Tabby connector.
pub struct TabbyConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for TabbyConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TabbyConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl TabbyConnector {
    /// Construct a Tabby connector.
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

    /// Override the Tabby base URL.
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

    /// Walk the payment list page-by-page, stopping on a short page.
    fn paginate_payments(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_after: Option<&str>,
    ) -> Result<Vec<TabbyPayment>> {
        let mut out = Vec::<TabbyPayment>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!(
                "{base_url}/api/v2/payments?per_page={}&page={page}",
                self.page_size
            );
            if let Some(after) = updated_after {
                url.push_str("&updated_after=");
                url.push_str(&percent_encode_path_component(after));
            }
            let resp: TabbyPaymentsResponse = bearer_get_json(
                &self.transport,
                "tabby",
                "/api/v2/payments",
                &url,
                token,
                &[],
            )?;
            let count = resp.payments.len();
            out.extend(resp.payments);
            if count < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "tabby /api/v2/payments exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn payment_watermark(p: &TabbyPayment) -> Option<DateTime<Utc>> {
    p.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| p.created_at.as_deref().and_then(parse_rfc3339))
}

impl Connector for TabbyConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("secret_key")
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
                    "tabby authenticate: auth_config_json.secret_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let payments = self.paginate_payments(&base_url, token, None)?;
        let mut events = Vec::with_capacity(payments.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for p in &payments {
            let occurred_at = payment_watermark(p).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(p.id.clone()),
                occurred_at,
            });
            if let Some(t) = payment_watermark(p) {
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
        let payments = self.paginate_payments(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for p in &payments {
            let Some(updated) = payment_watermark(p) else {
                continue;
            };
            if prior.is_some_and(|pp| updated <= pp) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(p.id.clone()),
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
        let url = format!("{base_url}/api/v2/payments/{id_enc}");
        let resp: TabbyPaymentResponse = bearer_get_json(
            &self.transport,
            "tabby",
            "/api/v2/payments/{id}",
            &url,
            token,
            &[],
        )?;
        let payment = resp.payment;
        let title = payment
            .order_reference
            .clone()
            .unwrap_or_else(|| format!("Payment {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(status) = payment.status.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Status:** ");
            md.push_str(status);
            md.push_str("\n\n");
        }
        if let Some(amount) = payment.amount.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Amount:** ");
            md.push_str(amount);
            if let Some(currency) = payment.currency.as_deref().filter(|s| !s.is_empty()) {
                md.push(' ');
                md.push_str(currency);
            }
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "tabby",
                "payment_id": id,
                "status": payment.status,
                "updated_at": payment.updated_at,
            }))
            .with_source_url(format!("{base_url}/payments/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/api/v1/webhooks");
        let body = serde_json::json!({
            "url": callback_url,
            "is_test": false,
        });
        let resp: TabbyWebhookResponse = bearer_post_json(
            &self.transport,
            "tabby",
            "/api/v1/webhooks",
            &url,
            token,
            &[],
            &body,
        )?;
        if resp.id.is_empty() {
            return Err(ConnectorError::Webhook(
                "tabby /api/v1/webhooks returned no webhook id".into(),
            ));
        }
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tabby-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<TabbyWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<TabbyWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<TabbyWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty tabby webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            if delivery.id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "tabby webhook event missing id".into(),
                ));
            }
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(delivery.id);
            // Tabby payments are created via the checkout flow and only
            // notified once they reach a terminal state; treat an
            // `expired`/`rejected` status as a deletion, everything else
            // as an update.
            let event = if delivery.status == "expired" || delivery.status == "rejected" {
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
    use chrono::Duration;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "tabby-access",
                "tabby-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Tabby, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "secret_key": "sk_test_123",
                "api_base_url": "https://api.test/tabby",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn small(c: TabbyConnector) -> TabbyConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_wraps_secret_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TabbyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "sk_test_123");
    }

    #[test]
    fn authenticate_falls_back_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TabbyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Tabby, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "authorization_code": "abc" }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "tabby-access"
        );
    }

    #[test]
    fn initial_sync_paginates_until_short_page() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/tabby/api/v2/payments?per_page=2&page=1".to_string(),
            ok_json(&serde_json::json!({"payments": [
                {"id": "1", "updated_at": "2024-01-01T00:00:00Z"},
                {"id": "2", "updated_at": "2024-01-02T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/tabby/api/v2/payments?per_page=2&page=2".to_string(),
            ok_json(&serde_json::json!({"payments": [
                {"id": "3", "updated_at": "2024-01-03T00:00:00Z"}
            ]})),
        );
        let c = small(TabbyConnector::new(
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
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/tabby/api/v2/payments?per_page=2&page=1&updated_after={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"payments": [
                {"id": "10", "updated_at": "2024-03-01T00:00:00Z"},
                {"id": "11", "updated_at": "2024-06-01T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/tabby/api/v2/payments?per_page=2&page=2&updated_after={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"payments": []})),
        );
        let c = small(TabbyConnector::new(
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
            "https://api.test/tabby/api/v2/payments/55".to_string(),
            ok_json(&serde_json::json!({"payment": {
                "id": "55",
                "order_reference": "ORD-55",
                "amount": "250.00",
                "currency": "AED",
                "status": "closed"
            }})),
        );
        let c = TabbyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# ORD-55"));
        assert!(body.contains("**Status:** closed"));
        assert!(body.contains("**Amount:** 250.00 AED"));
    }

    #[test]
    fn subscribe_webhook_records_provider_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/tabby/api/v1/webhooks".to_string(),
            ok_json(&serde_json::json!({"id": "wh_42"})),
        );
        let c = TabbyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/tabby")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh_42"));
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TabbyConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(br#"{"id": "7", "status": "closed"}"#)
            .unwrap();
        assert!(matches!(single[0], ConnectorEvent::DocumentUpdated { .. }));
        let batch = c
            .handle_webhook_event(
                br#"[{"id": "8", "status": "authorized"}, {"id": "9", "status": "expired"}]"#,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
