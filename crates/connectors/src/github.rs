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

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpResponse, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SourcePermissionLevel, SourceUserId,
    SyncRunResult, SyncState, WatermarkCursor, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
};
use serde::de::DeserializeOwned;
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
    /// Markdown body. Populated on the single-issue detail endpoint
    /// (and on list responses); `None` when GitHub omits it.
    #[serde(default)]
    pub body: Option<String>,
    /// Author of the issue / PR.
    #[serde(default)]
    pub user: Option<GitHubAuthor>,
    /// If present, the issue is actually a pull request.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

/// Author / commenter login envelope (`{login, ...}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitHubAuthor {
    /// GitHub username.
    #[serde(default)]
    pub login: String,
}

/// One comment as returned by
/// `GET /repos/{owner}/{repo}/issues/{number}/comments`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GitHubComment {
    /// Markdown body of the comment.
    #[serde(default)]
    pub body: String,
    /// Comment author.
    #[serde(default)]
    pub user: Option<GitHubAuthor>,
    /// When the comment was posted.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
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

    /// Apply the headers GitHub requires on every REST call: bearer
    /// auth, the `X-GitHub-Api-Version` pin, a `User-Agent` (GitHub
    /// rejects requests without one), and the recommended
    /// `Accept: application/vnd.github+json`. Shared by the GET and
    /// POST builders so every GitHub request — including the
    /// `subscribe_webhook` POST — sends an identical header set.
    fn with_github_headers(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
        req.with_bearer(token.access_token.expose())
            .with_header("Accept", "application/vnd.github+json")
            .with_header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .with_header("User-Agent", "knowledge-substrate")
    }

    /// Build a bearer-authenticated GET carrying the standard GitHub
    /// headers (see [`Self::with_github_headers`]).
    fn github_get(url: &str, token: &OAuth2Token) -> HttpRequest {
        Self::with_github_headers(HttpRequest::get(url), token)
    }

    /// Execute a POST with a JSON body, classifying failures with
    /// GitHub's rate-limit semantics (see [`classify_github_failure`])
    /// so a rate-limited POST (e.g. webhook creation hitting the
    /// secondary limit) is surfaced as a retriable
    /// [`ConnectorError::Sync`] rather than mis-mapped to
    /// [`ConnectorError::Auth`] by the generic classifier.
    fn github_post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
        body: &B,
    ) -> Result<R> {
        let body_bytes = serde_json::to_vec(body).map_err(|e| {
            ConnectorError::Sync(format!(
                "github {endpoint} request JSON serialise failed: {e}"
            ))
        })?;
        let req = Self::with_github_headers(HttpRequest::post(url, body_bytes), token)
            .with_header("Content-Type", "application/json");
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_github_failure(endpoint, &resp));
        }
        parse_github_json(endpoint, &resp.body)
    }

    /// Execute a GET and parse a JSON body, classifying failures with
    /// GitHub's rate-limit semantics (see [`classify_github_failure`]).
    fn github_get_json<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let resp = self.transport.execute(Self::github_get(url, token))?;
        if !resp.is_success() {
            return Err(classify_github_failure(endpoint, &resp));
        }
        parse_github_json(endpoint, &resp.body)
    }

    /// Execute a GET that returns one page of a paginated collection,
    /// returning the decoded page alongside the `rel="next"` URL parsed
    /// from the response `Link` header (GitHub's canonical pagination
    /// mechanism).
    fn github_get_page<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<GitHubPage<R>> {
        let resp = self.transport.execute(Self::github_get(url, token))?;
        if !resp.is_success() {
            return Err(classify_github_failure(endpoint, &resp));
        }
        let link_header = resp.header("link");
        let link_present = link_header.is_some();
        let next_url = parse_link_next(link_header);
        let items = parse_github_json::<R>(endpoint, &resp.body)?;
        Ok(GitHubPage {
            items,
            next_url,
            link_present,
        })
    }

    /// Construct the `page=N` `…/issues` list URL — used for the first
    /// request and for the no-`Link`-header fallback path.
    fn issues_page_url(
        &self,
        base_url: &str,
        repo: &str,
        since: Option<&str>,
        page: u32,
    ) -> String {
        let mut url = format!(
            "{base_url}/repos/{repo}/issues?state=all&sort=updated&direction=asc\
             &per_page={}&page={page}",
            self.page_size,
        );
        if let Some(s) = since {
            url.push_str("&since=");
            url.push_str(&percent_encode_path_component(s));
        }
        url
    }

    /// Walk every issues-list page, following the `Link: rel="next"`
    /// header GitHub returns. Manual `page=N` walking is used only as a
    /// fallback for servers that emit no `Link` header on *any* page
    /// (e.g. a proxy strips them wholesale): once any page has carried
    /// a `Link` header (`seen_link`), the absence of `next` is the
    /// authoritative end of the collection. Switching to manual walking
    /// mid-run would be unsound — after following opaque `Link` cursors
    /// the connector no longer knows its numeric page and would
    /// re-fetch (and duplicate) a page it already saw.
    /// [`MAX_LIST_PAGES`] bounds the walk against a mis-shaped server.
    fn paginate_issues(
        &self,
        base_url: &str,
        repo: &str,
        token: &OAuth2Token,
        since: Option<&str>,
    ) -> Result<Vec<GitHubIssue>> {
        let mut all_issues = Vec::<GitHubIssue>::new();
        let mut next_url = Some(self.issues_page_url(base_url, repo, since, 1));
        let mut manual_page: u32 = 1;
        let mut seen_link = false;
        for _ in 0..MAX_LIST_PAGES {
            let Some(url) = next_url.take() else {
                return Ok(all_issues);
            };
            let page: GitHubPage<Vec<GitHubIssue>> =
                self.github_get_page("/repos/issues", &url, token)?;
            let returned = page.items.len();
            seen_link |= page.link_present;
            next_url = if let Some(next) = page.next_url {
                Some(next)
            } else if seen_link {
                None
            } else if returned >= self.page_size as usize {
                manual_page = manual_page.saturating_add(1);
                Some(self.issues_page_url(base_url, repo, since, manual_page))
            } else {
                None
            };
            all_issues.extend(page.items);
        }
        Err(ConnectorError::Sync(format!(
            "github /repos/{repo}/issues exceeded {MAX_LIST_PAGES} pages"
        )))
    }

    /// Walk every comment page for an issue / PR, following the
    /// `Link: rel="next"` header. As in [`Self::paginate_issues`],
    /// manual `page=N` walking is only a fallback for servers that emit
    /// no `Link` header on any page; once a `Link` header has been seen
    /// the absence of `next` ends the walk.
    fn paginate_comments(
        &self,
        base_url: &str,
        repo: &str,
        number: &str,
        token: &OAuth2Token,
    ) -> Result<Vec<GitHubComment>> {
        let comments_url = |page: u32| {
            format!(
                "{base_url}/repos/{repo}/issues/{number}/comments?per_page={}&page={page}",
                self.page_size,
            )
        };
        let mut all = Vec::<GitHubComment>::new();
        let mut next_url = Some(comments_url(1));
        let mut manual_page: u32 = 1;
        let mut seen_link = false;
        for _ in 0..MAX_LIST_PAGES {
            let Some(url) = next_url.take() else {
                return Ok(all);
            };
            let page: GitHubPage<Vec<GitHubComment>> =
                self.github_get_page("/repos/issues/comments", &url, token)?;
            let returned = page.items.len();
            seen_link |= page.link_present;
            all.extend(page.items);
            next_url = if let Some(next) = page.next_url {
                Some(next)
            } else if seen_link {
                None
            } else if returned >= self.page_size as usize {
                manual_page = manual_page.saturating_add(1);
                Some(comments_url(manual_page))
            } else {
                None
            };
        }
        Err(ConnectorError::Sync(format!(
            "github /repos/{repo}/issues/{number}/comments exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

/// One page of a paginated GitHub collection plus the pagination
/// metadata parsed from the response `Link` header.
struct GitHubPage<R> {
    /// Decoded items on this page.
    items: R,
    /// The `rel="next"` URL from the `Link` header, if present.
    next_url: Option<String>,
    /// Whether the response carried a `Link` header at all. A `Link`
    /// header without a `next` relation authoritatively marks the
    /// final page; the absence of any `Link` header means the server
    /// did not paginate via `Link` and the caller must fall back to
    /// the short-page heuristic.
    link_present: bool,
}

/// Parse a JSON GitHub response body into `R`, mapping a decode
/// failure to a retriable [`ConnectorError::Sync`] with a bounded
/// body prefix for diagnostics.
fn parse_github_json<R: DeserializeOwned>(endpoint: &str, body: &[u8]) -> Result<R> {
    serde_json::from_slice::<R>(body).map_err(|e| {
        ConnectorError::Sync(format!(
            "github {endpoint} JSON parse failed: {e} (body prefix: {})",
            String::from_utf8_lossy(&body[..body.len().min(256)])
        ))
    })
}

/// Extract the `rel="next"` URL from an RFC 8288 `Link` header.
///
/// GitHub returns pagination cursors as e.g.
/// `<https://api.github.com/...?page=2>; rel="next", <...?page=9>; rel="last"`.
/// Malformed segments are skipped rather than aborting the parse so a
/// single bad entry can't strand pagination on page 1.
fn parse_link_next(link_header: Option<&str>) -> Option<String> {
    let header = link_header?;
    for part in header.split(',') {
        let part = part.trim();
        let mut segs = part.split(';');
        let Some(url_seg) = segs.next() else {
            continue;
        };
        let url_seg = url_seg.trim();
        let Some(url) = url_seg.strip_prefix('<').and_then(|u| u.strip_suffix('>')) else {
            continue;
        };
        for param in segs {
            let param = param.trim();
            if let Some(rel) = param.strip_prefix("rel=") {
                let rel = rel.trim().trim_matches('"');
                if rel.split_whitespace().any(|r| r == "next") {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

/// Classify a non-2xx GitHub response, honouring GitHub's rate-limit
/// semantics.
///
/// GitHub signals rate-limit exhaustion with **403** (primary limit)
/// or **429** (secondary limit) plus `X-RateLimit-Remaining: 0` and/or
/// a `Retry-After` header — *not* the 401/403-means-bad-credentials
/// shape the generic [`classify_failure`] assumes. Left to the generic
/// classifier a rate-limited 403 maps to [`ConnectorError::Auth`],
/// which would wrongly trigger a re-authorisation prompt instead of a
/// retry. Detect the rate-limit shape and surface it as a retriable
/// [`ConnectorError::Sync`] (the transport already honours
/// `Retry-After` on its own retry loop; this keeps the *connector-run*
/// classification correct when retries are exhausted).
fn classify_github_failure(endpoint: &str, resp: &HttpResponse) -> ConnectorError {
    let remaining_zero = resp
        .header("x-ratelimit-remaining")
        .is_some_and(|v| v.trim() == "0");
    let retry_after = resp.retry_after_seconds();
    let is_rate_limited =
        matches!(resp.status, 403 | 429) && (remaining_zero || retry_after.is_some());
    if is_rate_limited {
        let reset = resp.header("x-ratelimit-reset").unwrap_or("unknown");
        return ConnectorError::Sync(format!(
            "github {endpoint} rate limited (status {}, x-ratelimit-remaining=0={remaining_zero}, \
             x-ratelimit-reset={reset}, retry-after={retry_after:?}); sync will be retried",
            resp.status,
        ));
    }
    classify_failure("github", endpoint, resp)
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

/// Derive the human-facing web host from the configured REST API base
/// URL so citation `source_url`s resolve on the right instance instead
/// of always pointing at public `github.com`.
///
/// * Public GitHub — `https://api.github.com` → `https://github.com`.
/// * GitHub Enterprise Server — the REST API is rooted at `/api/v3`
///   while the web UI lives at the host root, so
///   `https://github.acme.com/api/v3` → `https://github.acme.com`.
/// * Any other shape is returned trimmed of a trailing slash, which is
///   still a better basis for a link than a hard-coded host.
fn web_base_url(api_base_url: &str) -> String {
    let trimmed = api_base_url.trim_end_matches('/');
    if let Some(web) = trimmed.strip_suffix("/api/v3") {
        return web.to_string();
    }
    if trimmed == "https://api.github.com" {
        return "https://github.com".to_string();
    }
    trimmed.to_string()
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
        let mut cursor = WatermarkCursor::empty();
        for issue in &issues {
            events.push(issue_to_event(issue));
            if let Some(t) = issue.updated_at.or(issue.created_at) {
                cursor.observe(t, &issue.number.to_string());
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
        let repo = Self::resolved_repo(config)?;
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let since = prior.query_since();
        let issues = self.paginate_issues(&base_url, &repo, token, since.as_deref())?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(issues.len());
        let mut cursor = prior.clone();
        for issue in &issues {
            // GitHub `since` is inclusive — the issue whose `updated_at`
            // exactly equals the cursor is returned every run, so dedup
            // client-side while still surfacing brand-new boundary records
            // sharing the watermark second.
            let Some(t) = issue.updated_at.or(issue.created_at) else {
                tracing::warn!(
                    issue_number = issue.number,
                    "github incremental_sync: issue has no updated_at or created_at; skipping"
                );
                continue;
            };
            let id = issue.number.to_string();
            if !prior.should_emit(t, &id) {
                continue;
            }
            events.push(issue_to_sync_event(issue));
            cursor.observe(t, &id);
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
        let repo = Self::resolved_repo(config)?;
        // Document ids are the bare issue / PR number (see
        // `issue_to_event`).
        let number = document_id.as_str();
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ConnectorError::Sync(format!(
                "github fetch_content: document id {number:?} is not an issue number"
            )));
        }
        let issue_url = format!("{base_url}/repos/{repo}/issues/{number}");
        let issue: GitHubIssue =
            self.github_get_json("/repos/issues/{number}", &issue_url, token)?;

        let comments = self.paginate_comments(&base_url, &repo, number, token)?;

        // Assemble a Markdown document: title heading, issue body,
        // then a comments section attributing each comment to its
        // author.
        let mut md = String::new();
        // Writing to a `String` is infallible, so the `write!`
        // results are deliberately discarded.
        let _ = write!(md, "# {} (#{})\n\n", issue.title, issue.number);
        let author = issue.user.as_ref().map_or("", |u| u.login.as_str());
        if !author.is_empty() {
            let _ = write!(md, "_Opened by @{author}_\n\n");
        }
        let body = issue.body.as_deref().unwrap_or("").trim();
        if body.is_empty() {
            md.push_str("_No description provided._\n");
        } else {
            md.push_str(body);
            md.push('\n');
        }
        if !comments.is_empty() {
            md.push_str("\n## Comments\n");
            for c in &comments {
                let login = c.user.as_ref().map_or("", |u| u.login.as_str());
                let when = c.created_at.map_or_else(String::new, |t| t.to_rfc3339());
                let _ = write!(md, "\n### @{login} ({when})\n\n");
                md.push_str(c.body.trim());
                md.push('\n');
            }
        }

        let is_pr = issue.pull_request.is_some();
        let web_base = web_base_url(&base_url);
        let kind_seg = if is_pr { "pull" } else { "issues" };
        let source_url = format!("{web_base}/{repo}/{kind_seg}/{number}");

        Ok(FetchedContent::text(md, "text/markdown")
            .with_title(issue.title.clone())
            .with_metadata(serde_json::json!({
                "repository": repo,
                "number": issue.number,
                "state": issue.state,
                "is_pull_request": is_pr,
                "author": author,
                "comment_count": comments.len(),
            }))
            .with_source_url(source_url))
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
        let resp: GitHubWebhookCreateResponse =
            self.github_post_json("/repos/hooks", &url, token, &body)?;
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
            body: None,
            user: None,
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
                // updated_at == watermark and id already seen — deduped.
                issue(1, "open", now - Duration::hours(1)),
                // updated_at == watermark but a brand-new id — surfaced.
                issue(3, "open", now - Duration::hours(1)),
                // strictly newer — emitted.
                issue(2, "open", now),
            ],
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let inst = ConnectorInstanceId::new_v4();
        let mut state = SyncState::new(inst);
        // Prior cursor: watermark at `cursor` with issue #1 already seen.
        state.cursor = Some(format!("{cursor}|1"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["3", "2"]);
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

    // ───────────── fetch_content ─────────────

    const GH_BASE: &str = "https://api.test/github";

    #[test]
    fn fetch_content_assembles_issue_body_and_comments() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/12"),
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "number": 12,
                    "id": 999,
                    "title": "Login flow broken",
                    "state": "open",
                    "body": "Steps to reproduce:\n1. Click login",
                    "user": { "login": "ada" }
                }))
                .unwrap(),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/12/comments?per_page=100&page=1"),
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!([
                    { "body": "I can repro.", "user": { "login": "grace" }, "created_at": "2026-06-01T10:00:00Z" }
                ]))
                .unwrap(),
            ),
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("12"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Login flow broken (#12)"));
        assert!(body.contains("_Opened by @ada_"));
        assert!(body.contains("Steps to reproduce:"));
        assert!(body.contains("## Comments"));
        assert!(body.contains("### @grace"));
        assert!(body.contains("I can repro."));
        assert_eq!(fc.mime_type, "text/markdown");
        assert_eq!(fc.title.as_deref(), Some("Login flow broken"));
        assert_eq!(fc.metadata["comment_count"], serde_json::json!(1));
        assert_eq!(fc.metadata["is_pull_request"], serde_json::json!(false));
        // The web host is derived from the API base URL (the mock base
        // here), not hard-coded to public github.com.
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://api.test/github/owner/test-repo/issues/12")
        );
    }

    #[test]
    fn fetch_content_handles_empty_body_and_no_comments() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/5"),
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "number": 5,
                    "title": "Empty",
                    "state": "closed",
                    "body": serde_json::Value::Null,
                    "pull_request": { "url": "https://api.test/github/repos/owner/test-repo/pulls/5" }
                }))
                .unwrap(),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/5/comments?per_page=100&page=1"),
            MockResponse::ok_json(b"[]".to_vec()),
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("5"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("_No description provided._"));
        assert!(!body.contains("## Comments"));
        // pull_request present → PR permalink + flag.
        assert_eq!(fc.metadata["is_pull_request"], serde_json::json!(true));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://api.test/github/owner/test-repo/pull/5")
        );
    }

    #[test]
    fn fetch_content_rejects_non_numeric_document_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("not-a-number"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/404"),
            MockResponse::status(404, b"{\"message\":\"Not Found\"}".to_vec()),
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("404"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_429_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/7"),
            MockResponse::status(429, b"rate limited".to_vec()),
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("7"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn web_base_url_maps_api_hosts_to_web_hosts() {
        // Public GitHub's API host maps to the web host.
        assert_eq!(web_base_url("https://api.github.com"), "https://github.com");
        assert_eq!(
            web_base_url("https://api.github.com/"),
            "https://github.com"
        );
        // GitHub Enterprise Server: the REST API lives under `/api/v3`,
        // the web UI at the host root.
        assert_eq!(
            web_base_url("https://github.acme.com/api/v3"),
            "https://github.acme.com"
        );
        // Unknown shapes are returned trimmed, not rewritten to github.com.
        assert_eq!(
            web_base_url("https://api.test/github"),
            "https://api.test/github"
        );
    }

    #[test]
    fn fetch_content_source_url_uses_enterprise_host() {
        const GHE_API: &str = "https://github.acme.com/api/v3";
        let ghe_cfg = || {
            ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "demo-code",
                    "api_base_url": GHE_API,
                    "repository": "owner/test-repo",
                    "webhook_secret": "test-webhook-secret",
                }))
        };
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GHE_API}/repos/owner/test-repo/issues/3"),
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "number": 3,
                    "title": "Enterprise issue",
                    "state": "open",
                    "body": "x",
                    "user": { "login": "ada" }
                }))
                .unwrap(),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            format!("{GHE_API}/repos/owner/test-repo/issues/3/comments?per_page=100&page=1"),
            MockResponse::ok_json(b"[]".to_vec()),
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&ghe_cfg()).unwrap();
        let fc = c
            .fetch_content(&ghe_cfg(), &tok, &SourceDocumentId::new("3"))
            .unwrap();
        // Citation resolves on the Enterprise host, not public github.com.
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://github.acme.com/owner/test-repo/issues/3")
        );
    }

    // ───────────── Link-header pagination ─────────────

    /// Build a 200 OK JSON response carrying a `Link` header.
    fn ok_json_with_link(body: Vec<u8>, link: &str) -> MockResponse {
        let mut r = MockResponse::ok_json(body);
        r.headers.push(("Link".into(), link.into()));
        r
    }

    #[test]
    fn parse_link_next_extracts_next_url() {
        let h = "<https://api.github.com/repositories/1/issues?page=2>; rel=\"next\", \
                 <https://api.github.com/repositories/1/issues?page=9>; rel=\"last\"";
        assert_eq!(
            parse_link_next(Some(h)),
            Some("https://api.github.com/repositories/1/issues?page=2".to_string())
        );
    }

    #[test]
    fn parse_link_next_none_when_no_next_rel_or_absent() {
        let h = "<https://api.github.com/repositories/1/issues?page=9>; rel=\"last\", \
                 <https://api.github.com/repositories/1/issues?page=1>; rel=\"first\"";
        assert_eq!(parse_link_next(Some(h)), None);
        assert_eq!(parse_link_next(None), None);
    }

    #[test]
    fn parse_link_next_skips_malformed_segments() {
        // A malformed leading segment must not strand pagination on
        // page 1 — the parser skips it and finds the real `next`.
        let h = "garbage-without-brackets, <https://api.example/issues?page=3>; rel=\"next\"";
        assert_eq!(
            parse_link_next(Some(h)),
            Some("https://api.example/issues?page=3".to_string())
        );
    }

    #[test]
    fn initial_sync_follows_link_header_pagination() {
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/github";
        let repo = "owner/test-repo";

        let page1_url = format!(
            "{base}/repos/{repo}/issues?state=all&sort=updated&direction=asc\
             &per_page={DEFAULT_PAGE_SIZE}&page=1",
        );
        // The `next` URL is opaque to the connector — it must follow it
        // verbatim rather than reconstruct `page=2` itself. Note page 1
        // returns FEWER than `page_size` results yet still advances,
        // proving the Link header (not the short-page heuristic) drives
        // pagination.
        let page2_url = format!("{base}/repos/{repo}/issues?page=2&cursor=opaque");
        transport.expect(
            HttpMethod::Get,
            page1_url,
            ok_json_with_link(
                serde_json::to_vec(&[issue(1, "open", now - Duration::hours(2))]).unwrap(),
                &format!("<{page2_url}>; rel=\"next\""),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            page2_url,
            MockResponse::ok_json(serde_json::to_vec(&[issue(2, "open", now)]).unwrap()),
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2, "both pages must be walked");
    }

    #[test]
    fn link_header_without_next_stops_even_on_full_page() {
        // A `Link` header that has `prev`/`first` but no `next` is the
        // authoritative last page — we must NOT fall back to manual
        // page walking even when the page is completely full.
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/github";
        let repo = "owner/test-repo";
        let page_size = 2u32;

        let page1_url = format!(
            "{base}/repos/{repo}/issues?state=all&sort=updated&direction=asc\
             &per_page={page_size}&page=1",
        );
        transport.expect(
            HttpMethod::Get,
            page1_url,
            ok_json_with_link(
                serde_json::to_vec(&[
                    issue(1, "open", now - Duration::hours(2)),
                    issue(2, "open", now),
                ])
                .unwrap(),
                "<https://api.test/github/repos/owner/test-repo/issues?page=1>; rel=\"first\"",
            ),
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(page_size);
        let tok = c.authenticate(&cfg()).unwrap();
        // If the connector wrongly fell back to manual `page=2` walking,
        // the mock would 404 on the unregistered URL and the JSON parse
        // would fail — so a clean 2-event result proves it stopped.
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
    }

    #[test]
    fn link_then_stripped_link_page_does_not_manual_refetch() {
        // Mixed mode: page 1 carries `Link: rel="next"` → page 2, but
        // page 2 comes back FULL with no `Link` header (e.g. a proxy
        // stripped it on that one response). The connector must treat
        // the run as Link-paginated and stop — NOT fall back to manual
        // `page=N` walking, which (after following an opaque cursor)
        // would re-fetch a numeric page and duplicate items.
        let transport = MockHttpTransport::new();
        let now = Utc::now();
        let base = "https://api.test/github";
        let repo = "owner/test-repo";
        let page_size = 2u32;

        let page1_url = format!(
            "{base}/repos/{repo}/issues?state=all&sort=updated&direction=asc\
             &per_page={page_size}&page=1",
        );
        // Opaque cursor URL — deliberately NOT the canonical `page=2`
        // URL the manual fallback would reconstruct.
        let page2_url = format!("{base}/repos/{repo}/issues?cursor=opaque&page=2");
        transport.expect(
            HttpMethod::Get,
            page1_url,
            ok_json_with_link(
                serde_json::to_vec(&[
                    issue(1, "open", now - Duration::hours(3)),
                    issue(2, "open", now - Duration::hours(2)),
                ])
                .unwrap(),
                &format!("<{page2_url}>; rel=\"next\""),
            ),
        );
        // Page 2 is full (== page_size) AND has no Link header.
        transport.expect(
            HttpMethod::Get,
            page2_url,
            MockResponse::ok_json(
                serde_json::to_vec(&[
                    issue(3, "open", now - Duration::hours(1)),
                    issue(4, "open", now),
                ])
                .unwrap(),
            ),
        );

        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(page_size);
        let tok = c.authenticate(&cfg()).unwrap();
        // With the bug, the connector would manual-walk to the canonical
        // (unregistered) `page=2` URL → synthetic 404 → Err. A clean
        // 4-event result proves it stopped after the two real pages.
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 4);
    }

    // ───────────── rate-limit classification ─────────────

    #[test]
    fn fetch_content_maps_rate_limited_403_to_sync_not_auth() {
        // GitHub's PRIMARY rate limit returns HTTP 403 with
        // `X-RateLimit-Remaining: 0`. The generic classifier maps 403 →
        // Auth (a re-auth prompt); for a rate limit that is wrong — it
        // must be a retriable Sync error.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/9"),
            MockResponse {
                status: 403,
                headers: vec![
                    ("x-ratelimit-remaining".into(), "0".into()),
                    ("x-ratelimit-reset".into(), "1700000000".into()),
                ],
                body: br#"{"message":"API rate limit exceeded"}"#.to_vec(),
            },
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("9"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)), "got {err:?}");
    }

    #[test]
    fn fetch_content_plain_403_still_maps_to_auth() {
        // A 403 WITHOUT rate-limit markers is a genuine permission
        // failure and must still surface as Auth so the host
        // re-authorises rather than silently retrying forever.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/8"),
            MockResponse::status(403, br#"{"message":"Forbidden"}"#.to_vec()),
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("8"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn subscribe_webhook_maps_rate_limited_403_to_sync_not_auth() {
        // The webhook-creation POST must share the GET paths'
        // rate-limit semantics: a 403 carrying `X-RateLimit-Remaining: 0`
        // is a retriable Sync error, not an Auth failure that would
        // wrongly prompt re-authorisation.
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            format!("{GH_BASE}/repos/owner/test-repo/hooks"),
            MockResponse {
                status: 403,
                headers: vec![
                    ("x-ratelimit-remaining".into(), "0".into()),
                    ("x-ratelimit-reset".into(), "1700000000".into()),
                ],
                body: br#"{"message":"API rate limit exceeded"}"#.to_vec(),
            },
        );
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhook")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)), "got {err:?}");
    }

    #[test]
    fn subscribe_webhook_plain_403_still_maps_to_auth() {
        // A bare 403 on webhook creation (e.g. the token lacks the
        // `admin:repo_hook` scope) is a genuine permission failure and
        // must still surface as Auth.
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            format!("{GH_BASE}/repos/owner/test-repo/hooks"),
            MockResponse::status(403, br#"{"message":"Forbidden"}"#.to_vec()),
        );
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhook")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn classify_github_failure_secondary_limit_429_with_retry_after_is_sync() {
        let resp = HttpResponse {
            status: 429,
            headers: vec![("retry-after".into(), "30".into())],
            body: br#"{"message":"secondary rate limit"}"#.to_vec(),
        };
        let err = classify_github_failure("/repos/issues", &resp);
        assert!(matches!(err, ConnectorError::Sync(_)), "got {err:?}");
    }

    #[test]
    fn comments_pagination_follows_link_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/12"),
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!({
                    "number": 12, "id": 999, "title": "Paged", "state": "open",
                    "body": "b", "user": { "login": "ada" }
                }))
                .unwrap(),
            ),
        );
        let comments_p2 =
            format!("{GH_BASE}/repos/owner/test-repo/issues/12/comments?page=2&cursor=x");
        transport.expect(
            HttpMethod::Get,
            format!("{GH_BASE}/repos/owner/test-repo/issues/12/comments?per_page=100&page=1"),
            ok_json_with_link(
                serde_json::to_vec(&serde_json::json!([
                    { "body": "first", "user": { "login": "g" }, "created_at": "2026-06-01T10:00:00Z" }
                ]))
                .unwrap(),
                &format!("<{comments_p2}>; rel=\"next\""),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            comments_p2,
            MockResponse::ok_json(
                serde_json::to_vec(&serde_json::json!([
                    { "body": "second", "user": { "login": "h" }, "created_at": "2026-06-02T10:00:00Z" }
                ]))
                .unwrap(),
            ),
        );
        let c = GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("12"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("first"), "page 1 comment missing");
        assert!(body.contains("second"), "page 2 comment missing");
        assert_eq!(fc.metadata["comment_count"], serde_json::json!(2));
    }
}
