//! GitHub connector — GitHub REST API v3.
//!
//! * `authenticate` POSTs the authorization code to
//!   `https://github.com/login/oauth/access_token` via the wired
//!   [`OAuth2CodeExchange`] (production: real `OAuth2Client` against
//!   GitHub's IdP; tests: `MockHttpTransport`).
//! * `initial_sync` walks `GET /repos/{owner}/{repo}/issues` (with
//!   `state=all`) keyed off `page` / `per_page`, emitting every issue
//!   and PR as a `ConnectorEvent`. Seeds the cursor with the most
//!   recent `updated_at` timestamp.
//! * `incremental_sync` walks the same endpoint filtered by
//!   `since=<cursor>` so only issues updated after the watermark are
//!   returned.
//! * `subscribe_webhook` POSTs `POST /repos/{owner}/{repo}/hooks` to
//!   register a webhook for issue / PR / push events.
//! * `handle_webhook_event` parses GitHub's `X-GitHub-Event` webhook
//!   payload (inlined as JSON) into [`ConnectorEvent`] variants.
//!
//! Wiring contract: the constructor takes an `Arc<dyn HttpTransport>`
//! and an `Arc<dyn OAuth2CodeExchange>`; production wires
//! `BlockingHttpTransport` + `OAuth2Client`, tests wire
//! `MockHttpTransport` + a fixed-token exchange.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult,
    SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default GitHub REST API base URL. Override via
/// `auth_config_json.api_base_url` for GitHub Enterprise instances.
pub const DEFAULT_API_BASE_URL: &str = "https://api.github.com";

/// Default page size for list endpoints. GitHub's documented maximum
/// is 100; we use 100 to minimise round-trips.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_LIST_PAGES: usize = 10_000;

/// GitHub API version header value.
const GITHUB_API_VERSION: &str = "2022-11-28";

// ─────────────────── wire types ───────────────────

/// One GitHub issue / pull request as returned by the Issues API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIssue {
    /// Issue / PR number.
    pub number: u64,
    /// Numeric id.
    #[serde(default)]
    pub id: u64,
    /// Title.
    #[serde(default)]
    pub title: String,
    /// State: `open` or `closed`.
    #[serde(default)]
    pub state: String,
    /// Created timestamp.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Updated timestamp.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Closed timestamp. `None` when the issue is open.
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    /// If present, the issue is actually a pull request.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

/// Response from `POST /repos/{owner}/{repo}/hooks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitHubWebhookCreateResponse {
    /// Webhook id GitHub assigned.
    #[serde(default)]
    pub id: Option<u64>,
    /// Active state.
    #[serde(default)]
    pub active: bool,
    /// Events the webhook is subscribed to.
    #[serde(default)]
    pub events: Vec<String>,
}

/// GitHub webhook payload (subset). The `action` field is common to
/// most event types; the concrete issue/PR body is nested under
/// `issue` or `pull_request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubWebhookPayload {
    /// `opened`, `edited`, `closed`, `reopened`, `deleted`, …
    #[serde(default)]
    pub action: String,
    /// Event type (from `X-GitHub-Event` header, inlined).
    #[serde(default)]
    pub event_type: String,
    /// Issue body (for `issues` events).
    #[serde(default)]
    pub issue: Option<GitHubIssue>,
    /// Pull request body (for `pull_request` events).
    #[serde(default)]
    pub pull_request: Option<GitHubIssue>,
    /// Push event: ref that was pushed.
    #[serde(default, rename = "ref")]
    pub push_ref: Option<String>,
    /// Push event: most recent commit SHA.
    #[serde(default)]
    pub after: Option<String>,
    /// Member object for `member` events.
    #[serde(default)]
    pub member: Option<GitHubMember>,
    /// Repository object (present on most events).
    #[serde(default)]
    pub repository: Option<GitHubRepo>,
    /// Changes object (for `member` `edited` events — contains
    /// `permission.to` with the new collaborator role).
    #[serde(default)]
    pub changes: Option<serde_json::Value>,
}

/// Minimal member info from a `member` webhook event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubMember {
    /// GitHub username.
    #[serde(default)]
    pub login: String,
    /// Numeric user id.
    #[serde(default)]
    pub id: u64,
}

/// Minimal repository info from webhook payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRepo {
    /// Full name (`owner/repo`).
    #[serde(default)]
    pub full_name: String,
    /// Numeric repo id.
    #[serde(default)]
    pub id: u64,
}

// ─────────────────── connector ───────────────────

/// GitHub connector.
///
/// Holds the wired [`HttpTransport`] + [`OAuth2CodeExchange`] used to
/// drive every GitHub REST call.
pub struct GitHubConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GitHubConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GitHubConnector {
    /// Construct a GitHub connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://github.com/login/oauth/access_token`. Production
    /// wires these to `BlockingHttpTransport` + `OAuth2Client`;
    /// tests use `MockHttpTransport`.
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

    /// Override the GitHub REST base URL (production wires the
    /// default; GitHub Enterprise wires its custom host).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size. Clamped to `[1, 100]` per GitHub's
    /// documented maximum.
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

    /// Read the `owner/repo` slug from `auth_config_json.repository`.
    fn resolved_repo(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("repository")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "github: auth_config_json.repository is required (format: owner/repo)".into(),
                )
            })
    }

    /// GitHub API version + User-Agent headers required by the API.
    fn extra_headers() -> Vec<(&'static str, &'static str)> {
        vec![
            ("X-GitHub-Api-Version", GITHUB_API_VERSION),
            ("User-Agent", "knowledge-substrate"),
        ]
    }

    /// Walk every issues-list page until either the returned page is
    /// smaller than `page_size` (signalling the final page) or
    /// [`MAX_LIST_PAGES`] is hit.
    fn paginate_issues(
        &self,
        base_url: &str,
        repo: &str,
        token: &OAuth2Token,
        since: Option<&str>,
    ) -> Result<Vec<GitHubIssue>> {
        let mut all_issues = Vec::<GitHubIssue>::new();
        let extra = Self::extra_headers();
        for page in 1..=MAX_LIST_PAGES {
            let mut url = format!(
                "{base_url}/repos/{repo}/issues?state=all&sort=updated&direction=asc\
                 &per_page={}&page={page}",
                self.page_size,
            );
            if let Some(s) = since {
                url.push_str("&since=");
                url.push_str(&percent_encode_path_component(s));
            }
            let page_issues: Vec<GitHubIssue> = bearer_get_json(
                &self.transport,
                "github",
                "/repos/issues",
                &url,
                token,
                &extra,
            )?;
            let returned = page_issues.len();
            all_issues.extend(page_issues);
            if returned < self.page_size as usize {
                return Ok(all_issues);
            }
        }
        Err(ConnectorError::Sync(format!(
            "github /repos/{repo}/issues exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn issue_to_event(issue: &GitHubIssue) -> ConnectorEvent {
    let occurred_at = issue
        .created_at
        .or(issue.updated_at)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(issue.number.to_string());
    ConnectorEvent::DocumentCreated {
        document_id: id,
        occurred_at,
    }
}

fn issue_to_sync_event(issue: &GitHubIssue) -> ConnectorEvent {
    let occurred_at = issue
        .updated_at
        .or(issue.created_at)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(issue.number.to_string());
    ConnectorEvent::DocumentUpdated {
        document_id: id,
        occurred_at,
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "read" | "pull" => Some(SourcePermissionLevel::Read),
        "write" | "push" | "triage" | "maintain" => Some(SourcePermissionLevel::Write),
        "admin" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

impl Connector for GitHubConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "github authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let repo = Self::resolved_repo(config)?;
        let issues = self.paginate_issues(&base_url, &repo, token, None)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(issues.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for issue in &issues {
            events.push(issue_to_event(issue));
            if let Some(t) = issue.updated_at.or(issue.created_at) {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let repo = Self::resolved_repo(config)?;
        let since = state.cursor.as_deref();
        let issues = self.paginate_issues(&base_url, &repo, token, since)?;
        let prior_watermark: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(issues.len());
        let mut watermark = prior_watermark;
        for issue in &issues {
            // GitHub `since` is inclusive — the issue whose
            // `updated_at` exactly equals the cursor is returned
            // every run. Skip it client-side to deduplicate.
            let when = issue.updated_at.or(issue.created_at);
            if when.is_none() {
                tracing::warn!(
                    issue_number = issue.number,
                    "github incremental_sync: issue has no updated_at or created_at; \
                     skipping dedup"
                );
            }
            if let (Some(prev), Some(t)) = (prior_watermark, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(issue_to_sync_event(issue));
            if let Some(t) = when {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        })
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let repo = Self::resolved_repo(config)?;
        let url = format!("{base_url}/repos/{repo}/hooks");
        let webhook_secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "github subscribe_webhook: auth_config_json.webhook_secret is required".into(),
                )
            })?
            .to_string();
        let body = serde_json::json!({
            "name": "web",
            "active": true,
            "events": ["issues", "pull_request", "push", "member"],
            "config": {
                "url": callback_url,
                "content_type": "json",
                "secret": webhook_secret,
                "insecure_ssl": "0"
            }
        });
        let extra = Self::extra_headers();
        let resp: GitHubWebhookCreateResponse = bearer_post_json(
            &self.transport,
            "github",
            "/repos/hooks",
            &url,
            token,
            &extra,
            &body,
        )?;
        let hook_id = resp.id.ok_or_else(|| {
            ConnectorError::Webhook("github /repos/hooks returned no webhook id".into())
        })?;
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(webhook_secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(hook_id.to_string());
        Ok(subscription)
    }

    /// Parse a GitHub webhook payload and emit [`ConnectorEvent`]s.
    ///
    /// **Inlining contract**: the caller must inject the value of the
    /// `X-GitHub-Event` HTTP header into the JSON body as
    /// `"event_type"` before calling this method.  GitHub does *not*
    /// include the event type in the JSON payload itself — it is
    /// delivered exclusively via the HTTP header.  The webhook server
    /// layer (see `webhook_server.rs`) is responsible for performing
    /// this inlining; if the raw GitHub payload is passed without
    /// inlining, all events will fall through to the error branch.
    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let p: GitHubWebhookPayload = serde_json::from_slice(body)?;
        let event = match p.event_type.as_str() {
            "issues" => {
                let issue = p
                    .issue
                    .ok_or_else(|| ConnectorError::Webhook("missing issue body".into()))?;
                let id = SourceDocumentId::new(issue.number.to_string());
                let occurred_at = issue
                    .updated_at
                    .or(issue.created_at)
                    .unwrap_or_else(Utc::now);
                match p.action.as_str() {
                    "opened" => ConnectorEvent::DocumentCreated {
                        document_id: id,
                        occurred_at,
                    },
                    "closed" | "deleted" => ConnectorEvent::DocumentDeleted {
                        document_id: id,
                        occurred_at,
                    },
                    _ => ConnectorEvent::DocumentUpdated {
                        document_id: id,
                        occurred_at,
                    },
                }
            }
            "pull_request" => {
                let pr = p
                    .pull_request
                    .ok_or_else(|| ConnectorError::Webhook("missing pull_request body".into()))?;
                let id = SourceDocumentId::new(pr.number.to_string());
                let occurred_at = pr.updated_at.or(pr.created_at).unwrap_or_else(Utc::now);
                match p.action.as_str() {
                    "opened" => ConnectorEvent::DocumentCreated {
                        document_id: id,
                        occurred_at,
                    },
                    "closed" => ConnectorEvent::DocumentDeleted {
                        document_id: id,
                        occurred_at,
                    },
                    _ => ConnectorEvent::DocumentUpdated {
                        document_id: id,
                        occurred_at,
                    },
                }
            }
            "push" => {
                let doc_ref = p.push_ref.unwrap_or_else(|| "refs/heads/main".into());
                let id = SourceDocumentId::new(format!("push:{doc_ref}"));
                ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at: Utc::now(),
                }
            }
            "member" => {
                let member = p
                    .member
                    .ok_or_else(|| ConnectorError::Webhook("missing member body".into()))?;
                let repo_name = p.repository.map(|r| r.full_name).unwrap_or_default();
                let new_level = if p.action.as_str() == "removed" {
                    None
                } else {
                    let role = p
                        .changes
                        .as_ref()
                        .and_then(|c| c.get("permission"))
                        .and_then(|perm| perm.get("to"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("write");
                    parse_role(role)
                };
                ConnectorEvent::PermissionChanged {
                    document_id: SourceDocumentId::new(repo_name),
                    user_id: SourceUserId::new(member.login),
                    new_level,
                    occurred_at: Utc::now(),
                }
            }
            other => {
                return Err(ConnectorError::Webhook(format!(
                    "unknown GitHub event type: {other}"
                )))
            }
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
                "gh-access",
                "gh-refresh",
                Utc::now() + Duration::hours(1),
                "repo",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/github",
                "repository": "owner/test-repo",
                "webhook_secret": "test-webhook-secret",
            }))
    }

    fn issue(number: u64, state: &str, updated: DateTime<Utc>) -> GitHubIssue {
        GitHubIssue {
            number,
            id: number,
            title: format!("Issue #{number}"),
            state: state.into(),
            created_at: Some(updated - Duration::hours(1)),
            updated_at: Some(updated),
            closed_at: if state == "closed" {
                Some(updated)
            } else {
                None
            },
            pull_request: None,
        }
    }

    fn expect_issues_list(
        transport: &MockHttpTransport,
        base_url: &str,
        repo: &str,
        page: usize,
        since: Option<&str>,
        response: &[GitHubIssue],
    ) {
        let mut url = format!(
            "{base_url}/repos/{repo}/issues?state=all&sort=updated&direction=asc\
             &per_page={}&page={page}",
            DEFAULT_PAGE_SIZE,
        );
        if let Some(s) = since {
            url.push_str("&since=");
            url.push_str(&percent_encode_path_component(s));
        }
        transport.expect(
            HttpMethod::Get,
            url,
            MockResponse::ok_json(serde_json::to_vec(response).unwrap()),
        );
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("repo"));
        assert_eq!(tok.access_token.expose(), "gh-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let bad_cfg =
            ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&bad_cfg).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_walks_issues_and_produces_watermark() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/github";
        let repo = "owner/test-repo";

        // Single page (fewer than page_size results).
        expect_issues_list(
            &transport,
            base,
            repo,
            1,
            None,
            &[
                issue(1, "open", now - Duration::hours(2)),
                issue(2, "open", now),
            ],
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn initial_sync_emits_created_for_closed_issues() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/github";
        let repo = "owner/test-repo";

        expect_issues_list(&transport, base, repo, 1, None, &[issue(1, "closed", now)]);

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn incremental_sync_skips_boundary_issue() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/github";
        let repo = "owner/test-repo";
        let cursor = (now - Duration::hours(1)).to_rfc3339();

        expect_issues_list(
            &transport,
            base,
            repo,
            1,
            Some(&cursor),
            &[
                // This issue's updated_at == cursor — should be
                // skipped (dedup).
                issue(1, "open", now - Duration::hours(1)),
                // This issue is newer — should be emitted.
                issue(2, "open", now),
            ],
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let inst = ConnectorInstanceId::new_v4();
        let mut state = SyncState::new(inst);
        state.cursor = Some(cursor);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            res.events[0].document_id(),
            &SourceDocumentId::new("2".to_string())
        );
    }

    #[test]
    fn subscribe_webhook_posts_and_returns_subscription() {
        let transport = MockHttpTransport::new();
        let base = "https://api.test/github";
        let repo = "owner/test-repo";
        let url = format!("{base}/repos/{repo}/hooks");

        transport.expect(
            HttpMethod::Post,
            url,
            MockResponse::ok_json(
                serde_json::to_vec(&GitHubWebhookCreateResponse {
                    id: Some(42),
                    active: true,
                    events: vec!["issues".into(), "pull_request".into()],
                })
                .unwrap(),
            ),
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let inst = ConnectorInstanceId::new_v4();
        let c = GitHubConnector::new(inst, transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhook")
            .unwrap();
        assert_eq!(sub.connector, inst);
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("42"));
    }

    #[test]
    fn handle_issue_opened_event() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let now = Utc::now();
        let payload = serde_json::json!({
            "event_type": "issues",
            "action": "opened",
            "issue": {
                "number": 42,
                "id": 42,
                "title": "Bug",
                "state": "open",
                "created_at": now,
                "updated_at": now,
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn handle_issue_closed_event() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let now = Utc::now();
        let payload = serde_json::json!({
            "event_type": "issues",
            "action": "closed",
            "issue": {
                "number": 7,
                "id": 7,
                "title": "Done",
                "state": "closed",
                "created_at": now,
                "updated_at": now,
                "closed_at": now,
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn handle_pull_request_opened_event() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let now = Utc::now();
        let payload = serde_json::json!({
            "event_type": "pull_request",
            "action": "opened",
            "pull_request": {
                "number": 10,
                "id": 10,
                "title": "PR",
                "state": "open",
                "created_at": now,
                "updated_at": now,
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(
            evs[0].document_id(),
            &SourceDocumentId::new("10".to_string())
        );
    }

    #[test]
    fn handle_push_event() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let payload = serde_json::json!({
            "event_type": "push",
            "ref": "refs/heads/main",
            "after": "abc123",
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn handle_member_added_event() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let payload = serde_json::json!({
            "event_type": "member",
            "action": "added",
            "member": {
                "login": "octocat",
                "id": 1,
            },
            "repository": {
                "full_name": "owner/repo",
                "id": 100,
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::PermissionChanged { .. }));
    }

    #[test]
    fn handle_member_edited_event_extracts_permission() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let payload = serde_json::json!({
            "event_type": "member",
            "action": "edited",
            "member": {
                "login": "octocat",
                "id": 1,
            },
            "repository": {
                "full_name": "owner/repo",
                "id": 100,
            },
            "changes": {
                "permission": {
                    "to": "admin"
                }
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(*new_level, Some(SourcePermissionLevel::Admin));
            }
            other => panic!("expected PermissionChanged, got {other:?}"),
        }
    }

    #[test]
    fn requires_repository_in_config() {
        let transport = MockHttpTransport::new();
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), Arc::new(transport), oauth());
        let bad_cfg =
            ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "code",
                    "api_base_url": "https://api.test/github",
                }));
        let tok = c.authenticate(&bad_cfg).unwrap();
        let err = c.initial_sync(&bad_cfg, &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }
}
