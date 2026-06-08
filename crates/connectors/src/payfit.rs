//! PayFit connector — PayFit Partner API (`https://partner-api.payfit.com`).
//!
//! PayFit — French HR/payroll API. This connector synchronises the
//! *collaborators* of a single company.
//!
//! Authentication uses a bearer token for both supported credential
//! shapes: PayFit's static customer API key *and* an OAuth2-issued
//! access token are presented identically as
//! `Authorization: Bearer <token>` (verified against the live API,
//! which rejects a custom `X-PayFit-Api-Key` header with
//! "Missing authorization header"). The static key is read from
//! `auth_config_json.api_key`; otherwise the injected
//! [`OAuth2CodeExchange`] is used with a configured
//! `authorization_code` grant.
//!
//! PayFit resources are company-scoped, so the company whose
//! collaborators are synced is named by `auth_config_json.company_id`
//! (fail-fast if missing — `GET /companies/{companyId}/collaborators`
//! is the only listing endpoint and has no global variant).
//!
//! * `initial_sync` walks every page of
//!   `GET /companies/{companyId}/collaborators`, following PayFit's
//!   opaque `meta.nextPageToken` cursor (`maxResults` page size, max
//!   50) until the token is null. Collaborator records carry no
//!   change timestamp, so each is emitted as `DocumentCreated` at the
//!   sync time and no time cursor is persisted.
//! * `incremental_sync` re-lists the collaborators (the API exposes no
//!   `modified-since` filter or per-record timestamp) and emits
//!   `DocumentUpdated`; the runtime deduplicates unchanged documents by
//!   content hash.
//! * `fetch_content` GETs a single collaborator
//!   (`/companies/{companyId}/collaborators/{id}`), which returns the
//!   bare collaborator object.
//! * Webhooks are configured in the PayFit partner dashboard, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payload.

use std::sync::Arc;

use chrono::Utc;
use connector_framework::{
    apply_auth_by_provenance, classify_failure, percent_encode_path_component, Connector,
    ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent,
    HttpRequest, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default PayFit Partner API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://partner-api.payfit.com";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "collaborators:read";
/// `OAuth2Token::token_type` marker retained for API compatibility.
/// PayFit's static API key is itself a bearer token, so the API-key
/// path keeps the default `Bearer` provenance and this marker is never
/// applied — both auth paths send `Authorization: Bearer <token>`.
pub const API_KEY_TOKEN_TYPE: &str = "ApiKey";
/// Page size for the collaborator listing (`maxResults`). PayFit caps
/// this at 50.
pub const DEFAULT_PAGE_SIZE: u32 = 50;
/// PayFit's documented hard ceiling on `maxResults`.
pub const MAX_PAGE_SIZE: u32 = 50;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

/// PayFit's paginated list envelope:
/// `{ "collaborators": [...], "meta": { "nextPageToken": ..., "count": ... } }`.
#[derive(Debug, Clone, Default, Deserialize)]
struct PayFitPage {
    #[serde(default)]
    collaborators: Vec<PayFitRecord>,
    #[serde(default)]
    meta: PayFitMeta,
}

/// PayFit's pagination metadata. `next_page_token` is `null`/absent on
/// the last page.
#[derive(Debug, Clone, Default, Deserialize)]
struct PayFitMeta {
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

/// A PayFit collaborator. PayFit uses camelCase field names and exposes
/// no create/update timestamps on this resource.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PayFitRecord {
    #[serde(default)]
    id: String,
    #[serde(default)]
    matricule: Option<String>,
    #[serde(default, rename = "firstName")]
    first_name: Option<String>,
    #[serde(default, rename = "lastName")]
    last_name: Option<String>,
    #[serde(default, rename = "terminationDate")]
    termination_date: Option<String>,
}

impl PayFitRecord {
    /// Best-effort display name for content rendering.
    fn display_name(&self) -> String {
        let first = self.first_name.as_deref().unwrap_or("").trim();
        let last = self.last_name.as_deref().unwrap_or("").trim();
        let joined = format!("{first} {last}");
        let joined = joined.trim();
        if joined.is_empty() {
            "(no name)".to_string()
        } else {
            joined.to_string()
        }
    }
}

/// A delivered PayFit webhook event.
///
/// PayFit webhooks are configured in the partner dashboard and their
/// delivered payload schema is not published in the public API
/// reference, so this parser is intentionally tolerant: it accepts the
/// collaborator identifier under PayFit's camelCase `collaboratorId`
/// (with a generic `id` fallback) and the event name under `eventType`
/// (with an `event` fallback).
#[derive(Debug, Clone, Default, Deserialize)]
struct PayFitWebhookEvent {
    #[serde(default, alias = "collaboratorId", alias = "id")]
    collaborator_id: serde_json::Value,
    #[serde(default, alias = "eventType", alias = "event")]
    event_type: String,
}

/// PayFit connector.
pub struct PayFitConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for PayFitConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayFitConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl PayFitConnector {
    /// Construct a PayFit connector.
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

    /// Override the PayFit API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size (clamped to PayFit's documented 1..=50 range).
    #[must_use]
    pub fn with_page_size(mut self, page_size: u32) -> Self {
        self.page_size = page_size.clamp(1, MAX_PAGE_SIZE);
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

    /// The company whose collaborators are synced. PayFit has no global
    /// collaborator listing, so this is required; fail fast with a clear
    /// message if it is missing.
    fn company_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("company_id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync(
                    "payfit list collaborators requires auth_config_json.company_id".into(),
                )
            })
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
            return Err(classify_failure("payfit", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "payfit {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every page of the company's collaborators, following PayFit's
    /// opaque `meta.nextPageToken` cursor until it is null/empty.
    fn paginate_records(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
    ) -> Result<Vec<PayFitRecord>> {
        let base_url = self.resolved_base_url(config);
        let company_id = Self::company_id(config)?;
        let company_enc = percent_encode_path_component(&company_id);
        let mut records = Vec::<PayFitRecord>::new();
        let mut next_page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!(
                "{base_url}/companies/{company_enc}/collaborators?maxResults={}",
                self.page_size
            );
            if let Some(cursor) = next_page_token.as_deref() {
                url.push_str("&nextPageToken=");
                url.push_str(&percent_encode_path_component(cursor));
            }
            let resp: PayFitPage =
                self.http_get("/companies/{companyId}/collaborators", &url, token)?;
            records.extend(resp.collaborators);
            match resp.meta.next_page_token {
                Some(t) if !t.is_empty() => next_page_token = Some(t),
                _ => return Ok(records),
            }
        }
        Err(ConnectorError::Sync(format!(
            "payfit /companies/{{companyId}}/collaborators exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Attach the auth header. PayFit's static API key is itself a bearer
/// token, so both the API-key and OAuth provenances send
/// `Authorization: <scheme> <token>` (scheme from `token_type`,
/// defaulting to `Bearer`). No token is ever diverted to a native
/// header, so the [`API_KEY_TOKEN_TYPE`] marker is never matched.
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    apply_auth_by_provenance(req, token, "Authorization", API_KEY_TOKEN_TYPE)
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for PayFitConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(api_key) = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
        {
            // PayFit's static API key is a bearer token, so keep the
            // default `Bearer` provenance (no native-header tag).
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
                    "payfit authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let records = self.paginate_records(config, token)?;
        let now = Utc::now();
        let mut events = Vec::with_capacity(records.len());
        for record in &records {
            if record.id.is_empty() {
                continue;
            }
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(record.id.clone()),
                occurred_at: now,
            });
        }
        // PayFit collaborators carry no change timestamp and the listing
        // exposes no incremental cursor, so there is nothing to persist.
        Ok(SyncRunResult {
            events,
            next_cursor: None,
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        _state: &SyncState,
    ) -> Result<SyncRunResult> {
        // The collaborators endpoint has no `modified-since` filter and
        // records carry no timestamps, so the only sound strategy is to
        // re-list and emit updates; the runtime deduplicates unchanged
        // documents by content hash.
        let records = self.paginate_records(config, token)?;
        let now = Utc::now();
        let mut events = Vec::with_capacity(records.len());
        for record in &records {
            if record.id.is_empty() {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(record.id.clone()),
                occurred_at: now,
            });
        }
        Ok(SyncRunResult {
            events,
            next_cursor: None,
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let base_url = self.resolved_base_url(config);
        let company_id = Self::company_id(config)?;
        let company_enc = percent_encode_path_component(&company_id);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base_url}/companies/{company_enc}/collaborators/{id_enc}");
        let record: PayFitRecord =
            self.http_get("/companies/{companyId}/collaborators/{id}", &url, token)?;
        let name = record.display_name();
        let matricule = record.matricule.as_deref().unwrap_or("(none)");
        let body = format!("# PayFit collaborator {id}\n\nName: {name}\nMatricule: {matricule}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("PayFit collaborator {name}"))
            .with_metadata(serde_json::json!({
                "provider": "payfit",
                "record_id": record.id,
                "matricule": record.matricule,
                "termination_date": record.termination_date,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // PayFit webhooks are configured in the partner dashboard; no
        // REST endpoint creates them. Record a polling-only subscription
        // so the runtime falls back to incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("payfit-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<PayFitWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<PayFitWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<PayFitWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("payfit webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty payfit webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.collaborator_id).ok_or_else(|| {
                ConnectorError::Webhook("payfit webhook event missing collaborator id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event_type = delivery.event_type.to_ascii_lowercase();
            let event = if event_type.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if event_type.contains("delete") || event_type.contains("offboard") {
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
                "collaborators:read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::PayFit, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "payfit-key",
                "api_base_url": "https://api.test/payfit",
                "company_id": "comp-1",
                "webhook_secret": "payfit-secret",
            }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::PayFit, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "auth-code",
                "api_base_url": "https://api.test/payfit",
                "company_id": "comp-1",
                "webhook_secret": "payfit-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "payfit-key");
        assert!(token.refresh_token.is_none());
        // PayFit's API key is a bearer token, not a native-header credential.
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg_oauth()).unwrap();
        assert_eq!(token.access_token.expose(), "unused");
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::PayFit, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_requires_company_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::PayFit, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "payfit-key",
                "api_base_url": "https://api.test/payfit",
            }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn initial_sync_follows_next_page_token_and_sends_bearer() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/payfit/companies/comp-1/collaborators?maxResults=2",
            ok_json(&serde_json::json!({
                "collaborators": [
                    {"id": "c-1", "firstName": "Alice", "lastName": "Dupond"},
                    {"id": "c-2", "firstName": "Bob", "lastName": "Martin"}
                ],
                "meta": {"nextPageToken": "tok-2", "count": 2}
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/payfit/companies/comp-1/collaborators?maxResults=2&nextPageToken=tok-2",
            ok_json(&serde_json::json!({
                "collaborators": [ {"id": "c-3", "firstName": "Carol", "lastName": "Bernard"} ],
                "meta": {"nextPageToken": serde_json::Value::Null, "count": 1}
            })),
        );
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(res.next_cursor.is_none());
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        // Auth header is Bearer, never the custom X-PayFit-Api-Key.
        let recorded = transport.recorded();
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer payfit-key"));
        assert!(!recorded[0]
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("X-PayFit-Api-Key")));
    }

    #[test]
    fn oauth_token_is_sent_as_bearer_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/payfit/companies/comp-1/collaborators?maxResults=2",
            ok_json(&serde_json::json!({
                "collaborators": [ {"id": "c-1", "firstName": "Alice"} ],
                "meta": {"nextPageToken": serde_json::Value::Null}
            })),
        );
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
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
    fn incremental_sync_relists_and_emits_updates() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/payfit/companies/comp-1/collaborators?maxResults=50",
            ok_json(&serde_json::json!({
                "collaborators": [
                    {"id": "c-1", "firstName": "Alice"},
                    {"id": "c-2", "firstName": "Bob"}
                ],
                "meta": {"nextPageToken": serde_json::Value::Null}
            })),
        );
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentUpdated { .. })));
        assert!(res.next_cursor.is_none());
    }

    #[test]
    fn fetch_content_renders_collaborator() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/payfit/companies/comp-1/collaborators/c-9",
            ok_json(&serde_json::json!({
                "id": "c-9",
                "firstName": "Alice",
                "lastName": "Dupond",
                "matricule": "M-42"
            })),
        );
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("c-9"))
            .unwrap();
        let text = String::from_utf8(content.body).unwrap();
        assert!(text.contains("Alice Dupond"));
        assert!(text.contains("M-42"));
        assert_eq!(
            content.title.as_deref(),
            Some("PayFit collaborator Alice Dupond")
        );
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://callback.test/payfit")
            .unwrap();
        assert_eq!(sub.callback_url, "https://callback.test/payfit");
        assert!(sub.provider_subscription_id.is_none());
    }

    #[test]
    fn handle_webhook_event_parses_camelcase_payload() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "collaboratorId": "c-7",
            "eventType": "collaborator.created"
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "c-7");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn handle_webhook_event_maps_delete() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!([{
            "collaboratorId": "c-8",
            "eventType": "collaborator.offboarded"
        }]))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn production_base_url_does_not_duplicate_version() {
        // Exercises the real DEFAULT_API_BASE_URL (the circular tests
        // override it). PayFit's host carries no version segment, and the
        // collaborator path must not invent a `/v1/` prefix.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://partner-api.payfit.com/companies/comp-1/collaborators?maxResults=50",
            ok_json(&serde_json::json!({
                "collaborators": [ {"id": "c-1"} ],
                "meta": {"nextPageToken": serde_json::Value::Null}
            })),
        );
        let prod_cfg =
            ConnectorConfig::new(ConnectorKind::PayFit, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "api_key": "payfit-key",
                    "company_id": "comp-1",
                }));
        let c = PayFitConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&prod_cfg).unwrap();
        let res = c.initial_sync(&prod_cfg, &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        let recorded = transport.recorded();
        assert_eq!(
            recorded[0].url,
            "https://partner-api.payfit.com/companies/comp-1/collaborators?maxResults=50"
        );
    }
}
