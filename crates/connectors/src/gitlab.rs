//! GitLab connector — GitLab REST API v4 + project webhooks.
//!
//! * `initial_sync` walks `GET /api/v4/projects/{id}/issues` ordered
//!   by `updated_at` ascending and pages via the `page` query
//!   parameter until a short page is returned.
//! * `incremental_sync` adds `updated_after=<iso>` keyed off the
//!   prior watermark and dedupes the inclusive boundary issue.
//! * `fetch_content` reads a single issue and renders its
//!   description as Markdown.
//! * `subscribe_webhook` POSTs `/api/v4/projects/{id}/hooks`.
//! * `handle_webhook_event` parses an issue hook
//!   (`{object_kind:"issue", object_attributes:{action,…}}`).
//!
//! GitLab accepts both OAuth2 access tokens and personal access
//! tokens via the `Authorization: Bearer` header, so the bearer
//! helpers apply directly. `authenticate` accepts a configured
//! `personal_access_token` or an OAuth2 `authorization_code`.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default GitLab REST base URL (gitlab.com SaaS).
pub const DEFAULT_API_BASE_URL: &str = "https://gitlab.com";

/// Page size for list endpoints. GitLab's documented max is 100.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured PAT.
const DEFAULT_SCOPE: &str = "read_api";

/// One GitLab issue (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitLabIssue {
    /// Global issue id.
    pub id: i64,
    /// Per-project issue number (`iid`).
    #[serde(default)]
    pub iid: i64,
    /// Issue title.
    #[serde(default)]
    pub title: Option<String>,
    /// Markdown description.
    #[serde(default)]
    pub description: Option<String>,
    /// `opened` / `closed`.
    #[serde(default)]
    pub state: Option<String>,
    /// Canonical web URL.
    #[serde(default)]
    pub web_url: Option<String>,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Response from `POST /api/v4/projects/{id}/hooks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitLabHookResponse {
    /// Hook id.
    #[serde(default)]
    pub id: i64,
}

/// Issue webhook payload (`object_kind: "issue"`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitLabIssueHook {
    /// Event discriminator (`issue`).
    #[serde(default)]
    pub object_kind: Option<String>,
    /// Issue attributes.
    #[serde(default)]
    pub object_attributes: Option<GitLabIssueAttributes>,
}

/// Attributes block of an issue webhook.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitLabIssueAttributes {
    /// Global issue id.
    #[serde(default)]
    pub id: i64,
    /// Per-project issue number (`iid`) — the id used to address the
    /// project-scoped issue endpoint.
    #[serde(default)]
    pub iid: i64,
    /// Lifecycle action (`open`, `update`, `close`, `reopen`).
    #[serde(default)]
    pub action: Option<String>,
    /// Update timestamp.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// GitLab connector.
pub struct GitLabConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GitLabConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GitLabConnector {
    /// Construct a GitLab connector.
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

    /// Override the GitLab REST base URL (self-managed instances).
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

    fn project_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("project_id")
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .ok_or_else(|| {
                ConnectorError::Sync("gitlab: auth_config_json.project_id is required".into())
            })
    }

    /// Walk every issues page until a short page is returned, paging
    /// by the `page` query parameter.
    fn paginate_issues(
        &self,
        base_url: &str,
        project_enc: &str,
        token: &OAuth2Token,
        updated_after: Option<&str>,
    ) -> Result<Vec<GitLabIssue>> {
        let mut out = Vec::<GitLabIssue>::new();
        for page in 1..=MAX_LIST_PAGES {
            let mut url = format!(
                "{base_url}/api/v4/projects/{project_enc}/issues?per_page={}&page={page}&order_by=updated_at&sort=asc",
                self.page_size
            );
            if let Some(after) = updated_after {
                let _ = write!(
                    url,
                    "&updated_after={}",
                    percent_encode_path_component(after)
                );
            }
            let issues: Vec<GitLabIssue> = bearer_get_json(
                &self.transport,
                "gitlab",
                "/api/v4/projects/{id}/issues",
                &url,
                token,
                &[],
            )?;
            let returned = issues.len();
            out.extend(issues);
            if returned < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "gitlab issues exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn issue_to_event(i: &GitLabIssue, kind: &str) -> ConnectorEvent {
    let occurred_at = i.updated_at.or(i.created_at).unwrap_or_else(Utc::now);
    // `fetch_content` addresses the project-scoped issue endpoint by
    // per-project `iid`, so emit the `iid` as the document id (the
    // global `id` would 404 / resolve the wrong issue there).
    let id = SourceDocumentId::new(i.iid.to_string());
    match kind {
        "create" => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        "delete" => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for GitLabConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(pat) = config
            .auth_config_json
            .get("personal_access_token")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                pat,
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
                    "gitlab authenticate: auth_config_json.personal_access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let project = Self::project_id(config)?;
        let project_enc = percent_encode_path_component(&project);
        let issues = self.paginate_issues(&base_url, &project_enc, token, None)?;
        let mut events = Vec::with_capacity(issues.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for i in &issues {
            events.push(issue_to_event(i, "create"));
            if let Some(t) = i.updated_at.or(i.created_at) {
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
        let project = Self::project_id(config)?;
        let project_enc = percent_encode_path_component(&project);
        let prior = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let issues =
            self.paginate_issues(&base_url, &project_enc, token, state.cursor.as_deref())?;
        let mut events = Vec::with_capacity(issues.len());
        let mut watermark = prior;
        for i in &issues {
            let when = i.updated_at.or(i.created_at);
            // `updated_after` is inclusive; skip the boundary issue
            // already emitted on the prior run.
            if let (Some(prev), Some(t)) = (prior, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(issue_to_event(i, "update"));
            if let Some(t) = when {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
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
        let project = Self::project_id(config)?;
        let project_enc = percent_encode_path_component(&project);
        // The document id is the per-project issue iid for the
        // single-issue endpoint.
        let iid_enc = percent_encode_path_component(document_id.as_str());
        let url = format!("{base_url}/api/v4/projects/{project_enc}/issues/{iid_enc}");
        let issue: GitLabIssue = bearer_get_json(
            &self.transport,
            "gitlab",
            "/api/v4/projects/{id}/issues/{iid}",
            &url,
            token,
            &[],
        )?;

        let title = issue
            .title
            .clone()
            .unwrap_or_else(|| format!("Issue {}", document_id.as_str()));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(desc) = issue.description.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(desc);
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "gitlab",
                "issue_id": issue.id,
                "iid": issue.iid,
                "state": issue.state,
            }))
            .with_source_url(
                issue
                    .web_url
                    .unwrap_or_else(|| format!("{base_url}/-/issues/{}", document_id.as_str())),
            ))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let project = Self::project_id(config)?;
        let project_enc = percent_encode_path_component(&project);
        let url = format!("{base_url}/api/v4/projects/{project_enc}/hooks");
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("gitlab-webhook-secret")
            .to_string();
        let body = serde_json::json!({
            "url": callback_url,
            "issues_events": true,
            "merge_requests_events": true,
            "push_events": false,
            "token": secret,
        });
        let resp: GitLabHookResponse = bearer_post_json(
            &self.transport,
            "gitlab",
            "/api/v4/projects/{id}/hooks",
            &url,
            token,
            &[],
            &body,
        )?;
        if resp.id == 0 {
            return Err(ConnectorError::Webhook(
                "gitlab /api/v4/projects/{id}/hooks returned no id".into(),
            ));
        }
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.id.to_string());
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let hook: GitLabIssueHook = serde_json::from_slice(body)?;
        let attrs = hook.object_attributes.ok_or_else(|| {
            ConnectorError::Webhook("gitlab webhook missing object_attributes".into())
        })?;
        if attrs.iid == 0 {
            return Err(ConnectorError::Webhook(
                "gitlab webhook missing issue iid".into(),
            ));
        }
        let occurred_at = attrs.updated_at.unwrap_or_else(Utc::now);
        let id = SourceDocumentId::new(attrs.iid.to_string());
        let event = match attrs.action.as_deref() {
            Some("open") => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at,
            },
            // GitLab does not deliver hard deletes for issues; a
            // closed/reopened/updated issue is a content update.
            _ => ConnectorEvent::DocumentUpdated {
                document_id: id,
                occurred_at,
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
                "gitlab-access",
                "gitlab-refresh",
                Utc::now() + Duration::hours(1),
                "read_api",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::GitLab, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "personal_access_token": "glpat-xxx",
                "project_id": "42",
                "api_base_url": "https://api.test/gitlab",
            }))
    }

    fn issue(id: i64, iid: i64, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "iid": iid, "title": format!("Issue {iid}"),
            "updated_at": updated, "web_url": format!("https://gitlab/-/issues/{iid}")
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_pat() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "glpat-xxx"
        );
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::GitLab, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(
                serde_json::json!({ "authorization_code": "abc", "project_id": "1" }),
            );
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "gitlab-access"
        );
    }

    #[test]
    fn initial_sync_paginates_by_page() {
        let transport = Arc::new(MockHttpTransport::new());
        let full: Vec<serde_json::Value> = (1..=50)
            .map(|i| issue(i, i, "2024-01-01T00:00:00Z"))
            .collect();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/gitlab/api/v4/projects/42/issues?per_page=50&page=1&order_by=updated_at&sort=asc",
            ok_json(&serde_json::json!(full)),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/gitlab/api/v4/projects/42/issues?per_page=50&page=2&order_by=updated_at&sort=asc",
            ok_json(&serde_json::json!([issue(51, 51, "2024-01-02T00:00:00Z")])),
        );
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 51);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_dedupes_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let prior = "2024-01-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/gitlab/api/v4/projects/42/issues?per_page=50&page=1&order_by=updated_at&sort=asc&updated_after={}", percent_encode_path_component(prior)),
            ok_json(&serde_json::json!([
                issue(10, 1, "2024-01-01T00:00:00Z"),
                issue(20, 2, "2024-02-01T00:00:00Z"),
            ])),
        );
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(prior.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        // The per-project `iid` (2), not the global `id` (20), is the
        // document id `fetch_content` later resolves.
        assert_eq!(res.events[0].document_id().as_str(), "2");
    }

    #[test]
    fn initial_sync_requires_project_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::GitLab, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "personal_access_token": "p" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/gitlab/api/v4/projects/42/issues?per_page=50&page=1&order_by=updated_at&sort=asc",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn subscribe_webhook_registers_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/gitlab/api/v4/projects/42/hooks",
            ok_json(&serde_json::json!({ "id": 555 })),
        );
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/gl")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("555"));
    }

    #[test]
    fn webhook_open_maps_to_created() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "object_kind": "issue",
            "object_attributes": { "id": 900, "iid": 9, "action": "open", "updated_at": "2024-03-01T00:00:00Z" }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        // Document id is the per-project `iid` (9), not global `id` (900).
        assert_eq!(evs[0].document_id().as_str(), "9");
    }

    #[test]
    fn webhook_close_maps_to_updated() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "object_kind": "issue",
            "object_attributes": { "id": 900, "iid": 9, "action": "close" }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_missing_attributes_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "object_kind": "issue" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }

    #[test]
    fn fetch_content_renders_description() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/gitlab/api/v4/projects/42/issues/7",
            ok_json(&serde_json::json!({
                "id": 700, "iid": 7, "title": "Bug", "description": "It broke", "state": "opened"
            })),
        );
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("7"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Bug"));
        assert!(body.contains("It broke"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/gitlab/api/v4/projects/42/issues/99",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = GitLabConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("99"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
