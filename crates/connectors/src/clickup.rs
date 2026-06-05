//! ClickUp connector — ClickUp REST API v2 (`/api/v2`).
//!
//! * `initial_sync` pages the team task endpoint
//!   (`/api/v2/team/{team_id}/task?page=N`) until ClickUp reports
//!   `last_page`.
//! * `incremental_sync` adds ClickUp's `date_updated_gt` filter
//!   (Unix-milliseconds, strictly greater-than, so no boundary dedup)
//!   keyed off the stored cursor.
//! * `fetch_content` GETs the single task (`/api/v2/task/{id}`) and
//!   reconstructs Markdown from `name` + `text_content`.
//! * `subscribe_webhook` POSTs `/api/v2/team/{team_id}/webhook` and
//!   persists ClickUp's returned webhook id.
//! * `handle_webhook_event` parses ClickUp's `{ event, task_id }`
//!   delivery (one change per POST) and tolerates a batched array.
//!
//! ClickUp timestamps are Unix-epoch **milliseconds** encoded as
//! JSON strings; the connector parses them via
//! [`DateTime::from_timestamp_millis`].

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default ClickUp API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.clickup.com";

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// One ClickUp task (subset of fields used by the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClickUpTask {
    /// Stable task id.
    #[serde(default)]
    pub id: String,
    /// Task name.
    #[serde(default)]
    pub name: Option<String>,
    /// Plain-text task body.
    #[serde(default)]
    pub text_content: Option<String>,
    /// Rich-text description (fallback when `text_content` is empty).
    #[serde(default)]
    pub description: Option<String>,
    /// Canonical task URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Creation time (Unix milliseconds, as a string).
    #[serde(default)]
    pub date_created: Option<String>,
    /// Last-update time (Unix milliseconds, as a string).
    #[serde(default)]
    pub date_updated: Option<String>,
}

/// One page of the team task endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClickUpTaskPage {
    /// Tasks on this page.
    #[serde(default)]
    pub tasks: Vec<ClickUpTask>,
    /// `true` once the final page has been reached.
    #[serde(default)]
    pub last_page: bool,
}

/// `POST /api/v2/team/{team_id}/webhook` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClickUpWebhookResponse {
    /// Top-level webhook id (ClickUp returns it here).
    #[serde(default)]
    pub id: Option<String>,
    /// Nested webhook handle (some responses carry it here instead).
    #[serde(default)]
    pub webhook: Option<ClickUpWebhookHandle>,
}

/// The id-bearing portion of a webhook response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClickUpWebhookHandle {
    /// ClickUp webhook id.
    #[serde(default)]
    pub id: String,
}

/// ClickUp webhook delivery (one task change per POST).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClickUpWebhookEvent {
    /// Event name, e.g. `taskCreated`, `taskUpdated`, `taskDeleted`.
    #[serde(default)]
    pub event: String,
    /// Affected task id.
    #[serde(default)]
    pub task_id: String,
}

/// ClickUp connector.
pub struct ClickUpConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
}

impl std::fmt::Debug for ClickUpConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickUpConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ClickUpConnector {
    /// Construct a ClickUp connector.
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
        }
    }

    /// Override the ClickUp API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
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

    fn team_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("team_id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("clickup: auth_config_json.team_id is required".into())
            })
    }

    /// Walk team tasks page-by-page until `last_page`.
    fn paginate_tasks(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        team: &str,
        date_updated_gt: Option<&str>,
    ) -> Result<Vec<ClickUpTask>> {
        let team_enc = percent_encode_path_component(team);
        let mut tasks = Vec::<ClickUpTask>::new();
        for page in 0..MAX_PAGES {
            let mut url = format!("{base_url}/api/v2/team/{team_enc}/task?page={page}");
            if let Some(gt) = date_updated_gt {
                url.push_str("&date_updated_gt=");
                url.push_str(&percent_encode_path_component(gt));
            }
            let resp: ClickUpTaskPage = bearer_get_json(
                &self.transport,
                "clickup",
                "/api/v2/team/{team_id}/task",
                &url,
                token,
                &[],
            )?;
            let empty = resp.tasks.is_empty();
            let last = resp.last_page;
            tasks.extend(resp.tasks);
            if last || empty {
                return Ok(tasks);
            }
        }
        Err(ConnectorError::Sync(format!(
            "clickup /task exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Parse a ClickUp Unix-milliseconds string into UTC.
fn parse_clickup_ms(s: &str) -> Option<DateTime<Utc>> {
    s.parse::<i64>()
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
}

fn task_watermark_ms(task: &ClickUpTask) -> Option<i64> {
    task.date_updated
        .as_deref()
        .or(task.date_created.as_deref())
        .and_then(|s| s.parse::<i64>().ok())
}

fn body_text(task: &ClickUpTask) -> String {
    match task.text_content.as_deref() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => task.description.clone().unwrap_or_default(),
    }
}

impl Connector for ClickUpConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "clickup authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let team = Self::team_id(config)?;
        let tasks = self.paginate_tasks(&base_url, token, &team, None)?;
        let mut events = Vec::with_capacity(tasks.len());
        let mut watermark_ms: Option<i64> = None;
        for task in &tasks {
            let occurred_at = task
                .date_updated
                .as_deref()
                .or(task.date_created.as_deref())
                .and_then(parse_clickup_ms)
                .unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(task.id.clone()),
                occurred_at,
            });
            if let Some(ms) = task_watermark_ms(task) {
                watermark_ms = Some(watermark_ms.map_or(ms, |w| w.max(ms)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark_ms.map(|ms| ms.to_string()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let team = Self::team_id(config)?;
        let prior_ms: Option<i64> = state.cursor.as_deref().and_then(|s| s.parse::<i64>().ok());
        let gt = prior_ms.map(|ms| ms.to_string());
        let tasks = self.paginate_tasks(&base_url, token, &team, gt.as_deref())?;
        let mut events = Vec::with_capacity(tasks.len());
        let mut watermark_ms = prior_ms;
        for task in &tasks {
            let occurred_at = task
                .date_updated
                .as_deref()
                .or(task.date_created.as_deref())
                .and_then(parse_clickup_ms)
                .unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(task.id.clone()),
                occurred_at,
            });
            if let Some(ms) = task_watermark_ms(task) {
                watermark_ms = Some(watermark_ms.map_or(ms, |w| w.max(ms)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark_ms
                .map(|ms| ms.to_string())
                .or_else(|| state.cursor.clone()),
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
        let url = format!("{base_url}/api/v2/task/{id_enc}");
        let task: ClickUpTask = bearer_get_json(
            &self.transport,
            "clickup",
            "/api/v2/task/{id}",
            &url,
            token,
            &[],
        )?;
        let name = task.name.clone().unwrap_or_default();
        let text = body_text(&task);
        let mut md = String::new();
        if !name.is_empty() {
            md.push_str("# ");
            md.push_str(&name);
            md.push_str("\n\n");
        }
        if !text.is_empty() {
            md.push_str(&text);
        }
        let body = md.trim_end().to_string();
        let mut fc = FetchedContent::text(body, "text/markdown")
            .with_title(name)
            .with_metadata(serde_json::json!({
                "provider": "clickup",
                "task_id": id,
                "date_updated": task.date_updated,
            }));
        if let Some(u) = task.url {
            fc = fc.with_source_url(u);
        }
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let team = Self::team_id(config)?;
        let team_enc = percent_encode_path_component(&team);
        let url = format!("{base_url}/api/v2/team/{team_enc}/webhook");
        let request = serde_json::json!({
            "endpoint": callback_url,
            "events": ["taskCreated", "taskUpdated", "taskDeleted"],
        });
        let resp: ClickUpWebhookResponse = bearer_post_json(
            &self.transport,
            "clickup",
            "/api/v2/team/{team_id}/webhook",
            &url,
            token,
            &[],
            &request,
        )?;
        let provider_id = resp
            .id
            .filter(|id| !id.is_empty())
            .or_else(|| resp.webhook.map(|w| w.id).filter(|id| !id.is_empty()));
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("clickup-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<ClickUpWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<ClickUpWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<ClickUpWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty clickup webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            if delivery.task_id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "clickup webhook event missing task_id".into(),
                ));
            }
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(delivery.task_id);
            let event = if delivery.event.contains("Created") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("Deleted") {
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
                "cu-access",
                "cu-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::ClickUp, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/cu",
                "team_id": "T1",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::ClickUp, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "cu-access"
        );
    }

    #[test]
    fn initial_sync_paginates_pages() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/cu/api/v2/team/T1/task?page=0".to_string(),
            ok_json(&serde_json::json!({
                "tasks": [{"id": "1", "name": "a", "date_created": "1700000000000", "date_updated": "1700000000000"}],
                "last_page": false
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/cu/api/v2/team/T1/task?page=1".to_string(),
            ok_json(&serde_json::json!({
                "tasks": [{"id": "2", "name": "b", "date_created": "1700000100000", "date_updated": "1700000100000"}],
                "last_page": true
            })),
        );
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("1700000100000"));
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn initial_sync_requires_team_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let mut config = cfg();
        config.auth_config_json = serde_json::json!({ "authorization_code": "x" });
        let tok = c.authenticate(&config).unwrap();
        assert!(matches!(
            c.initial_sync(&config, &tok),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn incremental_sync_uses_date_updated_gt() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/cu/api/v2/team/T1/task?page=0&date_updated_gt=1700000000000"
                .to_string(),
            ok_json(&serde_json::json!({
                "tasks": [{"id": "9", "date_updated": "1700000500000"}],
                "last_page": true
            })),
        );
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("1700000000000".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("1700000500000"));
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/cu/api/v2/task/abc".to_string(),
            ok_json(&serde_json::json!({
                "id": "abc",
                "name": "Fix bug",
                "text_content": "Null pointer in parser.",
                "url": "https://app.clickup.com/t/abc"
            })),
        );
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("abc"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Fix bug"));
        assert!(body.contains("Null pointer in parser."));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://app.clickup.com/t/abc")
        );
    }

    #[test]
    fn subscribe_webhook_captures_top_level_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/cu/api/v2/team/T1/webhook".to_string(),
            ok_json(&serde_json::json!({"id": "wh_42", "webhook": {"id": "wh_42"}})),
        );
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/cu")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh_42"));
    }

    #[test]
    fn webhook_maps_event_names() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let created = c
            .handle_webhook_event(
                &serde_json::to_vec(&serde_json::json!({"event": "taskCreated", "task_id": "1"}))
                    .unwrap(),
            )
            .unwrap();
        assert!(matches!(created[0], ConnectorEvent::DocumentCreated { .. }));
        let deleted = c
            .handle_webhook_event(
                &serde_json::to_vec(&serde_json::json!([{"event": "taskDeleted", "task_id": "2"}]))
                    .unwrap(),
            )
            .unwrap();
        assert!(matches!(deleted[0], ConnectorEvent::DocumentDeleted { .. }));
        let updated = c
            .handle_webhook_event(
                &serde_json::to_vec(&serde_json::json!({"event": "taskUpdated", "task_id": "3"}))
                    .unwrap(),
            )
            .unwrap();
        assert!(matches!(updated[0], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_missing_task_id_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ClickUpConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(
                &serde_json::to_vec(&serde_json::json!({"event": "taskUpdated"})).unwrap()
            ),
            Err(ConnectorError::Webhook(_))
        ));
    }

    #[test]
    fn parse_clickup_ms_works() {
        let dt = parse_clickup_ms("1700000000000").unwrap();
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }
}
