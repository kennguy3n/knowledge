//! Base.vn connector — enterprise collaboration suite.
//!
//! Base.vn is a Vietnamese enterprise collaboration platform (HR, CRM,
//! project management). The public API authenticates with a workspace
//! access token.
//!
//! This connector ingests **tasks** as the primary document stream;
//! requests and HR records are reachable through the same paginated
//! pattern and can be layered on later.
//!
//! * `initial_sync` walks `GET /publicapi/v2/task/list`, paging via the
//!   1-based `page` cursor until a short page is returned.
//! * `incremental_sync` adds a `since` filter built from the stored
//!   RFC 3339 watermark.
//! * `fetch_content` reads `GET /publicapi/v2/task/get` and renders a
//!   Markdown summary.
//! * `subscribe_webhook` is configuration-based — Base.vn event hooks
//!   are registered once in the workspace admin console, so no HTTP
//!   call is issued; the configured secret is surfaced for delivery
//!   validation.
//! * `handle_webhook_event` parses a task event keyed by task id.
//!
//! Base.vn authenticates with a bearer `Authorization` header, so
//! requests go through the injected [`HttpTransport`] directly.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default Base.vn API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://workflow.base.vn";

/// Default task list page size.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "tasks.read";

/// One Base.vn task (subset of fields ingested).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaseTask {
    /// Task id.
    #[serde(default)]
    pub id: String,
    /// Task name / title.
    #[serde(default)]
    pub name: Option<String>,
    /// Task status / state.
    #[serde(default, alias = "state")]
    pub status: Option<String>,
    /// Last-updated timestamp (RFC 3339).
    #[serde(default, rename = "updated_at", alias = "last_update")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Envelope for the task list endpoint (`{ "tasks": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaseTasksResponse {
    /// Tasks on this page.
    #[serde(default, alias = "data")]
    pub tasks: Vec<BaseTask>,
}

/// Envelope for the single-task endpoint (`{ "task": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaseTaskResponse {
    /// The task.
    #[serde(default)]
    pub task: BaseTask,
}

/// Task-event webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaseWebhookPayload {
    /// Event name (`task.create`, `task.update`, …).
    #[serde(default)]
    pub event: Option<String>,
    /// Affected task id.
    #[serde(default, rename = "task_id", alias = "id")]
    pub task_id: String,
}

/// Base.vn connector.
pub struct BaseVNConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for BaseVNConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseVNConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl BaseVNConnector {
    /// Construct a Base.vn connector.
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

    /// Override the API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 200]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 200);
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

    fn api_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = HttpRequest::get(url)
            .with_bearer(token.access_token.expose())
            .with_header("Accept", "application/json");
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("base_vn", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "base_vn {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every task page until a short page is returned.
    fn paginate_tasks(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        since: Option<&str>,
    ) -> Result<Vec<BaseTask>> {
        let mut out = Vec::<BaseTask>::new();
        let filter = since.map_or_else(String::new, |ts| {
            format!("&since={}", percent_encode_path_component(ts))
        });
        for page in 1..=MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/publicapi/v2/task/list?page={page}&page_size={}{filter}",
                self.page_size
            );
            let resp: BaseTasksResponse = self.api_get("/publicapi/v2/task/list", &url, token)?;
            let returned = resp.tasks.len();
            out.extend(resp.tasks);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "base_vn /publicapi/v2/task/list exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn task_to_event(t: &BaseTask, created: bool) -> ConnectorEvent {
    let occurred_at = t.updated_at.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(t.id.clone());
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

impl Connector for BaseVNConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("api_key")
            .or_else(|| config.auth_config_json.get("access_token"))
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
                    "base_vn authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let tasks = self.paginate_tasks(&base_url, token, None)?;
        let mut events = Vec::with_capacity(tasks.len());
        let mut cursor = WatermarkCursor::empty();
        for t in &tasks {
            events.push(task_to_event(t, true));
            if let Some(ts) = t.updated_at {
                cursor.observe(ts, &t.id);
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
        let tasks = self.paginate_tasks(&base_url, token, prior.query_since().as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for t in &tasks {
            match t.updated_at {
                Some(ts) => {
                    if !prior.should_emit(ts, &t.id) {
                        continue;
                    }
                    events.push(task_to_event(t, false));
                    cursor.observe(ts, &t.id);
                }
                None => events.push(task_to_event(t, false)),
            }
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
        let url = format!("{base_url}/publicapi/v2/task/get?id={id_enc}");
        let resp: BaseTaskResponse = self.api_get("/publicapi/v2/task/get", &url, token)?;
        let task = resp.task;

        let name = task.name.clone().unwrap_or_else(|| task.id.clone());
        let title = format!("Task {name}");
        let mut md = String::new();
        let _ = writeln!(md, "# {title}\n");
        if let Some(status) = task.status.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(md, "**Status:** {status}\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "base_vn",
                "task_id": task.id,
                "name": task.name,
                "status": task.status,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Base.vn event hooks are registered once in the workspace
        // admin console (no self-serve create-webhook endpoint), so we
        // do not issue an HTTP call here. Surface the configured secret
        // for delivery validation.
        let _ = token;
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "base_vn subscribe_webhook: auth_config_json.webhook_secret is required".into(),
                )
            })?;
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: BaseWebhookPayload = serde_json::from_slice(body)?;
        if payload.task_id.is_empty() {
            return Err(ConnectorError::Webhook(
                "base_vn webhook payload missing task_id".into(),
            ));
        }
        let id = SourceDocumentId::new(payload.task_id);
        let event = match payload.event.as_deref() {
            Some("task.create") => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at: Utc::now(),
            },
            _ => ConnectorEvent::DocumentUpdated {
                document_id: id,
                occurred_at: Utc::now(),
            },
        };
        Ok(vec![event])
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
                "x",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::BaseVN, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "base_tok_123",
                "api_base_url": "https://api.test/base",
                "webhook_secret": "base-secret",
            }))
    }

    fn task(id: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": format!("Task {id}"),
            "status": "open",
            "updated_at": updated,
        })
    }

    fn list_resp(tasks: &[serde_json::Value]) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(&serde_json::json!({ "tasks": tasks })).unwrap())
    }

    #[test]
    fn authenticate_wraps_api_key() {
        let c = BaseVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "base_tok_123"
        );
    }

    #[test]
    fn initial_sync_emits_created_with_bearer() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/base/publicapi/v2/task/list?page=1&page_size=50",
            list_resp(&[task("T1", "2024-01-01T00:00:00Z")]),
        );
        let c = BaseVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "T1");
        assert!(transport.recorded()[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer base_tok_123"));
    }

    #[test]
    fn incremental_sync_dedups_boundary_but_surfaces_new_same_second() {
        // Prior cursor: watermark at the boundary second with `T1`
        // already emitted. The server (inclusive `since`) re-returns
        // `T1`, a brand-new `T3` at the SAME second, and a later `T2`.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/base/publicapi/v2/task/list?page=1&page_size=50&since=2024-01-01T00%3A00%3A00%2B00%3A00",
            list_resp(&[
                task("T1", "2024-01-01T00:00:00Z"),
                task("T3", "2024-01-01T00:00:00Z"),
                task("T2", "2024-10-01T00:00:00Z"),
            ]),
        );
        let c = BaseVNConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00+00:00|T1".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // `T1` deduped (already seen at boundary); `T3` surfaced (the
        // regression: new same-second record must not be dropped); `T2`
        // advances the watermark.
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["T3", "T2"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-10-01T00:00:00+00:00|T2")
        );
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/base/publicapi/v2/task/list?page=1&page_size=50",
            MockResponse::status(401, b"bad".to_vec()),
        );
        let c = BaseVNConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            "https://api.test/base/publicapi/v2/task/get?id=T1",
            MockResponse::ok_json(
                serde_json::to_vec(
                    &serde_json::json!({ "task": task("T1", "2024-01-01T00:00:00Z") }),
                )
                .unwrap(),
            ),
        );
        let c = BaseVNConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("T1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Task Task T1"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("**Status:** open"));
    }

    #[test]
    fn subscribe_webhook_uses_secret_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BaseVNConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/base")
            .unwrap();
        assert_eq!(sub.secret.expose(), "base-secret");
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn handle_webhook_event_maps_created() {
        let c = BaseVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "event": "task.create", "task_id": "T9" });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "T9");
    }

    #[test]
    fn handle_webhook_event_missing_id_errors() {
        let c = BaseVNConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(MockHttpTransport::new()),
            oauth(),
        );
        let body = serde_json::json!({ "event": "task.update" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
