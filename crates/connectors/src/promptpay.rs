//! PromptPay connector — PromptPay QR reconciliation API
//! (`https://api.promptpay.io`).
//!
//! PromptPay is Thailand's national real-time payment rail. This
//! connector targets a reconciliation API that exposes settlement
//! records and authenticates with a static API key presented as a
//! bearer token, so [`PromptPayConnector::authenticate`] reads the
//! key out of `auth_config_json` (falling back to the injected
//! [`OAuth2CodeExchange`] when an authorization-code grant is
//! configured instead).
//!
//! * `initial_sync` / `incremental_sync` page `/v1/settlements`
//!   (`limit` / `offset`), tracking the maximum `settled_at` as an
//!   RFC-3339 watermark; incremental runs add `since` and dedup the
//!   inclusive boundary row.
//! * `fetch_content` GETs a single settlement
//!   (`/v1/settlements/{id}`).
//! * PromptPay reconciliation webhooks are provisioned out of band,
//!   so `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payment
//!   notification.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default PromptPay reconciliation API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.promptpay.io";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "settlements";
/// Page size for settlement listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct PromptPaySettlementsPage {
    #[serde(default)]
    settlements: Vec<PromptPaySettlement>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PromptPaySettlement {
    #[serde(default)]
    reference_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    amount: Option<f64>,
    #[serde(default)]
    payer: Option<String>,
    #[serde(default)]
    settled_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PromptPayWebhookEvent {
    #[serde(default)]
    reference_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// PromptPay connector.
pub struct PromptPayConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for PromptPayConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptPayConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl PromptPayConnector {
    /// Construct a PromptPay connector.
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

    /// Override the PromptPay API base URL.
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

    fn paginate_settlements(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        since: Option<&str>,
    ) -> Result<Vec<PromptPaySettlement>> {
        let mut settlements = Vec::<PromptPaySettlement>::new();
        for page in 0..MAX_PAGES {
            let offset = page * self.page_size as usize;
            let mut url = format!(
                "{base_url}/v1/settlements?limit={}&offset={offset}",
                self.page_size
            );
            if let Some(s) = since {
                url.push_str("&since=");
                url.push_str(&percent_encode_path_component(s));
            }
            let resp: PromptPaySettlementsPage = bearer_get_json(
                &self.transport,
                "promptpay",
                "/v1/settlements",
                &url,
                token,
                &[],
            )?;
            let count = resp.settlements.len();
            settlements.extend(resp.settlements);
            if count < self.page_size as usize {
                return Ok(settlements);
            }
        }
        Err(ConnectorError::Sync(format!(
            "promptpay /v1/settlements exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn settlement_watermark(s: &PromptPaySettlement) -> Option<DateTime<Utc>> {
    s.settled_at.as_deref().and_then(parse_rfc3339)
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for PromptPayConnector {
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
                    "promptpay authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let settlements = self.paginate_settlements(&base_url, token, None)?;
        let mut events = Vec::with_capacity(settlements.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for settlement in &settlements {
            let occurred_at = settlement_watermark(settlement).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(settlement.reference_id.clone()),
                occurred_at,
            });
            if let Some(t) = settlement_watermark(settlement) {
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
        let settlements = self.paginate_settlements(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for settlement in &settlements {
            let Some(updated) = settlement_watermark(settlement) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(settlement.reference_id.clone()),
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
        let url = format!("{base_url}/v1/settlements/{id_enc}");
        let settlement: PromptPaySettlement = bearer_get_json(
            &self.transport,
            "promptpay",
            "/v1/settlements/{id}",
            &url,
            token,
            &[],
        )?;
        let status = settlement.status.as_deref().unwrap_or("unknown");
        let amount = settlement.amount.unwrap_or(0.0);
        let payer = settlement.payer.as_deref().unwrap_or("");
        let body = format!(
            "# PromptPay settlement {id}\n\nStatus: {status}\nPayer: {payer}\nAmount: {amount} THB\n"
        );
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("PromptPay settlement {id}"))
            .with_metadata(serde_json::json!({
                "provider": "promptpay",
                "reference_id": settlement.reference_id,
                "status": settlement.status,
                "settled_at": settlement.settled_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // PromptPay reconciliation webhooks are provisioned out of
        // band with the acquirer; record a polling-only subscription
        // so the runtime falls back to incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("promptpay-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<PromptPayWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<PromptPayWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<PromptPayWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("promptpay webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty promptpay webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.reference_id).ok_or_else(|| {
                ConnectorError::Webhook("promptpay webhook event missing reference_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            // Settlement records are immutable; a paid/settled
            // notification is a creation, anything else an update.
            let event = if delivery.event.contains("settled") || delivery.event.contains("paid") {
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
                "settlements",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::PromptPay,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "pp-key",
            "api_base_url": "https://api.test/pp",
            "webhook_secret": "pp-secret",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PromptPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "pp-key");
    }

    #[test]
    fn initial_sync_paginates() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/pp/v1/settlements?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "settlements": [
                    {"reference_id": "r-1", "settled_at": "2024-01-01T00:00:00Z"},
                    {"reference_id": "r-2", "settled_at": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/pp/v1/settlements?limit=2&offset=2",
            ok_json(&serde_json::json!({ "settlements": [ {"reference_id": "r-3", "settled_at": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = PromptPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
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
                "https://api.test/pp/v1/settlements?limit=2&offset=0&since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "settlements": [
                    {"reference_id": "r-10", "settled_at": "2024-03-01T00:00:00Z"},
                    {"reference_id": "r-11", "settled_at": "2024-06-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/pp/v1/settlements?limit=2&offset=2&since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "settlements": [] })),
        );
        let c = PromptPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
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
            "https://api.test/pp/v1/settlements/r-1",
            ok_json(&serde_json::json!({
                "reference_id": "r-1",
                "status": "SETTLED",
                "amount": 500.0,
                "payer": "0812345678"
            })),
        );
        let c = PromptPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("r-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# PromptPay settlement r-1"));
        assert!(body.contains("SETTLED"));
    }

    #[test]
    fn handle_webhook_event_settlement_kinds() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PromptPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!([
            { "reference_id": "r-1", "event": "payment.settled" },
            { "reference_id": "r-2", "event": "payment.adjusted" }
        ]))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(events[1], ConnectorEvent::DocumentUpdated { .. }));
    }
}
