//! Jira connector — Jira REST API v3 + Atlassian webhooks.
//!
//! * `initial_sync` runs JQL `ORDER BY created ASC` against
//!   `/rest/api/3/search` and walks pages via `startAt` / `maxResults`.
//! * `incremental_sync` runs JQL `updated >= "<cursor>" ORDER BY updated ASC`
//!   keyed off the prior watermark.
//! * `subscribe_webhook` POSTs `/rest/api/3/webhook` to register
//!   issue events; the substrate persists Jira's returned webhook id
//!   into the `WebhookSubscription` metadata for later revocation.
//! * `handle_webhook_event` parses Jira's `webhookEvent` payload —
//!   `jira:issue_created`, `jira:issue_updated`, `jira:issue_deleted`,
//!   plus permission-scheme changes.
//!
//! Production wiring runs over [`HttpTransport`] — the substrate
//! constructs a [`JiraConnector`] with a real
//! `connector_framework::BlockingHttpTransport` (under the
//! `http-client` feature) and a real `OAuth2Client` for the
//! `https://auth.atlassian.com/oauth/token` exchange. Unit tests
//! pass `MockHttpTransport` + a fixture OAuth2 exchange.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult,
    SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Atlassian Jira REST base URL. Per-instance overrides go
/// through `auth_config_json.api_base_url` (Jira Cloud sites are
/// per-tenant: `https://your-tenant.atlassian.net`).
pub const DEFAULT_API_BASE_URL: &str = "https://your-tenant.atlassian.net";

/// Page size for JQL `/search`. Jira's documented max is 100; we
/// stay at 50 to balance latency vs round-trips for the median
/// workspace.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on number of pages a single sync will walk —
/// catches mis-shaped server responses that lie about `total`.
pub const MAX_SEARCH_PAGES: usize = 10_000;

/// One Jira issue (subset of fields used by the substrate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraIssue {
    /// Issue key (e.g. `PROJ-123`).
    pub key: String,
    /// Numeric id (Jira's stable internal id).
    #[serde(default)]
    pub id: String,
    /// Field bundle.
    #[serde(default)]
    pub fields: JiraFields,
}

/// Subset of `JiraIssue.fields` used by the substrate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraFields {
    /// Issue summary line.
    #[serde(default)]
    pub summary: String,
    /// Created timestamp.
    #[serde(default)]
    pub created: Option<DateTime<Utc>>,
    /// Updated timestamp.
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    /// Status object — surfaced for downstream evidence enrichment.
    #[serde(default)]
    pub status: Option<JiraStatus>,
}

/// Jira status sub-object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraStatus {
    /// Status name.
    pub name: String,
}

/// One page of a JQL `/search` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraSearchResponse {
    /// Issues on this page.
    #[serde(default)]
    pub issues: Vec<JiraIssue>,
    /// `startAt` echo — substrate-side cursor base.
    #[serde(default, rename = "startAt")]
    pub start_at: u32,
    /// `maxResults` echo.
    #[serde(default, rename = "maxResults")]
    pub max_results: u32,
    /// Total issues matching the JQL — used to determine end-of-page.
    #[serde(default)]
    pub total: u32,
}

/// Response from `POST /rest/api/3/webhook` — Jira returns the list
/// of webhooks created.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraWebhookCreateResponse {
    /// One entry per registered webhook event filter.
    #[serde(default, rename = "webhookRegistrationResult")]
    pub webhook_registration_result: Vec<JiraWebhookRegistrationEntry>,
}

/// One row of a Jira webhook registration result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraWebhookRegistrationEntry {
    /// Numeric webhook id Jira assigned. Present on success.
    #[serde(default, rename = "createdWebhookId")]
    pub created_webhook_id: Option<i64>,
    /// Validation errors Jira flagged.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Jira webhook payload (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraWebhookPayload {
    /// `jira:issue_created`, `jira:issue_updated`, `jira:issue_deleted`,
    /// `permissionscheme_updated`.
    #[serde(rename = "webhookEvent")]
    pub webhook_event: String,
    /// Issue body (absent on permission-scheme events).
    #[serde(default)]
    pub issue: Option<JiraIssue>,
    /// Wall-clock timestamp Jira emitted the event.
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// User account id whose permission changed (only on permission events).
    #[serde(default, rename = "accountId")]
    pub account_id: Option<String>,
    /// New role / permission level (`administrators`, `developers`, …).
    #[serde(default)]
    pub new_role: Option<String>,
    /// Document key (when `issue` is absent — permission events).
    #[serde(default, rename = "issueKey")]
    pub issue_key: Option<String>,
}

/// Jira connector.
pub struct JiraConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for JiraConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JiraConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl JiraConnector {
    /// Construct a Jira connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://auth.atlassian.com/oauth/token`. The production
    /// substrate wires these to `BlockingHttpTransport` +
    /// `OAuth2Client`; tests use `MockHttpTransport`.
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

    /// Override the Jira REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size used by JQL `/search`. Clamped to
    /// `[1, 100]` per Jira's documented maximum.
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

    /// Walk every JQL `/search` page until either `total` is
    /// satisfied, the server returns an empty page, or [`MAX_SEARCH_PAGES`]
    /// is hit.
    fn paginate_search(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        jql: &str,
    ) -> Result<Vec<JiraIssue>> {
        let mut issues = Vec::<JiraIssue>::new();
        let mut start_at: u32 = 0;
        for _ in 0..MAX_SEARCH_PAGES {
            let url = format!(
                "{base_url}/rest/api/3/search?jql={}&startAt={start_at}&maxResults={}\
                 &fields=summary,created,updated,status",
                percent_encode_path_component(jql),
                self.page_size,
            );
            let resp: JiraSearchResponse = bearer_get_json(
                &self.transport,
                "jira",
                "/rest/api/3/search",
                &url,
                token,
                &[],
            )?;
            let returned = u32::try_from(resp.issues.len()).unwrap_or(u32::MAX);
            issues.extend(resp.issues);
            // Jira returns an empty `issues` array when we've walked
            // past `total`. Stop on the first empty page even if
            // `total` claims more — protects against off-by-one in
            // the server-side total.
            if returned == 0 {
                return Ok(issues);
            }
            // Advance saturatingly to avoid u32 overflow on very
            // large datasets.
            start_at = start_at.saturating_add(returned);
            if start_at >= resp.total {
                return Ok(issues);
            }
        }
        Err(ConnectorError::Sync(format!(
            "jira /rest/api/3/search exceeded {MAX_SEARCH_PAGES} pages without exhausting total"
        )))
    }
}

fn issue_to_event(issue: &JiraIssue, kind: &str) -> ConnectorEvent {
    let occurred_at = issue
        .fields
        .updated
        .or(issue.fields.created)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(issue.key.clone());
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

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "browsers" | "browse" | "viewer" | "read" => Some(SourcePermissionLevel::Read),
        "developers" | "edit" | "write" => Some(SourcePermissionLevel::Write),
        "administrators" | "admin" | "owner" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// Build a JQL `updated >= "<cursor>"` clause keyed off the prior
/// watermark. Jira accepts RFC-3339 timestamps in single quotes; we
/// pre-escape any embedded single quote defensively even though the
/// watermark we wrote in the prior sync is always machine-formatted.
fn watermark_jql(cursor: &str) -> String {
    let escaped = cursor.replace('\'', "\\'");
    format!("updated >= '{escaped}' ORDER BY updated ASC")
}

impl Connector for JiraConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "jira authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let issues = self.paginate_search(&base_url, token, "ORDER BY created ASC")?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(issues.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for issue in &issues {
            events.push(issue_to_event(issue, "create"));
            if let Some(t) = issue.fields.updated.or(issue.fields.created) {
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
        let jql = state
            .cursor
            .as_deref()
            .map_or_else(|| "ORDER BY updated ASC".to_string(), watermark_jql);
        let issues = self.paginate_search(&base_url, token, &jql)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(issues.len());
        let prior_watermark: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let mut watermark = prior_watermark;
        for issue in &issues {
            // The JQL filter is `updated >= '<cursor>'` (Jira does
            // not support a strict `>` against the JQL time grammar
            // — its parser truncates to the precision of the
            // supplied literal). That means the boundary issue —
            // the one whose `updated` exactly equals the prior
            // cursor — is returned every incremental run. Skip it
            // client-side so the substrate sees each update at most
            // once, matching the Confluence / HubSpot dedup pattern.
            let when = issue.fields.updated.or(issue.fields.created);
            if let (Some(prev), Some(t)) = (prior_watermark, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(issue_to_event(issue, "update"));
            // Mirror `initial_sync` — fall back to `created` when
            // `updated` is absent so the watermark always
            // advances. Jira normally echoes `created` into
            // `updated` for new issues, but tolerating the
            // missing-`updated` case keeps the two sync paths
            // symmetric and defends against API responses where
            // the field is omitted (sparse-field projections).
            if let Some(t) = when {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/rest/api/3/webhook");
        // Per the Jira API docs (`/rest/api/3/webhook`), the body is
        // `{webhooks: [{events:[..], jqlFilter:"..."}], url: "..."}`.
        let body = serde_json::json!({
            "url": callback_url,
            "webhooks": [{
                "events": [
                    "jira:issue_created",
                    "jira:issue_updated",
                    "jira:issue_deleted"
                ],
                "jqlFilter": ""
            }],
        });
        let resp: JiraWebhookCreateResponse = bearer_post_json(
            &self.transport,
            "jira",
            "/rest/api/3/webhook",
            &url,
            token,
            &[],
            &body,
        )?;
        // Jira returns one entry per webhook in the request batch.
        // We sent exactly one — pull its id (or error if the
        // registration was rejected).
        let entry = resp
            .webhook_registration_result
            .into_iter()
            .next()
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "jira /rest/api/3/webhook returned empty registration result".into(),
                )
            })?;
        if !entry.errors.is_empty() {
            return Err(ConnectorError::Webhook(format!(
                "jira webhook registration failed: {}",
                entry.errors.join(", ")
            )));
        }
        let webhook_id = entry.created_webhook_id.ok_or_else(|| {
            ConnectorError::Webhook("jira /rest/api/3/webhook returned no createdWebhookId".into())
        })?;
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            // Jira generates its own webhook secret out-of-band (set in
            // the developer console); we surface the configured secret
            // from `auth_config_json.webhook_secret` if present, else
            // we record a placeholder so the substrate can sign incoming
            // requests once the operator fills it in.
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("jira-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            // Jira webhooks expire after 30 days per the API docs.
            Some(Utc::now() + chrono::Duration::days(30)),
        );
        // Stash the registration id in the metadata so the substrate
        // can revoke / re-register on rotation. Re-using
        // `provider_subscription_id` keeps this consistent with the
        // other connectors.
        subscription.provider_subscription_id = Some(webhook_id.to_string());
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Jira posts one webhook event per HTTP request.
        let p: JiraWebhookPayload = serde_json::from_slice(body)?;
        let event = match p.webhook_event.as_str() {
            "jira:issue_created" => {
                let issue = p
                    .issue
                    .ok_or_else(|| ConnectorError::Webhook("missing issue body".into()))?;
                issue_to_event(&issue, "create")
            }
            "jira:issue_updated" => {
                let issue = p
                    .issue
                    .ok_or_else(|| ConnectorError::Webhook("missing issue body".into()))?;
                issue_to_event(&issue, "update")
            }
            "jira:issue_deleted" => {
                let issue = p
                    .issue
                    .ok_or_else(|| ConnectorError::Webhook("missing issue body".into()))?;
                issue_to_event(&issue, "delete")
            }
            "permissionscheme_updated" => {
                let key = p
                    .issue_key
                    .or_else(|| p.issue.as_ref().map(|i| i.key.clone()))
                    .ok_or_else(|| {
                        ConnectorError::Webhook(
                            "permissionscheme_updated payload missing issueKey".into(),
                        )
                    })?;
                let occurred_at = p
                    .timestamp
                    .and_then(DateTime::<Utc>::from_timestamp_millis)
                    .unwrap_or_else(Utc::now);
                ConnectorEvent::PermissionChanged {
                    document_id: SourceDocumentId::new(key),
                    user_id: SourceUserId::new(p.account_id.unwrap_or_default()),
                    new_level: p.new_role.as_deref().and_then(parse_role),
                    occurred_at,
                }
            }
            other => {
                return Err(ConnectorError::Webhook(format!(
                    "unknown Jira webhookEvent: {other}"
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
                "jira-access",
                "jira-refresh",
                Utc::now() + Duration::hours(1),
                "read:jira-work read:jira-user manage:jira-webhook",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Jira, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/jira",
            }))
    }

    fn issue(key: &str, created: DateTime<Utc>, updated: DateTime<Utc>) -> JiraIssue {
        JiraIssue {
            key: key.into(),
            id: key.into(),
            fields: JiraFields {
                summary: "test".into(),
                created: Some(created),
                updated: Some(updated),
                status: None,
            },
        }
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "jira-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::Jira, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_events_and_watermark_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/jira/rest/api/3/search?jql=ORDER%20BY%20created%20ASC&startAt=0&maxResults=50&fields=summary,created,updated,status",
            ok_json(&serde_json::json!({
                "issues": [issue("PROJ-1", now, now)],
                "startAt": 0, "maxResults": 50, "total": 1,
            })),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn initial_sync_paginates_via_start_at() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        // First page: 2 issues, total=3 — must request page 2.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/jira/rest/api/3/search?jql=ORDER%20BY%20created%20ASC&startAt=0&maxResults=50&fields=summary,created,updated,status",
            ok_json(&serde_json::json!({
                "issues": [issue("PROJ-1", now, now), issue("PROJ-2", now, now)],
                "startAt": 0, "maxResults": 50, "total": 3,
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/jira/rest/api/3/search?jql=ORDER%20BY%20created%20ASC&startAt=2&maxResults=50&fields=summary,created,updated,status",
            ok_json(&serde_json::json!({
                "issues": [issue("PROJ-3", now, now)],
                "startAt": 2, "maxResults": 50, "total": 3,
            })),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn initial_sync_stops_when_total_is_satisfied_without_extra_round_trip() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/jira/rest/api/3/search?jql=ORDER%20BY%20created%20ASC&startAt=0&maxResults=50&fields=summary,created,updated,status",
            ok_json(&serde_json::json!({
                "issues": [issue("PROJ-1", now, now)],
                "startAt": 0, "maxResults": 50, "total": 1,
            })),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let _ = c.initial_sync(&cfg(), &tok).unwrap();
        // start_at + returned >= total ⇒ no second page fetch.
        assert_eq!(transport.recorded().len(), 1);
    }

    #[test]
    fn incremental_sync_keys_jql_off_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let cursor = (now - Duration::hours(1)).to_rfc3339();
        let expected_jql = format!("updated >= '{cursor}' ORDER BY updated ASC");
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/jira/rest/api/3/search?jql={}&startAt=0&maxResults=50&fields=summary,created,updated,status",
                percent_encode_path_component(&expected_jql)
            ),
            ok_json(&serde_json::json!({
                "issues": [issue("PROJ-2", now - Duration::days(1), now)],
                "startAt": 0, "maxResults": 50, "total": 1,
            })),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn incremental_sync_dedupes_boundary_issue_against_prior_cursor() {
        // Jira's JQL is `updated >= '<cursor>'` (inclusive), so the
        // last issue from the prior sync (whose `updated` equals
        // the watermark) is returned on every subsequent run. The
        // connector must skip it client-side and only surface
        // strictly-newer rows. Mirror the dedup invariant for
        // HubSpot / Confluence.
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let cursor_t = now - Duration::hours(1);
        let cursor = cursor_t.to_rfc3339();
        let expected_jql = format!("updated >= '{cursor}' ORDER BY updated ASC");
        // Page returns two issues: the boundary one (same `updated`
        // as cursor) and one strictly newer. Only the newer must
        // be emitted, and the watermark must advance to it.
        let newer = now;
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/jira/rest/api/3/search?jql={}&startAt=0&maxResults=50&fields=summary,created,updated,status",
                percent_encode_path_component(&expected_jql)
            ),
            ok_json(&serde_json::json!({
                "issues": [
                    issue("PROJ-1", now - Duration::days(1), cursor_t),
                    issue("PROJ-2", now - Duration::days(1), newer),
                ],
                "startAt": 0, "maxResults": 50, "total": 2,
            })),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(
            res.events.len(),
            1,
            "boundary issue must be skipped; only strictly-newer remains"
        );
        assert_eq!(
            res.events[0].document_id().as_str(),
            "PROJ-2",
            "the strictly-newer issue must be the one emitted"
        );
        let next = res.next_cursor.expect("watermark must advance");
        let next_t = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            next_t > cursor_t,
            "watermark must advance past the boundary"
        );
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/jira/rest/api/3/search?jql=ORDER%20BY%20created%20ASC&startAt=0&maxResults=50&fields=summary,created,updated,status",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn subscribe_webhook_registers_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/jira/rest/api/3/webhook",
            ok_json(&serde_json::json!({
                "webhookRegistrationResult": [
                    {"createdWebhookId": 42, "errors": []}
                ]
            })),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/jira")
            .unwrap();
        assert_eq!(sub.connector, c.instance);
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("42"));
    }

    #[test]
    fn subscribe_webhook_propagates_registration_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/jira/rest/api/3/webhook",
            ok_json(&serde_json::json!({
                "webhookRegistrationResult": [
                    {"errors": ["URL is not reachable from Jira"]}
                ]
            })),
        );
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/jira")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_parses_issue_created() {
        let body = serde_json::json!({
            "webhookEvent": "jira:issue_created",
            "timestamp": Utc::now().timestamp_millis(),
            "issue": {
                "key": "PROJ-99",
                "id": "10099",
                "fields": {
                    "summary": "demo",
                    "created": Utc::now(),
                    "updated": Utc::now(),
                }
            }
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn webhook_parses_permission_change() {
        let body = serde_json::json!({
            "webhookEvent": "permissionscheme_updated",
            "issueKey": "PROJ-50",
            "accountId": "acc-1",
            "new_role": "administrators",
            "timestamp": Utc::now().timestamp_millis(),
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(*new_level, Some(SourcePermissionLevel::Admin));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_event_errors() {
        let body = serde_json::json!({"webhookEvent": "weird:thing"});
        let transport = Arc::new(MockHttpTransport::new());
        let c = JiraConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
