//! Zoho connector — Zoho CRM API v3 (heavily used by GCC SMEs).
//!
//! * `initial_sync` pages `GET /crm/v3/Contacts?per_page=100&page=N`,
//!   stopping when the response `info.more_records` flag is false.
//! * `incremental_sync` sends Zoho's `If-Modified-Since` request
//!   header keyed off the stored RFC-3339 watermark and dedupes the
//!   inclusive boundary row client-side.
//! * `fetch_content` GETs `/crm/v3/Contacts/{id}` and renders a
//!   Markdown summary.
//! * Zoho's notification API needs a per-module channel handshake that
//!   is configured out-of-band, so `subscribe_webhook` records a
//!   polling-only subscription with no provider id.
//! * `handle_webhook_event` parses Zoho's notification payload
//!   (single object or batched array).
//!
//! Zoho authenticates with `Authorization: Zoho-oauthtoken <token>`
//! (not a bearer `Authorization`), so the connector issues requests
//! through the injected [`HttpTransport`] directly rather than the
//! bearer helpers. The token is obtained through the injected
//! [`OAuth2CodeExchange`].

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

/// Default Zoho API base URL (multi-DC; override per data centre, e.g.
/// `https://www.zohoapis.eu` or `https://www.zohoapis.sa`).
pub const DEFAULT_API_BASE_URL: &str = "https://www.zohoapis.com";

/// Page size for record listing (`per_page`). Zoho's documented max is
/// 200.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// One Zoho CRM contact (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZohoContact {
    /// Record id.
    #[serde(rename = "id", default)]
    pub id: String,
    /// Full display name.
    #[serde(rename = "Full_Name", default)]
    pub full_name: Option<String>,
    /// Primary email.
    #[serde(rename = "Email", default)]
    pub email: Option<String>,
    /// Owning account / company name.
    #[serde(rename = "Account_Name", default)]
    pub account_name: serde_json::Value,
    /// RFC-3339 creation timestamp.
    #[serde(rename = "Created_Time", default)]
    pub created_time: Option<String>,
    /// RFC-3339 last-modified timestamp.
    #[serde(rename = "Modified_Time", default)]
    pub modified_time: Option<String>,
}

/// Zoho list response (`{ "data": [...], "info": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZohoListResponse {
    /// Page of records.
    #[serde(default)]
    pub data: Vec<ZohoContact>,
    /// Pagination metadata.
    #[serde(default)]
    pub info: ZohoPageInfo,
}

/// Zoho pagination metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZohoPageInfo {
    /// True when at least one further page exists.
    #[serde(default)]
    pub more_records: bool,
}

/// Zoho single-record response (`{ "data": [ {...} ] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZohoRecordResponse {
    /// Single-element record list.
    #[serde(default)]
    pub data: Vec<ZohoContact>,
}

/// Zoho notification delivery payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZohoWebhookEvent {
    /// Affected record ids.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Operation label, e.g. `insert`, `update`, `delete`.
    #[serde(default)]
    pub operation: String,
}

/// Zoho connector.
pub struct ZohoConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ZohoConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZohoConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ZohoConnector {
    /// Construct a Zoho connector.
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

    /// Override the Zoho base URL (data-centre specific).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size. Clamped to `[1, 200]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 200);
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

    /// GET a JSON endpoint with Zoho's `Zoho-oauthtoken` authorization
    /// scheme and an optional `If-Modified-Since` filter header.
    fn zoho_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
        modified_since: Option<&str>,
    ) -> Result<R> {
        let mut req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header(
                "Authorization",
                format!("Zoho-oauthtoken {}", token.access_token.expose()),
            );
        if let Some(since) = modified_since {
            req = req.with_header("If-Modified-Since", since);
        }
        let resp = self.transport.execute(req)?;
        // Zoho answers `304 Not Modified` (and `204 No Content`) with
        // an empty body when nothing matches the `If-Modified-Since`
        // window — surface those as an empty result rather than a
        // parse error.
        if resp.status == 204 || resp.status == 304 {
            return serde_json::from_slice::<R>(b"{}").map_err(|e| {
                ConnectorError::Sync(format!("zoho {endpoint} empty-response decode failed: {e}"))
            });
        }
        if !resp.is_success() {
            return Err(classify_failure("zoho", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "zoho {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk the contact list page-by-page until `more_records` is
    /// false.
    fn paginate_contacts(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        modified_since: Option<&str>,
    ) -> Result<Vec<ZohoContact>> {
        let mut out = Vec::<ZohoContact>::new();
        for page in 1..=MAX_PAGES {
            let url = format!(
                "{base_url}/crm/v3/Contacts?per_page={}&page={page}",
                self.page_size
            );
            let resp: ZohoListResponse =
                self.zoho_get("/crm/v3/Contacts", &url, token, modified_since)?;
            out.extend(resp.data);
            if !resp.info.more_records {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "zoho /crm/v3/Contacts exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn contact_watermark(c: &ZohoContact) -> Option<DateTime<Utc>> {
    c.modified_time
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| c.created_time.as_deref().and_then(parse_rfc3339))
}

fn account_name_str(value: &serde_json::Value) -> Option<String> {
    // Zoho lookups serialise as `{ "name": "...", "id": "..." }`.
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

impl Connector for ZohoConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "zoho authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let contacts = self.paginate_contacts(&base_url, token, None)?;
        let mut events = Vec::with_capacity(contacts.len());
        let mut cursor = WatermarkCursor::empty();
        for c in &contacts {
            let occurred_at = contact_watermark(c).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(c.id.clone()),
                occurred_at,
            });
            if let Some(t) = contact_watermark(c) {
                cursor.observe(t, &c.id);
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
        let contacts = self.paginate_contacts(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for c in &contacts {
            let Some(modified) = contact_watermark(c) else {
                continue;
            };
            if !prior.should_emit(modified, &c.id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(c.id.clone()),
                occurred_at: modified,
            });
            cursor.observe(modified, &c.id);
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
        let url = format!("{base_url}/crm/v3/Contacts/{id_enc}");
        let resp: ZohoRecordResponse = self.zoho_get("/crm/v3/Contacts/{id}", &url, token, None)?;
        let contact = resp.data.into_iter().next().ok_or_else(|| {
            ConnectorError::Sync(format!("zoho /crm/v3/Contacts/{id} returned no record"))
        })?;
        let title = contact
            .full_name
            .clone()
            .unwrap_or_else(|| format!("Contact {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(email) = contact.email.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Email:** ");
            md.push_str(email);
            md.push_str("\n\n");
        }
        if let Some(account) = account_name_str(&contact.account_name) {
            md.push_str("**Account:** ");
            md.push_str(&account);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "zoho",
                "contact_id": id,
                "email": contact.email,
                "modified_time": contact.modified_time,
            }))
            .with_source_url(format!("{base_url}/crm/tab/Contacts/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Zoho notifications require a per-module channel handshake set
        // up out-of-band; record a polling-only subscription so the
        // runtime falls back to incremental_sync.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("zoho-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<ZohoWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<ZohoWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<ZohoWebhookEvent>(body)?]
            };
        let mut events = Vec::new();
        for delivery in deliveries {
            let occurred_at = Utc::now();
            for raw_id in delivery.ids {
                if raw_id.is_empty() {
                    continue;
                }
                let id = SourceDocumentId::new(raw_id);
                let event = match delivery.operation.as_str() {
                    "insert" => ConnectorEvent::DocumentCreated {
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
        }
        if events.is_empty() {
            return Err(ConnectorError::Webhook(
                "zoho webhook event carried no record ids".into(),
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
                "zoho-access",
                "zoho-refresh",
                Utc::now() + Duration::hours(1),
                "ZohoCRM.modules.ALL",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Zoho, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/zoho",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZohoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Zoho, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_paginates_until_more_records_false() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zoho/crm/v3/Contacts?per_page=100&page=1".to_string(),
            ok_json(&serde_json::json!({
                "data": [{"id": "1", "Modified_Time": "2024-01-01T00:00:00Z"}],
                "info": {"more_records": true}
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zoho/crm/v3/Contacts?per_page=100&page=2".to_string(),
            ok_json(&serde_json::json!({
                "data": [{"id": "2", "Modified_Time": "2024-01-02T00:00:00Z"}],
                "info": {"more_records": false}
            })),
        );
        let c = ZohoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-02T00:00:00+00:00|2")
        );
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zoho/crm/v3/Contacts?per_page=100&page=1".to_string(),
            ok_json(&serde_json::json!({
                "data": [
                    {"id": "10", "Modified_Time": "2024-03-01T00:00:00Z"},
                    {"id": "13", "Modified_Time": "2024-03-01T00:00:00Z"},
                    {"id": "11", "Modified_Time": "2024-06-01T00:00:00Z"}
                ],
                "info": {"more_records": false}
            })),
        );
        let c = ZohoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `10` at the boundary instant, so the cursor
        // records that id. This run re-queries the instant inclusively and must
        // (a) NOT re-emit `10`, (b) still surface the brand-new `13` that shares
        // the same second, and (c) advance past it.
        state.cursor = Some("2024-03-01T00:00:00+00:00|10".to_string());
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
            "https://api.test/zoho/crm/v3/Contacts/55".to_string(),
            ok_json(&serde_json::json!({"data": [{
                "id": "55",
                "Full_Name": "Layla Hassan",
                "Email": "layla@example.ae",
                "Account_Name": {"name": "Gulf Trading", "id": "900"},
                "Modified_Time": "2024-03-01T00:00:00Z"
            }]})),
        );
        let c = ZohoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Layla Hassan"));
        assert!(body.contains("**Email:** layla@example.ae"));
        assert!(body.contains("**Account:** Gulf Trading"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZohoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/zoho")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_expands_ids_and_maps_operation() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZohoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let events = c
            .handle_webhook_event(br#"{"ids": ["1", "2"], "operation": "update"}"#)
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ConnectorEvent::DocumentUpdated { .. }));
        let del = c
            .handle_webhook_event(br#"{"ids": ["3"], "operation": "delete"}"#)
            .unwrap();
        assert!(matches!(del[0], ConnectorEvent::DocumentDeleted { .. }));
    }
}
