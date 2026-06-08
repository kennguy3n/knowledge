//! Zoom connector — Zoom REST API v2.
//!
//! A Zoom connector instance ingests a user's cloud recordings
//! (`GET /users/{userId}/recordings`).
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`] against `https://zoom.us/oauth/token`.
//! * `initial_sync` walks the recordings list via Zoom's
//!   `next_page_token` cursor and emits one
//!   [`ConnectorEvent::DocumentCreated`] per recorded meeting; the most
//!   recent `start_time` seen becomes the substrate cursor.
//! * `incremental_sync` re-lists recordings `from` the stored
//!   watermark date and emits the newer meetings, advancing the
//!   watermark.
//! * `fetch_content` GETs a meeting's recording metadata and renders a
//!   text summary (topic + per-file manifest), linking the share URL.
//! * `subscribe_webhook` makes no REST call — Zoom event subscriptions
//!   are configured in the app dashboard and verified with a secret
//!   token, which the returned [`WebhookSubscription`] carries.
//! * `handle_webhook_event` parses a Zoom event envelope
//!   (`recording.completed`, `recording.deleted`, …), draining a
//!   batched array if present, and treats the `endpoint.url_validation`
//!   handshake as a no-op.
//!
//! Wiring contract mirrors the other connectors: the constructor takes
//! an `Arc<dyn HttpTransport>` and an `Arc<dyn OAuth2CodeExchange>`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WatermarkCursor,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Zoom REST base URL. Override via
/// `auth_config_json.api_base_url`.
pub const DEFAULT_API_BASE_URL: &str = "https://api.zoom.us/v2";

/// Default page size for the recordings list (Zoom max is 300).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_LIST_PAGES: usize = 10_000;

/// One recording file within a meeting's recording set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordingFile {
    /// Recording file id.
    #[serde(default)]
    pub id: String,
    /// File type (`MP4`, `M4A`, `TRANSCRIPT`, `CHAT`, …).
    #[serde(default)]
    pub file_type: String,
    /// Authenticated download URL.
    #[serde(default)]
    pub download_url: Option<String>,
    /// Public/play URL.
    #[serde(default)]
    pub play_url: Option<String>,
}

/// One recorded meeting in a `users/{id}/recordings` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordingMeeting {
    /// Meeting UUID (stable across the meeting's lifetime).
    #[serde(default)]
    pub uuid: String,
    /// Numeric meeting id.
    #[serde(default)]
    pub id: serde_json::Value,
    /// Meeting topic / title.
    #[serde(default)]
    pub topic: String,
    /// Meeting start time.
    #[serde(default)]
    pub start_time: Option<DateTime<Utc>>,
    /// Canonical share URL for the recording set.
    #[serde(default)]
    pub share_url: Option<String>,
    /// Recording files attached to this meeting.
    #[serde(default)]
    pub recording_files: Vec<RecordingFile>,
}

/// One page of `users/{id}/recordings` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordingListResponse {
    /// Recorded meetings on this page.
    #[serde(default)]
    pub meetings: Vec<RecordingMeeting>,
    /// Cursor to the next page; empty string on the final page.
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// Zoom connector. Holds the wired transport + OAuth exchange.
pub struct ZoomConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ZoomConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZoomConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ZoomConnector {
    /// Construct a Zoom connector.
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

    /// Override the Zoom REST base URL.
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

    /// Which user's recordings to ingest. Defaults to `me`.
    fn resolved_user_id(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("user_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("me")
            .to_string()
    }

    /// Walk every `next_page_token` page of a recordings list.
    /// `extra_query` is appended verbatim (e.g. `&from=2024-01-01`).
    fn paginate_recordings(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        extra_query: &str,
    ) -> Result<Vec<RecordingMeeting>> {
        let base = self.resolved_base_url(config);
        let user = percent_encode_path_component(&Self::resolved_user_id(config));
        let mut meetings = Vec::<RecordingMeeting>::new();
        let mut page_token: Option<String> = None;
        let mut prev_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let url = match page_token.as_deref() {
                Some(t) => format!(
                    "{base}/users/{user}/recordings?page_size={}{extra_query}&next_page_token={}",
                    self.page_size,
                    percent_encode_path_component(t)
                ),
                None => format!(
                    "{base}/users/{user}/recordings?page_size={}{extra_query}",
                    self.page_size
                ),
            };
            let page: RecordingListResponse = bearer_get_json(
                &self.transport,
                "zoom",
                "/users/{id}/recordings",
                &url,
                token,
                &[],
            )?;
            let returned = page.meetings.len();
            meetings.extend(page.meetings);
            match page.next_page_token {
                // Zoom signals "no more pages" with an empty-string token.
                Some(next) if !next.is_empty() => {
                    if prev_token.as_deref() == Some(next.as_str()) || returned == 0 {
                        return Ok(meetings);
                    }
                    prev_token = Some(next.clone());
                    page_token = Some(next);
                }
                _ => return Ok(meetings),
            }
        }
        Err(ConnectorError::Sync(format!(
            "zoom recordings list exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }
}

fn meeting_event(meeting: &RecordingMeeting) -> ConnectorEvent {
    let occurred_at = meeting.start_time.unwrap_or_else(Utc::now);
    ConnectorEvent::DocumentCreated {
        document_id: SourceDocumentId::new(meeting.uuid.clone()),
        occurred_at,
    }
}

/// Encode a Zoom meeting UUID for interpolation into a URL path segment.
///
/// Zoom meeting UUIDs are base64 strings that routinely contain `/` and
/// `==` (e.g. `kbkXWn+qTm6fk18u2BaH/A==`). Per Zoom's documented rule, a
/// UUID that begins with a `/` or contains `//` must be **double**
/// URL-encoded — once to escape the slashes into `%2F`, then again so the
/// `%2F` survives intermediate proxies that would otherwise normalise it
/// back to a path separator and mis-route the request. Every other UUID
/// is encoded exactly once.
fn encode_meeting_uuid(uuid: &str) -> String {
    let once = percent_encode_path_component(uuid);
    if uuid.starts_with('/') || uuid.contains("//") {
        percent_encode_path_component(&once)
    } else {
        once
    }
}

/// A Zoom webhook event envelope.
#[derive(Debug, Clone, Default, Deserialize)]
struct ZoomEvent {
    #[serde(default)]
    event: String,
    #[serde(default)]
    payload: ZoomEventPayload,
}

/// The `payload.object` carrying the affected meeting.
#[derive(Debug, Clone, Default, Deserialize)]
struct ZoomEventPayload {
    #[serde(default)]
    object: ZoomEventObject,
}

/// The affected meeting object inside a webhook payload.
#[derive(Debug, Clone, Default, Deserialize)]
struct ZoomEventObject {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    id: serde_json::Value,
}

/// Zoom delivers a single event object per POST; accept an array too
/// so a batched delivery is fully drained.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ZoomWebhookBody {
    Batch(Vec<ZoomEvent>),
    Single(ZoomEvent),
}

fn event_object_id(object: &ZoomEventObject) -> Option<String> {
    if let Some(uuid) = object.uuid.as_deref().filter(|s| !s.is_empty()) {
        return Some(uuid.to_string());
    }
    match &object.id {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn zoom_event_to_connector_event(event: &ZoomEvent) -> Option<ConnectorEvent> {
    let id = event_object_id(&event.payload.object)?;
    let occurred_at = Utc::now();
    let document_id = SourceDocumentId::new(id);
    match event.event.as_str() {
        "recording.completed" | "recording.started" | "meeting.created" => {
            Some(ConnectorEvent::DocumentCreated {
                document_id,
                occurred_at,
            })
        }
        "recording.transcript_completed" | "recording.renamed" | "meeting.updated" => {
            Some(ConnectorEvent::DocumentUpdated {
                document_id,
                occurred_at,
            })
        }
        "recording.deleted" | "recording.trashed" | "meeting.deleted" => {
            Some(ConnectorEvent::DocumentDeleted {
                document_id,
                occurred_at,
            })
        }
        // `endpoint.url_validation` and any unrecognised event are
        // skipped — the validation handshake is answered elsewhere.
        _ => None,
    }
}

impl Connector for ZoomConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "zoom authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let from = config
            .auth_config_json
            .get("from")
            .and_then(serde_json::Value::as_str)
            .map(|d| format!("&from={}", percent_encode_path_component(d)))
            .unwrap_or_default();
        let meetings = self.paginate_recordings(config, token, &from)?;
        // Seed the cursor with the latest start_time AND the UUIDs at
        // that instant, so the first incremental run neither re-emits
        // the boundary recording nor skips a later recording sharing
        // its exact sub-second timestamp.
        let mut cursor = WatermarkCursor::empty();
        for (t, id) in meetings
            .iter()
            .filter_map(|m| m.start_time.map(|t| (t, m.uuid.as_str())))
        {
            cursor.observe(t, id);
        }
        let next_cursor = cursor.to_cursor_string();
        let events: Vec<ConnectorEvent> = meetings.iter().map(meeting_event).collect();
        Ok(SyncRunResult {
            events,
            next_cursor,
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let cursor = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "zoom incremental_sync: missing cursor; initial_sync must seed \
                 the latest recording start_time first"
                    .into(),
            )
        })?;
        let cursor = WatermarkCursor::parse(Some(cursor));
        let watermark = cursor.watermark().ok_or_else(|| {
            ConnectorError::Sync("zoom incremental_sync: invalid cursor timestamp".into())
        })?;
        // Zoom's recordings list filters by calendar date (`from`), so
        // re-request from the watermark's day and drop anything already
        // emitted. A record exactly at the watermark instant is kept
        // only if its UUID was not emitted before — this catches a
        // second recording sharing the same sub-second start_time that
        // a strict `>` cursor would skip forever.
        let from = watermark.format("%Y-%m-%d").to_string();
        let extra = format!("&from={from}");
        let meetings: Vec<RecordingMeeting> = self
            .paginate_recordings(config, token, &extra)?
            .into_iter()
            .filter(|m| match m.start_time {
                Some(t) => cursor.should_emit(t, &m.uuid),
                None => true,
            })
            .collect();
        let mut next = cursor.clone();
        for (t, id) in meetings
            .iter()
            .filter_map(|m| m.start_time.map(|t| (t, m.uuid.as_str())))
        {
            next.observe(t, id);
        }
        let next_cursor = next.to_cursor_string();
        let events: Vec<ConnectorEvent> = meetings.iter().map(meeting_event).collect();
        Ok(SyncRunResult {
            events,
            next_cursor,
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let base = self.resolved_base_url(config);
        let id = document_id.as_str();
        let id_enc = encode_meeting_uuid(id);
        let url = format!("{base}/meetings/{id_enc}/recordings");
        let meeting: RecordingMeeting = bearer_get_json(
            &self.transport,
            "zoom",
            "/meetings/{id}/recordings",
            &url,
            token,
            &[],
        )?;
        let mut body = if meeting.topic.is_empty() {
            format!("Zoom meeting {id}")
        } else {
            meeting.topic.clone()
        };
        for file in &meeting.recording_files {
            let link = file
                .play_url
                .as_deref()
                .or(file.download_url.as_deref())
                .unwrap_or("");
            body.push_str("\n- ");
            body.push_str(&file.file_type);
            body.push(' ');
            body.push_str(link);
        }
        let title = if meeting.topic.is_empty() {
            format!("Zoom recording {id}")
        } else {
            meeting.topic.clone()
        };
        let mut fc = FetchedContent::text(body, "text/plain")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "zoom",
                "meeting_uuid": id,
            }));
        if let Some(url) = meeting.share_url {
            fc = fc.with_source_url(url);
        }
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Zoom event subscriptions are configured on the app in the
        // Marketplace dashboard; deliveries are authenticated with a
        // per-app secret token rather than a REST-installed channel, so
        // no HTTP call is made here.
        let _ = (token, &self.transport);
        let secret_token = config
            .auth_config_json
            .get("secret_token")
            .or_else(|| config.auth_config_json.get("verification_token"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("zoom-webhook-secret-token");
        let subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret_token),
            WebhookEventTypes::all(),
            None,
        );
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let parsed: ZoomWebhookBody = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!("zoom webhook: malformed event body: {e}"))
        })?;
        let events = match parsed {
            ZoomWebhookBody::Batch(v) => v,
            ZoomWebhookBody::Single(e) => vec![e],
        };
        let mut out: Vec<ConnectorEvent> = Vec::with_capacity(events.len());
        for e in &events {
            if let Some(ev) = zoom_event_to_connector_event(e) {
                out.push(ev);
            }
        }
        Ok(out)
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
                "zoom-access",
                "zoom-refresh",
                Utc::now() + Duration::hours(1),
                "recording:read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Zoom, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/zoom",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const REC_URL: &str = "https://api.test/zoom/users/me/recordings?page_size=100";

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "zoom-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::Zoom, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_and_seeds_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let t1 = Utc::now() - Duration::hours(2);
        let t2 = Utc::now() - Duration::hours(1);
        transport.expect(
            HttpMethod::Get,
            REC_URL,
            ok_json(&serde_json::json!({
                "meetings": [
                    { "uuid": "u1", "topic": "A", "start_time": t1 },
                    { "uuid": "u2", "topic": "B", "start_time": t2 },
                ],
                "next_page_token": ""
            })),
        );
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        // Cursor is seeded with the boundary instant AND the UUID at
        // that instant so the first incremental run can de-dup ties.
        assert_eq!(
            res.next_cursor.as_deref(),
            Some(format!("{}|u2", t2.to_rfc3339()).as_str())
        );
    }

    #[test]
    fn initial_sync_follows_next_page_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            REC_URL,
            ok_json(&serde_json::json!({
                "meetings": [ { "uuid": "u1", "topic": "A", "start_time": now } ],
                "next_page_token": "PAGE2"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zoom/users/me/recordings?page_size=100&next_page_token=PAGE2",
            ok_json(&serde_json::json!({
                "meetings": [ { "uuid": "u2", "topic": "B", "start_time": now } ],
                "next_page_token": ""
            })),
        );
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
    }

    #[test]
    fn incremental_sync_filters_at_or_before_watermark() {
        let transport = Arc::new(MockHttpTransport::new());
        let watermark = Utc::now() - Duration::hours(3);
        let newer = Utc::now() - Duration::hours(1);
        let from = watermark.format("%Y-%m-%d").to_string();
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/zoom/users/me/recordings?page_size=100&from={from}"),
            ok_json(&serde_json::json!({
                "meetings": [
                    { "uuid": "old", "topic": "old", "start_time": watermark },
                    { "uuid": "new", "topic": "new", "start_time": newer },
                ],
                "next_page_token": ""
            })),
        );
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Cursor records that "old" was already emitted at the boundary
        // instant, so it is suppressed while "new" comes through.
        state.cursor = Some(format!("{}|old", watermark.to_rfc3339()));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "new");
        // Watermark advances to the newer record and records its UUID.
        assert_eq!(
            res.next_cursor.as_deref(),
            Some(format!("{}|new", newer.to_rfc3339()).as_str())
        );
    }

    #[test]
    fn incremental_sync_emits_tie_recording_not_yet_seen() {
        // Two recordings share the EXACT same start_time. The first run
        // emitted only "a"; "b" appears later at the same instant. A
        // strict `>` cursor would drop "b" forever — here it is emitted
        // exactly once and then suppressed.
        let transport = Arc::new(MockHttpTransport::new());
        let boundary = Utc::now() - Duration::hours(2);
        let from = boundary.format("%Y-%m-%d").to_string();
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/zoom/users/me/recordings?page_size=100&from={from}"),
            ok_json(&serde_json::json!({
                "meetings": [
                    { "uuid": "a", "topic": "a", "start_time": boundary },
                    { "uuid": "b", "topic": "b", "start_time": boundary },
                ],
                "next_page_token": ""
            })),
        );
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(format!("{}|a", boundary.to_rfc3339()));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "b");
        assert_eq!(
            res.next_cursor.as_deref(),
            Some(format!("{}|a,b", boundary.to_rfc3339()).as_str())
        );
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            REC_URL,
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn encode_meeting_uuid_double_encodes_slash_cases() {
        // Slashless UUID: encoded exactly once.
        assert_eq!(encode_meeting_uuid("u1"), "u1");
        // A single internal '/' is NOT double-encoded per Zoom's rule —
        // `%2F` alone is enough for these.
        assert_eq!(encode_meeting_uuid("abc+de/fg=="), "abc%2Bde%2Ffg%3D%3D");
        // Leading '/': double-encoded so the `%2F` survives proxies.
        assert_eq!(encode_meeting_uuid("/abc=="), "%252Fabc%253D%253D");
        // Embedded '//': double-encoded.
        assert_eq!(encode_meeting_uuid("ab//cd=="), "ab%252F%252Fcd%253D%253D");
    }

    #[test]
    fn fetch_content_double_encodes_slash_prefixed_uuid() {
        let transport = Arc::new(MockHttpTransport::new());
        // A real-world Zoom UUID beginning with '/' must be double-encoded
        // in the request path.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zoom/meetings/%252Fabc%253D%253D/recordings",
            ok_json(&serde_json::json!({
                "uuid": "/abc==",
                "topic": "Edge UUID",
                "recording_files": []
            })),
        );
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("/abc=="))
            .unwrap();
        assert_eq!(fc.title.as_deref(), Some("Edge UUID"));
    }

    #[test]
    fn fetch_content_renders_topic_and_files() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zoom/meetings/u1/recordings",
            ok_json(&serde_json::json!({
                "uuid": "u1",
                "topic": "Quarterly review",
                "share_url": "https://zoom.us/rec/share/u1",
                "recording_files": [
                    { "id": "f1", "file_type": "MP4", "play_url": "https://zoom.us/rec/play/f1" },
                    { "id": "f2", "file_type": "TRANSCRIPT", "download_url": "https://zoom.us/rec/dl/f2" },
                ]
            })),
        );
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("u1"))
            .unwrap();
        assert_eq!(fc.title.as_deref(), Some("Quarterly review"));
        let text = String::from_utf8(fc.body.clone()).unwrap();
        assert!(text.contains("Quarterly review"));
        assert!(text.contains("MP4"));
        assert!(text.contains("TRANSCRIPT"));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://zoom.us/rec/share/u1")
        );
    }

    #[test]
    fn subscribe_webhook_makes_no_call_and_carries_secret() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let cfg_secret =
            ConnectorConfig::new(ConnectorKind::Zoom, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "demo-code",
                    "secret_token": "zt-123",
                }));
        let sub = c
            .subscribe_webhook(&cfg_secret, &tok, "https://hook.example/zoom")
            .unwrap();
        assert_eq!(sub.secret.expose(), "zt-123");
    }

    #[test]
    fn webhook_recording_completed_maps_created() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "event": "recording.completed",
            "payload": { "object": { "uuid": "mtg-1", "id": 99 } }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "mtg-1");
    }

    #[test]
    fn webhook_deleted_maps_deleted() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "event": "recording.deleted",
            "payload": { "object": { "id": 1234567 } }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
        assert_eq!(evs[0].document_id().as_str(), "1234567");
    }

    #[test]
    fn webhook_url_validation_is_noop() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "event": "endpoint.url_validation",
            "payload": { "plainToken": "abc" }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_malformed_body_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZoomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
