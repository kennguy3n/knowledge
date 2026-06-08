//! Pennylane connector — Pennylane partner API (`https://app.pennylane.com/api/external/v1`).
//!
//! Pennylane — French accounting API (invoices, suppliers).
//!
//! Authentication uses Pennylane's single bearer-token scheme: both a
//! static API token (read from `auth_config_json.api_key`) and an
//! OAuth-issued access token are sent in the `Authorization: Bearer
//! <token>` header. The injected [`OAuth2CodeExchange`] is used when a
//! rotating `authorization_code` grant is configured instead of a
//! static token.
//!
//! * `initial_sync` / `incremental_sync` page `/customer_invoices`
//!   (`page` / `per_page`), following the response `current_page` /
//!   `total_pages` cursor and tracking the maximum `updated_at` as an
//!   RFC-3339 watermark. Pennylane does not expose `updated_at` as a
//!   server-side filter, so incremental runs page the same listing and
//!   dedup client-side against the stored watermark.
//! * `fetch_content` GETs a single invoice (`/customer_invoices/{id}`,
//!   response wrapped in an `invoice` object).
//! * Webhooks are configured in the provider dashboard, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payload.

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

/// Default Pennylane API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://app.pennylane.com/api/external/v1";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "invoices";
/// `OAuth2Token::token_type` marker retained for API compatibility.
/// Pennylane uses a single bearer scheme, so both credential kinds are
/// sent as `Authorization: Bearer <token>`.
pub const API_KEY_TOKEN_TYPE: &str = "ApiKey";
/// Page size for invoice listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct PennylanePage {
    #[serde(default)]
    invoices: Vec<PennylaneRecord>,
    #[serde(default)]
    current_page: Option<u32>,
    #[serde(default)]
    total_pages: Option<u32>,
}

/// Single-invoice retrieve envelope (`GET /customer_invoices/{id}`),
/// which wraps the record in an `invoice` object.
#[derive(Debug, Clone, Default, Deserialize)]
struct PennylaneInvoiceEnvelope {
    #[serde(default)]
    invoice: PennylaneRecord,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PennylaneRecord {
    #[serde(default)]
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PennylaneWebhookEvent {
    #[serde(default)]
    invoice_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Pennylane connector.
pub struct PennylaneConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for PennylaneConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PennylaneConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl PennylaneConnector {
    /// Construct a Pennylane connector.
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

    /// Override the Pennylane API base URL.
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

    fn http_get<R: DeserializeOwned>(
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
            return Err(classify_failure("pennylane", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "pennylane {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    fn paginate_records(
        &self,
        base_url: &str,
        token: &OAuth2Token,
    ) -> Result<Vec<PennylaneRecord>> {
        let mut records = Vec::<PennylaneRecord>::new();
        for page in 1..=MAX_PAGES {
            let url = format!(
                "{base_url}/customer_invoices?page={page}&per_page={}",
                self.page_size
            );
            let resp: PennylanePage = self.http_get("/customer_invoices", &url, token)?;
            let count = resp.invoices.len();
            records.extend(resp.invoices);
            // Prefer Pennylane's own pagination cursor; fall back to a
            // short-page heuristic when the envelope omits it.
            let done = match (resp.current_page, resp.total_pages) {
                (Some(current), Some(total)) => current >= total,
                _ => count < self.page_size as usize,
            };
            if done {
                return Ok(records);
            }
        }
        Err(ConnectorError::Sync(format!(
            "pennylane /customer_invoices exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Attach Pennylane's bearer auth header. Both a static API token and
/// an OAuth-issued token are sent as `Authorization: Bearer <token>`
/// (scheme from `token_type`, defaulting to `Bearer`).
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    apply_auth_by_provenance(req, token, "Authorization", API_KEY_TOKEN_TYPE)
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn record_watermark(o: &PennylaneRecord) -> Option<DateTime<Utc>> {
    o.updated_at.as_deref().and_then(parse_rfc3339)
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for PennylaneConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(api_key) = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
        {
            // Pennylane's static API token is itself a bearer token, so
            // keep the default `Bearer` provenance (no native-header tag).
            let token = OAuth2Token::new_without_refresh(
                api_key,
                Utc::now() + chrono::Duration::days(365),
                DEFAULT_SCOPE,
            );
            return Ok(token);
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "pennylane authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let records = self.paginate_records(&base_url, token)?;
        let mut events = Vec::with_capacity(records.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for record in &records {
            let occurred_at = record_watermark(record).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(record.id.clone()),
                occurred_at,
            });
            if let Some(t) = record_watermark(record) {
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
        let records = self.paginate_records(&base_url, token)?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for record in &records {
            let Some(updated) = record_watermark(record) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(record.id.clone()),
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
        let url = format!("{base_url}/customer_invoices/{id_enc}");
        let envelope: PennylaneInvoiceEnvelope =
            self.http_get("/customer_invoices/{id}", &url, token)?;
        let record = envelope.invoice;
        let status = record.status.as_deref().unwrap_or("unknown");
        let label = record.label.as_deref().unwrap_or("(no label)");
        let body = format!("# Pennylane invoice {id}\n\nLabel: {label}\nStatus: {status}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("Pennylane invoice {id}"))
            .with_metadata(serde_json::json!({
                "provider": "pennylane",
                "record_id": record.id,
                "status": record.status,
                "updated_at": record.updated_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Pennylane webhooks are registered in the provider
        // dashboard; no REST endpoint creates them. Record a
        // polling-only subscription so the runtime falls back to
        // incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("pennylane-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<PennylaneWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<PennylaneWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<PennylaneWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("pennylane webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty pennylane webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.invoice_id).ok_or_else(|| {
                ConnectorError::Webhook("pennylane webhook event missing invoice_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("cancel") || delivery.event.contains("delete") {
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
                "unused",
                "unused",
                Utc::now() + Duration::hours(1),
                "invoices",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Pennylane,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "pennylane-key",
            "api_base_url": "https://api.test/pennylane",
            "webhook_secret": "pennylane-secret",
        }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Pennylane,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "auth-code",
            "api_base_url": "https://api.test/pennylane",
            "webhook_secret": "pennylane-secret",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "pennylane-key");
        assert!(token.refresh_token.is_none());
        // Pennylane's static token is a bearer token.
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg_oauth()).unwrap();
        // OAuth-issued token keeps the bearer token_type, not the
        // API-key marker, so requests use `Authorization: Bearer`.
        assert_eq!(token.access_token.expose(), "unused");
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn oauth_token_is_sent_as_bearer_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/pennylane/customer_invoices?page=1&per_page=2",
            ok_json(&serde_json::json!({
                "invoices": [ {"id": "o-1", "updated_at": "2024-01-01T00:00:00Z"} ],
                "current_page": 1, "total_pages": 1
            })),
        );
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg_oauth()).unwrap();
        let res = c.initial_sync(&cfg_oauth(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        let recorded = transport.recorded();
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer unused"));
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(
            ConnectorKind::Pennylane,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        );
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_follows_page_cursor_and_sends_bearer_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/pennylane/customer_invoices?page=1&per_page=2",
            ok_json(&serde_json::json!({
                "invoices": [
                    {"id": "o-1", "updated_at": "2024-01-01T00:00:00Z"},
                    {"id": "o-2", "updated_at": "2024-01-02T00:00:00Z"}
                ],
                "current_page": 1, "total_pages": 2
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/pennylane/customer_invoices?page=2&per_page=2",
            ok_json(&serde_json::json!({
                "invoices": [ {"id": "o-3", "updated_at": "2024-01-03T00:00:00Z"} ],
                "current_page": 2, "total_pages": 2
            })),
        );
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00")
        );
        let recorded = transport.recorded();
        // Pennylane's static API token is sent as a bearer token, not a
        // custom `X-Pennylane-Api-Key` header.
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer pennylane-key"));
        assert!(!recorded[0]
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("X-Pennylane-Api-Key")));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        // Pennylane has no `updated_at` server filter, so incremental
        // pages the same listing and dedups client-side against the
        // stored watermark (the inclusive boundary row is dropped).
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            "https://api.test/pennylane/customer_invoices?page=1&per_page=2",
            ok_json(&serde_json::json!({
                "invoices": [
                    {"id": "o-10", "updated_at": "2024-03-01T00:00:00Z"},
                    {"id": "o-11", "updated_at": "2024-06-01T00:00:00Z"}
                ],
                "current_page": 1, "total_pages": 1
            })),
        );
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
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
            "https://api.test/pennylane/customer_invoices/o-1",
            ok_json(&serde_json::json!({
                "invoice": {
                    "id": "o-1",
                    "status": "paid",
                    "label": "Sample invoice"
                }
            })),
        );
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("o-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Pennylane invoice o-1"));
        assert!(body.contains("Sample invoice"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/pennylane")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "pennylane-secret");
    }

    #[test]
    fn handle_webhook_event_parses_single() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(
            &serde_json::json!({ "invoice_id": 42, "event": "invoice.updated" }),
        )
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ConnectorEvent::DocumentUpdated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "42");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Regression guard: the production base URL already carries the
    /// `/api/external/v1` API version, so the request path must NOT
    /// re-introduce a version segment. Exercises `DEFAULT_API_BASE_URL`
    /// (no override) because the other tests mask version-duplication
    /// bugs by pointing at a versionless test host.
    #[test]
    fn production_base_url_does_not_duplicate_version() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PennylaneConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let prod_cfg = ConnectorConfig::new(
            ConnectorKind::Pennylane,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({ "api_key": "pennylane-key" }));
        let tok = c.authenticate(&prod_cfg).unwrap();
        let _ = c.initial_sync(&prod_cfg, &tok);
        let recorded = transport.recorded();
        assert_eq!(
            recorded[0].url,
            "https://app.pennylane.com/api/external/v1/customer_invoices?page=1&per_page=2",
            "request URL must not duplicate the API version"
        );
    }
}
