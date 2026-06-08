//! Talenox connector — Talenox API (`https://api.talenox.com`).
//!
//! Talenox is a Singapore HR / payroll SaaS popular with SMEs. The
//! API authenticates with a personal API key presented as a bearer
//! token, so [`TalenoxConnector::authenticate`] reads the key out of
//! `auth_config_json` (falling back to the injected
//! [`OAuth2CodeExchange`] when an authorization-code grant is
//! configured instead).
//!
//! * `initial_sync` / `incremental_sync` page `/v2/employees`
//!   (`per_page` / `page`), tracking the maximum `updated_at` as an
//!   RFC-3339 watermark; incremental runs add `updated_after` and
//!   dedup the inclusive boundary row.
//! * `fetch_content` GETs a single employee (`/v2/employees/{id}`).
//! * Talenox exposes no webhook-creation endpoint, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses any payload Talenox automations
//!   are configured to deliver.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WatermarkCursor,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Talenox API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.talenox.com";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "employees";
/// Page size for employee listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct TalenoxEmployeesPage {
    #[serde(default)]
    employees: Vec<TalenoxEmployee>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TalenoxEmployee {
    #[serde(default)]
    id: serde_json::Value,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    job_title: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TalenoxWebhookEvent {
    #[serde(default)]
    employee_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Talenox connector.
pub struct TalenoxConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for TalenoxConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TalenoxConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl TalenoxConnector {
    /// Construct a Talenox connector.
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

    /// Override the Talenox API base URL.
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

    fn paginate_employees(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_after: Option<&str>,
    ) -> Result<Vec<TalenoxEmployee>> {
        let mut employees = Vec::<TalenoxEmployee>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!(
                "{base_url}/v2/employees?per_page={}&page={page}",
                self.page_size
            );
            if let Some(since) = updated_after {
                url.push_str("&updated_after=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: TalenoxEmployeesPage = bearer_get_json(
                &self.transport,
                "talenox",
                "/v2/employees",
                &url,
                token,
                &[],
            )?;
            let count = resp.employees.len();
            employees.extend(resp.employees);
            if count < self.page_size as usize {
                return Ok(employees);
            }
        }
        Err(ConnectorError::Sync(format!(
            "talenox /v2/employees exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn employee_watermark(e: &TalenoxEmployee) -> Option<DateTime<Utc>> {
    e.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| e.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for TalenoxConnector {
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
                    "talenox authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let employees = self.paginate_employees(&base_url, token, None)?;
        let mut events = Vec::with_capacity(employees.len());
        let mut cursor = WatermarkCursor::empty();
        for employee in &employees {
            let Some(id_str) = id_value_to_string(&employee.id) else {
                continue;
            };
            let occurred_at = employee_watermark(employee).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(id_str.clone()),
                occurred_at,
            });
            if let Some(t) = employee_watermark(employee) {
                cursor.observe(t, &id_str);
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
        let employees = self.paginate_employees(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for employee in &employees {
            let Some(id_str) = id_value_to_string(&employee.id) else {
                continue;
            };
            let Some(updated) = employee_watermark(employee) else {
                continue;
            };
            if !prior.should_emit(updated, &id_str) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(id_str.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &id_str);
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
        let url = format!("{base_url}/v2/employees/{id_enc}");
        let employee: TalenoxEmployee = bearer_get_json(
            &self.transport,
            "talenox",
            "/v2/employees/{id}",
            &url,
            token,
            &[],
        )?;
        let name = employee
            .full_name
            .as_deref()
            .unwrap_or("(unnamed employee)");
        let title = employee.job_title.as_deref().unwrap_or("");
        let email = employee.email.as_deref().unwrap_or("");
        let body = format!("# {name}\n\nJob title: {title}\nEmail: {email}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(name.to_string())
            .with_metadata(serde_json::json!({
                "provider": "talenox",
                "employee_id": id,
                "updated_at": employee.updated_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Talenox exposes no webhook-creation API; record a
        // polling-only subscription so the runtime falls back to
        // incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("talenox-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<TalenoxWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<TalenoxWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<TalenoxWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("talenox webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty talenox webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.employee_id).ok_or_else(|| {
                ConnectorError::Webhook("talenox webhook event missing employee_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("delete") || delivery.event.contains("terminate") {
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
                "employees",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Talenox, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "talenox-key",
                "api_base_url": "https://api.test/talenox",
                "webhook_secret": "talenox-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TalenoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "talenox-key");
    }

    #[test]
    fn initial_sync_paginates() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/talenox/v2/employees?per_page=2&page=1",
            ok_json(&serde_json::json!({
                "employees": [
                    {"id": 1, "updated_at": "2024-01-01T00:00:00Z"},
                    {"id": 2, "updated_at": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/talenox/v2/employees?per_page=2&page=2",
            ok_json(&serde_json::json!({ "employees": [ {"id": 3, "updated_at": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = TalenoxConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|3")
        );
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/talenox/v2/employees?per_page=2&page=1&updated_after={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "employees": [
                    {"id": 10, "updated_at": "2024-03-01T00:00:00Z"},
                    {"id": 13, "updated_at": "2024-03-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/talenox/v2/employees?per_page=2&page=2&updated_after={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "employees": [ {"id": 11, "updated_at": "2024-06-01T00:00:00Z"} ] })),
        );
        let c = TalenoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must NOT
        // re-emit `10`, still surface the brand-new `13` at the same second,
        // and advance past the later row.
        state.cursor = Some(format!("{since}|10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["13", "11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|11")
        );
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/talenox/v2/employees/7",
            ok_json(&serde_json::json!({
                "id": 7,
                "full_name": "Siti Rahman",
                "job_title": "Engineer",
                "email": "siti@example.com"
            })),
        );
        let c = TalenoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("7"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Siti Rahman"));
        assert!(body.contains("Engineer"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TalenoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/talenox")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "talenox-secret");
    }

    #[test]
    fn handle_webhook_event_parses() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = TalenoxConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "employee_id": "e-1", "event": "employee.terminated"
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ConnectorEvent::DocumentDeleted { .. }));
    }
}
