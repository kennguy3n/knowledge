//! Pipedrive connector — Pipedrive REST API v1 (`/v1`).
//!
//! * `initial_sync` pages `/v1/deals?start=N&limit=100` following the
//!   `additional_data.pagination.next_start` cursor until
//!   `more_items_in_collection` is false.
//! * Pipedrive's `/v1/deals` listing has no server-side
//!   updated-since filter, so `incremental_sync` re-walks deals and
//!   emits only those whose `update_time` is strictly newer than the
//!   stored watermark (client-side, strict `>` ⇒ no boundary dedup).
//! * `fetch_content` GETs the single deal (`/v1/deals/{id}`) and
//!   reconstructs Markdown from `title` + key deal fields.
//! * `subscribe_webhook` POSTs `/v1/webhooks` and persists
//!   Pipedrive's returned webhook id.
//! * `handle_webhook_event` parses Pipedrive's `{ meta: { action,
//!   object, id } }` envelope, tolerating a batched array.
//!
//! Pipedrive timestamps use the space-separated UTC form
//! `YYYY-MM-DD HH:MM:SS`.

use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Pipedrive API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.pipedrive.com";

/// Page size for deal listing (`limit`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// One Pipedrive deal (subset of fields used by the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveDeal {
    /// Numeric deal id.
    #[serde(default)]
    pub id: u64,
    /// Deal title.
    #[serde(default)]
    pub title: Option<String>,
    /// Pipeline status (`open`, `won`, `lost`, …).
    #[serde(default)]
    pub status: Option<String>,
    /// Monetary value.
    #[serde(default)]
    pub value: Option<f64>,
    /// Currency code.
    #[serde(default)]
    pub currency: Option<String>,
    /// Creation time (`YYYY-MM-DD HH:MM:SS`, UTC).
    #[serde(default)]
    pub add_time: Option<String>,
    /// Last-update time (`YYYY-MM-DD HH:MM:SS`, UTC).
    #[serde(default)]
    pub update_time: Option<String>,
}

/// Pagination block inside `additional_data`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedrivePagination {
    /// Offset to request for the next page.
    #[serde(default)]
    pub next_start: Option<u64>,
    /// Whether more rows remain after this page.
    #[serde(default)]
    pub more_items_in_collection: bool,
}

/// The `additional_data` block of a list response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveAdditionalData {
    /// Pagination descriptor.
    #[serde(default)]
    pub pagination: PipedrivePagination,
}

/// One page of `/v1/deals`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveDealListResponse {
    /// Whether the request succeeded.
    #[serde(default)]
    pub success: bool,
    /// Deals on this page (null when empty).
    #[serde(default)]
    pub data: Option<Vec<PipedriveDeal>>,
    /// Pagination wrapper.
    #[serde(default)]
    pub additional_data: PipedriveAdditionalData,
}

/// Single-deal response (`GET /v1/deals/{id}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveDealResponse {
    /// Whether the request succeeded.
    #[serde(default)]
    pub success: bool,
    /// The deal body.
    #[serde(default)]
    pub data: PipedriveDeal,
}

/// `POST /v1/webhooks` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveWebhookResponse {
    /// Created webhook.
    #[serde(default)]
    pub data: PipedriveWebhookHandle,
}

/// The id-bearing portion of a webhook response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveWebhookHandle {
    /// Pipedrive webhook id.
    #[serde(default)]
    pub id: serde_json::Value,
}

/// Pipedrive webhook delivery envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveWebhookEvent {
    /// Event metadata.
    #[serde(default)]
    pub meta: PipedriveWebhookMeta,
}

/// The `meta` block of a webhook delivery.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipedriveWebhookMeta {
    /// `added`, `updated`, `deleted`, `merged`.
    #[serde(default)]
    pub action: String,
    /// Object type, e.g. `deal`.
    #[serde(default)]
    pub object: String,
    /// Affected object id.
    #[serde(default)]
    pub id: serde_json::Value,
}

/// Pipedrive connector.
pub struct PipedriveConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for PipedriveConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipedriveConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl PipedriveConnector {
    /// Construct a Pipedrive connector.
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

    /// Override the Pipedrive API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size.
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

    /// Walk `/v1/deals` following `next_start` until exhausted.
    fn paginate_deals(&self, base_url: &str, token: &OAuth2Token) -> Result<Vec<PipedriveDeal>> {
        let mut deals = Vec::<PipedriveDeal>::new();
        let mut start: u64 = 0;
        for _ in 0..MAX_PAGES {
            let url = format!("{base_url}/v1/deals?start={start}&limit={}", self.page_size);
            let resp: PipedriveDealListResponse =
                bearer_get_json(&self.transport, "pipedrive", "/v1/deals", &url, token, &[])?;
            if let Some(rows) = resp.data {
                deals.extend(rows);
            }
            let pagination = resp.additional_data.pagination;
            if pagination.more_items_in_collection {
                match pagination.next_start {
                    Some(next) => start = next,
                    None => return Ok(deals),
                }
            } else {
                return Ok(deals);
            }
        }
        Err(ConnectorError::Sync(format!(
            "pipedrive /deals exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Parse a Pipedrive timestamp (`YYYY-MM-DD HH:MM:SS` UTC, or
/// RFC-3339 as a fallback) into UTC.
fn parse_pipedrive_dt(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

fn deal_watermark(deal: &PipedriveDeal) -> Option<DateTime<Utc>> {
    deal.update_time
        .as_deref()
        .and_then(parse_pipedrive_dt)
        .or_else(|| deal.add_time.as_deref().and_then(parse_pipedrive_dt))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for PipedriveConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "pipedrive authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let deals = self.paginate_deals(&base_url, token)?;
        let mut events = Vec::with_capacity(deals.len());
        let mut cursor = WatermarkCursor::empty();
        for deal in &deals {
            let occurred_at = deal_watermark(deal).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(deal.id.to_string()),
                occurred_at,
            });
            if let Some(t) = deal_watermark(deal) {
                cursor.observe(t, &deal.id.to_string());
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
        let deals = self.paginate_deals(&base_url, token)?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for deal in &deals {
            let Some(updated) = deal_watermark(deal) else {
                continue;
            };
            if !prior.should_emit(updated, &deal.id.to_string()) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(deal.id.to_string()),
                occurred_at: updated,
            });
            cursor.observe(updated, &deal.id.to_string());
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
        let url = format!("{base_url}/v1/deals/{id_enc}");
        let resp: PipedriveDealResponse = bearer_get_json(
            &self.transport,
            "pipedrive",
            "/v1/deals/{id}",
            &url,
            token,
            &[],
        )?;
        let deal = resp.data;
        let title = deal.title.clone().unwrap_or_default();
        let mut md = String::new();
        if !title.is_empty() {
            md.push_str("# ");
            md.push_str(&title);
            md.push_str("\n\n");
        }
        if let Some(status) = &deal.status {
            md.push_str("**Status**: ");
            md.push_str(status);
            md.push('\n');
        }
        if let Some(value) = deal.value {
            let currency = deal.currency.clone().unwrap_or_default();
            md.push_str("**Value**: ");
            md.push_str(&value.to_string());
            md.push(' ');
            md.push_str(&currency);
            md.push('\n');
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "pipedrive",
                "deal_id": id,
                "update_time": deal.update_time,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/v1/webhooks");
        let request = serde_json::json!({
            "subscription_url": callback_url,
            "event_action": "*",
            "event_object": "deal",
        });
        let resp: PipedriveWebhookResponse = bearer_post_json(
            &self.transport,
            "pipedrive",
            "/v1/webhooks",
            &url,
            token,
            &[],
            &request,
        )?;
        let provider_id = id_value_to_string(&resp.data.id);
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("pipedrive-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<PipedriveWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<PipedriveWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<PipedriveWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty pipedrive webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.meta.id).ok_or_else(|| {
                ConnectorError::Webhook("pipedrive webhook event missing meta.id".into())
            })?;
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(id_str);
            let event = match delivery.meta.action.as_str() {
                "added" => ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                },
                "deleted" => ConnectorEvent::DocumentDeleted {
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
                "pd-access",
                "pd-refresh",
                Utc::now() + Duration::hours(1),
                "deals:read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Pipedrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/pd",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn deals_url(start: u64) -> String {
        format!("https://api.test/pd/v1/deals?start={start}&limit={DEFAULT_PAGE_SIZE}")
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(
            ConnectorKind::Pipedrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "pd-access"
        );
    }

    #[test]
    fn initial_sync_paginates_next_start() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            deals_url(0),
            ok_json(&serde_json::json!({
                "success": true,
                "data": [{"id": 1, "title": "a", "update_time": "2024-01-01 00:00:00"}],
                "additional_data": {"pagination": {"next_start": 100, "more_items_in_collection": true}}
            })),
        );
        transport.expect(
            HttpMethod::Get,
            deals_url(100),
            ok_json(&serde_json::json!({
                "success": true,
                "data": [{"id": 2, "title": "b", "update_time": "2024-01-02 00:00:00"}],
                "additional_data": {"pagination": {"more_items_in_collection": false}}
            })),
        );
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-02T00:00:00+00:00|2")
        );
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_client_side() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            deals_url(0),
            ok_json(&serde_json::json!({
                "success": true,
                "data": [
                    {"id": 1, "update_time": "2024-01-01 00:00:00"},
                    {"id": 2, "update_time": "2024-06-01 00:00:00"}
                ],
                "additional_data": {"pagination": {"more_items_in_collection": false}}
            })),
        );
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-03-01T00:00:00+00:00".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|2")
        );
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/pd/v1/deals/77".to_string(),
            ok_json(&serde_json::json!({
                "success": true,
                "data": {
                    "id": 77,
                    "title": "Big deal",
                    "status": "open",
                    "value": 5000.0,
                    "currency": "USD",
                    "update_time": "2024-01-01 00:00:00"
                }
            })),
        );
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("77"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Big deal"));
        assert!(body.contains("**Status**: open"));
        assert!(body.contains("**Value**: 5000 USD"));
    }

    #[test]
    fn subscribe_webhook_creates_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/pd/v1/webhooks".to_string(),
            ok_json(&serde_json::json!({"status": "ok", "data": {"id": 314}})),
        );
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/pd")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("314"));
    }

    #[test]
    fn webhook_maps_actions() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let added = c
            .handle_webhook_event(
                &serde_json::to_vec(
                    &serde_json::json!({"meta": {"action": "added", "object": "deal", "id": 1}}),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(added[0], ConnectorEvent::DocumentCreated { .. }));
        let batch = c
            .handle_webhook_event(
                &serde_json::to_vec(&serde_json::json!([
                    {"meta": {"action": "updated", "object": "deal", "id": 2}},
                    {"meta": {"action": "deleted", "object": "deal", "id": 3}}
                ]))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[0], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(batch[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_missing_id_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = PipedriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(
                &serde_json::to_vec(
                    &serde_json::json!({"meta": {"action": "updated", "object": "deal"}})
                )
                .unwrap()
            ),
            Err(ConnectorError::Webhook(_))
        ));
    }
}
