//! Notion connector — Notion REST API + polling-only delta.
//!
//! Notion does **not** ship native webhooks (as of 2026-05). Steady
//! state is therefore polled — `incremental_sync` re-queries `/v1/search`
//! and filters by `last_edited_time >= cursor`. `subscribe_webhook`
//! and `handle_webhook_event` exist for trait completeness but return
//! a [`ConnectorError::Webhook`] tagged as polling-only so the
//! substrate routes Notion to its scheduled-poll path.
//!
//! Production wiring runs over [`HttpTransport`] — the substrate
//! constructs a [`NotionConnector`] with a real
//! `connector_framework::BlockingHttpTransport` (under the
//! `http-client` feature) and a real `OAuth2Client` for the
//! `https://api.notion.com/v1/oauth/token` exchange. Unit tests
//! pass `MockHttpTransport` + a fixture OAuth2 exchange so the
//! pagination + classification logic runs against real wire-format
//! JSON without hitting `api.notion.com`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_post_json, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SyncRunResult, SyncState, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Notion REST base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.notion.com/v1";

/// Page size cap for `/v1/search`. Notion accepts up to 100; we
/// default to the API maximum to minimise round-trips.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on the number of `/v1/search` pages a single sync
/// will walk. Notion workspaces with more than 1 000 000 pages
/// (10 000 pages × 100/page) need a redesigned ingestion strategy.
pub const MAX_SEARCH_PAGES: usize = 10_000;

/// Notion `object` enum (subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotionObjectKind {
    /// `"page"`.
    Page,
    /// `"database"`.
    Database,
    /// `"block"`.
    Block,
}

/// One page or database returned by `/v1/search` or
/// `/v1/databases/{id}/query`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionObject {
    /// Object id (UUID v4 string).
    pub id: String,
    /// Object kind.
    pub object: NotionObjectKind,
    /// Wall-clock created time.
    #[serde(default)]
    pub created_time: Option<DateTime<Utc>>,
    /// Wall-clock last-edited time.
    #[serde(default)]
    pub last_edited_time: Option<DateTime<Utc>>,
    /// Notion's `archived = true` is the deletion signal.
    #[serde(default)]
    pub archived: bool,
}

/// `/v1/search` (or `/v1/databases/{id}/query`) response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotionSearchResponse {
    /// Page of results.
    #[serde(default)]
    pub results: Vec<NotionObject>,
    /// `next_cursor` for the following page; `None` when exhausted.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// `has_more` flag for explicit pagination control.
    #[serde(default)]
    pub has_more: bool,
}

/// Notion connector. Pure poll-based — no webhook surface.
pub struct NotionConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for NotionConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl NotionConnector {
    /// Construct a Notion connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against
    /// `https://api.notion.com/v1/oauth/token`. The production
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

    /// Override the Notion REST base URL. Only useful when proxying
    /// the API through a gateway during local development.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the per-page size used by `/v1/search`. Clamped to
    /// `[1, 100]` per Notion's documented maximum.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 100);
        self
    }

    /// Resolve the base URL from `auth_config_json` if set,
    /// otherwise fall back to the constructor-time default.
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

    /// Notion mandates a `Notion-Version` header — pin the
    /// `2022-06-28` stable schema since that's what `NotionObject`
    /// is modelled against. `bearer_post_json` already sets
    /// `Authorization: Bearer <token>` / `Content-Type` / `Accept`.
    const NOTION_VERSION_HEADER: (&'static str, &'static str) = ("Notion-Version", "2022-06-28");

    /// Run `/v1/search` with the supplied filter payload and walk
    /// every page (capped at [`MAX_SEARCH_PAGES`]).
    ///
    /// `filter_payload` is the JSON body that Notion accepts on
    /// `POST /v1/search` — `initial_sync` posts a `{}` body which
    /// matches everything; `incremental_sync` posts a sort over
    /// `last_edited_time` descending and filters server-side.
    fn paginate_search(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        filter_payload: &serde_json::Value,
    ) -> Result<Vec<NotionObject>> {
        let mut results = Vec::<NotionObject>::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_SEARCH_PAGES {
            let mut body = filter_payload.clone();
            let body_map = body.as_object_mut().ok_or_else(|| {
                ConnectorError::Sync("notion /v1/search body must be a JSON object".into())
            })?;
            body_map.insert(
                "page_size".to_string(),
                serde_json::Value::from(self.page_size),
            );
            if let Some(c) = cursor.as_ref() {
                body_map.insert(
                    "start_cursor".to_string(),
                    serde_json::Value::String(c.clone()),
                );
            }
            let url = format!("{base_url}/search");
            let resp: NotionSearchResponse = bearer_post_json(
                &self.transport,
                "notion",
                "/v1/search",
                &url,
                token,
                &[Self::NOTION_VERSION_HEADER],
                &body,
            )?;
            results.extend(resp.results);
            let next = resp.next_cursor;
            if !resp.has_more || next.is_none() {
                return Ok(results);
            }
            if next == cursor {
                // Defence-in-depth: Notion claims has_more=true but
                // hands back the same cursor — abort instead of
                // looping forever.
                return Err(ConnectorError::Sync(
                    "notion /v1/search returned the same cursor twice; aborting to avoid infinite loop"
                        .into(),
                ));
            }
            cursor = next;
        }
        Err(ConnectorError::Sync(format!(
            "notion /v1/search exceeded {MAX_SEARCH_PAGES} pages without a terminating cursor"
        )))
    }
}

/// Which sync pass produced this object — we use this instead of
/// comparing `created_time == last_edited_time` because Notion may
/// stamp the two fields at slightly different millisecond instants
/// even on creation, which would silently misclassify an event as
/// `DocumentUpdated`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

fn object_to_event(obj: &NotionObject, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = obj
        .last_edited_time
        .or(obj.created_time)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(obj.id.clone());
    if obj.archived {
        return ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        };
    }
    match mode {
        SyncMode::Initial => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        SyncMode::Incremental => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for NotionConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "notion authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        // Initial sync — `/v1/search` with an empty body matches
        // every page and database the integration can see.
        let objects = self.paginate_search(&base_url, token, &serde_json::json!({}))?;
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(objects.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for obj in &objects {
            events.push(object_to_event(obj, SyncMode::Initial));
            if let Some(t) = obj.last_edited_time {
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
        // Incremental — sort `last_edited_time` descending so we can
        // short-circuit pagination once we cross the prior watermark.
        let search_payload = serde_json::json!({
            "sort": {
                "direction": "descending",
                "timestamp": "last_edited_time"
            }
        });
        let cursor_watermark: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let objects = self.paginate_search(&base_url, token, &search_payload)?;
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = cursor_watermark;
        for obj in &objects {
            // Server-side filtering by `last_edited_time` isn't
            // supported on `/v1/search`, so we filter client-side
            // against the prior cursor. Skip objects we've
            // already processed.
            if let (Some(prev), Some(t)) = (cursor_watermark, obj.last_edited_time) {
                if t <= prev {
                    continue;
                }
            }
            events.push(object_to_event(obj, SyncMode::Incremental));
            if let Some(t) = obj.last_edited_time {
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
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        _callback_url: &str,
    ) -> Result<WebhookSubscription> {
        Err(ConnectorError::Webhook(
            "polling-only mode: Notion has no native webhook surface".to_string(),
        ))
    }

    fn handle_webhook_event(&self, _body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        Err(ConnectorError::Webhook(
            "polling-only mode: Notion does not deliver webhooks; use incremental_sync".to_string(),
        ))
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

    /// Stub OAuth2 exchange that returns a deterministic token so the
    /// tests can focus on the HTTP / pagination logic.
    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "notion-access",
                "notion-refresh",
                Utc::now() + Duration::days(180),
                "read_content read_user_with_email",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/notion/v1",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "notion-access");
        assert!(tok.scope.contains("read_content"));
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_events_for_each_object() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [
                    {
                        "id": "page-1",
                        "object": "page",
                        "created_time": now,
                        "last_edited_time": now,
                        "archived": false,
                    },
                    {
                        "id": "db-1",
                        "object": "database",
                        "created_time": now - Duration::hours(2),
                        "last_edited_time": now - Duration::hours(1),
                        "archived": false,
                    },
                ],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(res.next_cursor.is_some());
        // Verify the request was POST /search with page_size=100.
        let last = transport.recorded().last().cloned().unwrap();
        assert_eq!(last.method, HttpMethod::Post);
        let body: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
        assert_eq!(body["page_size"], serde_json::json!(100));
    }

    #[test]
    fn initial_sync_paginates_via_next_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-1", "object": "page",
                    "created_time": now, "last_edited_time": now, "archived": false,
                }],
                "next_cursor": "cur-2",
                "has_more": true,
            })),
        );
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-2", "object": "page",
                    "created_time": now, "last_edited_time": now, "archived": false,
                }],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        // Second call must carry start_cursor=cur-2.
        let recorded = transport.recorded();
        assert_eq!(recorded.len(), 2);
        let second: serde_json::Value = serde_json::from_slice(&recorded[1].body).unwrap();
        assert_eq!(second["start_cursor"], serde_json::json!("cur-2"));
    }

    #[test]
    fn initial_sync_classifies_objects_as_created_regardless_of_timestamps() {
        // Regression test: earlier revisions used `created_time ==
        // last_edited_time` to decide DocumentCreated vs
        // DocumentUpdated, which silently misclassified real-world
        // payloads where Notion stamps the two values a few
        // milliseconds apart even on first creation.
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-1",
                    "object": "page",
                    "created_time": now,
                    "last_edited_time": now + Duration::milliseconds(7),
                    "archived": false,
                }],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn incremental_sync_skips_objects_before_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        // Cursor = now - 1 hour; only the page edited at `now`
        // should slip through. The older page must be filtered.
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [
                    {
                        "id": "new-page",
                        "object": "page",
                        "created_time": now - Duration::days(1),
                        "last_edited_time": now,
                        "archived": false,
                    },
                    {
                        "id": "old-page",
                        "object": "page",
                        "created_time": now - Duration::days(2),
                        "last_edited_time": now - Duration::hours(2),
                        "archived": false,
                    },
                ],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some((now - Duration::hours(1)).to_rfc3339());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn incremental_sync_emits_archived_as_deleted() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "page-2",
                    "object": "page",
                    "created_time": now - Duration::days(1),
                    "last_edited_time": now,
                    "archived": true,
                }],
                "next_cursor": null,
                "has_more": false,
            })),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            MockResponse::status(401, b"{\"error\":\"unauthorized\"}".to_vec()),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_maps_500_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            MockResponse::status(500, b"{\"error\":\"server\"}".to_vec()),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn webhook_subscribe_is_unsupported() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://example/webhook")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_event_is_unsupported() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"{}").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn paginate_search_aborts_on_repeated_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let page = serde_json::json!({
            "results": [{
                "id": "page-1", "object": "page",
                "created_time": now, "last_edited_time": now, "archived": false,
            }],
            "next_cursor": "stuck",
            "has_more": true,
        });
        // Register the same response twice — connector receives the
        // same cursor on both pages and must error out.
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&page),
        );
        transport.expect(
            HttpMethod::Post,
            "https://api.test/notion/v1/search",
            ok_json(&page),
        );
        let c = NotionConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        // We need two requests to be issued (first sets cursor=stuck,
        // second uses start_cursor=stuck and gets the same cursor
        // back). Manually exercise paginate_search.
        let base_url = c.resolved_base_url(&cfg());
        let err = c
            .paginate_search(&base_url, &tok, &serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }
}
