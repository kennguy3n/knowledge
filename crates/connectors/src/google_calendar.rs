//! Google Calendar connector — Calendar API v3.
//!
//! * `authenticate` exchanges the authorization code via the wired
//!   [`OAuth2CodeExchange`] against `https://oauth2.googleapis.com/token`.
//! * `initial_sync` walks
//!   `GET /calendar/v3/calendars/{id}/events`, paginating via
//!   `pageToken`, and seeds the substrate cursor from the
//!   `nextSyncToken` Calendar returns on the final page.
//! * `incremental_sync` re-issues the events list with
//!   `syncToken=<cursor>`; events with `status = "cancelled"` map to
//!   [`ConnectorEvent::DocumentDeleted`], everything else to
//!   created / updated. The fresh `nextSyncToken` becomes the cursor.
//! * `fetch_content` GETs a single event and renders its summary +
//!   description as text.
//! * `subscribe_webhook` POSTs `…/events/watch` to install a push
//!   channel; the channel id + resource id are stashed on the
//!   [`WebhookSubscription`].
//! * `handle_webhook_event` validates Calendar's resource-state push
//!   envelope. Calendar pushes carry no per-event payload (they only
//!   signal "something changed, poll the sync token"), so a valid
//!   notification yields an empty event vector that prompts the
//!   substrate to run `incremental_sync`.
//!
//! Wiring contract mirrors the other connectors: the constructor takes
//! an `Arc<dyn HttpTransport>` and an `Arc<dyn OAuth2CodeExchange>`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Calendar REST base URL. Override via
/// `auth_config_json.api_base_url`.
pub const DEFAULT_API_BASE_URL: &str = "https://www.googleapis.com";

/// Default page size for `events.list`.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_LIST_PAGES: usize = 10_000;

/// One Calendar event (subset relevant to substrate ingestion).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Event id.
    #[serde(default)]
    pub id: String,
    /// Status: `confirmed`, `tentative`, or `cancelled`.
    #[serde(default)]
    pub status: String,
    /// Event title.
    #[serde(default)]
    pub summary: Option<String>,
    /// Free-text description.
    #[serde(default)]
    pub description: Option<String>,
    /// Canonical event URL.
    #[serde(default, rename = "htmlLink")]
    pub html_link: Option<String>,
    /// Creation time.
    #[serde(default)]
    pub created: Option<DateTime<Utc>>,
    /// Last-update time.
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
}

/// One page of `events.list` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventListResponse {
    /// Events on this page.
    #[serde(default)]
    pub items: Vec<CalendarEvent>,
    /// Token for the next page; absent on the final page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    /// Cursor to seed the next incremental run; present on the final
    /// page only.
    #[serde(default, rename = "nextSyncToken")]
    pub next_sync_token: Option<String>,
}

/// `…/events/watch` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchResponse {
    /// Channel id (echoes the UUID we sent).
    #[serde(default)]
    pub id: Option<String>,
    /// Resource id Calendar surfaces on push pings.
    #[serde(default, rename = "resourceId")]
    pub resource_id: Option<String>,
    /// Channel expiry, milliseconds since the Unix epoch (string).
    #[serde(default)]
    pub expiration: Option<String>,
}

/// Calendar resource-state push envelope. Calendar's real push is
/// header-only (`X-Goog-Resource-State`); the substrate inlines the
/// state into a JSON body for parity with the other webhook handlers.
#[derive(Debug, Clone, Default, Deserialize)]
struct CalendarPushNotification {
    #[serde(
        default,
        rename = "resourceState",
        alias = "state",
        alias = "resource_state"
    )]
    resource_state: Option<String>,
}

/// Google Calendar connector. Holds the wired transport + OAuth.
pub struct GoogleCalendarConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for GoogleCalendarConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleCalendarConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl GoogleCalendarConnector {
    /// Construct a Google Calendar connector.
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

    /// Override the Calendar REST base URL.
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

    /// Which calendar to ingest. Defaults to `primary`.
    fn resolved_calendar_id(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("calendar_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("primary")
            .to_string()
    }

    fn events_base(&self, config: &ConnectorConfig) -> String {
        let base = self.resolved_base_url(config);
        let cal = percent_encode_path_component(&Self::resolved_calendar_id(config));
        format!("{base}/calendar/v3/calendars/{cal}/events")
    }

    /// Walk every `pageToken` page of an events list, collecting events
    /// and returning the final `nextSyncToken`.
    fn paginate_events(
        &self,
        first_url: &str,
        token: &OAuth2Token,
    ) -> Result<(Vec<CalendarEvent>, Option<String>)> {
        let mut items = Vec::<CalendarEvent>::new();
        let mut sync_token: Option<String> = None;
        let mut page_token: Option<String> = None;
        let mut prev_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let url = match page_token.as_deref() {
                Some(t) => format!("{first_url}&pageToken={}", percent_encode_path_component(t)),
                None => first_url.to_string(),
            };
            let page: EventListResponse = bearer_get_json(
                &self.transport,
                "google_calendar",
                "/events",
                &url,
                token,
                &[],
            )?;
            let returned = page.items.len();
            items.extend(page.items);
            if page.next_sync_token.is_some() {
                sync_token = page.next_sync_token;
            }
            let Some(next) = page.next_page_token else {
                return Ok((items, sync_token));
            };
            if prev_token.as_deref() == Some(next.as_str()) || returned == 0 {
                return Ok((items, sync_token));
            }
            prev_token = Some(next.clone());
            page_token = Some(next);
        }
        Err(ConnectorError::Sync(format!(
            "google_calendar events.list exceeded {MAX_LIST_PAGES} pages without exhausting cursor"
        )))
    }
}

/// Which sync pass produced this event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

fn event_to_connector_event(event: &CalendarEvent, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = event.updated.or(event.created).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(event.id.clone());
    if event.status == "cancelled" {
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

impl Connector for GoogleCalendarConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "google_calendar authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let first = format!(
            "{}?maxResults={}&singleEvents=true&showDeleted=false",
            self.events_base(config),
            self.page_size
        );
        let (events, sync_token) = self.paginate_events(&first, token)?;
        let connector_events: Vec<ConnectorEvent> = events
            .iter()
            .map(|e| event_to_connector_event(e, SyncMode::Initial))
            .collect();
        Ok(SyncRunResult {
            events: connector_events,
            next_cursor: sync_token,
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let sync_token = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "google_calendar incremental_sync: missing cursor; \
                 initial_sync must seed nextSyncToken first"
                    .into(),
            )
        })?;
        let first = format!(
            "{}?maxResults={}&showDeleted=true&syncToken={}",
            self.events_base(config),
            self.page_size,
            percent_encode_path_component(sync_token)
        );
        let (events, new_sync_token) = self.paginate_events(&first, token)?;
        let connector_events: Vec<ConnectorEvent> = events
            .iter()
            .map(|e| event_to_connector_event(e, SyncMode::Incremental))
            .collect();
        let next_cursor = new_sync_token.or_else(|| Some(sync_token.to_string()));
        Ok(SyncRunResult {
            events: connector_events,
            next_cursor,
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{}/{id_enc}", self.events_base(config));
        let event: CalendarEvent = bearer_get_json(
            &self.transport,
            "google_calendar",
            "/events/{id}",
            &url,
            token,
            &[],
        )?;
        let summary = event.summary.clone().unwrap_or_default();
        let description = event.description.clone().unwrap_or_default();
        let body = if description.is_empty() {
            summary.clone()
        } else if summary.is_empty() {
            description
        } else {
            format!("{summary}\n\n{description}")
        };
        let title = if summary.is_empty() {
            format!("Calendar event {id}")
        } else {
            summary
        };
        let mut fc = FetchedContent::text(body, "text/plain")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "google_calendar",
                "event_id": id,
            }));
        if let Some(link) = event.html_link {
            fc = fc.with_source_url(link);
        }
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let url = format!("{}/watch", self.events_base(config));
        let channel_id = config
            .auth_config_json
            .get("channel_id")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| self.instance.as_uuid().to_string(), str::to_string);
        let client_token = config
            .auth_config_json
            .get("channel_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("google-calendar-channel-token")
            .to_string();
        let body = serde_json::json!({
            "id": channel_id,
            "type": "web_hook",
            "address": callback_url,
            "token": client_token,
        });
        let resp: WatchResponse = bearer_post_json(
            &self.transport,
            "google_calendar",
            "/events/watch",
            &url,
            token,
            &[],
            &body,
        )?;
        let expires_at = resp
            .expiration
            .as_deref()
            .and_then(|ms| ms.parse::<i64>().ok())
            .and_then(DateTime::<Utc>::from_timestamp_millis);
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(client_token),
            WebhookEventTypes::all(),
            expires_at,
        );
        subscription.provider_subscription_id = resp.id.or(Some(channel_id));
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let notification: CalendarPushNotification = serde_json::from_slice(body).map_err(|e| {
            ConnectorError::Webhook(format!("google_calendar webhook: malformed body: {e}"))
        })?;
        // Calendar pushes are signal-only — a valid notification means
        // "poll the sync token". Require the resource-state marker so a
        // truly empty / malformed body is rejected.
        if notification.resource_state.is_none() {
            return Err(ConnectorError::Webhook(
                "google_calendar webhook: missing resourceState".into(),
            ));
        }
        Ok(Vec::new())
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
                "gcal-access",
                "gcal-refresh",
                Utc::now() + Duration::hours(1),
                "https://www.googleapis.com/auth/calendar.readonly",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::GoogleCalendar,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/g",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const EVENTS_URL: &str =
        "https://api.test/g/calendar/v3/calendars/primary/events?maxResults=100&singleEvents=true&showDeleted=false";

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "gcal-access");
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code = ConnectorConfig::new(
            ConnectorKind::GoogleCalendar,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_created_and_seeds_sync_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            EVENTS_URL,
            ok_json(&serde_json::json!({
                "items": [
                    { "id": "e1", "status": "confirmed", "summary": "Standup", "updated": now },
                    { "id": "e2", "status": "confirmed", "summary": "Review", "updated": now },
                ],
                "nextSyncToken": "SYNC1"
            })),
        );
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(res.next_cursor.as_deref(), Some("SYNC1"));
    }

    #[test]
    fn initial_sync_follows_page_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        transport.expect(
            HttpMethod::Get,
            EVENTS_URL,
            ok_json(&serde_json::json!({
                "items": [ { "id": "e1", "status": "confirmed", "updated": now } ],
                "nextPageToken": "P2"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            format!("{EVENTS_URL}&pageToken=P2"),
            ok_json(&serde_json::json!({
                "items": [ { "id": "e2", "status": "confirmed", "updated": now } ],
                "nextSyncToken": "SYNC2"
            })),
        );
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(res.next_cursor.as_deref(), Some("SYNC2"));
    }

    #[test]
    fn incremental_sync_maps_cancelled_to_deleted() {
        let transport = Arc::new(MockHttpTransport::new());
        let now = Utc::now();
        let url = "https://api.test/g/calendar/v3/calendars/primary/events?maxResults=100&showDeleted=true&syncToken=SYNC1";
        transport.expect(
            HttpMethod::Get,
            url,
            ok_json(&serde_json::json!({
                "items": [
                    { "id": "e9", "status": "cancelled", "updated": now },
                ],
                "nextSyncToken": "SYNC2"
            })),
        );
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("SYNC1".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("SYNC2"));
    }

    #[test]
    fn incremental_sync_requires_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
            EVENTS_URL,
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn fetch_content_renders_summary_and_description() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/g/calendar/v3/calendars/primary/events/e1",
            ok_json(&serde_json::json!({
                "id": "e1",
                "summary": "Sprint planning",
                "description": "Agenda: backlog grooming",
                "htmlLink": "https://calendar.google.com/event?eid=e1"
            })),
        );
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("e1"))
            .unwrap();
        assert_eq!(fc.title.as_deref(), Some("Sprint planning"));
        let text = String::from_utf8(fc.body.clone()).unwrap();
        assert!(text.contains("Sprint planning"));
        assert!(text.contains("backlog grooming"));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://calendar.google.com/event?eid=e1")
        );
    }

    #[test]
    fn subscribe_webhook_posts_watch_and_keeps_channel_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/g/calendar/v3/calendars/primary/events/watch",
            ok_json(&serde_json::json!({
                "id": "chan-1",
                "resourceId": "res-1",
                "expiration": "1735689600000"
            })),
        );
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/gcal")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("chan-1"));
        assert!(sub.expires_at.is_some());
    }

    #[test]
    fn webhook_valid_notification_yields_empty_events() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(br#"{"resourceState":"exists"}"#)
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_missing_state_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(br#"{}"#).unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_malformed_body_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = GoogleCalendarConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c.handle_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
