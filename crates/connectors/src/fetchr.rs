//! Fetchr connector — Fetchr API (UAE last-mile logistics).
//!
//! * `initial_sync` pages `GET /v1/shipments?per_page=100&page=N`,
//!   stopping on a short page.
//! * `incremental_sync` adds the `updated_since` filter keyed off the
//!   stored RFC-3339 watermark; the filter is inclusive, so the
//!   boundary row is deduped client-side.
//! * `fetch_content` GETs `/v1/shipments/{id}` and renders a Markdown
//!   tracking summary.
//! * `subscribe_webhook` POSTs `/v1/webhooks` and records the returned
//!   provider subscription id.
//! * `handle_webhook_event` parses Fetchr's tracking payload (single
//!   object or batched array).
//!
//! Fetchr authenticates with an API key carried in the
//! `X-Fetchr-Api-Key` header (not a bearer `Authorization`), so the
//! connector issues requests through the injected [`HttpTransport`]
//! directly rather than the bearer helpers.

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

/// Default Fetchr API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.fetchr.us";

/// Page size for shipment listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// Scope recorded on a token synthesised from a configured API key.
const DEFAULT_SCOPE: &str = "shipments.read";

/// One Fetchr shipment (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchrShipment {
    /// Shipment id.
    #[serde(default)]
    pub id: String,
    /// Tracking number.
    #[serde(default)]
    pub tracking_number: Option<String>,
    /// Delivery status (e.g. `in_transit`, `delivered`).
    #[serde(default)]
    pub status: Option<String>,
    /// Recipient name.
    #[serde(default)]
    pub recipient_name: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Fetchr shipment list response (`{ "shipments": [...] }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchrShipmentsResponse {
    /// Page of shipments.
    #[serde(default)]
    pub shipments: Vec<FetchrShipment>,
}

/// Fetchr single-shipment response (`{ "shipment": {...} }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchrShipmentResponse {
    /// The shipment.
    #[serde(default)]
    pub shipment: FetchrShipment,
}

/// Fetchr webhook-create response (`{ "id": ... }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchrWebhookResponse {
    /// Provider subscription id.
    #[serde(default)]
    pub id: String,
}

/// Fetchr webhook delivery payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchrWebhookEvent {
    /// Affected shipment id (string or number).
    #[serde(default)]
    pub shipment_id: serde_json::Value,
    /// Event label, e.g. `shipment_created`, `status_updated`.
    #[serde(default)]
    pub event: String,
}

/// Fetchr connector.
pub struct FetchrConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for FetchrConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchrConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .finish()
    }
}

impl FetchrConnector {
    /// Construct a Fetchr connector.
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

    /// Override the Fetchr base URL.
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

    /// GET a JSON endpoint with Fetchr's `X-Fetchr-Api-Key` header.
    fn fetchr_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("X-Fetchr-Api-Key", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("fetchr", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "fetchr {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// POST a JSON body with the `X-Fetchr-Api-Key` header and parse
    /// the response.
    fn fetchr_post<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
        body: &serde_json::Value,
    ) -> Result<R> {
        let body_bytes = serde_json::to_vec(body).map_err(|e| {
            ConnectorError::Sync(format!("fetchr {endpoint} serialise body failed: {e}"))
        })?;
        let req = HttpRequest::post(url, body_bytes)
            .with_header("Accept", "application/json")
            .with_header("Content-Type", "application/json")
            .with_header("X-Fetchr-Api-Key", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("fetchr", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "fetchr {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk the shipment list page-by-page, stopping on a short page.
    fn paginate_shipments(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_since: Option<&str>,
    ) -> Result<Vec<FetchrShipment>> {
        let mut out = Vec::<FetchrShipment>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!(
                "{base_url}/v1/shipments?per_page={}&page={page}",
                self.page_size
            );
            if let Some(since) = updated_since {
                url.push_str("&updated_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let resp: FetchrShipmentsResponse = self.fetchr_get("/v1/shipments", &url, token)?;
            let count = resp.shipments.len();
            out.extend(resp.shipments);
            if count < self.page_size as usize {
                return Ok(out);
            }
        }
        Err(ConnectorError::Sync(format!(
            "fetchr /v1/shipments exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn shipment_watermark(s: &FetchrShipment) -> Option<DateTime<Utc>> {
    s.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| s.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for FetchrConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let api_key = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "fetchr authenticate: auth_config_json.api_key is required".into(),
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
        let shipments = self.paginate_shipments(&base_url, token, None)?;
        let mut events = Vec::with_capacity(shipments.len());
        let mut cursor = WatermarkCursor::empty();
        for s in &shipments {
            let occurred_at = shipment_watermark(s).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(s.id.clone()),
                occurred_at,
            });
            if let Some(t) = shipment_watermark(s) {
                cursor.observe(t, &s.id);
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
        let shipments = self.paginate_shipments(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for s in &shipments {
            let Some(updated) = shipment_watermark(s) else {
                continue;
            };
            if !prior.should_emit(updated, &s.id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(s.id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &s.id);
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
        let url = format!("{base_url}/v1/shipments/{id_enc}");
        let resp: FetchrShipmentResponse = self.fetchr_get("/v1/shipments/{id}", &url, token)?;
        let shipment = resp.shipment;
        let title = shipment
            .tracking_number
            .clone()
            .unwrap_or_else(|| format!("Shipment {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(status) = shipment.status.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Status:** ");
            md.push_str(status);
            md.push_str("\n\n");
        }
        if let Some(recipient) = shipment.recipient_name.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Recipient:** ");
            md.push_str(recipient);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "fetchr",
                "shipment_id": id,
                "status": shipment.status,
                "updated_at": shipment.updated_at,
            }))
            .with_source_url(format!("{base_url}/track/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/v1/webhooks");
        let body = serde_json::json!({
            "url": callback_url,
            "events": ["shipment_created", "status_updated"],
        });
        let resp: FetchrWebhookResponse = self.fetchr_post("/v1/webhooks", &url, token, &body)?;
        if resp.id.is_empty() {
            return Err(ConnectorError::Webhook(
                "fetchr /v1/webhooks returned no webhook id".into(),
            ));
        }
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("fetchr-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<FetchrWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<FetchrWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<FetchrWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook("empty fetchr webhook batch".into()));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.shipment_id).ok_or_else(|| {
                ConnectorError::Webhook("fetchr webhook event missing shipment_id".into())
            })?;
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(id_str);
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("cancel") || delivery.event.contains("delete") {
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
        ConnectorConfig::new(ConnectorKind::Fetchr, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "ftc_123",
                "api_base_url": "https://api.test/fetchr",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn small(c: FetchrConnector) -> FetchrConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_wraps_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FetchrConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "ftc_123"
        );
    }

    #[test]
    fn authenticate_requires_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FetchrConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Fetchr, AuthKind::ApiKey, ScopeId::new_v4());
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
            "https://api.test/fetchr/v1/shipments?per_page=2&page=1".to_string(),
            ok_json(&serde_json::json!({"shipments": [
                {"id": "1", "updated_at": "2024-01-01T00:00:00Z"},
                {"id": "2", "updated_at": "2024-01-02T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/fetchr/v1/shipments?per_page=2&page=2".to_string(),
            ok_json(&serde_json::json!({"shipments": [
                {"id": "3", "updated_at": "2024-01-03T00:00:00Z"}
            ]})),
        );
        let c = small(FetchrConnector::new(
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
                "https://api.test/fetchr/v1/shipments?per_page=2&page=1&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"shipments": [
                {"id": "10", "updated_at": "2024-03-01T00:00:00Z"},
                {"id": "13", "updated_at": "2024-03-01T00:00:00Z"}
            ]})),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/fetchr/v1/shipments?per_page=2&page=2&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!({"shipments": [ {"id": "11", "updated_at": "2024-06-01T00:00:00Z"} ]})),
        );
        let c = small(FetchrConnector::new(
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
            "https://api.test/fetchr/v1/shipments/55".to_string(),
            ok_json(&serde_json::json!({"shipment": {
                "id": "55",
                "tracking_number": "FTC-55",
                "status": "in_transit",
                "recipient_name": "Omar"
            }})),
        );
        let c = FetchrConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# FTC-55"));
        assert!(body.contains("**Status:** in_transit"));
        assert!(body.contains("**Recipient:** Omar"));
    }

    #[test]
    fn subscribe_webhook_records_provider_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/fetchr/v1/webhooks".to_string(),
            ok_json(&serde_json::json!({"id": "wh_77"})),
        );
        let c = FetchrConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/fetchr")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("wh_77"));
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FetchrConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(br#"{"shipment_id": "7", "event": "shipment_created"}"#)
            .unwrap();
        assert!(matches!(single[0], ConnectorEvent::DocumentCreated { .. }));
        let batch = c
            .handle_webhook_event(
                br#"[{"shipment_id": 8, "event": "status_updated"}, {"shipment_id": "9", "event": "shipment_cancelled"}]"#,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[1], ConnectorEvent::DocumentDeleted { .. }));
    }
}
