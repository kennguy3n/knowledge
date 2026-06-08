//! Salesforce connector — Salesforce REST API v59 + Streaming API
//! `PushTopic` push notifications.
//!
//! * `initial_sync` runs a SOQL query
//!   (`SELECT … FROM Case ORDER BY CreatedDate ASC`) against
//!   `/services/data/v59.0/query` and walks pages via the
//!   `nextRecordsUrl` cursor Salesforce returns until `done` is true.
//! * `incremental_sync` runs the same query with a
//!   `WHERE LastModifiedDate > <cursor>` filter keyed off the prior
//!   watermark. Salesforce's `>` is a strict comparison, so unlike
//!   Jira there is no boundary row to dedup client-side.
//! * `fetch_content` GETs the single sObject
//!   (`/services/data/v59.0/sobjects/Case/{id}`) and reconstructs a
//!   Markdown body from `Subject` + `Description` + `Status`.
//! * `subscribe_webhook` POSTs a `PushTopic` sObject; the substrate
//!   persists Salesforce's returned id for later revocation.
//! * `handle_webhook_event` parses the Streaming API (CometD) push
//!   envelope — a single delivery may batch several messages, so
//!   every message in the payload is emitted.
//!
//! Production wiring runs over [`HttpTransport`]; unit tests pass
//! [`connector_framework::MockHttpTransport`] + a fixture OAuth2
//! exchange.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Salesforce REST base URL. Per-instance overrides go
/// through `auth_config_json.api_base_url` (Salesforce orgs are
/// per-tenant: `https://your-instance.my.salesforce.com`).
pub const DEFAULT_API_BASE_URL: &str = "https://your-instance.my.salesforce.com";

/// Salesforce REST API version path segment used by every endpoint.
pub const API_VERSION: &str = "v59.0";

/// The sObject the connector syncs. Cases are the canonical
/// support-record object; the substrate ingests their subject +
/// description + comments as evidence.
pub const SOBJECT: &str = "Case";

/// Safety ceiling on number of `query` / `queryMore` pages a single
/// sync will walk — catches a server that never sets `done`.
pub const MAX_QUERY_PAGES: usize = 10_000;

/// One Salesforce record (subset of fields used by the substrate).
///
/// Salesforce field names are PascalCase on the wire; we rename them
/// to idiomatic snake_case Rust fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SalesforceRecord {
    /// Record id (Salesforce's stable 18-char id).
    #[serde(default, rename = "Id")]
    pub id: String,
    /// Case subject line.
    #[serde(default, rename = "Subject")]
    pub subject: Option<String>,
    /// Created timestamp (raw Salesforce datetime string).
    #[serde(default, rename = "CreatedDate")]
    pub created_date: Option<String>,
    /// Last-modified timestamp (raw Salesforce datetime string).
    #[serde(default, rename = "LastModifiedDate")]
    pub last_modified_date: Option<String>,
}

/// One page of a SOQL `/query` (or `/queryMore`) response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SalesforceQueryResponse {
    /// Total records matching the SOQL — informational only; the
    /// `done` / `nextRecordsUrl` pair drives pagination.
    #[serde(default, rename = "totalSize")]
    pub total_size: i64,
    /// `true` when this is the final page.
    #[serde(default)]
    pub done: bool,
    /// Relative path to the next page (`/services/data/vXX.X/query/<locator>`).
    /// Absent on the final page.
    #[serde(default, rename = "nextRecordsUrl")]
    pub next_records_url: Option<String>,
    /// Records on this page.
    #[serde(default)]
    pub records: Vec<SalesforceRecord>,
}

/// Response from `POST /sobjects/PushTopic` — Salesforce returns the
/// created record's id plus a success flag.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SalesforceCreateResponse {
    /// Id of the created sObject (the `PushTopic` id).
    #[serde(default)]
    pub id: Option<String>,
    /// Whether the create succeeded.
    #[serde(default)]
    pub success: bool,
    /// Validation / DML errors Salesforce flagged.
    #[serde(default)]
    pub errors: Vec<serde_json::Value>,
}

/// Streaming API (CometD) push envelope for one `PushTopic` event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SalesforceStreamingMessage {
    /// The `data` block carries the `event` metadata + the `sobject`
    /// snapshot.
    #[serde(default)]
    pub data: Option<SalesforceStreamingData>,
}

/// `data` block of a Streaming API message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SalesforceStreamingData {
    /// Event metadata (`type`, `createdDate`, replay id).
    #[serde(default)]
    pub event: Option<SalesforceStreamingEvent>,
    /// The changed record snapshot — at minimum carries `Id`.
    #[serde(default)]
    pub sobject: Option<SalesforceRecord>,
}

/// Event metadata sub-block of a Streaming API message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SalesforceStreamingEvent {
    /// `created`, `updated`, `deleted`, or `undeleted`.
    #[serde(default, rename = "type")]
    pub event_type: String,
    /// Wall-clock time Salesforce emitted the event.
    #[serde(default, rename = "createdDate")]
    pub created_date: Option<String>,
}

/// Salesforce connector.
pub struct SalesforceConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
}

impl std::fmt::Debug for SalesforceConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SalesforceConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl SalesforceConnector {
    /// Construct a Salesforce connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against the org's
    /// `/services/oauth2/token` endpoint. Production wires
    /// `BlockingHttpTransport` + `OAuth2Client`; tests use
    /// `MockHttpTransport`.
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
        }
    }

    /// Override the Salesforce REST base URL (the org instance URL).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
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

    /// Walk every SOQL page, following Salesforce's `nextRecordsUrl`
    /// locator until `done` is set or [`MAX_QUERY_PAGES`] is hit.
    fn paginate_query(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        soql: &str,
    ) -> Result<Vec<SalesforceRecord>> {
        let mut records = Vec::<SalesforceRecord>::new();
        let first = format!(
            "{base_url}/services/data/{API_VERSION}/query?q={}",
            percent_encode_path_component(soql),
        );
        let mut next_url = Some(first);
        for _ in 0..MAX_QUERY_PAGES {
            let Some(url) = next_url.take() else {
                return Ok(records);
            };
            let resp: SalesforceQueryResponse = bearer_get_json(
                &self.transport,
                "salesforce",
                "/services/data/{version}/query",
                &url,
                token,
                &[],
            )?;
            records.extend(resp.records);
            if resp.done {
                return Ok(records);
            }
            // Salesforce hands back a relative locator path; resolve
            // it against the org base URL for the follow-up GET.
            match resp.next_records_url {
                Some(path) => next_url = Some(format!("{base_url}{path}")),
                None => return Ok(records),
            }
        }
        Err(ConnectorError::Sync(format!(
            "salesforce /query exceeded {MAX_QUERY_PAGES} pages without `done`"
        )))
    }
}

/// Parse a Salesforce datetime string into UTC. Salesforce emits the
/// `2024-01-02T03:04:05.000+0000` form (numeric offset, no colon),
/// which is not strict RFC-3339; we try RFC-3339 first (covers the
/// `Z` / `+00:00` forms test fixtures and other providers use) then
/// fall back to the `%z` numeric-offset parse.
fn parse_sf_datetime(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .or_else(|| DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z").ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Render a UTC instant as a Salesforce SOQL datetime literal
/// (`2024-01-02T03:04:05Z`). SOQL datetime literals are bare (no
/// surrounding quotes) and accept the `Z` zone designator.
fn soql_datetime_literal(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Best-effort modification time for a record: last-modified, falling
/// back to created.
fn record_time(record: &SalesforceRecord) -> Option<DateTime<Utc>> {
    record
        .last_modified_date
        .as_deref()
        .and_then(parse_sf_datetime)
        .or_else(|| record.created_date.as_deref().and_then(parse_sf_datetime))
}

fn record_event(record: &SalesforceRecord, kind: &str) -> ConnectorEvent {
    let occurred_at = record_time(record).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(record.id.clone());
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

/// Map a Streaming API event `type` to the internal create/update/
/// delete tag `record_event` understands.
fn streaming_kind(event_type: &str) -> &'static str {
    match event_type {
        "created" => "create",
        "deleted" => "delete",
        _ => "update",
    }
}

impl Connector for SalesforceConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "salesforce authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let soql = format!(
            "SELECT Id, Subject, CreatedDate, LastModifiedDate FROM {SOBJECT} ORDER BY CreatedDate ASC"
        );
        let records = self.paginate_query(&base_url, token, &soql)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(records.len());
        let mut cursor = WatermarkCursor::empty();
        for record in &records {
            events.push(record_event(record, "create"));
            if let Some(t) = record_time(record) {
                cursor.observe(t, &record.id);
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
        // Inclusive `>=` so a record sharing the watermark second is
        // re-returned by the server; `WatermarkCursor` then dedups the
        // ids already emitted while surfacing brand-new boundary records.
        let soql = match prior.watermark() {
            Some(t) => format!(
                "SELECT Id, Subject, CreatedDate, LastModifiedDate FROM {SOBJECT} \
                 WHERE LastModifiedDate >= {} ORDER BY LastModifiedDate ASC",
                soql_datetime_literal(t)
            ),
            None => format!(
                "SELECT Id, Subject, CreatedDate, LastModifiedDate FROM {SOBJECT} \
                 ORDER BY LastModifiedDate ASC"
            ),
        };
        let records = self.paginate_query(&base_url, token, &soql)?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(records.len());
        let mut cursor = prior.clone();
        for record in &records {
            let Some(t) = record_time(record) else {
                continue;
            };
            if !prior.should_emit(t, &record.id) {
                continue;
            }
            events.push(record_event(record, "update"));
            cursor.observe(t, &record.id);
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
        let url = format!("{base_url}/services/data/{API_VERSION}/sobjects/{SOBJECT}/{id_enc}");
        let record: serde_json::Value = bearer_get_json(
            &self.transport,
            "salesforce",
            "/services/data/{version}/sobjects/{type}/{id}",
            &url,
            token,
            &[],
        )?;
        let subject = record
            .get("Subject")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let status = record
            .get("Status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let description = record
            .get("Description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        let mut md = String::new();
        if !subject.is_empty() {
            md.push_str("# ");
            md.push_str(&subject);
            md.push_str("\n\n");
        }
        if !description.is_empty() {
            md.push_str(&description);
        }
        let body = md.trim_end().to_string();

        let source_url = format!("{base_url}/{id}");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(subject)
            .with_metadata(serde_json::json!({
                "provider": "salesforce",
                "sobject": SOBJECT,
                "record_id": id,
                "status": status,
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
        let url = format!("{base_url}/services/data/{API_VERSION}/sobjects/PushTopic");
        // A PushTopic is the Streaming API's server-side subscription
        // object. The substrate's CometD client (out-of-band) then
        // subscribes to `/topic/<Name>` to receive pushes; the
        // `callback_url` is recorded on the substrate side for the
        // dispatcher that forwards CometD messages to our HTTP
        // ingress.
        let body = serde_json::json!({
            "Name": format!("knowledge_{SOBJECT}"),
            "Query": format!("SELECT Id, Subject, LastModifiedDate FROM {SOBJECT}"),
            "ApiVersion": 59.0,
            "NotifyForOperationCreate": true,
            "NotifyForOperationUpdate": true,
            "NotifyForOperationDelete": true,
            "NotifyForFields": "Referenced",
        });
        let resp: SalesforceCreateResponse = bearer_post_json(
            &self.transport,
            "salesforce",
            "/services/data/{version}/sobjects/PushTopic",
            &url,
            token,
            &[],
            &body,
        )?;
        if !resp.success || resp.id.is_none() {
            return Err(ConnectorError::Webhook(format!(
                "salesforce PushTopic create failed: success={}, errors={:?}",
                resp.success, resp.errors
            )));
        }
        let topic_id = resp.id.unwrap_or_default();
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("salesforce-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            // PushTopics do not auto-expire; leave the expiry open.
            None,
        );
        subscription.provider_subscription_id = Some(topic_id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // The Streaming API delivers an array of CometD messages per
        // long-poll response; tolerate a single bare object too.
        let messages: Vec<SalesforceStreamingMessage> =
            if let Ok(batch) = serde_json::from_slice::<Vec<SalesforceStreamingMessage>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<SalesforceStreamingMessage>(body)?]
            };
        if messages.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty salesforce streaming batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(messages.len());
        for msg in messages {
            let Some(data) = msg.data else { continue };
            let event_type = data
                .event
                .as_ref()
                .map(|e| e.event_type.clone())
                .unwrap_or_default();
            let Some(mut record) = data.sobject else {
                continue;
            };
            // Streaming events carry the event `createdDate` rather
            // than the record's `LastModifiedDate`; fold it in so the
            // emitted `occurred_at` reflects the event time when the
            // record snapshot omits its own timestamp.
            if record.last_modified_date.is_none() {
                record.last_modified_date =
                    data.event.as_ref().and_then(|e| e.created_date.clone());
            }
            if record.id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "salesforce streaming message missing sobject Id".into(),
                ));
            }
            events.push(record_event(&record, streaming_kind(&event_type)));
        }
        if events.is_empty() {
            return Err(ConnectorError::Webhook(
                "salesforce streaming batch carried no usable messages".into(),
            ));
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
                "sf-access",
                "sf-refresh",
                Utc::now() + Duration::hours(1),
                "api refresh_token",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Salesforce,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/sf",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn rec(id: &str, created: DateTime<Utc>, modified: DateTime<Utc>) -> serde_json::Value {
        serde_json::json!({
            "Id": id,
            "Subject": "test case",
            "CreatedDate": created.to_rfc3339(),
            "LastModifiedDate": modified.to_rfc3339(),
        })
    }

    fn initial_soql_url() -> String {
        let soql = format!(
            "SELECT Id, Subject, CreatedDate, LastModifiedDate FROM {SOBJECT} ORDER BY CreatedDate ASC"
        );
        format!(
            "https://api.test/sf/services/data/{API_VERSION}/query?q={}",
            percent_encode_path_component(&soql)
        )
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "sf-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::Salesforce,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_events_and_watermark_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            initial_soql_url(),
            ok_json(&serde_json::json!({
                "totalSize": 1, "done": true,
                "records": [rec("500A0", now, now)],
            })),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
    fn initial_sync_paginates_via_next_records_url() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            initial_soql_url(),
            ok_json(&serde_json::json!({
                "totalSize": 2, "done": false,
                "nextRecordsUrl": format!("/services/data/{API_VERSION}/query/01g-2000"),
                "records": [rec("500A0", now, now)],
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sf/services/data/{API_VERSION}/query/01g-2000"),
            ok_json(&serde_json::json!({
                "totalSize": 2, "done": true,
                "records": [rec("500A1", now, now)],
            })),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_on_last_modified_date_and_dedupes_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let boundary = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let newer = DateTime::parse_from_rfc3339("2024-02-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // Inclusive `>=` so the boundary row is re-returned by the server.
        let soql = format!(
            "SELECT Id, Subject, CreatedDate, LastModifiedDate FROM {SOBJECT} \
             WHERE LastModifiedDate >= {} ORDER BY LastModifiedDate ASC",
            soql_datetime_literal(boundary)
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/sf/services/data/{API_VERSION}/query?q={}",
                percent_encode_path_component(&soql)
            ),
            ok_json(&serde_json::json!({
                "totalSize": 3, "done": true,
                "records": [
                    rec("seen", boundary, boundary),
                    rec("boundary_new", boundary, boundary),
                    rec("newer", newer, newer),
                ],
            })),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior cursor: watermark at the boundary with id "seen" already emitted.
        state.cursor = Some("2024-01-01T00:00:00+00:00|seen".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // "seen" deduped; the brand-new same-second "boundary_new" surfaces,
        // as does the strictly-newer "newer".
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["boundary_new", "newer"]);
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            initial_soql_url(),
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn subscribe_webhook_creates_push_topic_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            format!("https://api.test/sf/services/data/{API_VERSION}/sobjects/PushTopic"),
            ok_json(&serde_json::json!({ "id": "0IF000", "success": true, "errors": [] })),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/sf")
            .unwrap();
        assert_eq!(sub.connector, c.instance);
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("0IF000"));
    }

    #[test]
    fn subscribe_webhook_propagates_create_failure() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            format!("https://api.test/sf/services/data/{API_VERSION}/sobjects/PushTopic"),
            ok_json(&serde_json::json!({
                "success": false,
                "errors": [{ "message": "duplicate name" }],
            })),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/sf")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_parses_batched_streaming_messages() {
        let now = Utc::now();
        let body = serde_json::json!([
            {
                "data": {
                    "event": { "type": "created", "createdDate": now.to_rfc3339() },
                    "sobject": { "Id": "500B0", "LastModifiedDate": now.to_rfc3339() }
                }
            },
            {
                "data": {
                    "event": { "type": "updated", "createdDate": now.to_rfc3339() },
                    "sobject": { "Id": "500B1", "LastModifiedDate": now.to_rfc3339() }
                }
            }
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_parses_single_object_form() {
        let now = Utc::now();
        let body = serde_json::json!({
            "data": {
                "event": { "type": "deleted", "createdDate": now.to_rfc3339() },
                "sobject": { "Id": "500B2" }
            }
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_missing_sobject_id_errors() {
        let body = serde_json::json!({
            "data": { "event": { "type": "updated" }, "sobject": { "Id": "" } }
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn fetch_content_assembles_subject_and_description() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sf/services/data/{API_VERSION}/sobjects/{SOBJECT}/500C0"),
            ok_json(&serde_json::json!({
                "Id": "500C0",
                "Subject": "Login broken",
                "Status": "New",
                "Description": "Users cannot sign in.",
            })),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("500C0"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Login broken"));
        assert!(body.contains("Users cannot sign in."));
        assert_eq!(fc.mime_type, "text/markdown");
        assert_eq!(fc.title.as_deref(), Some("Login broken"));
        assert_eq!(fc.metadata["status"], serde_json::json!("New"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sf/services/data/{API_VERSION}/sobjects/{SOBJECT}/NOPE"),
            MockResponse::status(404, br#"{"error":"not found"}"#.to_vec()),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("NOPE"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_429_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/sf/services/data/{API_VERSION}/sobjects/{SOBJECT}/500C9"),
            MockResponse::status(429, b"rate limited".to_vec()),
        );
        let c = SalesforceConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("500C9"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn parse_sf_datetime_handles_numeric_offset() {
        let dt = parse_sf_datetime("2024-01-02T03:04:05.000+0000").unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-01-02T03:04:05+00:00");
    }
}
