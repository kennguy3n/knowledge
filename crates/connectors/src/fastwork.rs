//! Fastwork connector — Fastwork API (`https://api.fastwork.co`).
//!
//! Fastwork is a Thai freelance marketplace. The API is OAuth2
//! (authorization-code), so [`FastworkConnector::authenticate`]
//! delegates to the injected [`OAuth2CodeExchange`].
//!
//! * `initial_sync` / `incremental_sync` page `/v1/projects`
//!   (`limit` / `offset`), tracking the maximum `updated_at` as an
//!   RFC-3339 watermark; incremental runs add `updated_since` and
//!   dedup the inclusive boundary row.
//! * `fetch_content` GETs a single project (`/v1/projects/{id}`).
//! * `subscribe_webhook` registers a push subscription
//!   (`POST /v1/webhooks`) and records the returned id.
//! * `handle_webhook_event` parses the delivered payload.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Fastwork API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.fastwork.co";
/// Page size for project listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Default, Deserialize)]
struct FastworkProjectsPage {
    #[serde(default)]
    data: Vec<FastworkProject>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FastworkProject {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    budget: Option<f64>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FastworkWebhookCreateResponse {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FastworkWebhookEvent {
    #[serde(default)]
    project_id: serde_json::Value,
    #[serde(default)]
    event: String,
}

/// Fastwork connector.
pub struct FastworkConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for FastworkConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastworkConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl FastworkConnector {
    /// Construct a Fastwork connector.
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

    /// Override the Fastwork API base URL.
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

    fn paginate_projects(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_since: Option<&str>,
    ) -> Result<Vec<FastworkProject>> {
        let mut projects = Vec::<FastworkProject>::new();
        for page in 0..MAX_PAGES {
            let offset = page * self.page_size as usize;
            let mut url = format!(
                "{base_url}/v1/projects?limit={}&offset={offset}",
                self.page_size
            );
            if let Some(since) = updated_since {
                url.push_str("&updated_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: FastworkProjectsPage = bearer_get_json(
                &self.transport,
                "fastwork",
                "/v1/projects",
                &url,
                token,
                &[],
            )?;
            let count = resp.data.len();
            projects.extend(resp.data);
            if count < self.page_size as usize {
                return Ok(projects);
            }
        }
        Err(ConnectorError::Sync(format!(
            "fastwork /v1/projects exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn project_watermark(p: &FastworkProject) -> Option<DateTime<Utc>> {
    p.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| p.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for FastworkConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "fastwork authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let projects = self.paginate_projects(&base_url, token, None)?;
        let mut events = Vec::with_capacity(projects.len());
        let mut cursor = WatermarkCursor::empty();
        for project in &projects {
            let occurred_at = project_watermark(project).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(project.id.clone()),
                occurred_at,
            });
            if let Some(t) = project_watermark(project) {
                cursor.observe(t, &project.id);
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
        let projects = self.paginate_projects(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for project in &projects {
            let Some(updated) = project_watermark(project) else {
                continue;
            };
            if !prior.should_emit(updated, &project.id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(project.id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &project.id);
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
        let url = format!("{base_url}/v1/projects/{id_enc}");
        let project: FastworkProject = bearer_get_json(
            &self.transport,
            "fastwork",
            "/v1/projects/{id}",
            &url,
            token,
            &[],
        )?;
        let title = project.title.as_deref().unwrap_or("(untitled project)");
        let status = project.status.as_deref().unwrap_or("");
        let budget = project.budget.unwrap_or(0.0);
        let body = format!("# {title}\n\nStatus: {status}\nBudget: {budget}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title.to_string())
            .with_metadata(serde_json::json!({
                "provider": "fastwork",
                "project_id": project.id,
                "status": project.status,
                "updated_at": project.updated_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/v1/webhooks");
        let body = serde_json::json!({
            "callback_url": callback_url,
            "events": ["project.created", "project.updated", "project.closed"],
        });
        let resp: FastworkWebhookCreateResponse = bearer_post_json(
            &self.transport,
            "fastwork",
            "/v1/webhooks",
            &url,
            token,
            &[],
            &body,
        )?;
        let provider_id = resp.id.ok_or_else(|| {
            ConnectorError::Webhook("fastwork /v1/webhooks returned no id".into())
        })?;
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("fastwork-webhook-secret");
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(provider_id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<FastworkWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<FastworkWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<FastworkWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("fastwork webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty fastwork webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.project_id).ok_or_else(|| {
                ConnectorError::Webhook("fastwork webhook event missing project_id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("delete") || delivery.event.contains("close") {
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
                "fastwork-access",
                "fastwork-refresh",
                Utc::now() + Duration::hours(1),
                "projects",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Fastwork, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/fastwork",
                "webhook_secret": "fastwork-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FastworkConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::Fastwork, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/fastwork/v1/projects?limit=2&offset=0",
            ok_json(&serde_json::json!({
                "data": [
                    {"id": "p-1", "updated_at": "2024-01-01T00:00:00Z"},
                    {"id": "p-2", "updated_at": "2024-01-02T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/fastwork/v1/projects?limit=2&offset=2",
            ok_json(&serde_json::json!({ "data": [ {"id": "p-3", "updated_at": "2024-01-03T00:00:00Z"} ] })),
        );
        let c = FastworkConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|p-3")
        );
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/fastwork/v1/projects?limit=2&offset=0&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({
                "data": [
                    {"id": "p-10", "updated_at": "2024-03-01T00:00:00Z"},
                    {"id": "p-13", "updated_at": "2024-03-01T00:00:00Z"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/fastwork/v1/projects?limit=2&offset=2&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({ "data": [ {"id": "p-11", "updated_at": "2024-06-01T00:00:00Z"} ] })),
        );
        let c = FastworkConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `p-10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must
        // NOT re-emit `p-10`, still surface the brand-new `p-13` at the same
        // second, and advance past the later row.
        state.cursor = Some(format!("{since}|p-10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["p-13", "p-11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|p-11")
        );
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/fastwork/v1/projects/p-1",
            ok_json(&serde_json::json!({
                "id": "p-1",
                "title": "Logo design",
                "status": "in_progress",
                "budget": 5000.0
            })),
        );
        let c = FastworkConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("p-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Logo design"));
        assert!(body.contains("in_progress"));
    }

    #[test]
    fn subscribe_webhook_registers_and_records_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/fastwork/v1/webhooks",
            ok_json(&serde_json::json!({ "id": "wh-7" })),
        );
        let c = FastworkConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/fastwork")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh-7"));
    }

    #[test]
    fn handle_webhook_event_maps_kinds() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FastworkConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!([
            { "project_id": "p-1", "event": "project.created" },
            { "project_id": "p-2", "event": "project.closed" }
        ]))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(events[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
