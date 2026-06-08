//! Airtable connector — Airtable REST API + webhooks.
//!
//! * `initial_sync` walks `GET /v0/{baseId}/{table}` and pages via
//!   Airtable's opaque `offset` token.
//! * `incremental_sync` adds a `filterByFormula` on `CREATED_TIME()`
//!   keyed off the prior watermark.
//! * `fetch_content` reads `GET /v0/{baseId}/{table}/{recordId}` and
//!   renders the record's fields as Markdown.
//! * `subscribe_webhook` POSTs `/v0/bases/{baseId}/webhooks`.
//! * `handle_webhook_event` parses Airtable's payload-list shape
//!   (`{payloads:[{changedTablesById:{…}}]}`) and emits one event per
//!   created / changed / destroyed record across **every** payload.
//!
//! Airtable authenticates with a personal access token (PAT) used as
//! a bearer token, so the bearer helpers apply directly.
//! `authenticate` accepts a configured `personal_access_token` (the
//! common case) or an OAuth2 `authorization_code`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Airtable REST base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.airtable.com";

/// Page size for list endpoints. Airtable's documented max is 100.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured PAT.
const DEFAULT_SCOPE: &str = "data.records:read";

/// One Airtable record (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirtableRecord {
    /// Record id (e.g. `rec123`).
    pub id: String,
    /// Server-assigned creation time.
    #[serde(rename = "createdTime", default)]
    pub created_time: Option<DateTime<Utc>>,
    /// Arbitrary record fields.
    #[serde(default)]
    pub fields: serde_json::Value,
}

/// One page of `GET /v0/{baseId}/{table}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirtableListResponse {
    /// Records on this page.
    #[serde(default)]
    pub records: Vec<AirtableRecord>,
    /// Opaque pagination token for the next page, when present.
    #[serde(default)]
    pub offset: Option<String>,
}

/// Response from `POST /v0/bases/{baseId}/webhooks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirtableWebhookResponse {
    /// Webhook id.
    #[serde(default)]
    pub id: String,
    /// Base64 MAC secret returned on creation.
    #[serde(rename = "macSecretBase64", default)]
    pub mac_secret_base64: Option<String>,
}

/// Airtable webhook payload list (the body of
/// `GET /v0/bases/{baseId}/webhooks/{webhookId}/payloads`, also
/// delivered in batched notifications).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirtablePayloadList {
    /// Ordered batch of change payloads.
    #[serde(default)]
    pub payloads: Vec<AirtablePayload>,
}

/// One Airtable change payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirtablePayload {
    /// Timestamp of the change.
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    /// Per-table change sets keyed by table id.
    #[serde(rename = "changedTablesById", default)]
    pub changed_tables_by_id: BTreeMap<String, AirtableTableChange>,
}

/// Change set for one table within a payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirtableTableChange {
    /// Newly-created records keyed by record id.
    #[serde(rename = "createdRecordsById", default)]
    pub created_records_by_id: BTreeMap<String, serde_json::Value>,
    /// Changed records keyed by record id.
    #[serde(rename = "changedRecordsById", default)]
    pub changed_records_by_id: BTreeMap<String, serde_json::Value>,
    /// Ids of destroyed records.
    #[serde(rename = "destroyedRecordIds", default)]
    pub destroyed_record_ids: Vec<String>,
}

/// Airtable connector.
pub struct AirtableConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for AirtableConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AirtableConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl AirtableConnector {
    /// Construct an Airtable connector.
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

    /// Override the Airtable REST base URL.
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

    fn base_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("base_id")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("airtable: auth_config_json.base_id is required".into())
            })
    }

    fn table(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("table")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("airtable: auth_config_json.table is required".into())
            })
    }

    /// Walk every record page until no `offset` token is returned.
    fn paginate_records(
        &self,
        base_url: &str,
        base_id: &str,
        table: &str,
        token: &OAuth2Token,
        filter_formula: Option<&str>,
    ) -> Result<Vec<AirtableRecord>> {
        let base_enc = percent_encode_path_component(base_id);
        let table_enc = percent_encode_path_component(table);
        let mut out = Vec::<AirtableRecord>::new();
        let mut offset: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut url = format!(
                "{base_url}/v0/{base_enc}/{table_enc}?pageSize={}",
                self.page_size
            );
            if let Some(f) = filter_formula {
                let _ = write!(url, "&filterByFormula={}", percent_encode_path_component(f));
            }
            if let Some(ref o) = offset {
                let _ = write!(url, "&offset={}", percent_encode_path_component(o));
            }
            let resp: AirtableListResponse = bearer_get_json(
                &self.transport,
                "airtable",
                "/v0/{base}/{table}",
                &url,
                token,
                &[],
            )?;
            out.extend(resp.records);
            match resp.offset {
                Some(next) if !next.is_empty() => offset = Some(next),
                _ => return Ok(out),
            }
        }
        Err(ConnectorError::Sync(format!(
            "airtable list exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn record_to_event(r: &AirtableRecord, kind: &str) -> ConnectorEvent {
    let occurred_at = r.created_time.unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(r.id.clone());
    match kind {
        "update" => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for AirtableConnector {
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
                    "airtable authenticate: auth_config_json.personal_access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let base_id = Self::base_id(config)?;
        let table = Self::table(config)?;
        let records = self.paginate_records(&base_url, &base_id, &table, token, None)?;
        let mut events = Vec::with_capacity(records.len());
        let mut cursor = WatermarkCursor::empty();
        for r in &records {
            events.push(record_to_event(r, "create"));
            if let Some(t) = r.created_time {
                cursor.observe(t, &r.id);
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
        let base_id = Self::base_id(config)?;
        let table = Self::table(config)?;
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        // Airtable has no server-side "updated since" filter for the
        // built-in `LAST_MODIFIED_TIME` (it needs a configured field),
        // so we filter on `CREATED_TIME()` which every base exposes.
        // Airtable has no `IS_ON_OR_AFTER`; `NOT(IS_BEFORE(...))` is the
        // inclusive (`>=`) equivalent, so a record sharing the watermark
        // second is re-returned and `WatermarkCursor` dedups by id while
        // surfacing brand-new boundary records.
        let formula = prior
            .query_since()
            .map(|s| format!("NOT(IS_BEFORE(CREATED_TIME(), '{s}'))"));
        let records =
            self.paginate_records(&base_url, &base_id, &table, token, formula.as_deref())?;
        let mut events = Vec::with_capacity(records.len());
        let mut cursor = prior.clone();
        for r in &records {
            let Some(t) = r.created_time else {
                continue;
            };
            if !prior.should_emit(t, &r.id) {
                continue;
            }
            events.push(record_to_event(r, "update"));
            cursor.observe(t, &r.id);
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
        let base_id = Self::base_id(config)?;
        let table = Self::table(config)?;
        let base_enc = percent_encode_path_component(&base_id);
        let table_enc = percent_encode_path_component(&table);
        let rec_enc = percent_encode_path_component(document_id.as_str());
        let url = format!("{base_url}/v0/{base_enc}/{table_enc}/{rec_enc}");
        let record: AirtableRecord = bearer_get_json(
            &self.transport,
            "airtable",
            "/v0/{base}/{table}/{record}",
            &url,
            token,
            &[],
        )?;

        // Title heuristic: prefer a "Name"/"Title" field, else the id.
        let title = record
            .fields
            .get("Name")
            .or_else(|| record.fields.get("Title"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| record.id.clone(), std::string::ToString::to_string);

        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(obj) = record.fields.as_object() {
            for (k, v) in obj {
                let rendered = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let _ = writeln!(md, "- **{k}:** {rendered}");
            }
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "airtable",
                "base_id": base_id,
                "table": table,
                "record_id": record.id,
            }))
            .with_source_url(format!(
                "https://airtable.com/{base_id}/{table}/{}",
                record.id
            )))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let base_id = Self::base_id(config)?;
        let base_enc = percent_encode_path_component(&base_id);
        let url = format!("{base_url}/v0/bases/{base_enc}/webhooks");
        let body = serde_json::json!({
            "notificationUrl": callback_url,
            "specification": {
                "options": {
                    "filters": { "dataTypes": ["tableData"] }
                }
            }
        });
        let resp: AirtableWebhookResponse = bearer_post_json(
            &self.transport,
            "airtable",
            "/v0/bases/{base}/webhooks",
            &url,
            token,
            &[],
            &body,
        )?;
        if resp.id.is_empty() {
            return Err(ConnectorError::Webhook(
                "airtable /v0/bases/{base}/webhooks returned no id".into(),
            ));
        }
        let secret = resp.mac_secret_base64.clone().unwrap_or_else(|| {
            config
                .auth_config_json
                .get("webhook_secret")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("airtable-webhook-secret")
                .to_string()
        });
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payloads: AirtablePayloadList = serde_json::from_slice(body)?;
        let mut events = Vec::new();
        for payload in &payloads.payloads {
            let occurred_at = payload.timestamp.unwrap_or_else(Utc::now);
            for change in payload.changed_tables_by_id.values() {
                for rec_id in change.created_records_by_id.keys() {
                    events.push(ConnectorEvent::DocumentCreated {
                        document_id: SourceDocumentId::new(rec_id.clone()),
                        occurred_at,
                    });
                }
                for rec_id in change.changed_records_by_id.keys() {
                    events.push(ConnectorEvent::DocumentUpdated {
                        document_id: SourceDocumentId::new(rec_id.clone()),
                        occurred_at,
                    });
                }
                for rec_id in &change.destroyed_record_ids {
                    events.push(ConnectorEvent::DocumentDeleted {
                        document_id: SourceDocumentId::new(rec_id.clone()),
                        occurred_at,
                    });
                }
            }
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
                "airtable-access",
                "airtable-refresh",
                Utc::now() + Duration::hours(1),
                "data.records:read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Airtable, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "personal_access_token": "patABC",
                "base_id": "appBASE",
                "table": "Tasks",
                "api_base_url": "https://api.test/airtable",
            }))
    }

    fn record(id: &str, created: &str) -> serde_json::Value {
        serde_json::json!({ "id": id, "createdTime": created, "fields": { "Name": id } })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_pat() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "patABC");
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg =
            ConnectorConfig::new(ConnectorKind::Airtable, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "authorization_code": "abc" }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "airtable-access"
        );
    }

    #[test]
    fn initial_sync_paginates_via_offset() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/airtable/v0/appBASE/Tasks?pageSize=100",
            ok_json(&serde_json::json!({
                "records": [record("rec1", "2024-01-01T00:00:00Z")],
                "offset": "off2"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/airtable/v0/appBASE/Tasks?pageSize=100&offset=off2",
            ok_json(&serde_json::json!({
                "records": [record("rec2", "2024-01-02T00:00:00Z")]
            })),
        );
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_applies_created_time_filter() {
        let transport = Arc::new(MockHttpTransport::new());
        // Inclusive `>=` via `NOT(IS_BEFORE(...))` so the boundary row is
        // re-returned by the server.
        let formula = format!(
            "NOT(IS_BEFORE(CREATED_TIME(), '{}'))",
            "2024-01-01T00:00:00+00:00"
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/airtable/v0/appBASE/Tasks?pageSize=100&filterByFormula={}",
                percent_encode_path_component(&formula)
            ),
            ok_json(&serde_json::json!({
                "records": [
                    record("seen", "2024-01-01T00:00:00Z"),
                    record("boundary_new", "2024-01-01T00:00:00Z"),
                    record("newer", "2024-02-01T00:00:00Z")
                ]
            })),
        );
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
    fn initial_sync_requires_base_and_table() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg =
            ConnectorConfig::new(ConnectorKind::Airtable, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "personal_access_token": "p" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn subscribe_webhook_registers_and_captures_id_and_secret() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/airtable/v0/bases/appBASE/webhooks",
            ok_json(&serde_json::json!({ "id": "wbh1", "macSecretBase64": "c2VjcmV0" })),
        );
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/at")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wbh1"));
        assert_eq!(sub.secret.expose(), "c2VjcmV0");
    }

    #[test]
    fn webhook_emits_event_per_record_across_payloads() {
        let body = serde_json::json!({
            "payloads": [
                {
                    "timestamp": "2024-03-01T00:00:00Z",
                    "changedTablesById": {
                        "tbl1": {
                            "createdRecordsById": { "recA": {}, "recB": {} },
                            "changedRecordsById": { "recC": {} },
                            "destroyedRecordIds": ["recD"]
                        }
                    }
                },
                {
                    "timestamp": "2024-03-02T00:00:00Z",
                    "changedTablesById": {
                        "tbl1": { "changedRecordsById": { "recE": {} } }
                    }
                }
            ]
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        // recA, recB (create), recC (update), recD (delete), recE (update)
        assert_eq!(evs.len(), 5);
        let created = evs
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentCreated { .. }))
            .count();
        let deleted = evs
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }))
            .count();
        assert_eq!(created, 2);
        assert_eq!(deleted, 1);
    }

    #[test]
    fn webhook_empty_payload_list_is_ok() {
        let body = serde_json::json!({ "payloads": [] });
        let transport = Arc::new(MockHttpTransport::new());
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn fetch_content_renders_fields() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/airtable/v0/appBASE/Tasks/rec7",
            ok_json(&serde_json::json!({
                "id": "rec7",
                "fields": { "Name": "Ship it", "Status": "Done" }
            })),
        );
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("rec7"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Ship it"));
        assert!(body.contains("**Status:** Done"));
        assert_eq!(fc.title.as_deref(), Some("Ship it"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/airtable/v0/appBASE/Tasks/recX",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = AirtableConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("recX"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
