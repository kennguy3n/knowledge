//! ServiceNow connector — ServiceNow Table API
//! (`/api/now/table/{table}`).
//!
//! * `initial_sync` pages the `incident` table ordered by
//!   `sys_created_on`, walking `sysparm_offset` until a short page
//!   signals the end.
//! * `incremental_sync` filters `sys_updated_on > <cursor>` (a strict
//!   comparison, so no boundary dedup is needed) and orders by
//!   `sys_updated_on`.
//! * `fetch_content` GETs the single record
//!   (`/api/now/table/{table}/{sys_id}`) and reconstructs Markdown
//!   from `short_description` + `description`.
//! * `subscribe_webhook` is polling-only: ServiceNow has no public
//!   "create webhook" REST endpoint (outbound notifications are
//!   configured via Business Rules / REST Messages in the instance),
//!   so the connector records a polling-backstop subscription with no
//!   provider-side id.
//! * `handle_webhook_event` parses an outbound Business-Rule payload;
//!   a single POST may batch several record changes, all of which are
//!   emitted.

use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default ServiceNow base URL. Per-instance overrides go through
/// `auth_config_json.api_base_url` (instances are per-tenant:
/// `https://your-instance.service-now.com`).
pub const DEFAULT_API_BASE_URL: &str = "https://your-instance.service-now.com";

/// The table the connector syncs. Incidents are the canonical
/// support record; the substrate ingests their short description +
/// description as evidence.
pub const TABLE: &str = "incident";

/// Page size for Table API pagination (`sysparm_limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 10_000;

/// One ServiceNow table record (subset of fields used by the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceNowRecord {
    /// Stable 32-char `sys_id`.
    #[serde(default)]
    pub sys_id: String,
    /// One-line summary.
    #[serde(default)]
    pub short_description: Option<String>,
    /// Full incident description.
    #[serde(default)]
    pub description: Option<String>,
    /// Created timestamp (ServiceNow `YYYY-MM-DD HH:MM:SS`, UTC).
    #[serde(default)]
    pub sys_created_on: Option<String>,
    /// Updated timestamp (ServiceNow `YYYY-MM-DD HH:MM:SS`, UTC).
    #[serde(default)]
    pub sys_updated_on: Option<String>,
}

/// One page of a Table API list response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceNowListResponse {
    /// The records on this page.
    #[serde(default)]
    pub result: Vec<ServiceNowRecord>,
}

/// Single-record Table API response (`GET …/{sys_id}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceNowSingleResponse {
    /// The record body as free-form JSON (ServiceNow returns every
    /// column; we pull the handful we cite from).
    #[serde(default)]
    pub result: serde_json::Value,
}

/// Outbound Business-Rule webhook payload — a batch of record changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceNowWebhookPayload {
    /// Changed records.
    #[serde(default)]
    pub records: Vec<ServiceNowWebhookRecord>,
}

/// One record-change entry inside a webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceNowWebhookRecord {
    /// Affected record's `sys_id`.
    #[serde(default)]
    pub sys_id: String,
    /// `inserted`, `updated`, or `deleted`.
    #[serde(default)]
    pub operation: String,
    /// Updated timestamp at the time of the change.
    #[serde(default)]
    pub sys_updated_on: Option<String>,
}

/// ServiceNow connector.
pub struct ServiceNowConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ServiceNowConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceNowConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ServiceNowConnector {
    /// Construct a ServiceNow connector.
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

    /// Override the ServiceNow base URL (the instance URL).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the Table API page size.
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

    /// Walk the Table API by `sysparm_offset` until a page returns
    /// fewer than `page_size` rows (the end) or [`MAX_PAGES`] is hit.
    fn paginate_table(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        encoded_query: &str,
    ) -> Result<Vec<ServiceNowRecord>> {
        let mut records = Vec::<ServiceNowRecord>::new();
        let mut offset: u32 = 0;
        for _ in 0..MAX_PAGES {
            let url = format!(
                "{base_url}/api/now/table/{TABLE}?sysparm_limit={}&sysparm_offset={offset}&sysparm_query={encoded_query}",
                self.page_size,
            );
            let resp: ServiceNowListResponse = bearer_get_json(
                &self.transport,
                "servicenow",
                "/api/now/table/{table}",
                &url,
                token,
                &[],
            )?;
            let returned = u32::try_from(resp.result.len()).unwrap_or(u32::MAX);
            records.extend(resp.result);
            if returned < self.page_size {
                return Ok(records);
            }
            offset = offset.saturating_add(self.page_size);
        }
        Err(ConnectorError::Sync(format!(
            "servicenow /table/{TABLE} exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Parse a ServiceNow datetime into UTC. ServiceNow stores
/// `sys_*_on` columns in UTC as `2024-01-02 03:04:05` (space
/// separator, no zone); we try RFC-3339 first (test fixtures /
/// alternate exports) then the naive form interpreted as UTC.
fn parse_sn_datetime(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Render a UTC instant as a ServiceNow query datetime literal
/// (`2024-01-02 03:04:05`, UTC).
fn sn_datetime_literal(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn record_watermark(record: &ServiceNowRecord) -> Option<DateTime<Utc>> {
    record
        .sys_updated_on
        .as_deref()
        .and_then(parse_sn_datetime)
        .or_else(|| record.sys_created_on.as_deref().and_then(parse_sn_datetime))
}

fn operation_kind(op: &str) -> &'static str {
    match op {
        "inserted" | "created" => "create",
        "deleted" => "delete",
        _ => "update",
    }
}

impl Connector for ServiceNowConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "servicenow authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let query = percent_encode_path_component("ORDERBYsys_created_on");
        let records = self.paginate_table(&base_url, token, &query)?;
        let mut events = Vec::with_capacity(records.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for record in &records {
            let occurred_at = record_watermark(record).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(record.sys_id.clone()),
                occurred_at,
            });
            if let Some(t) = record_watermark(record) {
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
        let prior: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let raw_query = match prior {
            Some(t) => format!(
                "sys_updated_on>{}^ORDERBYsys_updated_on",
                sn_datetime_literal(t)
            ),
            None => "ORDERBYsys_updated_on".to_string(),
        };
        let query = percent_encode_path_component(&raw_query);
        let records = self.paginate_table(&base_url, token, &query)?;
        let mut events = Vec::with_capacity(records.len());
        let mut watermark = prior;
        for record in &records {
            let occurred_at = record_watermark(record).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(record.sys_id.clone()),
                occurred_at,
            });
            if let Some(t) = record_watermark(record) {
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
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base_url}/api/now/table/{TABLE}/{id_enc}");
        let resp: ServiceNowSingleResponse = bearer_get_json(
            &self.transport,
            "servicenow",
            "/api/now/table/{table}/{sys_id}",
            &url,
            token,
            &[],
        )?;
        let record = resp.result;
        let short = record
            .get("short_description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let description = record
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let number = record
            .get("number")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        let mut md = String::new();
        if !short.is_empty() {
            md.push_str("# ");
            md.push_str(&short);
            md.push_str("\n\n");
        }
        if !description.is_empty() {
            md.push_str(&description);
        }
        let body = md.trim_end().to_string();
        let source_url = format!("{base_url}/nav_to.do?uri={TABLE}.do?sys_id={id}");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(short)
            .with_metadata(serde_json::json!({
                "provider": "servicenow",
                "table": TABLE,
                "sys_id": id,
                "number": number,
            }))
            .with_source_url(source_url))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // ServiceNow does not expose a public REST endpoint to create
        // outbound notifications — operators wire a Business Rule /
        // REST Message that POSTs to `callback_url`. We therefore
        // model `subscribe_webhook` as metadata-only: record the
        // callback URL + signing secret so `handle_webhook_event` can
        // process pushes, and leave `provider_subscription_id` unset
        // (polling stays the backstop, like the Slack / Notion
        // polling-only connectors).
        let subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("servicenow-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Accept the documented `{ "records": [...] }` envelope, a
        // bare array, or a single record object — Business Rules can
        // be authored to emit any of the three.
        let records: Vec<ServiceNowWebhookRecord> = if let Ok(payload) =
            serde_json::from_slice::<ServiceNowWebhookPayload>(body)
        {
            if payload.records.is_empty() {
                if let Ok(batch) = serde_json::from_slice::<Vec<ServiceNowWebhookRecord>>(body) {
                    batch
                } else {
                    vec![serde_json::from_slice::<ServiceNowWebhookRecord>(body)?]
                }
            } else {
                payload.records
            }
        } else if let Ok(batch) = serde_json::from_slice::<Vec<ServiceNowWebhookRecord>>(body) {
            batch
        } else {
            vec![serde_json::from_slice::<ServiceNowWebhookRecord>(body)?]
        };
        if records.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty servicenow webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(records.len());
        for record in records {
            if record.sys_id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "servicenow webhook record missing sys_id".into(),
                ));
            }
            let occurred_at = record
                .sys_updated_on
                .as_deref()
                .and_then(parse_sn_datetime)
                .unwrap_or_else(Utc::now);
            let id = SourceDocumentId::new(record.sys_id);
            let event = match operation_kind(&record.operation) {
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
                "sn-access",
                "sn-refresh",
                Utc::now() + Duration::hours(1),
                "useraccount",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::ServiceNow,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/sn",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn list_url(offset: u32, query: &str) -> String {
        format!(
            "https://api.test/sn/api/now/table/{TABLE}?sysparm_limit={DEFAULT_PAGE_SIZE}&sysparm_offset={offset}&sysparm_query={}",
            percent_encode_path_component(query)
        )
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::ServiceNow,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "sn-access");
    }

    #[test]
    fn initial_sync_emits_created_and_watermark() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            list_url(0, "ORDERBYsys_created_on"),
            ok_json(&serde_json::json!({
                "result": [{
                    "sys_id": "abc123",
                    "short_description": "printer down",
                    "sys_created_on": "2024-01-01 00:00:00",
                    "sys_updated_on": "2024-01-02 00:00:00",
                }]
            })),
        );
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-02T00:00:00+00:00")
        );
    }

    #[test]
    fn initial_sync_paginates_until_short_page() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let q = percent_encode_path_component("ORDERBYsys_created_on");
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sn/api/now/table/{TABLE}?sysparm_limit=2&sysparm_offset=0&sysparm_query={q}"),
            ok_json(&serde_json::json!({
                "result": [
                    {"sys_id": "a", "sys_created_on": "2024-01-01 00:00:00"},
                    {"sys_id": "b", "sys_created_on": "2024-01-01 00:01:00"}
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sn/api/now/table/{TABLE}?sysparm_limit=2&sysparm_offset=2&sysparm_query={q}"),
            ok_json(&serde_json::json!({
                "result": [{"sys_id": "c", "sys_created_on": "2024-01-01 00:02:00"}]
            })),
        );
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_on_sys_updated_on() {
        let transport = Arc::new(MockHttpTransport::new());
        let cursor_t = Utc::now() - Duration::hours(1);
        let raw_query = format!(
            "sys_updated_on>{}^ORDERBYsys_updated_on",
            sn_datetime_literal(cursor_t)
        );
        transport.expect(
            HttpMethod::Get,
            list_url(0, &raw_query),
            ok_json(&serde_json::json!({
                "result": [{
                    "sys_id": "z9",
                    "sys_updated_on": "2024-06-01 12:00:00",
                }]
            })),
        );
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor_t.to_rfc3339());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn initial_sync_maps_403_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            list_url(0, "ORDERBYsys_created_on"),
            MockResponse::status(403, b"forbidden".to_vec()),
        );
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sn/api/now/table/{TABLE}/abc123"),
            ok_json(&serde_json::json!({
                "result": {
                    "sys_id": "abc123",
                    "number": "INC0010",
                    "short_description": "VPN broken",
                    "description": "Remote staff cannot connect.",
                }
            })),
        );
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("abc123"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# VPN broken"));
        assert!(body.contains("Remote staff cannot connect."));
        assert_eq!(fc.title.as_deref(), Some("VPN broken"));
        assert_eq!(fc.metadata["number"], serde_json::json!("INC0010"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sn/api/now/table/{TABLE}/nope"),
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("nope"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/sn")
            .unwrap();
        assert_eq!(sub.connector, c.instance);
        assert!(sub.provider_subscription_id.is_none());
        // No HTTP call is made for a polling-only subscription.
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_parses_records_envelope() {
        let body = serde_json::json!({
            "records": [
                {"sys_id": "r1", "operation": "inserted", "sys_updated_on": "2024-01-01 00:00:00"},
                {"sys_id": "r2", "operation": "updated", "sys_updated_on": "2024-01-02 00:00:00"},
                {"sys_id": "r3", "operation": "deleted", "sys_updated_on": "2024-01-03 00:00:00"}
            ]
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_parses_single_record() {
        let body = serde_json::json!({
            "sys_id": "r9", "operation": "updated", "sys_updated_on": "2024-01-01 00:00:00"
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_missing_sys_id_errors() {
        let body = serde_json::json!({"records": [{"sys_id": "", "operation": "updated"}]});
        let transport = Arc::new(MockHttpTransport::new());
        let c = ServiceNowConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn parse_sn_datetime_assumes_utc() {
        let dt = parse_sn_datetime("2024-01-02 03:04:05").unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-01-02T03:04:05+00:00");
    }
}
