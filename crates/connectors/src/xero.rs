//! Xero connector — Xero Accounting API + Xero webhooks.
//!
//! * `initial_sync` walks `GET /api.xro/2.0/Invoices` and pages via
//!   the `page` query parameter (with an explicit `pageSize`) until a
//!   short page is returned.
//! * `incremental_sync` sends an `If-Modified-Since` header derived
//!   from the prior watermark and dedupes client-side on
//!   `UpdatedDateUTC`.
//! * `fetch_content` reads a single invoice and renders a Markdown
//!   summary.
//! * `subscribe_webhook` does **not** call the API — Xero webhooks
//!   are configured once in the developer portal, so the connector
//!   surfaces the operator-provided signing key as the subscription
//!   secret.
//! * `handle_webhook_event` parses Xero's batched `events` payload
//!   and emits **every** event.
//!
//! Xero authenticates with OAuth2 bearer tokens and scopes each call
//! to a tenant via the `Xero-tenant-id` header, so the connector uses
//! the bearer helpers with that extra header. `authenticate` accepts
//! a configured `access_token` or an OAuth2 `authorization_code`.
//!
//! Note: the Xero Accounting API renders `UpdatedDateUTC` in the
//! Microsoft JSON `/Date(…)/` form by default; this connector models
//! it as an ISO-8601 timestamp, matching the representation returned
//! when the `Accept` negotiation requests ISO dates.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WatermarkCursor,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Xero API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.xero.com";

/// Page size for the invoices endpoint. Xero accepts `[1, 1000]`.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured token.
const DEFAULT_SCOPE: &str = "accounting.transactions.read";

/// One Xero invoice (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XeroInvoice {
    /// Invoice GUID.
    #[serde(rename = "InvoiceID", default)]
    pub invoice_id: String,
    /// Human-facing invoice number.
    #[serde(rename = "InvoiceNumber", default)]
    pub invoice_number: Option<String>,
    /// Invoice status (`DRAFT`, `AUTHORISED`, `PAID`, …).
    #[serde(rename = "Status", default)]
    pub status: Option<String>,
    /// Invoice total.
    #[serde(rename = "Total", default)]
    pub total: Option<f64>,
    /// Last-update timestamp.
    #[serde(rename = "UpdatedDateUTC", default)]
    pub updated_date_utc: Option<DateTime<Utc>>,
}

/// `GET /api.xro/2.0/Invoices` envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XeroInvoicesResponse {
    /// Invoices on this page.
    #[serde(rename = "Invoices", default)]
    pub invoices: Vec<XeroInvoice>,
}

/// Xero webhook payload (`{events:[…]}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XeroWebhookPayload {
    /// Ordered batch of events.
    #[serde(default)]
    pub events: Vec<XeroEvent>,
}

/// One Xero webhook event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XeroEvent {
    /// Resource id the event concerns.
    #[serde(rename = "resourceId", default)]
    pub resource_id: String,
    /// Event category (`INVOICE`, `CONTACT`, …).
    #[serde(rename = "eventCategory", default)]
    pub event_category: Option<String>,
    /// Event type (`CREATE`, `UPDATE`).
    #[serde(rename = "eventType", default)]
    pub event_type: Option<String>,
    /// Event timestamp.
    #[serde(rename = "eventDateUtc", default)]
    pub event_date_utc: Option<DateTime<Utc>>,
}

/// Xero connector.
pub struct XeroConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for XeroConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XeroConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl XeroConnector {
    /// Construct a Xero connector.
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

    /// Override the Xero API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the invoices page size. Clamped to `[1, 1000]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 1000);
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

    fn tenant_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("tenant_id")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("xero: auth_config_json.tenant_id is required".into())
            })
    }

    /// Walk every invoices page until a short page is returned. When
    /// `if_modified_since` is set it is sent as the `If-Modified-Since`
    /// header so Xero returns only invoices changed since that instant.
    fn paginate_invoices(
        &self,
        base_url: &str,
        tenant: &str,
        token: &OAuth2Token,
        if_modified_since: Option<&str>,
    ) -> Result<Vec<XeroInvoice>> {
        let mut out = Vec::<XeroInvoice>::new();
        let mut headers: Vec<(&str, &str)> = vec![("Xero-tenant-id", tenant)];
        if let Some(ims) = if_modified_since {
            headers.push(("If-Modified-Since", ims));
        }
        for page in 1..=MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/api.xro/2.0/Invoices?page={page}&pageSize={}&order=UpdatedDateUTC%20ASC",
                self.page_size
            );
            let resp: XeroInvoicesResponse = bearer_get_json(
                &self.transport,
                "xero",
                "/api.xro/2.0/Invoices",
                &url,
                token,
                &headers,
            )?;
            let returned = resp.invoices.len();
            out.extend(resp.invoices);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "xero invoices exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn invoice_to_event(i: &XeroInvoice, kind: &str) -> ConnectorEvent {
    let occurred_at = i.updated_date_utc.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(i.invoice_id.clone());
    match kind {
        "create" => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

/// Format an instant as an RFC1123 `If-Modified-Since` header value.
fn http_date(dt: DateTime<Utc>) -> String {
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

impl Connector for XeroConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(tok) = config
            .auth_config_json
            .get("access_token")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                tok,
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
                    "xero authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let tenant = Self::tenant_id(config)?;
        let invoices = self.paginate_invoices(&base_url, &tenant, token, None)?;
        let mut events = Vec::with_capacity(invoices.len());
        let mut cursor = WatermarkCursor::empty();
        for i in &invoices {
            events.push(invoice_to_event(i, "create"));
            if let Some(t) = i.updated_date_utc {
                cursor.observe(t, &i.invoice_id);
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
        let tenant = Self::tenant_id(config)?;
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let ims = prior.watermark().map(http_date);
        let invoices = self.paginate_invoices(&base_url, &tenant, token, ims.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for i in &invoices {
            // `If-Modified-Since` is whole-second and inclusive, so the
            // boundary invoice is re-returned every run; dedup it while
            // still surfacing a brand-new invoice sharing that second.
            let Some(t) = i.updated_date_utc else {
                continue;
            };
            if !prior.should_emit(t, &i.invoice_id) {
                continue;
            }
            events.push(invoice_to_event(i, "update"));
            cursor.observe(t, &i.invoice_id);
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
        let tenant = Self::tenant_id(config)?;
        let id_enc = percent_encode_path_component(document_id.as_str());
        let url = format!("{base_url}/api.xro/2.0/Invoices/{id_enc}");
        let resp: XeroInvoicesResponse = bearer_get_json(
            &self.transport,
            "xero",
            "/api.xro/2.0/Invoices/{id}",
            &url,
            token,
            &[("Xero-tenant-id", tenant.as_str())],
        )?;
        let invoice = resp.invoices.into_iter().next().ok_or_else(|| {
            ConnectorError::Sync(format!(
                "xero invoice {} not found in response",
                document_id.as_str()
            ))
        })?;

        let title = invoice
            .invoice_number
            .clone()
            .unwrap_or_else(|| format!("Invoice {}", document_id.as_str()));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(status) = invoice.status.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Status:** ");
            md.push_str(status);
            md.push_str("\n\n");
        }
        if let Some(total) = invoice.total {
            let _ = write!(md, "**Total:** {total}\n\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "xero",
                "tenant_id": tenant,
                "invoice_id": invoice.invoice_id,
                "status": invoice.status,
            }))
            .with_source_url(format!(
                "https://go.xero.com/AccountsReceivable/View.aspx?InvoiceID={}",
                document_id.as_str()
            )))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Xero webhooks are configured once in the developer portal
        // (there is no create-webhook REST endpoint), so we do not
        // issue an HTTP call here. We surface the operator-provided
        // signing key as the secret so the substrate can validate the
        // `x-xero-signature` HMAC on delivery.
        let _ = token;
        let secret = config
            .auth_config_json
            .get("webhook_signing_key")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "xero subscribe_webhook: auth_config_json.webhook_signing_key is required"
                        .into(),
                )
            })?;
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        if let Ok(tenant) = Self::tenant_id(config) {
            subscription.provider_subscription_id = Some(tenant);
        }
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: XeroWebhookPayload = serde_json::from_slice(body)?;
        let mut events = Vec::with_capacity(payload.events.len());
        for ev in &payload.events {
            if ev.resource_id.is_empty() {
                continue;
            }
            let occurred_at = ev.event_date_utc.unwrap_or_else(Utc::now);
            let id = SourceDocumentId::new(ev.resource_id.clone());
            let mapped = match ev.event_type.as_deref() {
                Some("CREATE") => ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                },
                _ => ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                },
            };
            events.push(mapped);
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
                "xero-access",
                "xero-refresh",
                Utc::now() + Duration::hours(1),
                "accounting.transactions.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Xero, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "xero-tok",
                "tenant_id": "tenant-1",
                "webhook_signing_key": "sign-key",
                "api_base_url": "https://api.test/xero",
            }))
    }

    fn invoice(id: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "InvoiceID": id, "InvoiceNumber": format!("INV-{id}"),
            "Status": "AUTHORISED", "Total": 10.0, "UpdatedDateUTC": updated
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn page_url(page: u32) -> String {
        format!("https://api.test/xero/api.xro/2.0/Invoices?page={page}&pageSize=100&order=UpdatedDateUTC%20ASC")
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "xero-tok"
        );
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Xero, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "authorization_code": "abc", "tenant_id": "t" }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "xero-access"
        );
    }

    #[test]
    fn initial_sync_sends_tenant_header_and_paginates() {
        let transport = Arc::new(MockHttpTransport::new());
        let full: Vec<serde_json::Value> = (0..100)
            .map(|i| invoice(&format!("i{i}"), "2024-01-01T00:00:00Z"))
            .collect();
        transport.expect(
            HttpMethod::Get,
            page_url(1),
            ok_json(&serde_json::json!({ "Invoices": full })),
        );
        transport.expect(
            HttpMethod::Get,
            page_url(2),
            ok_json(&serde_json::json!({ "Invoices": [invoice("i100", "2024-01-02T00:00:00Z")] })),
        );
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 101);
        let rec = transport.recorded();
        assert!(rec[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Xero-tenant-id" && v == "tenant-1"));
    }

    #[test]
    fn incremental_sync_sends_if_modified_since_and_dedupes() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            page_url(1),
            ok_json(&serde_json::json!({ "Invoices": [
                invoice("old", "2024-01-01T00:00:00Z"),
                invoice("boundary_new", "2024-01-01T00:00:00Z"),
                invoice("new", "2024-02-01T00:00:00Z"),
            ] })),
        );
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior cursor: watermark at 2024-01-01 with id "old" already seen.
        state.cursor = Some("2024-01-01T00:00:00+00:00|old".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // "old" deduped; the brand-new same-second "boundary_new" surfaces,
        // as does the strictly-newer "new".
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["boundary_new", "new"]);
        let rec = transport.recorded();
        assert!(rec[0].headers.iter().any(|(k, _)| k == "If-Modified-Since"));
    }

    #[test]
    fn initial_sync_requires_tenant_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Xero, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "access_token": "t" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn subscribe_webhook_uses_signing_key_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/xero")
            .unwrap();
        assert_eq!(sub.secret.expose(), "sign-key");
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("tenant-1"));
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn subscribe_webhook_requires_signing_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Xero, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "access_token": "t", "tenant_id": "t" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.subscribe_webhook(&cfg, &tok, "https://hook").unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }

    #[test]
    fn webhook_emits_every_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "events": [
                { "resourceId": "r1", "eventCategory": "INVOICE", "eventType": "CREATE", "eventDateUtc": "2024-03-01T00:00:00Z" },
                { "resourceId": "r2", "eventCategory": "INVOICE", "eventType": "UPDATE" }
            ]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/xero/api.xro/2.0/Invoices/inv7",
            ok_json(&serde_json::json!({ "Invoices": [
                { "InvoiceID": "inv7", "InvoiceNumber": "INV-007", "Status": "PAID", "Total": 99.5 }
            ] })),
        );
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("inv7"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# INV-007"));
        assert!(body.contains("**Status:** PAID"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/xero/api.xro/2.0/Invoices/none",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = XeroConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("none"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
