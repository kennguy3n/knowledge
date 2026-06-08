//! Bayt connector — Bayt.com API (largest Middle East job board).
//!
//! * `initial_sync` pages `GET /v1/jobs?per_page=100&page=N`, stopping
//!   on a short page.
//! * `incremental_sync` adds the `updated_since` filter keyed off the
//!   stored RFC-3339 watermark; the filter is inclusive, so the
//!   boundary row is deduped client-side.
//! * `fetch_content` GETs `/v1/jobs/{id}` and renders the posting as
//!   Markdown.
//! * Bayt exposes no API to create webhooks (notifications are
//!   configured in the employer portal), so `subscribe_webhook` records
//!   a polling-only subscription with no provider id.
//! * `handle_webhook_event` parses the portal-delivered payload
//!   (single object or batched array).
//!
//! Bayt authenticates with an API key carried in the `X-Bayt-Api-Key`
//! header (not a bearer `Authorization`), so the connector issues
//! requests through the injected [`HttpTransport`] directly rather than
//! the bearer helpers.

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Default Bayt API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.bayt.com";

/// Page size for job listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "jobs.read";

/// One Bayt job posting (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaytJob {
    /// Job posting id.
    #[serde(default)]
    pub id: String,
    /// Job title.
    #[serde(default)]
    pub title: Option<String>,
    /// Hiring company name.
    #[serde(default)]
    pub company_name: Option<String>,
    /// Job location (city / country).
    #[serde(default)]
    pub location: Option<String>,
    /// Full job description (plain text).
    #[serde(default)]
    pub description: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Bayt job list response (`{ "jobs": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaytJobsResponse {
    /// Page of jobs.
    #[serde(default)]
    pub jobs: Vec<BaytJob>,
}

/// Bayt single-job response (`{ "job": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaytJobResponse {
    /// The job posting.
    #[serde(default)]
    pub job: BaytJob,
}

/// Bayt webhook delivery payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaytWebhookEvent {
    /// Affected job id (string or number).
    #[serde(default)]
    pub job_id: serde_json::Value,
    /// Event label, e.g. `job_posted`, `job_closed`.
    #[serde(default)]
    pub event: String,
}

/// Bayt connector.
pub struct BaytConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for BaytConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaytConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .finish()
    }
}

impl BaytConnector {
    /// Construct a Bayt connector.
    pub fn new(
        instance: ConnectorInstanceId,
        transport: Arc<dyn HttpTransport>,
        _oauth: Arc<dyn OAuth2CodeExchange>,
    ) -> Self {
        Self {
            instance,
            transport,
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Override the Bayt base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size (clamped to at least 1).
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

    /// GET a JSON endpoint with Bayt's `X-Bayt-Api-Key` header.
    fn bayt_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("X-Bayt-Api-Key", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("bayt", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "bayt {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk the job list page-by-page, stopping on a short page.
    fn paginate_jobs(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_since: Option<&str>,
    ) -> Result<Vec<BaytJob>> {
        let mut out = Vec::<BaytJob>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!("{base_url}/v1/jobs?per_page={}&page={page}", self.page_size);
            if let Some(since) = updated_since {
                url.push_str("&updated_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: BaytJobsResponse = self.bayt_get("/v1/jobs", &url, token)?;
            let count = resp.jobs.len();
            out.extend(resp.jobs);
            if count < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "bayt /v1/jobs exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn job_watermark(j: &BaytJob) -> Option<DateTime<Utc>> {
    j.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| j.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for BaytConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let api_key = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "bayt authenticate: auth_config_json.api_key is required".into(),
                )
            })?;
        Ok(OAuth2Token::new_without_refresh(
            api_key,
            Utc::now() + chrono::Duration::days(3650),
            DEFAULT_SCOPE,
        ))
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let jobs = self.paginate_jobs(&base_url, token, None)?;
        let mut events = Vec::with_capacity(jobs.len());
        let mut cursor = WatermarkCursor::empty();
        for j in &jobs {
            let occurred_at = job_watermark(j).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(j.id.clone()),
                occurred_at,
            });
            if let Some(t) = job_watermark(j) {
                cursor.observe(t, &j.id);
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
        let jobs = self.paginate_jobs(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for j in &jobs {
            let Some(updated) = job_watermark(j) else {
                continue;
            };
            if !prior.should_emit(updated, &j.id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(j.id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &j.id);
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
        let url = format!("{base_url}/v1/jobs/{id_enc}");
        let resp: BaytJobResponse = self.bayt_get("/v1/jobs/{id}", &url, token)?;
        let job = resp.job;
        let title = job.title.clone().unwrap_or_else(|| format!("Job {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(company) = job.company_name.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Company:** ");
            md.push_str(company);
            md.push_str("\n\n");
        }
        if let Some(location) = job.location.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Location:** ");
            md.push_str(location);
            md.push_str("\n\n");
        }
        if let Some(description) = job.description.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(description);
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "bayt",
                "job_id": id,
                "company_name": job.company_name,
                "updated_at": job.updated_at,
            }))
            .with_source_url(format!("{base_url}/jobs/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Bayt exposes no API to create webhooks — notifications are
        // configured in the employer portal. Record a polling-only
        // subscription so the runtime falls back to incremental_sync.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("bayt-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<BaytWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<BaytWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<BaytWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty bayt webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.job_id).ok_or_else(|| {
                ConnectorError::Webhook("bayt webhook event missing job_id".into())
            })?;
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(id_str);
            let event = if delivery.event.contains("post") || delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("close") || delivery.event.contains("delete") {
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
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new_without_refresh(
                "unused",
                Utc::now() + chrono::Duration::hours(1),
                "x",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Bayt, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "bayt_123",
                "api_base_url": "https://api.test/bayt",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn small(c: BaytConnector) -> BaytConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_wraps_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BaytConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "bayt_123"
        );
    }

    #[test]
    fn authenticate_requires_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BaytConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Bayt, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates_until_short_page() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/bayt/v1/jobs?per_page=2&page=1".to_string(),
            ok_json(&serde_json::json!({"jobs": [
                {"id": "1", "updated_at": "2024-01-01T00:00:00Z"},
                {"id": "2", "updated_at": "2024-01-02T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/bayt/v1/jobs?per_page=2&page=2".to_string(),
            ok_json(&serde_json::json!({"jobs": [
                {"id": "3", "updated_at": "2024-01-03T00:00:00Z"}
            ]})),
        );
        let c = small(BaytConnector::new(
            ConnectorInstanceId::new_v4(),
            transport.clone(),
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|3")
        );
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/bayt/v1/jobs?per_page=2&page=1&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"jobs": [
                {"id": "10", "updated_at": "2024-03-01T00:00:00Z"},
                {"id": "13", "updated_at": "2024-03-01T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/bayt/v1/jobs?per_page=2&page=2&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"jobs": [ {"id": "11", "updated_at": "2024-06-01T00:00:00Z"} ]})),
        );
        let c = small(BaytConnector::new(
            ConnectorInstanceId::new_v4(),
            transport,
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must
        // NOT re-emit `10`, still surface the brand-new `13` at the same
        // second, and advance past the later row.
        state.cursor = Some(format!("{since}|10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["13", "11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|11")
        );
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/bayt/v1/jobs/55".to_string(),
            ok_json(&serde_json::json!({"job": {
                "id": "55",
                "title": "Senior Engineer",
                "company_name": "Emirates Group",
                "location": "Dubai, UAE",
                "description": "Build the future."
            }})),
        );
        let c = BaytConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Senior Engineer"));
        assert!(body.contains("**Company:** Emirates Group"));
        assert!(body.contains("**Location:** Dubai, UAE"));
        assert!(body.contains("Build the future."));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BaytConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/bayt")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = BaytConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(br#"{"job_id": "7", "event": "job_posted"}"#)
            .unwrap();
        assert!(matches!(single[0], ConnectorEvent::DocumentCreated { .. }));
        let batch = c
            .handle_webhook_event(
                br#"[{"job_id": 8, "event": "job_updated"}, {"job_id": "9", "event": "job_closed"}]"#,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
