//! Brevo (Sendinblue) connector — Brevo (Sendinblue) partner API (`https://api.brevo.com/v3`).
//!
//! Brevo (Sendinblue) — French marketing API (contacts, campaigns).
//!
//! Authentication mirrors the SEA/GCC batches' dual-credential
//! pattern: a static API key presented in the provider-native
//! `X-Sendinblue-Api-Key` header (read from `auth_config_json.api_key`),
//! falling back to the injected [`OAuth2CodeExchange`] when a
//! rotating `authorization_code` grant is configured instead. The
//! request auth header is chosen from the token's provenance
//! (recorded in [`OAuth2Token::token_type`]).
//!
//! * `initial_sync` / `incremental_sync` page `/v1/contacts`
//!   (`limit` / `offset`), tracking the maximum `updated_at` as an
//!   RFC-3339 watermark; incremental runs add `modified_since` and
//!   dedup the inclusive boundary row.
//! * `fetch_content` GETs a single contact (`/v1/contacts/{id}`).
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

/// Default Brevo (Sendinblue) API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.brevo.com/v3";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "contacts";
/// `OAuth2Token::token_type` marker for a static API-key credential.
/// Distinguishes the API-key auth path (provider-native
/// `X-Sendinblue-Api-Key` header) from an OAuth-issued bearer token.
pub const API_KEY_TOKEN_TYPE: &str = "ApiKey";
/// Page size for contact listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct SendinbluePage {
    #[serde(default)]
    data: Vec<SendinblueRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SendinblueRecord {
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
struct SendinblueWebhookEvent {
    #[serde(default)]
    contact_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Brevo (Sendinblue) connector.
pub struct SendinblueConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for SendinblueConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendinblueConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl SendinblueConnector {
    /// Construct a Brevo (Sendinblue) connector.
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

    /// Override the Brevo (Sendinblue) API base URL.
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
            return Err(classify_failure("sendinblue", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "sendinblue {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    fn paginate_records(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        modified_since: Option<&str>,
    ) -> Result<Vec<SendinblueRecord>> {
        let mut records = Vec::<SendinblueRecord>::new();
        for page in 0..MAX_PAGES {
            let offset = page * self.page_size as usize;
            let mut url = format!(
                "{base_url}/v1/contacts?limit={}&offset={offset}",
                self.page_size
            );
            if let Some(since) = modified_since {
                url.push_str("&modified_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: SendinbluePage = self.http_get("/v1/contacts", &url, token)?;
            let count = resp.data.len();
            records.extend(resp.data);
            if count < self.page_size as usize {
                return Ok(records);
            }
        }
        Err(ConnectorError::Sync(format!(
            "sendinblue /v1/contacts exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Attach the auth header matching the token's provenance: a static
/// API-key token (tagged [`API_KEY_TOKEN_TYPE`] in `authenticate`)
/// goes in the provider-native `X-Sendinblue-Api-Key` header, while an
/// OAuth-issued token is sent as `Authorization: <scheme> <token>`
/// (scheme from `token_type`, defaulting to `Bearer`).
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    apply_auth_by_provenance(req, token, "X-Sendinblue-Api-Key", API_KEY_TOKEN_TYPE)
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn record_watermark(o: &SendinblueRecord) -> Option<DateTime<Utc>> {
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

impl Connector for SendinblueConnector {
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
                    "sendinblue authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let records = self.paginate_records(&base_url, token, None)?;
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
        let since = prior.map(|t| t.to_rfc3339());
        let records = self.paginate_records(&base_url, token, since.as_deref())?;
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
        let url = format!("{base_url}/v1/contacts/{id_enc}");
        let record: SendinblueRecord = self.http_get("/v1/contacts/{id}", &url, token)?;
        let status = record.status.as_deref().unwrap_or("unknown");
        let title = record.title.as_deref().unwrap_or("(untitled)");
        let body =
            format!("# Brevo (Sendinblue) contact {id}\n\nTitle: {title}\nStatus: {status}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("Brevo (Sendinblue) contact {id}"))
            .with_metadata(serde_json::json!({
                "provider": "sendinblue",
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
        // Brevo (Sendinblue) webhooks are registered in the provider
        // dashboard; no REST endpoint creates them. Record a
        // polling-only subscription so the runtime falls back to
        // incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("sendinblue-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<SendinblueWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<SendinblueWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<SendinblueWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("sendinblue webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty sendinblue webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.contact_id).ok_or_else(|| {
                ConnectorError::Webhook("sendinblue webhook event missing contact_id".into())
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
                "contacts",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Sendinblue,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "sendinblue-key",
            "api_base_url": "https://api.test/sendinblue",
            "webhook_secret": "sendinblue-secret",
        }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Sendinblue,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "auth-code",
            "api_base_url": "https://api.test/sendinblue",
            "webhook_secret": "sendinblue-secret",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "sendinblue-key");
        assert!(token.refresh_token.is_none());
        assert_eq!(token.token_type, API_KEY_TOKEN_TYPE);
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            "https://api.test/sendinblue/v1/contacts?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "data": [ {"id": "o-1", "updated_at": "2024-01-01T00:00:00Z"} ]
            })),
        );
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
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
            .any(|(k, _)| k.eq_ignore_ascii_case("X-Sendinblue-Api-Key")));
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(
            ConnectorKind::Sendinblue,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        );
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
            "https://api.test/sendinblue/v1/contacts?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "data": [
                    {"id": "o-1", "updated_at": "2024-01-01T00:00:00Z"},
                    {"id": "o-2", "updated_at": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/sendinblue/v1/contacts?limit=2&offset=2",
            ok_json(&serde_json::json!({ "data": [ {"id": "o-3", "updated_at": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
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
            .any(|(k, v)| k.eq_ignore_ascii_case("X-Sendinblue-Api-Key") && v == "sendinblue-key"));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/sendinblue/v1/contacts?limit=2&offset=0&modified_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "data": [
                    {"id": "o-10", "updated_at": "2024-03-01T00:00:00Z"},
                    {"id": "o-11", "updated_at": "2024-06-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/sendinblue/v1/contacts?limit=2&offset=2&modified_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "data": [] })),
        );
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
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
            "https://api.test/sendinblue/v1/contacts/o-1",
            ok_json(&serde_json::json!({
                "id": "o-1",
                "status": "COMPLETED",
                "title": "Sample contact"
            })),
        );
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("o-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Brevo (Sendinblue) contact o-1"));
        assert!(body.contains("Sample contact"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(
                &cfg(),
                &tok,
                "https://substrate.example/webhooks/sendinblue",
            )
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "sendinblue-secret");
    }

    #[test]
    fn handle_webhook_event_parses_single() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SendinblueConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(
            &serde_json::json!({ "contact_id": 42, "event": "contact.updated" }),
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
}
