//! Linear connector — Linear GraphQL API
//! (`https://api.linear.app/graphql`).
//!
//! * `initial_sync` pages `issues(first: N, after: $cursor)` using
//!   the GraphQL relay `pageInfo { hasNextPage endCursor }` cursor
//!   until `hasNextPage` is false.
//! * `incremental_sync` adds a
//!   `filter: { updatedAt: { gt: $since } }` argument keyed off the
//!   stored RFC-3339 watermark.
//! * `fetch_content` queries a single `issue(id:)` and reconstructs
//!   Markdown from `title` + `description`.
//! * `subscribe_webhook` runs the `webhookCreate` mutation and
//!   persists Linear's returned webhook id.
//! * `handle_webhook_event` parses Linear's delivery envelope
//!   (`{ action, type, data, … }`); a delivery may batch entries, all
//!   of which are emitted.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_post_json, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result,
    SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
};
use serde::Deserialize;

/// Default Linear API base URL. Linear is single-tenant (no
/// per-customer host), but the base is overridable through
/// `auth_config_json.api_base_url` for proxies / tests.
pub const DEFAULT_API_BASE_URL: &str = "https://api.linear.app";

/// Page size for the `issues` connection.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on number of GraphQL pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// Generic GraphQL error entry.
#[derive(Debug, Clone, Deserialize)]
struct GraphQlError {
    #[serde(default)]
    message: String,
}

/// Generic GraphQL envelope.
#[derive(Debug, Clone, Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQlError>>,
}

impl<T> GraphQlResponse<T> {
    /// Unwrap `data`, mapping a populated `errors` array to a
    /// `ConnectorError::Sync`.
    fn into_data(self, ctx: &str) -> Result<T> {
        if let Some(errors) = self.errors.filter(|e| !e.is_empty()) {
            let joined = errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ConnectorError::Sync(format!(
                "linear {ctx} GraphQL error: {joined}"
            )));
        }
        self.data.ok_or_else(|| {
            ConnectorError::Sync(format!("linear {ctx}: GraphQL response had no data"))
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct IssuesData {
    issues: IssueConnection,
}

#[derive(Debug, Clone, Deserialize)]
struct IssueConnection {
    #[serde(default)]
    nodes: Vec<LinearIssue>,
    #[serde(rename = "pageInfo", default)]
    page_info: PageInfo,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage", default)]
    has_next_page: bool,
    #[serde(rename = "endCursor", default)]
    end_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LinearIssue {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
    #[serde(rename = "updatedAt", default)]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct IssueData {
    issue: LinearIssue,
}

#[derive(Debug, Clone, Deserialize)]
struct WebhookCreateData {
    #[serde(rename = "webhookCreate")]
    webhook_create: WebhookCreatePayload,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WebhookCreatePayload {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    webhook: Option<WebhookHandle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WebhookHandle {
    #[serde(default)]
    id: String,
}

/// Linear webhook delivery envelope.
#[derive(Debug, Clone, Default, Deserialize)]
struct LinearWebhookDelivery {
    #[serde(default)]
    action: String,
    #[serde(default)]
    data: LinearWebhookData,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LinearWebhookData {
    #[serde(default)]
    id: String,
    #[serde(rename = "updatedAt", default)]
    updated_at: Option<String>,
}

/// Linear connector.
pub struct LinearConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for LinearConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl LinearConnector {
    /// Construct a Linear connector.
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

    /// Override the Linear API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the GraphQL page size.
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

    fn graphql_url(&self, config: &ConnectorConfig) -> String {
        format!("{}/graphql", self.resolved_base_url(config))
    }

    /// Run one GraphQL POST and deserialize the typed `data` payload.
    fn graphql<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        token: &OAuth2Token,
        query: &str,
        variables: &serde_json::Value,
        ctx: &str,
    ) -> Result<T> {
        let request = serde_json::json!({ "query": query, "variables": variables });
        let resp: GraphQlResponse<T> = bearer_post_json(
            &self.transport,
            "linear",
            "/graphql",
            url,
            token,
            &[],
            &request,
        )?;
        resp.into_data(ctx)
    }

    /// Walk the `issues` connection, collecting every node.
    fn paginate_issues(
        &self,
        url: &str,
        token: &OAuth2Token,
        filter: Option<&str>,
    ) -> Result<Vec<LinearIssue>> {
        let query = format!(
            "query Issues($first: Int!, $after: String) {{ issues(first: $first, after: $after{}) {{ nodes {{ id title description url createdAt updatedAt }} pageInfo {{ hasNextPage endCursor }} }} }}",
            filter.map(|f| format!(", {f}")).unwrap_or_default()
        );
        let mut nodes = Vec::<LinearIssue>::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let variables = serde_json::json!({
                "first": self.page_size,
                "after": after,
            });
            let data: IssuesData = self.graphql(url, token, &query, &variables, "issues")?;
            nodes.extend(data.issues.nodes);
            if data.issues.page_info.has_next_page {
                match data.issues.page_info.end_cursor {
                    Some(cursor) => after = Some(cursor),
                    None => return Ok(nodes),
                }
            } else {
                return Ok(nodes);
            }
        }
        Err(ConnectorError::Sync(format!(
            "linear issues exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn issue_watermark(issue: &LinearIssue) -> Option<DateTime<Utc>> {
    issue
        .updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| issue.created_at.as_deref().and_then(parse_rfc3339))
}

impl Connector for LinearConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "linear authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let url = self.graphql_url(config);
        let issues = self.paginate_issues(&url, token, None)?;
        let mut events = Vec::with_capacity(issues.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for issue in &issues {
            let occurred_at = issue_watermark(issue).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(issue.id.clone()),
                occurred_at,
            });
            if let Some(t) = issue_watermark(issue) {
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
        let url = self.graphql_url(config);
        let prior: Option<DateTime<Utc>> = state.cursor.as_deref().and_then(parse_rfc3339);
        // Linear's `updatedAt` filter is a strict `gt`, so the prior
        // watermark row is excluded — no client-side dedup needed.
        let filter =
            prior.map(|t| format!("filter: {{ updatedAt: {{ gt: \"{}\" }} }}", t.to_rfc3339()));
        let issues = self.paginate_issues(&url, token, filter.as_deref())?;
        let mut events = Vec::with_capacity(issues.len());
        let mut watermark = prior;
        for issue in &issues {
            let occurred_at = issue_watermark(issue).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(issue.id.clone()),
                occurred_at,
            });
            if let Some(t) = issue_watermark(issue) {
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
        let url = self.graphql_url(config);
        let query = "query Issue($id: String!) { issue(id: $id) { id title description url createdAt updatedAt } }";
        let data: IssueData = self.graphql(
            &url,
            token,
            query,
            &serde_json::json!({ "id": document_id.as_str() }),
            "issue",
        )?;
        let issue = data.issue;
        let title = issue.title.clone().unwrap_or_default();
        let description = issue.description.clone().unwrap_or_default();
        let mut md = String::new();
        if !title.is_empty() {
            md.push_str("# ");
            md.push_str(&title);
            md.push_str("\n\n");
        }
        if !description.is_empty() {
            md.push_str(&description);
        }
        let body = md.trim_end().to_string();
        let mut fc = FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "linear",
                "issue_id": issue.id,
                "updated_at": issue.updated_at,
            }));
        if let Some(u) = issue.url {
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
        let url = self.graphql_url(config);
        let query = "mutation WebhookCreate($url: String!) { webhookCreate(input: { url: $url, resourceTypes: [\"Issue\"] }) { success webhook { id } } }";
        let data: WebhookCreateData = self.graphql(
            &url,
            token,
            query,
            &serde_json::json!({ "url": callback_url }),
            "webhookCreate",
        )?;
        if !data.webhook_create.success {
            return Err(ConnectorError::Webhook(
                "linear webhookCreate returned success=false".into(),
            ));
        }
        let provider_id = data
            .webhook_create
            .webhook
            .map(|w| w.id)
            .filter(|id| !id.is_empty());
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("linear-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<LinearWebhookDelivery> =
            if let Ok(batch) = serde_json::from_slice::<Vec<LinearWebhookDelivery>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<LinearWebhookDelivery>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty linear webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            if delivery.data.id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "linear webhook delivery missing data.id".into(),
                ));
            }
            let occurred_at = delivery
                .data
                .updated_at
                .as_deref()
                .or(delivery.created_at.as_deref())
                .and_then(parse_rfc3339)
                .unwrap_or_else(Utc::now);
            let id = SourceDocumentId::new(delivery.data.id);
            let event = match delivery.action.as_str() {
                "create" => ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                },
                "remove" => ConnectorEvent::DocumentDeleted {
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
                "ln-access",
                "ln-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Linear, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/ln",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const GQL_URL: &str = "https://api.test/ln/graphql";

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Linear, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "ln-access"
        );
    }

    #[test]
    fn initial_sync_paginates_relay_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "issues": {
                    "nodes": [
                        {"id": "i1", "title": "one", "createdAt": "2024-01-01T00:00:00Z", "updatedAt": "2024-01-01T00:00:00Z"}
                    ],
                    "pageInfo": { "hasNextPage": true, "endCursor": "C1" }
                }}
            })),
        );
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "issues": {
                    "nodes": [
                        {"id": "i2", "title": "two", "createdAt": "2024-01-02T00:00:00Z", "updatedAt": "2024-01-02T00:00:00Z"}
                    ],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }}
            })),
        );
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
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
    fn incremental_sync_emits_updated() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "issues": {
                    "nodes": [
                        {"id": "i9", "title": "z", "updatedAt": "2024-06-01T00:00:00Z"}
                    ],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }}
            })),
        );
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-01T00:00:00Z".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn graphql_errors_map_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "errors": [{"message": "rate limited"}]
            })),
        );
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "issue": {
                    "id": "i1",
                    "title": "Bug: crash",
                    "description": "Steps to reproduce.",
                    "url": "https://linear.app/x/issue/i1"
                }}
            })),
        );
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("i1"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Bug: crash"));
        assert!(body.contains("Steps to reproduce."));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://linear.app/x/issue/i1")
        );
    }

    #[test]
    fn subscribe_webhook_creates_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "webhookCreate": { "success": true, "webhook": { "id": "wh_1" } } }
            })),
        );
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/ln")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh_1"));
    }

    #[test]
    fn subscribe_webhook_failure_when_success_false() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "webhookCreate": { "success": false, "webhook": null } }
            })),
        );
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.subscribe_webhook(&cfg(), &tok, "https://hook.example/ln"),
            Err(ConnectorError::Webhook(_))
        ));
    }

    #[test]
    fn webhook_parses_single_delivery() {
        let body = serde_json::json!({
            "action": "create",
            "type": "Issue",
            "data": { "id": "i1", "updatedAt": "2024-01-01T00:00:00Z" },
            "createdAt": "2024-01-01T00:00:00Z"
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn webhook_parses_batch_and_maps_actions() {
        let body = serde_json::json!([
            {"action": "create", "data": {"id": "a"}},
            {"action": "update", "data": {"id": "b"}},
            {"action": "remove", "data": {"id": "c"}}
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_missing_id_errors() {
        let body = serde_json::json!({"action": "update", "data": {"id": ""}});
        let transport = Arc::new(MockHttpTransport::new());
        let c = LinearConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap()),
            Err(ConnectorError::Webhook(_))
        ));
    }
}
