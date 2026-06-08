//! VTEX connector — VTEX partner API (`https://api.vtex.com`).
//!
//! VTEX — Latin American commerce API (orders, catalog, payments).
//!
//! Authentication mirrors the SEA/GCC batches' dual-credential
//! pattern: a static API key presented in the provider-native
//! `X-Vtex-Api-Key` header (read from `auth_config_json.api_key`),
//! falling back to the injected [`OAuth2CodeExchange`] when a
//! rotating `authorization_code` grant is configured instead. The
//! request auth header is chosen from the token's provenance
//! (recorded in [`OAuth2Token::token_type`]).
//!
//! * `initial_sync` / `incremental_sync` page `/v1/orders`
//!   (`limit` / `offset`), tracking the maximum `updated_at` as an
//!   RFC-3339 watermark; incremental runs add `modified_since` and
//!   dedup the inclusive boundary row.
//! * `fetch_content` GETs a single order (`/v1/orders/{id}`).
//! * Webhooks are configured in the provider dashboard, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payload.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    apply_auth_by_provenance, classify_failure, percent_encode_path_component, Connector,
    ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent,
    HttpRequest, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SyncRunResult, SyncState, WatermarkCursor, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default VTEX API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.vtex.com";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "orders";
/// `OAuth2Token::token_type` marker for a static API-key credential.
/// Distinguishes the API-key auth path (provider-native
/// `X-Vtex-Api-Key` header) from an OAuth-issued bearer token.
pub const API_KEY_TOKEN_TYPE: &str = "ApiKey";
/// Page size for order listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct VtexPage {
    #[serde(default)]
    data: Vec<VtexRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VtexRecord {
    #[serde(default)]
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct VtexWebhookEvent {
    #[serde(default)]
    order_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// VTEX connector.
pub struct VtexConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for VtexConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtexConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl VtexConnector {
    /// Construct a VTEX connector.
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

    /// Override the VTEX API base URL.
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
            return Err(classify_failure("vtex", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "vtex {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    fn paginate_records(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        modified_since: Option<&str>,
    ) -> Result<Vec<VtexRecord>> {
        let mut records = Vec::<VtexRecord>::new();
        for page in 0..MAX_PAGES {
            let offset = page * self.page_size as usize;
            let mut url = format!(
                "{base_url}/v1/orders?limit={}&offset={offset}",
                self.page_size
            );
            if let Some(since) = modified_since {
                url.push_str("&modified_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: VtexPage = self.http_get("/v1/orders", &url, token)?;
            let count = resp.data.len();
            records.extend(resp.data);
            if count < self.page_size as usize {
                return Ok(records);
            }
        }
        Err(ConnectorError::Sync(format!(
            "vtex /v1/orders exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Attach the auth header matching the token's provenance: a static
/// API-key token (tagged [`API_KEY_TOKEN_TYPE`] in `authenticate`)
/// goes in the provider-native `X-Vtex-Api-Key` header, while an
/// OAuth-issued token is sent as `Authorization: <scheme> <token>`
/// (scheme from `token_type`, defaulting to `Bearer`).
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    apply_auth_by_provenance(req, token, "X-Vtex-Api-Key", API_KEY_TOKEN_TYPE)
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn record_watermark(o: &VtexRecord) -> Option<DateTime<Utc>> {
    o.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| o.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for VtexConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(api_key) = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
        {
            let mut token = OAuth2Token::new_without_refresh(
                api_key,
                Utc::now() + chrono::Duration::days(365),
                DEFAULT_SCOPE,
            );
            token.token_type = API_KEY_TOKEN_TYPE.to_string();
            return Ok(token);
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "vtex authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let records = self.paginate_records(&base_url, token, None)?;
        let mut events = Vec::with_capacity(records.len());
        let mut cursor = WatermarkCursor::empty();
        for record in &records {
            let occurred_at = record_watermark(record).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(record.id.clone()),
                occurred_at,
            });
            if let Some(t) = record_watermark(record) {
                cursor.observe(t, &record.id);
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
        let records = self.paginate_records(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for record in &records {
            let Some(updated) = record_watermark(record) else {
                continue;
            };
            if !prior.should_emit(updated, &record.id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(record.id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &record.id);
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
        let url = format!("{base_url}/v1/orders/{id_enc}");
        let record: VtexRecord = self.http_get("/v1/orders/{id}", &url, token)?;
        let status = record.status.as_deref().unwrap_or("unknown");
        let title = record.title.as_deref().unwrap_or("(untitled)");
        let body = format!("# VTEX order {id}\n\nTitle: {title}\nStatus: {status}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("VTEX order {id}"))
            .with_metadata(serde_json::json!({
                "provider": "vtex",
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
        // VTEX webhooks are registered in the provider
        // dashboard; no REST endpoint creates them. Record a
        // polling-only subscription so the runtime falls back to
        // incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("vtex-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<VtexWebhookEvent> = if let Ok(batch) =
            serde_json::from_slice::<Vec<VtexWebhookEvent>>(body)
        {
            batch
        } else {
            vec![serde_json::from_slice::<VtexWebhookEvent>(body)
                .map_err(|e| ConnectorError::Webhook(format!("vtex webhook parse failed: {e}")))?]
        };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty vtex webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.order_id).ok_or_else(|| {
                ConnectorError::Webhook("vtex webhook event missing order_id".into())
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
                "orders",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Vtex, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "vtex-key",
                "api_base_url": "https://api.test/vtex",
                "webhook_secret": "vtex-secret",
            }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Vtex, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "auth-code",
                "api_base_url": "https://api.test/vtex",
                "webhook_secret": "vtex-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "vtex-key");
        assert!(token.refresh_token.is_none());
        assert_eq!(token.token_type, API_KEY_TOKEN_TYPE);
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            "https://api.test/vtex/v1/orders?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "data": [ {"id": "o-1", "updated_at": "2024-01-01T00:00:00Z"} ]
            })),
        );
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
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
            .any(|(k, _)| k.eq_ignore_ascii_case("X-Vtex-Api-Key")));
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Vtex, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates_and_sends_api_key_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vtex/v1/orders?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "data": [
                    {"id": "o-1", "updated_at": "2024-01-01T00:00:00Z"},
                    {"id": "o-2", "updated_at": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vtex/v1/orders?limit=2&offset=2",
            ok_json(&serde_json::json!({ "data": [ {"id": "o-3", "updated_at": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|o-3")
        );
        let recorded = transport.recorded();
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("X-Vtex-Api-Key") && v == "vtex-key"));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/vtex/v1/orders?limit=2&offset=0&modified_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "data": [
                    {"id": "o-10", "updated_at": "2024-03-01T00:00:00Z"},
                    {"id": "o-13", "updated_at": "2024-03-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/vtex/v1/orders?limit=2&offset=2&modified_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "data": [ {"id": "o-11", "updated_at": "2024-06-01T00:00:00Z"} ] })),
        );
        let c =
            VtexConnector::new(ConnectorInstanceId::new_v4(), transport, oauth()).with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `o-10` at the boundary instant, so the
        // cursor records that id. This run re-queries the instant inclusively
        // and must (a) NOT re-emit `o-10`, (b) still surface the brand-new
        // `o-13` that shares the same second, and (c) advance past it.
        state.cursor = Some(format!("{since}|o-10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["o-13", "o-11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|o-11")
        );
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/vtex/v1/orders/o-1",
            ok_json(&serde_json::json!({
                "id": "o-1",
                "status": "COMPLETED",
                "title": "Sample order"
            })),
        );
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("o-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# VTEX order o-1"));
        assert!(body.contains("Sample order"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/vtex")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "vtex-secret");
    }

    #[test]
    fn handle_webhook_event_parses_single() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = VtexConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body =
            serde_json::to_vec(&serde_json::json!({ "order_id": 42, "event": "order.updated" }))
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
}
