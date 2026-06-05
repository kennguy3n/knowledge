//! Asana connector — Asana REST API (`/api/1.0`).
//!
//! * `initial_sync` pages `/api/1.0/tasks?project=<gid>` following the
//!   `next_page.offset` opaque cursor until it is null.
//! * `incremental_sync` adds Asana's `modified_since` query parameter
//!   keyed off the stored RFC-3339 watermark. Asana's
//!   `modified_since` is inclusive, so the boundary row is deduped
//!   client-side.
//! * `fetch_content` GETs the single task
//!   (`/api/1.0/tasks/{gid}`) and reconstructs Markdown from `name` +
//!   `notes`.
//! * `subscribe_webhook` POSTs `/api/1.0/webhooks` and persists
//!   Asana's returned webhook gid.
//! * `handle_webhook_event` parses Asana's `{ "events": [...] }`
//!   delivery, emitting one event per entry.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Asana API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://app.asana.com";

/// Page size for task listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// `opt_fields` requested for task listings.
const TASK_OPT_FIELDS: &str = "name,notes,created_at,modified_at";

/// One Asana task (subset of fields used by the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaTask {
    /// Stable task gid.
    #[serde(default)]
    pub gid: String,
    /// Task name.
    #[serde(default)]
    pub name: Option<String>,
    /// Task notes (description body).
    #[serde(default)]
    pub notes: Option<String>,
    /// Canonical task URL.
    #[serde(default)]
    pub permalink_url: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-modified timestamp.
    #[serde(default)]
    pub modified_at: Option<String>,
}

/// Pagination block returned by list endpoints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaNextPage {
    /// Opaque offset token for the next page request.
    #[serde(default)]
    pub offset: Option<String>,
}

/// One page of a task-list response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaTaskListResponse {
    /// Tasks on this page.
    #[serde(default)]
    pub data: Vec<AsanaTask>,
    /// Pagination cursor (null on the last page).
    #[serde(default)]
    pub next_page: Option<AsanaNextPage>,
}

/// Single-task response (`GET /api/1.0/tasks/{gid}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaTaskResponse {
    /// The task body.
    #[serde(default)]
    pub data: AsanaTask,
}

/// `POST /api/1.0/webhooks` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaWebhookResponse {
    /// Created webhook.
    #[serde(default)]
    pub data: AsanaWebhookHandle,
}

/// The gid-bearing portion of a webhook response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaWebhookHandle {
    /// Asana webhook gid.
    #[serde(default)]
    pub gid: String,
}

/// Asana webhook delivery envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaWebhookPayload {
    /// Change events.
    #[serde(default)]
    pub events: Vec<AsanaWebhookEvent>,
}

/// One Asana change event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaWebhookEvent {
    /// Affected resource.
    #[serde(default)]
    pub resource: AsanaWebhookResource,
    /// `added`, `changed`, `deleted`, `removed`.
    #[serde(default)]
    pub action: String,
    /// RFC-3339 timestamp of the change.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// The resource a webhook event refers to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsanaWebhookResource {
    /// Resource gid.
    #[serde(default)]
    pub gid: String,
}

/// Asana connector.
pub struct AsanaConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for AsanaConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsanaConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl AsanaConnector {
    /// Construct an Asana connector.
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

    /// Override the Asana API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the task page size.
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

    fn project_gid(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("project")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("asana: auth_config_json.project (gid) is required".into())
            })
    }

    /// Walk the task list, following `next_page.offset` until null.
    fn paginate_tasks(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        project: &str,
        modified_since: Option<&str>,
    ) -> Result<Vec<AsanaTask>> {
        let project_enc = percent_encode_path_component(project);
        let mut tasks = Vec::<AsanaTask>::new();
        let mut offset: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!(
                "{base_url}/api/1.0/tasks?project={project_enc}&limit={}&opt_fields={TASK_OPT_FIELDS}",
                self.page_size,
            );
            if let Some(since) = modified_since {
                url.push_str("&modified_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            if let Some(o) = &offset {
                url.push_str("&offset=");
                url.push_str(&percent_encode_path_component(o));
            }
            let resp: AsanaTaskListResponse =
                bearer_get_json(&self.transport, "asana", "/api/1.0/tasks", &url, token, &[])?;
            tasks.extend(resp.data);
            match resp.next_page.and_then(|p| p.offset) {
                Some(next) if !next.is_empty() => offset = Some(next),
                _ => return Ok(tasks),
            }
        }
        Err(ConnectorError::Sync(format!(
            "asana /tasks exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn task_watermark(task: &AsanaTask) -> Option<DateTime<Utc>> {
    task.modified_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| task.created_at.as_deref().and_then(parse_rfc3339))
}

impl Connector for AsanaConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "asana authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let project = Self::project_gid(config)?;
        let tasks = self.paginate_tasks(&base_url, token, &project, None)?;
        let mut events = Vec::with_capacity(tasks.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for task in &tasks {
            let occurred_at = task_watermark(task).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(task.gid.clone()),
                occurred_at,
            });
            if let Some(t) = task_watermark(task) {
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
        let project = Self::project_gid(config)?;
        let prior: Option<DateTime<Utc>> = state.cursor.as_deref().and_then(parse_rfc3339);
        let since = prior.map(|t| t.to_rfc3339());
        let tasks = self.paginate_tasks(&base_url, token, &project, since.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for task in &tasks {
            let Some(modified) = task_watermark(task) else {
                continue;
            };
            // `modified_since` is inclusive — drop the boundary row.
            if prior.is_some_and(|p| modified <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(task.gid.clone()),
                occurred_at: modified,
            });
            watermark = Some(watermark.map_or(modified, |w| w.max(modified)));
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
        let gid = document_id.as_str();
        let gid_enc = percent_encode_path_component(gid);
        let url = format!(
            "{base_url}/api/1.0/tasks/{gid_enc}?opt_fields=name,notes,permalink_url,modified_at"
        );
        let resp: AsanaTaskResponse = bearer_get_json(
            &self.transport,
            "asana",
            "/api/1.0/tasks/{gid}",
            &url,
            token,
            &[],
        )?;
        let task = resp.data;
        let name = task.name.clone().unwrap_or_default();
        let notes = task.notes.clone().unwrap_or_default();
        let mut md = String::new();
        if !name.is_empty() {
            md.push_str("# ");
            md.push_str(&name);
            md.push_str("\n\n");
        }
        if !notes.is_empty() {
            md.push_str(&notes);
        }
        let body = md.trim_end().to_string();
        let mut fc = FetchedContent::text(body, "text/markdown")
            .with_title(name)
            .with_metadata(serde_json::json!({
                "provider": "asana",
                "task_gid": gid,
                "modified_at": task.modified_at,
            }));
        if let Some(u) = task.permalink_url {
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
        let project = Self::project_gid(config)?;
        let url = format!("{base_url}/api/1.0/webhooks");
        let request = serde_json::json!({
            "data": {
                "resource": project,
                "target": callback_url,
            }
        });
        let resp: AsanaWebhookResponse = bearer_post_json(
            &self.transport,
            "asana",
            "/api/1.0/webhooks",
            &url,
            token,
            &[],
            &request,
        )?;
        let provider_id = if resp.data.gid.is_empty() {
            None
        } else {
            Some(resp.data.gid)
        };
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("asana-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: AsanaWebhookPayload = serde_json::from_slice(body)?;
        if payload.events.is_empty() {
            return Err(ConnectorError::Webhook("empty asana webhook batch".into()));
        }
        let mut events = Vec::with_capacity(payload.events.len());
        for event in payload.events {
            if event.resource.gid.is_empty() {
                return Err(ConnectorError::Webhook(
                    "asana webhook event missing resource.gid".into(),
                ));
            }
            let occurred_at = event
                .created_at
                .as_deref()
                .and_then(parse_rfc3339)
                .unwrap_or_else(Utc::now);
            let id = SourceDocumentId::new(event.resource.gid);
            let event = match event.action.as_str() {
                "added" => ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                },
                "deleted" | "removed" => ConnectorEvent::DocumentDeleted {
                    document_id: id,
                    occurred_at,
                },
                _ => ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                },
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
                "as-access",
                "as-refresh",
                Utc::now() + Duration::hours(1),
                "default",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Asana, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/as",
                "project": "P1",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn list_url(offset: Option<&str>, since: Option<&str>) -> String {
        let mut url = format!(
            "https://api.test/as/api/1.0/tasks?project=P1&limit={DEFAULT_PAGE_SIZE}&opt_fields={TASK_OPT_FIELDS}"
        );
        if let Some(s) = since {
            url.push_str("&modified_since=");
            url.push_str(&percent_encode_path_component(s));
        }
        if let Some(o) = offset {
            url.push_str("&offset=");
            url.push_str(&percent_encode_path_component(o));
        }
        url
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Asana, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "as-access"
        );
    }

    #[test]
    fn initial_sync_paginates_offset() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            list_url(None, None),
            ok_json(&serde_json::json!({
                "data": [{"gid": "1", "name": "a", "created_at": "2024-01-01T00:00:00Z", "modified_at": "2024-01-01T00:00:00Z"}],
                "next_page": {"offset": "OFF2"}
            })),
        );
        transport.expect(
            HttpMethod::Get,
            list_url(Some("OFF2"), None),
            ok_json(&serde_json::json!({
                "data": [{"gid": "2", "name": "b", "created_at": "2024-01-02T00:00:00Z", "modified_at": "2024-01-02T00:00:00Z"}],
                "next_page": null
            })),
        );
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-02T00:00:00+00:00")
        );
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn initial_sync_requires_project() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let mut config = cfg();
        config.auth_config_json = serde_json::json!({ "authorization_code": "x" });
        let tok = c.authenticate(&config).unwrap();
        assert!(matches!(
            c.initial_sync(&config, &tok),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn incremental_sync_uses_modified_since_and_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            list_url(None, Some(since)),
            ok_json(&serde_json::json!({
                "data": [
                    {"gid": "boundary", "modified_at": "2024-03-01T00:00:00Z"},
                    {"gid": "newer", "modified_at": "2024-06-01T00:00:00Z"}
                ],
                "next_page": null
            })),
        );
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(since.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00")
        );
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/as/api/1.0/tasks/9?opt_fields=name,notes,permalink_url,modified_at"
                .to_string(),
            ok_json(&serde_json::json!({
                "data": {
                    "gid": "9",
                    "name": "Ship release",
                    "notes": "Cut the 1.0 tag.",
                    "permalink_url": "https://app.asana.com/0/0/9"
                }
            })),
        );
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("9"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Ship release"));
        assert!(body.contains("Cut the 1.0 tag."));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://app.asana.com/0/0/9")
        );
    }

    #[test]
    fn subscribe_webhook_creates_and_captures_gid() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/as/api/1.0/webhooks".to_string(),
            ok_json(&serde_json::json!({"data": {"gid": "wh_9"}})),
        );
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/as")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh_9"));
    }

    #[test]
    fn webhook_parses_events_and_maps_actions() {
        let body = serde_json::json!({
            "events": [
                {"resource": {"gid": "a"}, "action": "added", "created_at": "2024-01-01T00:00:00Z"},
                {"resource": {"gid": "b"}, "action": "changed", "created_at": "2024-01-02T00:00:00Z"},
                {"resource": {"gid": "c"}, "action": "deleted", "created_at": "2024-01-03T00:00:00Z"}
            ]
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_empty_batch_errors() {
        let body = serde_json::json!({"events": []});
        let transport = Arc::new(MockHttpTransport::new());
        let c = AsanaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap()),
            Err(ConnectorError::Webhook(_))
        ));
    }
}
