//! KiotViet connector — products, invoices, customers, inventory.
//!
//! KiotViet is Vietnam's #1 POS / retail SaaS. The public API
//! authenticates with an OAuth2 bearer `access_token` and scopes every
//! request to a retailer via the `Retailer` header.
//!
//! This connector ingests **invoices** as the primary document stream
//! (the highest-signal retail record); products, customers and
//! inventory are reachable through the same paginated pattern and can
//! be layered on later.
//!
//! * `initial_sync` walks `GET /invoices`, paging via the `currentItem`
//!   offset cursor until a short page is returned.
//! * `incremental_sync` adds a `lastModifiedFrom` filter built from the
//!   stored RFC 3339 watermark.
//! * `fetch_content` reads `GET /invoices/{id}` and renders a Markdown
//!   summary.
//! * `subscribe_webhook` registers the callback through
//!   `POST /webhooks` (KiotViet exposes a create-webhook endpoint) and
//!   stores the returned subscription id.
//! * `handle_webhook_event` parses an `invoice.update` payload keyed by
//!   invoice id.
//!
//! `authenticate` accepts a configured `access_token` or exchanges an
//! OAuth2 `authorization_code` through the injected
//! [`OAuth2CodeExchange`].

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, classify_failure, percent_encode_form_component,
    percent_encode_path_component, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default KiotViet public API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://public.kiotapi.com";

/// Default page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured access token.
const DEFAULT_SCOPE: &str = "invoices.read";

/// One KiotViet invoice (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KiotInvoice {
    /// Invoice id.
    #[serde(default)]
    pub id: i64,
    /// Invoice code.
    #[serde(default)]
    pub code: Option<String>,
    /// Invoice total.
    #[serde(default)]
    pub total: Option<f64>,
    /// Last-modified timestamp (RFC 3339).
    #[serde(default, rename = "modifiedDate", alias = "createdDate")]
    pub modified_date: Option<DateTime<Utc>>,
}

/// Envelope for the invoice list endpoint (`{ "data": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KiotInvoicesResponse {
    /// Invoices on this page.
    #[serde(default)]
    pub data: Vec<KiotInvoice>,
}

/// Response from the create-webhook endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KiotWebhookResponse {
    /// Provider subscription id.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
}

/// Webhook payload (`{ "Notifications": [{ "Data": [{ "Id": .. }] }] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KiotWebhookPayload {
    /// Event type (`invoice.update`, …).
    #[serde(default, rename = "Type")]
    pub event_type: Option<String>,
    /// Notification batches.
    #[serde(default, rename = "Notifications")]
    pub notifications: Vec<KiotNotification>,
}

/// One notification batch within a webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KiotNotification {
    /// Affected entities.
    #[serde(default, rename = "Data")]
    pub data: Vec<KiotNotificationData>,
}

/// One affected entity within a notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KiotNotificationData {
    /// Entity id.
    #[serde(default, rename = "Id")]
    pub id: i64,
}

/// KiotViet connector.
pub struct KiotVietConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for KiotVietConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiotVietConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl KiotVietConnector {
    /// Construct a KiotViet connector.
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

    /// Override the public API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 100]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 100);
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

    /// Read the retailer slug, required on every request as the
    /// `Retailer` header.
    fn retailer(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("retailer")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Auth("kiotviet: auth_config_json.retailer is required".into())
            })
    }

    /// Walk every invoice page until a short page is returned.
    fn paginate_invoices(
        &self,
        base_url: &str,
        retailer: &str,
        token: &OAuth2Token,
        modified_from: Option<&str>,
    ) -> Result<Vec<KiotInvoice>> {
        let mut out = Vec::<KiotInvoice>::new();
        let filter = modified_from.map_or_else(String::new, |ts| {
            format!("&lastModifiedFrom={}", percent_encode_form_component(ts))
        });
        for page in 0..MAX_LIST_PAGES {
            let current_item = page * self.page_size as usize;
            let url = format!(
                "{base_url}/invoices?currentItem={current_item}&pageSize={}{filter}",
                self.page_size
            );
            let resp: KiotInvoicesResponse = bearer_get_json(
                &self.transport,
                "kiotviet",
                "/invoices",
                &url,
                token,
                &[("Retailer", retailer)],
            )?;
            let returned = resp.data.len();
            out.extend(resp.data);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "kiotviet /invoices exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn invoice_to_event(i: &KiotInvoice, created: bool) -> ConnectorEvent {
    let occurred_at = i.modified_date.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(i.id.to_string());
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

impl Connector for KiotVietConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        // The retailer slug is mandatory for every request.
        Self::retailer(config)?;
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
                    "kiotviet authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let retailer = Self::retailer(config)?;
        let invoices = self.paginate_invoices(&base_url, &retailer, token, None)?;
        let mut events = Vec::with_capacity(invoices.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for i in &invoices {
            events.push(invoice_to_event(i, true));
            if let Some(ts) = i.modified_date {
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
        let retailer = Self::retailer(config)?;
        let prior: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let invoices =
            self.paginate_invoices(&base_url, &retailer, token, state.cursor.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for i in &invoices {
            if let (Some(prev), Some(ts)) = (prior, i.modified_date) {
                if ts <= prev {
                    continue;
                }
            }
            events.push(invoice_to_event(i, false));
            if let Some(ts) = i.modified_date {
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
        let retailer = Self::retailer(config)?;
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base_url}/invoices/{id_enc}");
        let invoice: KiotInvoice = bearer_get_json(
            &self.transport,
            "kiotviet",
            "/invoices/{id}",
            &url,
            token,
            &[("Retailer", retailer.as_str())],
        )?;

        let code = invoice
            .code
            .clone()
            .unwrap_or_else(|| invoice.id.to_string());
        let title = format!("Invoice {code}");
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(total) = invoice.total {
            let _ = writeln!(md, "**Total:** {total} VND\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "kiotviet",
                "invoice_id": invoice.id,
                "code": invoice.code,
                "total": invoice.total,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let retailer = Self::retailer(config)?;
        let url = format!("{base_url}/webhooks");
        let body = serde_json::to_vec(&serde_json::json!({
            "Webhook": {
                "Type": "invoice.update",
                "Url": callback_url,
                "IsActive": true,
            }
        }))
        .map_err(|e| ConnectorError::Webhook(format!("kiotviet webhook body serialise: {e}")))?;
        let req = HttpRequest::post(url, body)
            .with_bearer(token.access_token.expose())
            .with_header("Accept", "application/json")
            .with_header("Content-Type", "application/json")
            .with_header("Retailer", retailer.as_str());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("kiotviet", "/webhooks", &resp));
        }
        let parsed: KiotWebhookResponse = serde_json::from_slice(&resp.body).unwrap_or_default();
        let provider_id = parsed.id.map(|v| match v {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        });
        // No per-subscription signing secret is returned; the webhook
        // secret is the retailer's configured token, validated by the
        // substrate out of band.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(retailer.as_str());
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: KiotWebhookPayload = serde_json::from_slice(body)?;
        let mut events = Vec::new();
        let created = payload.event_type.as_deref() == Some("invoice.create");
        for notification in &payload.notifications {
            for entity in &notification.data {
                let id = SourceDocumentId::new(entity.id.to_string());
                events.push(if created {
                    ConnectorEvent::DocumentCreated {
                        document_id: id,
                        occurred_at: Utc::now(),
                    }
                } else {
                    ConnectorEvent::DocumentUpdated {
                        document_id: id,
                        occurred_at: Utc::now(),
                    }
                });
            }
        }
        if events.is_empty() {
            return Err(ConnectorError::Webhook(
                "kiotviet webhook payload contained no notification data".into(),
            ));
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
                "kv-access",
                "kv-refresh",
                Utc::now() + Duration::hours(1),
                "invoices.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::KiotViet, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "kv_tok_123",
                "retailer": "demo-shop",
                "api_base_url": "https://api.test/kiotviet",
                "webhook_secret": "kv-secret",
            }))
    }

    fn invoice(id: i64, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "code": format!("HD{id}"),
            "total": 120_000.0,
            "modifiedDate": updated,
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_requires_retailer() {
        let c = KiotVietConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "kv_tok_123"
        );
        let missing =
            ConnectorConfig::new(ConnectorKind::KiotViet, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "access_token": "t" }));
        assert!(matches!(
            c.authenticate(&missing).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn initial_sync_emits_created_with_retailer_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/kiotviet/invoices?currentItem=0&pageSize=50",
            ok_json(&serde_json::json!({ "data": [invoice(1, "2024-01-01T00:00:00Z")] })),
        );
        let c = KiotVietConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "1");
        let rec = &transport.recorded()[0];
        assert!(rec
            .headers
            .iter()
            .any(|(k, v)| k == "Retailer" && v == "demo-shop"));
        assert!(rec
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer kv_tok_123"));
    }

    #[test]
    fn incremental_sync_filters_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/kiotviet/invoices?currentItem=0&pageSize=50&lastModifiedFrom=2024-01-01T00%3A00%3A00%2B00%3A00",
            ok_json(&serde_json::json!({ "data": [
                invoice(1, "2024-01-01T00:00:00Z"),
                invoice(2, "2024-08-01T00:00:00Z"),
            ] })),
        );
        let c = KiotVietConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "2");
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/kiotviet/invoices?currentItem=0&pageSize=50",
            MockResponse::status(401, b"bad".to_vec()),
        );
        let c = KiotVietConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/kiotviet/invoices/1",
            ok_json(&invoice(1, "2024-01-01T00:00:00Z")),
        );
        let c = KiotVietConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Invoice HD1"));
    }

    #[test]
    fn subscribe_webhook_posts_and_stores_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/kiotviet/webhooks",
            ok_json(&serde_json::json!({ "id": 42 })),
        );
        let c = KiotVietConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/kv")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("42"));
        assert_eq!(sub.secret.expose(), "kv-secret");
    }

    #[test]
    fn handle_webhook_event_maps_nested_data() {
        let c = KiotVietConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({
            "Type": "invoice.update",
            "Notifications": [{ "Data": [{ "Id": 7 }, { "Id": 8 }] }],
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].document_id().as_str(), "7");
        assert_eq!(evs[1].document_id().as_str(), "8");
    }

    #[test]
    fn handle_webhook_event_empty_errors() {
        let c = KiotVietConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "Type": "invoice.update", "Notifications": [] });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
