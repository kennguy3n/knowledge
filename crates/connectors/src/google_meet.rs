//! Google Meet connector — Meet REST API v2.
//!
//! A Meet connector instance ingests conference records
//! (`GET /v2/conferenceRecords`) and reconstructs each meeting's text
//! from its transcript entries.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`].
//! * `initial_sync` walks `conferenceRecords.list` via the
//!   `pageToken` cursor and emits one
//!   [`ConnectorEvent::DocumentCreated`] per record; the latest record
//!   `endTime` becomes the substrate cursor.
//! * `incremental_sync` re-lists records with a
//!   `filter=start_time>="…"` bound derived from the cursor and emits
//!   the newer records.
//! * `fetch_content` lists a record's transcripts, concatenates every
//!   transcript's entries (`participant: text`), and returns plain
//!   text.
//! * `subscribe_webhook` makes no REST call — Meet events flow through
//!   the Google Workspace Events API + Pub/Sub, configured
//!   out-of-band; the returned [`WebhookSubscription`] carries the
//!   verification secret.
//! * `handle_webhook_event` maps Workspace Events conference
//!   notifications (`…conference.v2.started`, `…ended`, …) to
//!   [`ConnectorEvent`]s, draining a batched array if present.
//!
//! Wiring contract mirrors the other connectors: the constructor takes
//! an `Arc<dyn HttpTransport>` and an `Arc<dyn OAuth2CodeExchange>`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::Deserialize;

/// Default Meet REST base URL. Override via
/// `auth_config_json.api_base_url`.
pub const DEFAULT_API_BASE_URL: &str = "https://meet.googleapis.com";

/// Default page size for the conference-records list.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_LIST_PAGES: usize = 10_000;

/// One conference record (a single meeting session).
#[derive(Debug, Clone, Default, Deserialize)]
struct ConferenceRecord {
    /// Resource name, e.g. `conferenceRecords/{record_id}`.
    #[serde(default)]
    name: String,
    #[serde(default, rename = "startTime")]
    start_time: Option<DateTime<Utc>>,
    #[serde(default, rename = "endTime")]
    end_time: Option<DateTime<Utc>>,
}

/// One page of `conferenceRecords.list` results.
#[derive(Debug, Clone, Default, Deserialize)]
struct ConferenceRecordList {
    #[serde(default, rename = "conferenceRecords")]
    conference_records: Vec<ConferenceRecord>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

/// One transcript belonging to a conference record.
#[derive(Debug, Clone, Default, Deserialize)]
struct Transcript {
    /// Resource name, e.g. `conferenceRecords/{r}/transcripts/{t}`.
    #[serde(default)]
    name: String,
}

/// One page of `conferenceRecords.transcripts.list` results.
#[derive(Debug, Clone, Default, Deserialize)]
struct TranscriptList {
    #[serde(default)]
    transcripts: Vec<Transcript>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

/// One transcript entry (a single spoken utterance).
#[derive(Debug, Clone, Default, Deserialize)]
struct TranscriptEntry {
    #[serde(default)]
    participant: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// One page of `…transcripts.entries.list` results.
#[derive(Debug, Clone, Default, Deserialize)]
struct TranscriptEntryList {
    #[serde(default, rename = "transcriptEntries")]
    transcript_entries: Vec<TranscriptEntry>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

/// Google Meet connector. Holds the wired transport + OAuth exchange.
pub struct GoogleMeetConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GoogleMeetConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleMeetConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GoogleMeetConnector {
    /// Construct a Google Meet connector.
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

    /// Override the Meet REST base URL.
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

    /// Walk every `pageToken` page of `conferenceRecords.list`.
    /// `extra_query` is appended verbatim (e.g. a `&filter=` clause).
    fn paginate_records(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        extra_query: &str,
    ) -> Result<Vec<ConferenceRecord>> {
        let mut records = Vec::<ConferenceRecord>::new();
        let mut page_token: Option<String> = None;
        let mut prev_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let url = match page_token.as_deref() {
                Some(t) => format!(
                    "{base_url}/v2/conferenceRecords?pageSize={}{extra_query}&pageToken={}",
                    self.page_size,
                    percent_encode_path_component(t)
                ),
                None => format!(
                    "{base_url}/v2/conferenceRecords?pageSize={}{extra_query}",
                    self.page_size
                ),
            };
            let resp: ConferenceRecordList = bearer_get_json(
                &self.transport,
                "google_meet",
                "/v2/conferenceRecords",
                &url,
                token,
                &[],
            )?;
            let returned = resp.conference_records.len();
            records.extend(resp.conference_records);
            let Some(next) = resp.next_page_token else {
                return Ok(records);
            };
            if prev_token.as_deref() == Some(next.as_str()) || returned == 0 {
                return Ok(records);
            }
            prev_token = Some(next.clone());
            page_token = Some(next);
        }
        Err(ConnectorError::Sync(format!(
            "google_meet conferenceRecords.list exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }

    /// List the transcripts attached to a conference record.
    fn list_transcripts(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        record_name: &str,
    ) -> Result<Vec<Transcript>> {
        let mut transcripts = Vec::<Transcript>::new();
        let mut page_token: Option<String> = None;
        let mut prev_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let url = match page_token.as_deref() {
                Some(t) => format!(
                    "{base_url}/v2/{record_name}/transcripts?pageToken={}",
                    percent_encode_path_component(t)
                ),
                None => format!("{base_url}/v2/{record_name}/transcripts"),
            };
            let resp: TranscriptList = bearer_get_json(
                &self.transport,
                "google_meet",
                "/v2/conferenceRecords/{id}/transcripts",
                &url,
                token,
                &[],
            )?;
            let returned = resp.transcripts.len();
            transcripts.extend(resp.transcripts);
            let Some(next) = resp.next_page_token else {
                return Ok(transcripts);
            };
            if prev_token.as_deref() == Some(next.as_str()) || returned == 0 {
                return Ok(transcripts);
            }
            prev_token = Some(next.clone());
            page_token = Some(next);
        }
        Err(ConnectorError::Sync(format!(
            "google_meet transcripts.list exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }

    /// List every entry of a single transcript.
    fn list_entries(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        transcript_name: &str,
    ) -> Result<Vec<TranscriptEntry>> {
        let mut entries = Vec::<TranscriptEntry>::new();
        let mut page_token: Option<String> = None;
        let mut prev_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let url = match page_token.as_deref() {
                Some(t) => format!(
                    "{base_url}/v2/{transcript_name}/entries?pageToken={}",
                    percent_encode_path_component(t)
                ),
                None => format!("{base_url}/v2/{transcript_name}/entries"),
            };
            let resp: TranscriptEntryList = bearer_get_json(
                &self.transport,
                "google_meet",
                "/v2/conferenceRecords/{id}/transcripts/{id}/entries",
                &url,
                token,
                &[],
            )?;
            let returned = resp.transcript_entries.len();
            entries.extend(resp.transcript_entries);
            let Some(next) = resp.next_page_token else {
                return Ok(entries);
            };
            if prev_token.as_deref() == Some(next.as_str()) || returned == 0 {
                return Ok(entries);
            }
            prev_token = Some(next.clone());
            page_token = Some(next);
        }
        Err(ConnectorError::Sync(format!(
            "google_meet transcript entries.list exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }
}

fn record_event(record: &ConferenceRecord) -> ConnectorEvent {
    let occurred_at = record
        .start_time
        .or(record.end_time)
        .unwrap_or_else(Utc::now);
    ConnectorEvent::DocumentCreated {
        document_id: SourceDocumentId::new(record.name.clone()),
        occurred_at,
    }
}

/// Latest `endTime` (falling back to `startTime`) across records.
fn latest_end(records: &[ConferenceRecord]) -> Option<DateTime<Utc>> {
    records
        .iter()
        .filter_map(|r| r.end_time.or(r.start_time))
        .max()
}

/// A Workspace Events conference notification.
#[derive(Debug, Clone, Default, Deserialize)]
struct MeetEvent {
    #[serde(default, rename = "eventType")]
    event_type: String,
    #[serde(default, rename = "conferenceRecord")]
    conference_record: Option<MeetEventRecord>,
}

/// The conference record referenced by a Workspace Events payload.
#[derive(Debug, Clone, Default, Deserialize)]
struct MeetEventRecord {
    #[serde(default)]
    name: String,
}

/// Meet delivers one event per push but a batched array is accepted so
/// every event is drained.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum MeetWebhookBody {
    Batch(Vec<MeetEvent>),
    Single(MeetEvent),
}

fn meet_event_to_connector_event(event: &MeetEvent) -> Option<ConnectorEvent> {
    let name = event
        .conference_record
        .as_ref()
        .map(|r| r.name.clone())
        .filter(|n| !n.is_empty())?;
    let occurred_at = Utc::now();
    let document_id = SourceDocumentId::new(name);
    match event.event_type.as_str() {
        "google.workspace.meet.conference.v2.started" => Some(ConnectorEvent::DocumentCreated {
            document_id,
            occurred_at,
        }),
        "google.workspace.meet.conference.v2.ended"
        | "google.workspace.meet.recording.v2.fileGenerated"
        | "google.workspace.meet.transcript.v2.fileGenerated" => {
            Some(ConnectorEvent::DocumentUpdated {
                document_id,
                occurred_at,
            })
        }
        _ => None,
    }
}

impl Connector for GoogleMeetConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "google_meet authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let records = self.paginate_records(&base_url, token, "")?;
        let next_cursor = latest_end(&records).map(|d| d.to_rfc3339());
        let events: Vec<ConnectorEvent> = records.iter().map(record_event).collect();
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
        let base_url = self.resolved_base_url(config);
        let cursor = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "google_meet incremental_sync: missing cursor; initial_sync must seed \
                 the latest conference endTime first"
                    .into(),
            )
        })?;
        let watermark = DateTime::parse_from_rfc3339(cursor)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| {
                ConnectorError::Sync(format!("google_meet incremental_sync: invalid cursor: {e}"))
            })?;
        // Meet's list filter binds on `start_time`; request records that
        // started at/after the watermark, then drop any whose end time
        // is at or before it to avoid re-emitting the boundary record.
        let filter = format!("start_time>=\"{}\"", watermark.to_rfc3339());
        let extra = format!("&filter={}", percent_encode_path_component(&filter));
        let records: Vec<ConferenceRecord> = self
            .paginate_records(&base_url, token, &extra)?
            .into_iter()
            .filter(|r| r.end_time.or(r.start_time).is_none_or(|t| t > watermark))
            .collect();
        let next_cursor = latest_end(&records)
            .map(|d| d.to_rfc3339())
            .or_else(|| Some(cursor.to_string()));
        let events: Vec<ConnectorEvent> = records.iter().map(record_event).collect();
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
        let base_url = self.resolved_base_url(config);
        // `document_id` is a Meet resource name (`conferenceRecords/{id}`);
        // its slashes are path structure, so it is appended verbatim.
        let record_name = document_id.as_str();
        let transcripts = self.list_transcripts(&base_url, token, record_name)?;
        let mut body = String::new();
        for transcript in &transcripts {
            let entries = self.list_entries(&base_url, token, &transcript.name)?;
            for entry in &entries {
                let speaker = entry.participant.as_deref().unwrap_or("unknown");
                let text = entry.text.as_deref().unwrap_or("");
                body.push_str(speaker);
                body.push_str(": ");
                body.push_str(text);
                body.push('\n');
            }
        }
        let fc = FetchedContent::text(body, "text/plain")
            .with_title(format!("Meet transcript {record_name}"))
            .with_metadata(serde_json::json!({
                "provider": "google_meet",
                "conference_record": record_name,
            }));
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Meet events are delivered through the Google Workspace Events
        // API into a Pub/Sub topic, provisioned out-of-band; deliveries
        // are authenticated with a shared secret rather than a
        // REST-installed channel, so no HTTP call is made here.
        let _ = (token, &self.transport);
        let secret = config
            .auth_config_json
            .get("verification_token")
            .or_else(|| config.auth_config_json.get("secret_token"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("google-meet-webhook-secret");
        let subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let parsed: MeetWebhookBody = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!("google_meet webhook: malformed event body: {e}"))
        })?;
        let events = match parsed {
            MeetWebhookBody::Batch(v) => v,
            MeetWebhookBody::Single(e) => vec![e],
        };
        let mut out: Vec<ConnectorEvent> = Vec::with_capacity(events.len());
        for e in &events {
            if let Some(ev) = meet_event_to_connector_event(e) {
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
                "gmeet-access",
                "gmeet-refresh",
                Utc::now() + Duration::hours(1),
                "https://www.googleapis.com/auth/meetings.space.readonly",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::GoogleMeet,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/meet",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const REC_URL: &str = "https://api.test/meet/v2/conferenceRecords?pageSize=100";

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "gmeet-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::GoogleMeet,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_and_seeds_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let t1 = Utc::now() - Duration::hours(3);
        let t2 = Utc::now() - Duration::hours(1);
        transport.expect(
            HttpMethod::Get,
            REC_URL,
            ok_json(&serde_json::json!({
                "conferenceRecords": [
                    { "name": "conferenceRecords/a", "startTime": t1, "endTime": t1 },
                    { "name": "conferenceRecords/b", "startTime": t2, "endTime": t2 },
                ]
            })),
        );
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(res.next_cursor.as_deref(), Some(t2.to_rfc3339().as_str()));
    }

    #[test]
    fn initial_sync_follows_page_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            REC_URL,
            ok_json(&serde_json::json!({
                "conferenceRecords": [ { "name": "conferenceRecords/a", "endTime": now } ],
                "nextPageToken": "P2"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/meet/v2/conferenceRecords?pageSize=100&pageToken=P2",
            ok_json(&serde_json::json!({
                "conferenceRecords": [ { "name": "conferenceRecords/b", "endTime": now } ]
            })),
        );
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
    }

    #[test]
    fn incremental_sync_filters_at_or_before_watermark() {
        let transport = Arc::new(MockHttpTransport::new());
        let watermark = Utc::now() - Duration::hours(3);
        let newer = Utc::now() - Duration::hours(1);
        let filter = format!("start_time>=\"{}\"", watermark.to_rfc3339());
        let url = format!(
            "https://api.test/meet/v2/conferenceRecords?pageSize=100&filter={}",
            percent_encode_path_component(&filter)
        );
        transport.expect(
            HttpMethod::Get,
            url,
            ok_json(&serde_json::json!({
                "conferenceRecords": [
                    { "name": "conferenceRecords/old", "endTime": watermark },
                    { "name": "conferenceRecords/new", "endTime": newer },
                ]
            })),
        );
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(watermark.to_rfc3339());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            res.events[0].document_id().as_str(),
            "conferenceRecords/new"
        );
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg(), &tok, &state).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_concatenates_transcript_entries() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/meet/v2/conferenceRecords/a/transcripts",
            ok_json(&serde_json::json!({
                "transcripts": [ { "name": "conferenceRecords/a/transcripts/t1" } ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/meet/v2/conferenceRecords/a/transcripts/t1/entries",
            ok_json(&serde_json::json!({
                "transcriptEntries": [
                    { "participant": "Alice", "text": "Hello team" },
                    { "participant": "Bob", "text": "Hi Alice" }
                ]
            })),
        );
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("conferenceRecords/a"))
            .unwrap();
        let text = String::from_utf8(fc.body.clone()).unwrap();
        assert!(text.contains("Alice: Hello team"));
        assert!(text.contains("Bob: Hi Alice"));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            REC_URL,
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn subscribe_webhook_makes_no_call_and_carries_secret() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let cfg_secret = ConnectorConfig::new(
            ConnectorKind::GoogleMeet,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "verification_token": "vt-9",
        }));
        let sub = c
            .subscribe_webhook(&cfg_secret, &tok, "https://hook.example/meet")
            .unwrap();
        assert_eq!(sub.secret.expose(), "vt-9");
    }

    #[test]
    fn webhook_started_maps_created() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "eventType": "google.workspace.meet.conference.v2.started",
            "conferenceRecord": { "name": "conferenceRecords/x" }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "conferenceRecords/x");
    }

    #[test]
    fn webhook_batch_drains_all_events() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!([
            { "eventType": "google.workspace.meet.conference.v2.started", "conferenceRecord": { "name": "conferenceRecords/x" } },
            { "eventType": "google.workspace.meet.conference.v2.ended", "conferenceRecord": { "name": "conferenceRecords/x" } },
            { "eventType": "google.workspace.meet.unknown.v2.thing", "conferenceRecord": { "name": "conferenceRecords/x" } }
        ]);
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        // started + ended map; the unknown type is skipped.
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_malformed_body_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleMeetConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
