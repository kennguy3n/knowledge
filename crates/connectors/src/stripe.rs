//! Stripe connector — Stripe REST API + Stripe webhooks.
//!
//! * `initial_sync` walks `GET /v1/customers` and pages via Stripe's
//!   cursor pagination (`starting_after=<last object id>` /
//!   `has_more`).
//! * `incremental_sync` adds a `created[gte]=<unix>` filter keyed off
//!   the prior watermark so steady-state runs only pull objects
//!   created since the last cursor.
//! * `fetch_content` reads `GET /v1/customers/{id}` and renders a
//!   Markdown summary (name, email, description, metadata).
//! * `subscribe_webhook` POSTs `/v1/webhook_endpoints` to register a
//!   push endpoint; Stripe returns the endpoint id + signing secret,
//!   both persisted into the [`WebhookSubscription`].
//! * `handle_webhook_event` parses Stripe's `Event` envelope —
//!   `customer.created`, `customer.updated`, `customer.deleted`.
//!
//! Authentication accepts either a Stripe secret API key
//! (`auth_config_json.api_key`, the common case) wrapped as a
//! non-refreshable bearer token, or an OAuth2 `authorization_code`
//! exchanged through the injected [`OAuth2CodeExchange`] for Stripe
//! Connect platforms. Production wiring runs over [`HttpTransport`];
//! unit tests pass `MockHttpTransport`.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_form, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Stripe REST base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.stripe.com";

/// Page size for list endpoints. Stripe's documented max is 100.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on the number of list pages a single sync walks —
/// guards against a server that never sets `has_more = false`.
pub const MAX_LIST_PAGES: usize = 10_000;

/// OAuth2 / API-key scope recorded on the synthesised token. Stripe
/// secret keys are not scoped per-request; the value is descriptive.
const DEFAULT_SCOPE: &str = "read_only";

/// One Stripe customer (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StripeCustomer {
    /// Stripe object id (e.g. `cus_123`).
    pub id: String,
    /// Customer display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Customer email.
    #[serde(default)]
    pub email: Option<String>,
    /// Free-form description.
    #[serde(default)]
    pub description: Option<String>,
    /// Unix creation timestamp (seconds).
    #[serde(default)]
    pub created: Option<i64>,
}

/// One page of a Stripe list response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StripeListResponse {
    /// Objects on this page.
    #[serde(default)]
    pub data: Vec<StripeCustomer>,
    /// Whether more pages follow this one.
    #[serde(default)]
    pub has_more: bool,
}

/// Response from `POST /v1/webhook_endpoints`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StripeWebhookEndpoint {
    /// Endpoint id (e.g. `we_123`).
    #[serde(default)]
    pub id: String,
    /// Signing secret (`whsec_…`) returned on creation.
    #[serde(default)]
    pub secret: Option<String>,
}

/// Stripe webhook `Event` envelope (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StripeEvent {
    /// Event id (e.g. `evt_123`).
    #[serde(default)]
    pub id: String,
    /// Event type (e.g. `customer.created`).
    #[serde(rename = "type", default)]
    pub event_type: String,
    /// Unix timestamp the event was created.
    #[serde(default)]
    pub created: Option<i64>,
    /// Event data wrapper.
    #[serde(default)]
    pub data: StripeEventData,
}

/// `data` wrapper of a Stripe event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StripeEventData {
    /// The API resource the event concerns.
    #[serde(default)]
    pub object: StripeEventObject,
}

/// The `data.object` of a Stripe event (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StripeEventObject {
    /// Object id (e.g. `cus_123`).
    #[serde(default)]
    pub id: String,
}

/// Stripe connector.
pub struct StripeConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for StripeConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl StripeConnector {
    /// Construct a Stripe connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// optional Stripe Connect `authorization_code` exchange. Tests
    /// pass `MockHttpTransport`.
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

    /// Override the Stripe REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 100]` per
    /// Stripe's documented maximum.
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

    /// Walk every `/v1/customers` page until `has_more` is false, an
    /// empty page is returned, or [`MAX_LIST_PAGES`] is hit.
    ///
    /// `created_gte` adds a `created[gte]=<unix>` filter for
    /// incremental runs; `None` walks the full list.
    fn paginate_customers(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        created_gte: Option<i64>,
    ) -> Result<Vec<StripeCustomer>> {
        let mut out = Vec::<StripeCustomer>::new();
        let mut starting_after: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut url = format!("{base_url}/v1/customers?limit={}", self.page_size);
            if let Some(ts) = created_gte {
                let _ = write!(url, "&created[gte]={ts}");
            }
            if let Some(ref cursor) = starting_after {
                let _ = write!(
                    url,
                    "&starting_after={}",
                    percent_encode_path_component(cursor)
                );
            }
            let resp: StripeListResponse =
                bearer_get_json(&self.transport, "stripe", "/v1/customers", &url, token, &[])?;
            let last_id = resp.data.last().map(|c| c.id.clone());
            let returned = resp.data.len();
            out.extend(resp.data);
            if !resp.has_more || returned == 0 {
                return Ok(out);
            }
            // `has_more` is true but the page carried no usable cursor
            // — stop rather than loop forever on a malformed response.
            match last_id {
                Some(id) => starting_after = Some(id),
                None => return Ok(out),
            }
        }
        Err(ConnectorError::Sync(format!(
            "stripe /v1/customers exceeded {MAX_LIST_PAGES} pages without exhausting has_more"
        )))
    }
}

fn unix_to_utc(secs: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(secs, 0)
}

fn customer_to_event(c: &StripeCustomer, kind: &str) -> ConnectorEvent {
    let occurred_at = c.created.and_then(unix_to_utc).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(c.id.clone());
    match kind {
        "delete" => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
        "update" => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for StripeConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        // Prefer a directly-configured secret API key (the common
        // Stripe case). Fall back to the OAuth2 code exchange for
        // Stripe Connect platforms that onboard via `authorization_code`.
        if let Some(key) = config
            .auth_config_json
            .get("api_key")
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
                    "stripe authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let customers = self.paginate_customers(&base_url, token, None)?;
        let mut events = Vec::with_capacity(customers.len());
        let mut watermark: Option<i64> = None;
        for c in &customers {
            events.push(customer_to_event(c, "create"));
            if let Some(t) = c.created {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_string()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let prior: Option<i64> = state.cursor.as_deref().and_then(|s| s.parse::<i64>().ok());
        let customers = self.paginate_customers(&base_url, token, prior)?;
        let mut events = Vec::with_capacity(customers.len());
        let mut watermark = prior;
        for c in &customers {
            // Stripe's `created[gte]` is inclusive, so the boundary
            // object from the prior run is returned again. Skip
            // anything at or before the prior watermark so the
            // substrate sees each customer at most once.
            if let (Some(prev), Some(t)) = (prior, c.created) {
                if t <= prev {
                    continue;
                }
            }
            events.push(customer_to_event(c, "update"));
            if let Some(t) = c.created {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_string()),
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
        let url = format!("{base_url}/v1/customers/{id_enc}");
        let customer: StripeCustomer = bearer_get_json(
            &self.transport,
            "stripe",
            "/v1/customers/{id}",
            &url,
            token,
            &[],
        )?;

        let name = customer.name.clone().unwrap_or_default();
        let mut md = String::new();
        let title = if name.is_empty() {
            id.to_string()
        } else {
            name.clone()
        };
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(email) = customer.email.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Email:** ");
            md.push_str(email);
            md.push_str("\n\n");
        }
        if let Some(desc) = customer.description.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(desc);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();

        let source_url = format!("https://dashboard.stripe.com/customers/{id}");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "stripe",
                "customer_id": id,
                "email": customer.email,
            }))
            .with_source_url(source_url))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/v1/webhook_endpoints");
        // Stripe's webhook-endpoint create accepts
        // `application/x-www-form-urlencoded` with repeated
        // `enabled_events[]` keys.
        let resp: StripeWebhookEndpoint = bearer_post_form(
            &self.transport,
            "stripe",
            "/v1/webhook_endpoints",
            &url,
            token,
            &[],
            &[
                ("url", callback_url),
                ("enabled_events[]", "customer.created"),
                ("enabled_events[]", "customer.updated"),
                ("enabled_events[]", "customer.deleted"),
            ],
        )?;
        if resp.id.is_empty() {
            return Err(ConnectorError::Webhook(
                "stripe /v1/webhook_endpoints returned no endpoint id".into(),
            ));
        }
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            // Stripe returns the signing secret only on creation; fall
            // back to a configured value, then a placeholder.
            WebhookSecret::new(resp.secret.clone().unwrap_or_else(|| {
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("stripe-webhook-secret")
                    .to_string()
            })),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Stripe posts exactly one `Event` per webhook request.
        let event: StripeEvent = serde_json::from_slice(body)?;
        let occurred_at = event.created.and_then(unix_to_utc).unwrap_or_else(Utc::now);
        let id = SourceDocumentId::new(event.data.object.id.clone());
        let mapped = match event.event_type.as_str() {
            "customer.created" => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at,
            },
            "customer.updated" => ConnectorEvent::DocumentUpdated {
                document_id: id,
                occurred_at,
            },
            "customer.deleted" => ConnectorEvent::DocumentDeleted {
                document_id: id,
                occurred_at,
            },
            other => {
                return Err(ConnectorError::Webhook(format!(
                    "unknown Stripe event type: {other}"
                )))
            }
        };
        Ok(vec![mapped])
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
                "stripe-access",
                "stripe-refresh",
                Utc::now() + Duration::hours(1),
                "read_only",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Stripe, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "sk_test_123",
                "api_base_url": "https://api.test/stripe",
            }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Stripe, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/stripe",
            }))
    }

    fn customer(id: &str, created: i64) -> serde_json::Value {
        serde_json::json!({ "id": id, "name": "Acme", "created": created })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "sk_test_123");
        assert!(tok.refresh_token.is_none());
    }

    #[test]
    fn authenticate_falls_back_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg_oauth()).unwrap();
        assert_eq!(tok.access_token.expose(), "stripe-access");
    }

    #[test]
    fn authenticate_requires_key_or_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Stripe, AuthKind::ApiKey, ScopeId::new_v4());
        let err = c.authenticate(&bare).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_and_watermark() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/stripe/v1/customers?limit=50",
            ok_json(&serde_json::json!({
                "data": [customer("cus_1", 1_700_000_000)],
                "has_more": false,
            })),
        );
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("1700000000"));
    }

    #[test]
    fn initial_sync_paginates_via_starting_after() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/stripe/v1/customers?limit=50",
            ok_json(&serde_json::json!({
                "data": [customer("cus_1", 1_700_000_000), customer("cus_2", 1_700_000_001)],
                "has_more": true,
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/stripe/v1/customers?limit=50&starting_after=cus_2",
            ok_json(&serde_json::json!({
                "data": [customer("cus_3", 1_700_000_002)],
                "has_more": false,
            })),
        );
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_created_gte_and_dedupes_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let prior = 1_700_000_000_i64;
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/stripe/v1/customers?limit=50&created[gte]={prior}"),
            ok_json(&serde_json::json!({
                "data": [customer("cus_boundary", prior), customer("cus_new", prior + 10)],
                "has_more": false,
            })),
        );
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(prior.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1, "boundary object must be skipped");
        assert_eq!(res.events[0].document_id().as_str(), "cus_new");
        assert_eq!(res.next_cursor.as_deref(), Some(&*(prior + 10).to_string()));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/stripe/v1/customers?limit=50",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn subscribe_webhook_registers_and_captures_id_and_secret() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/stripe/v1/webhook_endpoints",
            ok_json(&serde_json::json!({ "id": "we_42", "secret": "whsec_abc" })),
        );
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/stripe")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("we_42"));
        assert_eq!(sub.secret.expose(), "whsec_abc");
    }

    #[test]
    fn webhook_parses_customer_created() {
        let body = serde_json::json!({
            "id": "evt_1",
            "type": "customer.created",
            "created": 1_700_000_000,
            "data": { "object": { "id": "cus_99" } }
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "cus_99");
    }

    #[test]
    fn webhook_parses_customer_deleted() {
        let body = serde_json::json!({
            "id": "evt_2",
            "type": "customer.deleted",
            "data": { "object": { "id": "cus_gone" } }
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_unknown_event_errors() {
        let body =
            serde_json::json!({ "type": "charge.refunded", "data": {"object": {"id": "ch_1"}} });
        let transport = Arc::new(MockHttpTransport::new());
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn fetch_content_assembles_customer_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/stripe/v1/customers/cus_7",
            ok_json(&serde_json::json!({
                "id": "cus_7",
                "name": "Globex",
                "email": "ops@globex.test",
                "description": "Enterprise account",
            })),
        );
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("cus_7"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Globex"));
        assert!(body.contains("ops@globex.test"));
        assert!(body.contains("Enterprise account"));
        assert_eq!(fc.title.as_deref(), Some("Globex"));
        assert_eq!(fc.mime_type, "text/markdown");
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/stripe/v1/customers/cus_missing",
            MockResponse::status(404, br#"{"error":{"message":"No such customer"}}"#.to_vec()),
        );
        let c = StripeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("cus_missing"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }
}
