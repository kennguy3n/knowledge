//! Bitbucket connector — Bitbucket Cloud REST API 2.0 + webhooks.
//!
//! * `initial_sync` walks
//!   `GET /2.0/repositories/{ws}/{repo}/pullrequests` and follows the
//!   absolute `next` page URL Bitbucket returns until it is absent.
//! * `incremental_sync` adds a `q=updated_on>"<iso>"` filter keyed
//!   off the prior watermark and dedupes the boundary PR.
//! * `fetch_content` reads a single pull request and renders its
//!   description as Markdown.
//! * `subscribe_webhook` POSTs `/2.0/repositories/{ws}/{repo}/hooks`.
//! * `handle_webhook_event` parses a pull-request payload; the event
//!   key arrives in the `X-Event-Key` header, so an optional
//!   `event_key` field is honoured when present (default update).
//!
//! Bitbucket Cloud authenticates with OAuth2 bearer tokens (or an
//! app-password access token used as a bearer), so the bearer helpers
//! apply directly. `authenticate` accepts a configured `access_token`
//! or an OAuth2 `authorization_code`.

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

/// Default Bitbucket Cloud REST base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.bitbucket.org";

/// Page size for list endpoints. Bitbucket's documented max is 100.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured token.
const DEFAULT_SCOPE: &str = "pullrequest";

/// Nested HTML link block (`links.html.href`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitbucketLinks {
    /// The HTML link, when present.
    #[serde(default)]
    pub html: Option<BitbucketHref>,
}

/// A single `{href}` link.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitbucketHref {
    /// Target URL.
    #[serde(default)]
    pub href: Option<String>,
}

/// One Bitbucket pull request (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitbucketPullRequest {
    /// Pull request id (unique within the repository).
    pub id: i64,
    /// PR title.
    #[serde(default)]
    pub title: Option<String>,
    /// Plain-text/Markdown description.
    #[serde(default)]
    pub description: Option<String>,
    /// `OPEN` / `MERGED` / `DECLINED`.
    #[serde(default)]
    pub state: Option<String>,
    /// Creation timestamp.
    #[serde(default)]
    pub created_on: Option<DateTime<Utc>>,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated_on: Option<DateTime<Utc>>,
    /// Web links.
    #[serde(default)]
    pub links: Option<BitbucketLinks>,
}

/// One page of a paginated Bitbucket collection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitbucketPage {
    /// Items on this page.
    #[serde(default)]
    pub values: Vec<BitbucketPullRequest>,
    /// Absolute URL of the next page, when present.
    #[serde(default)]
    pub next: Option<String>,
}

/// Response from `POST /2.0/repositories/{ws}/{repo}/hooks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitbucketHookResponse {
    /// Hook uuid.
    #[serde(default)]
    pub uuid: Option<String>,
}

/// Pull-request webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitbucketWebhookPayload {
    /// Optional event-key hint (`pullrequest:created`,
    /// `pullrequest:updated`, `pullrequest:fulfilled`,
    /// `pullrequest:rejected`); normally carried in `X-Event-Key`.
    #[serde(default)]
    pub event_key: Option<String>,
    /// The pull request the event concerns.
    #[serde(default)]
    pub pullrequest: Option<BitbucketPullRequest>,
}

/// Bitbucket connector.
pub struct BitbucketConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for BitbucketConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitbucketConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl BitbucketConnector {
    /// Construct a Bitbucket connector.
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

    /// Override the Bitbucket REST base URL.
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

    fn workspace(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("bitbucket: auth_config_json.workspace is required".into())
            })
    }

    fn repo_slug(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("repo_slug")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("bitbucket: auth_config_json.repo_slug is required".into())
            })
    }

    /// Walk every PR page, following the absolute `next` URL.
    fn paginate_pull_requests(
        &self,
        first_url: &str,
        token: &OAuth2Token,
    ) -> Result<Vec<BitbucketPullRequest>> {
        let mut out = Vec::<BitbucketPullRequest>::new();
        let mut next = Some(first_url.to_string());
        for _ in 0..MAX_LIST_PAGES {
            let Some(url) = next.take() else {
                return Ok(out);
            };
            let page: BitbucketPage = bearer_get_json(
                &self.transport,
                "bitbucket",
                "/2.0/repositories/{ws}/{repo}/pullrequests",
                &url,
                token,
                &[],
            )?;
            out.extend(page.values);
            match page.next {
                Some(n) if !n.is_empty() => next = Some(n),
                _ => return Ok(out),
            }
        }
        Err(ConnectorError::Sync(format!(
            "bitbucket pullrequests exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn pr_to_event(pr: &BitbucketPullRequest, kind: &str) -> ConnectorEvent {
    let occurred_at = pr.updated_on.or(pr.created_on).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(pr.id.to_string());
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

impl Connector for BitbucketConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(tok) = config
            .auth_config_json
            .get("access_token")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                tok,
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
                    "bitbucket authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let ws = Self::workspace(config)?;
        let repo = Self::repo_slug(config)?;
        let ws_enc = percent_encode_path_component(&ws);
        let repo_enc = percent_encode_path_component(&repo);
        let url = format!(
            "{base_url}/2.0/repositories/{ws_enc}/{repo_enc}/pullrequests?state=OPEN&state=MERGED&state=DECLINED&pagelen={}&sort=updated_on",
            self.page_size
        );
        let prs = self.paginate_pull_requests(&url, token)?;
        let mut events = Vec::with_capacity(prs.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for pr in &prs {
            events.push(pr_to_event(pr, "create"));
            if let Some(t) = pr.updated_on.or(pr.created_on) {
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
        let ws = Self::workspace(config)?;
        let repo = Self::repo_slug(config)?;
        let ws_enc = percent_encode_path_component(&ws);
        let repo_enc = percent_encode_path_component(&repo);
        let prior = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let mut url = format!(
            "{base_url}/2.0/repositories/{ws_enc}/{repo_enc}/pullrequests?state=OPEN&state=MERGED&state=DECLINED&pagelen={}&sort=updated_on",
            self.page_size
        );
        if let Some(p) = prior {
            let query = format!("updated_on>\"{}\"", p.to_rfc3339());
            let _ = write!(url, "&q={}", percent_encode_path_component(&query));
        }
        let prs = self.paginate_pull_requests(&url, token)?;
        let mut events = Vec::with_capacity(prs.len());
        let mut watermark = prior;
        for pr in &prs {
            let when = pr.updated_on.or(pr.created_on);
            // The `updated_on>` filter is exclusive, but guard the
            // boundary anyway for safety.
            if let (Some(prev), Some(t)) = (prior, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(pr_to_event(pr, "update"));
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
        let ws = Self::workspace(config)?;
        let repo = Self::repo_slug(config)?;
        let ws_enc = percent_encode_path_component(&ws);
        let repo_enc = percent_encode_path_component(&repo);
        let id_enc = percent_encode_path_component(document_id.as_str());
        let url = format!("{base_url}/2.0/repositories/{ws_enc}/{repo_enc}/pullrequests/{id_enc}");
        let pr: BitbucketPullRequest = bearer_get_json(
            &self.transport,
            "bitbucket",
            "/2.0/repositories/{ws}/{repo}/pullrequests/{id}",
            &url,
            token,
            &[],
        )?;

        let title = pr
            .title
            .clone()
            .unwrap_or_else(|| format!("Pull request {}", document_id.as_str()));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(desc) = pr.description.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(desc);
        }
        let body = md.trim_end().to_string();

        let source_url = pr
            .links
            .and_then(|l| l.html)
            .and_then(|h| h.href)
            .unwrap_or_else(|| {
                format!(
                    "https://bitbucket.org/{ws}/{repo}/pull-requests/{}",
                    document_id.as_str()
                )
            });

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "bitbucket",
                "pull_request_id": pr.id,
                "state": pr.state,
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
        let ws = Self::workspace(config)?;
        let repo = Self::repo_slug(config)?;
        let ws_enc = percent_encode_path_component(&ws);
        let repo_enc = percent_encode_path_component(&repo);
        let url = format!("{base_url}/2.0/repositories/{ws_enc}/{repo_enc}/hooks");
        let body = serde_json::json!({
            "description": "knowledge-substrate",
            "url": callback_url,
            "active": true,
            "events": [
                "pullrequest:created",
                "pullrequest:updated",
                "pullrequest:fulfilled",
                "pullrequest:rejected"
            ],
        });
        let resp: BitbucketHookResponse = bearer_post_json(
            &self.transport,
            "bitbucket",
            "/2.0/repositories/{ws}/{repo}/hooks",
            &url,
            token,
            &[],
            &body,
        )?;
        let uuid = resp.uuid.filter(|s| !s.is_empty()).ok_or_else(|| {
            ConnectorError::Webhook(
                "bitbucket /2.0/repositories/{ws}/{repo}/hooks returned no uuid".into(),
            )
        })?;
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("bitbucket-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(uuid);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: BitbucketWebhookPayload = serde_json::from_slice(body)?;
        let pr = payload.pullrequest.ok_or_else(|| {
            ConnectorError::Webhook("bitbucket webhook missing pullrequest".into())
        })?;
        if pr.id == 0 {
            return Err(ConnectorError::Webhook(
                "bitbucket webhook missing pullrequest id".into(),
            ));
        }
        let kind = match payload.event_key.as_deref() {
            Some("pullrequest:created") => "create",
            Some("pullrequest:rejected") => "delete",
            _ => "update",
        };
        Ok(vec![pr_to_event(&pr, kind)])
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
                "bb-access",
                "bb-refresh",
                Utc::now() + Duration::hours(1),
                "pullrequest",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Bitbucket,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "access_token": "bbtok",
            "workspace": "acme",
            "repo_slug": "app",
            "api_base_url": "https://api.test/bb",
        }))
    }

    fn pr(id: i64, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "title": format!("PR {id}"), "updated_on": updated,
            "links": { "html": { "href": format!("https://bb/pr/{id}") } }
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn first_url() -> &'static str {
        "https://api.test/bb/2.0/repositories/acme/app/pullrequests?state=OPEN&state=MERGED&state=DECLINED&pagelen=50&sort=updated_on"
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "bbtok"
        );
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(
            ConnectorKind::Bitbucket,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "abc", "workspace": "w", "repo_slug": "r"
        }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "bb-access"
        );
    }

    #[test]
    fn initial_sync_follows_next_url() {
        let transport = Arc::new(MockHttpTransport::new());
        let page2 = "https://api.test/bb/2.0/repositories/acme/app/pullrequests?page=2";
        transport.expect(
            HttpMethod::Get,
            first_url(),
            ok_json(
                &serde_json::json!({ "values": [pr(1, "2024-01-01T00:00:00Z")], "next": page2 }),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            page2,
            ok_json(&serde_json::json!({ "values": [pr(2, "2024-01-02T00:00:00Z")] })),
        );
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_applies_q_filter_and_dedupes() {
        let transport = Arc::new(MockHttpTransport::new());
        let prior = "2024-01-01T00:00:00+00:00";
        let query = format!("updated_on>\"{}\"", "2024-01-01T00:00:00+00:00");
        transport.expect(
            HttpMethod::Get,
            format!(
                "{}&q={}",
                first_url(),
                percent_encode_path_component(&query)
            ),
            ok_json(&serde_json::json!({ "values": [
                pr(1, "2024-01-01T00:00:00Z"),
                pr(2, "2024-02-01T00:00:00Z"),
            ] })),
        );
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(prior.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "2");
    }

    #[test]
    fn initial_sync_requires_workspace_and_repo() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(
            ConnectorKind::Bitbucket,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({ "access_token": "t", "workspace": "w" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn subscribe_webhook_captures_uuid() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/bb/2.0/repositories/acme/app/hooks",
            ok_json(&serde_json::json!({ "uuid": "{abc-123}" })),
        );
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/bb")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("{abc-123}"));
    }

    #[test]
    fn webhook_created_and_rejected_map_correctly() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let created = serde_json::json!({
            "event_key": "pullrequest:created", "pullrequest": pr(5, "2024-01-01T00:00:00Z")
        });
        let rejected = serde_json::json!({
            "event_key": "pullrequest:rejected", "pullrequest": pr(6, "2024-01-01T00:00:00Z")
        });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&created).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&rejected).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn webhook_missing_pullrequest_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "event_key": "pullrequest:updated" });
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
            "https://api.test/bb/2.0/repositories/acme/app/pullrequests/12",
            ok_json(&serde_json::json!({
                "id": 12, "title": "Add feature", "description": "Body text", "state": "OPEN"
            })),
        );
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("12"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Add feature"));
        assert!(body.contains("Body text"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/bb/2.0/repositories/acme/app/pullrequests/99",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = BitbucketConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("99"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
