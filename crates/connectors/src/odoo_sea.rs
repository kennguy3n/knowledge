//! Odoo connector — Odoo REST API (`https://your-instance.odoo.com`).
//!
//! Odoo is a widely deployed ERP across SEA SMEs. This connector
//! targets the JSON REST surface and authenticates with a *session
//! token* (`X-Openerp-Session-Id`), so
//! [`OdooSeaConnector::authenticate`] reads the token out of
//! `auth_config_json` and falls back to the injected
//! [`OAuth2CodeExchange`] when an authorization-code grant is
//! configured instead.
//!
//! Requests pick their auth header from the token's provenance
//! (recorded in [`OAuth2Token::token_type`], following the same
//! convention as the Discord connector): a static session token is
//! sent in the provider-native `X-Openerp-Session-Id` header, while
//! an OAuth-issued access token is sent as `Authorization: Bearer`.
//!
//! * `initial_sync` / `incremental_sync` page `/api/v1/invoices`
//!   (`limit` / `offset`), tracking the maximum `write_date` as an
//!   RFC-3339 watermark; incremental runs add `write_date_gt` and
//!   dedup the inclusive boundary row.
//! * `fetch_content` GETs a single invoice (`/api/v1/invoices/{id}`).
//! * Odoo automations deliver webhooks but expose no creation
//!   endpoint, so `subscribe_webhook` records a polling-only
//!   subscription.
//! * `handle_webhook_event` parses the delivered payload.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default Odoo base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://your-instance.odoo.com";
/// Default scope recorded on the synthesised session token.
pub const DEFAULT_SCOPE: &str = "invoices";
/// `OAuth2Token::token_type` marker for a static session-token
/// credential. Distinguishes the session-token auth path
/// (provider-native `X-Openerp-Session-Id` header) from an
/// OAuth-issued bearer token.
pub const SESSION_TOKEN_TYPE: &str = "Session";
/// Page size for invoice listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct OdooInvoicesPage {
    #[serde(default)]
    records: Vec<OdooInvoice>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OdooInvoice {
    #[serde(default)]
    id: serde_json::Value,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    partner_name: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    amount_total: Option<f64>,
    #[serde(default)]
    write_date: Option<String>,
    #[serde(default)]
    create_date: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OdooWebhookEvent {
    #[serde(default)]
    record_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Odoo (SEA) connector.
pub struct OdooSeaConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for OdooSeaConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OdooSeaConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl OdooSeaConnector {
    /// Construct an Odoo connector.
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

    /// Override the Odoo base URL.
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

    fn odoo_get<R: DeserializeOwned>(
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
            return Err(classify_failure("odoo_sea", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "odoo_sea {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    fn paginate_invoices(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        write_date_gt: Option<&str>,
    ) -> Result<Vec<OdooInvoice>> {
        let mut invoices = Vec::<OdooInvoice>::new();
        for page in 0..MAX_PAGES {
            let offset = page * self.page_size as usize;
            let mut url = format!(
                "{base_url}/api/v1/invoices?limit={}&offset={offset}",
                self.page_size
            );
            if let Some(since) = write_date_gt {
                url.push_str("&write_date_gt=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: OdooInvoicesPage = self.odoo_get("/api/v1/invoices", &url, token)?;
            let count = resp.records.len();
            invoices.extend(resp.records);
            if count < self.page_size as usize {
                return Ok(invoices);
            }
        }
        Err(ConnectorError::Sync(format!(
            "odoo_sea /api/v1/invoices exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Attach the auth header matching the token's provenance: a static
/// session token (tagged [`SESSION_TOKEN_TYPE`] in `authenticate`)
/// goes in the provider-native `X-Openerp-Session-Id` header, while
/// an OAuth-issued token is sent as `Authorization: <scheme> <token>`
/// (scheme from `token_type`, defaulting to `Bearer`).
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    if token.token_type == SESSION_TOKEN_TYPE {
        req.with_header("X-Openerp-Session-Id", token.access_token.expose())
    } else {
        let scheme = if token.token_type.is_empty() {
            "Bearer"
        } else {
            token.token_type.as_str()
        };
        req.with_header(
            "Authorization",
            format!("{scheme} {}", token.access_token.expose()),
        )
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn invoice_watermark(i: &OdooInvoice) -> Option<DateTime<Utc>> {
    i.write_date
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| i.create_date.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for OdooSeaConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(session) = config
            .auth_config_json
            .get("session_token")
            .and_then(serde_json::Value::as_str)
        {
            let mut token = OAuth2Token::new_without_refresh(
                session,
                Utc::now() + chrono::Duration::days(7),
                DEFAULT_SCOPE,
            );
            token.token_type = SESSION_TOKEN_TYPE.to_string();
            return Ok(token);
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "odoo_sea authenticate: auth_config_json.session_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let invoices = self.paginate_invoices(&base_url, token, None)?;
        let mut events = Vec::with_capacity(invoices.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for invoice in &invoices {
            let Some(id_str) = id_value_to_string(&invoice.id) else {
                continue;
            };
            let occurred_at = invoice_watermark(invoice).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(id_str),
                occurred_at,
            });
            if let Some(t) = invoice_watermark(invoice) {
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
        let invoices = self.paginate_invoices(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for invoice in &invoices {
            let Some(id_str) = id_value_to_string(&invoice.id) else {
                continue;
            };
            let Some(updated) = invoice_watermark(invoice) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(id_str),
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
        let url = format!("{base_url}/api/v1/invoices/{id_enc}");
        let invoice: OdooInvoice = self.odoo_get("/api/v1/invoices/{id}", &url, token)?;
        let name = invoice.name.as_deref().unwrap_or("(untitled invoice)");
        let partner = invoice.partner_name.as_deref().unwrap_or("");
        let state = invoice.state.as_deref().unwrap_or("");
        let total = invoice.amount_total.unwrap_or(0.0);
        let body = format!("# {name}\n\nPartner: {partner}\nState: {state}\nTotal: {total}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(name.to_string())
            .with_metadata(serde_json::json!({
                "provider": "odoo_sea",
                "invoice_id": id,
                "state": invoice.state,
                "write_date": invoice.write_date,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Odoo automations push webhooks but expose no REST creation
        // endpoint; record a polling-only subscription so the runtime
        // falls back to incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("odoo-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<OdooWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<OdooWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<OdooWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("odoo_sea webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty odoo_sea webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.record_id).ok_or_else(|| {
                ConnectorError::Webhook("odoo_sea webhook event missing record_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("unlink") || delivery.event.contains("delete") {
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
        ConnectorConfig::new(ConnectorKind::OdooSea, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "session_token": "odoo-session",
                "api_base_url": "https://api.test/odoo",
                "webhook_secret": "odoo-secret",
            }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::OdooSea, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "auth-code",
                "api_base_url": "https://api.test/odoo",
                "webhook_secret": "odoo-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_session_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "odoo-session");
        assert_eq!(token.token_type, SESSION_TOKEN_TYPE);
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg_oauth()).unwrap();
        // OAuth-issued token keeps the bearer token_type, not the
        // session marker, so requests use `Authorization: Bearer`.
        assert_eq!(token.access_token.expose(), "unused");
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn oauth_token_is_sent_as_bearer_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/odoo/api/v1/invoices?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "records": [ {"id": 1, "write_date": "2024-01-01T00:00:00Z"} ]
            })),
        );
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
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
            .any(|(k, _)| k.eq_ignore_ascii_case("x-openerp-session-id")));
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::OdooSea, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates_and_sends_session_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/odoo/api/v1/invoices?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "records": [
                    {"id": 1, "write_date": "2024-01-01T00:00:00Z"},
                    {"id": 2, "write_date": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/odoo/api/v1/invoices?limit=2&offset=2",
            ok_json(&serde_json::json!({ "records": [ {"id": 3, "write_date": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
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
            .any(|(k, v)| k.eq_ignore_ascii_case("x-openerp-session-id") && v == "odoo-session"));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/odoo/api/v1/invoices?limit=2&offset=0&write_date_gt={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "records": [
                    {"id": 10, "write_date": "2024-03-01T00:00:00Z"},
                    {"id": 11, "write_date": "2024-06-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/odoo/api/v1/invoices?limit=2&offset=2&write_date_gt={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "records": [] })),
        );
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
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
            "https://api.test/odoo/api/v1/invoices/5",
            ok_json(&serde_json::json!({
                "id": 5,
                "name": "INV/2024/0005",
                "partner_name": "Acme Co",
                "state": "posted",
                "amount_total": 1200.0
            })),
        );
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("5"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# INV/2024/0005"));
        assert!(body.contains("Acme Co"));
        assert!(body.contains("posted"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/odoo")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "odoo-secret");
    }

    #[test]
    fn handle_webhook_event_parses() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OdooSeaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "record_id": 9, "event": "invoice.unlink"
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ConnectorEvent::DocumentDeleted { .. }));
    }
}
