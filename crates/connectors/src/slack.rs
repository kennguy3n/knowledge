//! Slack connector — Slack Web API + Events API.
//!
//! Per `docs/DESIGN.md` §10.1 the substrate ingests Slack messages
//! and file shares as observation evidence. Slack ships **two**
//! integration surfaces:
//!
//! * **Web API** — `conversations.list` and `conversations.history`.
//!   Used for `initial_sync` (full pull of every channel the bot
//!   can read) and `incremental_sync` (delta pull keyed off the
//!   `oldest` message timestamp).
//! * **Events API** — push notifications. The substrate registers an
//!   HTTPS callback in the Slack app dashboard; Slack POSTs an event
//!   envelope per change. The first POST is a one-shot URL-verification
//!   challenge that must be echoed back before subscriptions activate.
//!
//! Production wiring runs over [`HttpTransport`] — the substrate
//! constructs a [`SlackConnector`] with a real
//! `connector_framework::BlockingHttpTransport` (under the
//! `http-client` feature) and a real
//! `connector_framework::OAuth2Client`. Unit tests pass
//! `MockHttpTransport` + a fixture OAuth2 exchange so the parsing
//! and pagination logic is exercised against real wire-format JSON
//! without touching `slack.com`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// One Slack channel from `conversations.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackChannel {
    /// Channel id (e.g. `C0123456789`).
    pub id: String,
    /// Channel name (e.g. `general`).
    #[serde(default)]
    pub name: String,
    /// `true` if the channel has been archived.
    #[serde(default)]
    pub is_archived: bool,
}

/// One message returned by `conversations.history`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackMessage {
    /// Slack message timestamp — `"1234567890.000123"`. Doubles as
    /// the message id and as the `oldest` cursor.
    pub ts: String,
    /// `"message"`, `"file_share"`, ...
    #[serde(default, rename = "type")]
    pub message_type: String,
    /// `"message_changed"`, `"message_deleted"`, etc. — only present
    /// for edits / deletes (subtype on the parent `message` type).
    #[serde(default)]
    pub subtype: Option<String>,
    /// Channel id this message belongs to. Slack's
    /// `conversations.history` does not echo it back, so the
    /// substrate fills it in from the requesting channel id.
    #[serde(default)]
    pub channel: String,
    /// Slack user id of the sender (when present).
    #[serde(default)]
    pub user: Option<String>,
    /// Plain-text body.
    #[serde(default)]
    pub text: String,
}

/// One page of `conversations.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackChannelListResponse {
    /// `ok` flag echoed by Slack.
    #[serde(default)]
    pub ok: bool,
    /// Channels returned.
    #[serde(default)]
    pub channels: Vec<SlackChannel>,
    /// Slack response metadata (next page cursor).
    #[serde(default)]
    pub response_metadata: SlackResponseMetadata,
    /// Error message echoed by Slack on failure (e.g.
    /// `"not_authed"`). The substrate uses HTTP status for hard
    /// failures, but Slack sometimes returns 200 with `ok=false` and
    /// a `error` field — the connector treats that as a sync failure.
    #[serde(default)]
    pub error: Option<String>,
}

/// One page of `conversations.history`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackHistoryResponse {
    /// `ok` flag echoed by Slack.
    #[serde(default)]
    pub ok: bool,
    /// Channel id this page came from. Used to backfill
    /// [`SlackMessage::channel`] when Slack's response omits it.
    #[serde(default)]
    pub channel: String,
    /// Messages returned, newest first per Slack convention; the
    /// connector reverses these into chronological order before
    /// emitting events.
    #[serde(default)]
    pub messages: Vec<SlackMessage>,
    /// `has_more` flag — paged responses set this to true.
    #[serde(default)]
    pub has_more: bool,
    /// Slack response metadata (next page cursor).
    #[serde(default)]
    pub response_metadata: SlackResponseMetadata,
    /// Error message echoed by Slack on failure.
    #[serde(default)]
    pub error: Option<String>,
}

/// Slack `response_metadata` envelope (next-page cursor).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackResponseMetadata {
    /// Cursor for `conversations.history` / `conversations.list`.
    #[serde(default)]
    pub next_cursor: String,
}

/// Slack Events API outer envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackEventEnvelope {
    /// Top-level type — `"event_callback"`, `"url_verification"`, ...
    #[serde(default, rename = "type")]
    pub envelope_type: String,
    /// URL verification challenge (only on `url_verification`).
    #[serde(default)]
    pub challenge: Option<String>,
    /// Inner event (only on `event_callback`).
    #[serde(default)]
    pub event: Option<SlackInnerEvent>,
    /// Slack-side wall-clock event time (Unix seconds).
    #[serde(default)]
    pub event_time: Option<i64>,
}

/// Slack Events API inner event payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackInnerEvent {
    /// Inner event type — `"message"`, `"file_shared"`,
    /// `"channel_archive"`, ...
    #[serde(default, rename = "type")]
    pub event_type: String,
    /// Subtype — set on edits / deletes.
    #[serde(default)]
    pub subtype: Option<String>,
    /// Channel id.
    #[serde(default)]
    pub channel: Option<String>,
    /// Message timestamp (and id).
    #[serde(default)]
    pub ts: Option<String>,
    /// Original message timestamp on edits / deletes.
    #[serde(default)]
    pub deleted_ts: Option<String>,
    /// File id on `file_shared` events.
    #[serde(default)]
    pub file_id: Option<String>,
}

/// Default Slack API base URL. Override via
/// `config.auth_config_json["api_base_url"]` when proxying through a
/// gateway (uncommon — Slack's public API does not offer a self-hosted
/// variant). Returned as an `&'static str` so the default lives in
/// `.rodata` without a per-call allocation.
pub const DEFAULT_API_BASE_URL: &str = "https://slack.com/api";

/// Default page size for `conversations.history`. Slack's documented
/// maximum is 1000; 200 is the Slack-recommended sweet spot — large
/// enough to amortise the rate-limit cost, small enough to avoid
/// per-call latency that would surface as a sync stall.
pub const DEFAULT_HISTORY_PAGE_LIMIT: u32 = 200;

/// Cap on the number of `conversations.list` pages walked during an
/// initial sync. Workspaces with more than ~50,000 channels (500
/// pages × 100/page) are vanishingly rare for the substrate's target
/// deployments; this guard prevents a misbehaving paginator from
/// looping forever if Slack ever ships a bug that returns the same
/// cursor twice.
pub const MAX_LIST_PAGES: usize = 500;

/// Cap on the number of `conversations.history` pages walked per
/// channel during one sync run. 250 pages × 200 messages/page = 50k
/// messages per channel per run. Anything larger should be split
/// across multiple incremental runs.
pub const MAX_HISTORY_PAGES_PER_CHANNEL: usize = 250;

/// Slack connector. Drives the Web API over an injected
/// [`HttpTransport`] so the same code runs in production (against
/// `slack.com`) and in unit tests (against `MockHttpTransport`).
#[derive(Clone)]
pub struct SlackConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    history_page_limit: u32,
}

impl std::fmt::Debug for SlackConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("history_page_limit", &self.history_page_limit)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl SlackConnector {
    /// Construct a Slack connector.
    ///
    /// `transport` carries every Web API call; `oauth` drives the
    /// `authorization_code` exchange for `authenticate`. The
    /// production substrate wires these to
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
            history_page_limit: DEFAULT_HISTORY_PAGE_LIMIT,
        }
    }

    /// Override the Slack API base URL. Only useful when proxying
    /// Slack through a gateway during local development.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the per-page limit used by `conversations.history`.
    /// Clamped to `[1, 1000]` per Slack's documented maximum.
    #[must_use]
    pub fn with_history_page_limit(mut self, limit: u32) -> Self {
        self.history_page_limit = limit.clamp(1, 1000);
        self
    }

    /// Compose the source document id Slack uses — `"slack:<channel>:<ts>"`.
    pub fn document_id(channel: &str, ts: &str) -> SourceDocumentId {
        SourceDocumentId::new(format!("slack:{channel}:{ts}"))
    }

    /// Compose the source document id for a Slack file share —
    /// `"slack:file:<file_id>"`.
    pub fn file_document_id(file_id: &str) -> SourceDocumentId {
        SourceDocumentId::new(format!("slack:file:{file_id}"))
    }

    /// Compose the source document id for a Slack channel —
    /// `"slack:channel:<channel_id>"`. Used when the channel itself
    /// is archived, which the substrate models as a tombstone on the
    /// containing object.
    pub fn channel_document_id(channel_id: &str) -> SourceDocumentId {
        SourceDocumentId::new(format!("slack:channel:{channel_id}"))
    }

    fn resolved_base_url(&self, config: &ConnectorConfig) -> String {
        // Allow per-instance override via auth_config_json; fall
        // back to whatever was configured at construction time.
        config
            .auth_config_json
            .get("api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    fn list_channels(&self, base_url: &str, token: &OAuth2Token) -> Result<Vec<SlackChannel>> {
        let mut channels = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            // `exclude_archived=true` keeps the response cheap — the
            // connector also filters defensively in case a workspace
            // policy overrides the flag at the API gateway.
            let url = match &cursor {
                Some(c) => format!(
                    "{base_url}/conversations.list?exclude_archived=true&limit=200&cursor={}",
                    connector_framework::percent_encode_form_component(c)
                ),
                None => format!("{base_url}/conversations.list?exclude_archived=true&limit=200"),
            };
            let resp: SlackChannelListResponse =
                bearer_get_json(&self.transport, "slack", "conversations.list", &url, token, &[])?;
            check_slack_ok(&resp.ok, &resp.error, "conversations.list")?;
            channels.extend(resp.channels.into_iter().filter(|c| !c.is_archived));
            let next = resp.response_metadata.next_cursor;
            if next.is_empty() {
                return Ok(channels);
            }
            // Defence-in-depth: if Slack ever returns the same cursor
            // twice in a row, break instead of looping forever.
            if Some(&next) == cursor.as_ref() {
                return Err(ConnectorError::Sync(
                    "slack conversations.list returned the same cursor twice; aborting to avoid \
                     infinite loop"
                        .into(),
                ));
            }
            cursor = Some(next);
        }
        Err(ConnectorError::Sync(format!(
            "slack conversations.list exceeded {MAX_LIST_PAGES} page cap; channel list truncated"
        )))
    }

    fn fetch_history(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        channel_id: &str,
        oldest: Option<&str>,
    ) -> Result<Vec<SlackMessage>> {
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_HISTORY_PAGES_PER_CHANNEL {
            let mut url = format!(
                "{base_url}/conversations.history?channel={}&limit={}",
                connector_framework::percent_encode_form_component(channel_id),
                self.history_page_limit
            );
            if let Some(o) = oldest {
                url.push_str("&oldest=");
                url.push_str(&connector_framework::percent_encode_form_component(o));
            }
            if let Some(c) = &cursor {
                url.push_str("&cursor=");
                url.push_str(&connector_framework::percent_encode_form_component(c));
            }
            let resp: SlackHistoryResponse = bearer_get_json(
                &self.transport,
                "slack",
                "conversations.history",
                &url,
                token,
                &[],
            )?;
            check_slack_ok(&resp.ok, &resp.error, "conversations.history")?;
            for mut msg in resp.messages {
                if msg.channel.is_empty() {
                    msg.channel = channel_id.to_string();
                }
                messages.push(msg);
            }
            if !resp.has_more {
                return Ok(messages);
            }
            let next = resp.response_metadata.next_cursor;
            if next.is_empty() {
                return Ok(messages);
            }
            if Some(&next) == cursor.as_ref() {
                return Err(ConnectorError::Sync(format!(
                    "slack conversations.history returned the same cursor twice for channel \
                     {channel_id}; aborting to avoid infinite loop"
                )));
            }
            cursor = Some(next);
        }
        Err(ConnectorError::Sync(format!(
            "slack conversations.history exceeded {MAX_HISTORY_PAGES_PER_CHANNEL} page cap for \
             channel {channel_id}; history truncated"
        )))
    }
}

/// Slack returns `200 OK` with `{"ok":false,"error":"..."}` for
/// application-layer failures (permission denied, channel not found,
/// rate-limited tokens). Map those onto `ConnectorError::Sync` so the
/// runtime backs off instead of treating the parsed-JSON as a real
/// page.
fn check_slack_ok(ok: &bool, error: &Option<String>, endpoint: &str) -> Result<()> {
    if *ok {
        return Ok(());
    }
    let msg = error.clone().unwrap_or_else(|| "unknown_error".into());
    // Slack's documented error code for an invalidated bearer is
    // `invalid_auth` / `not_authed` / `token_revoked`. Map those onto
    // `Auth` so the runtime triggers a re-authorisation prompt; every
    // other ok=false maps to `Sync` for retriable failures.
    match msg.as_str() {
        "invalid_auth" | "not_authed" | "token_revoked" | "token_expired" | "account_inactive" => {
            Err(ConnectorError::Auth(format!(
                "slack {endpoint} responded ok=false error={msg}"
            )))
        }
        _ => Err(ConnectorError::Sync(format!(
            "slack {endpoint} responded ok=false error={msg}"
        ))),
    }
}

/// Decode a Slack `ts` (`"1234567890.000123"`) into a `DateTime<Utc>`.
fn slack_ts_to_datetime(ts: &str) -> Option<DateTime<Utc>> {
    let mut parts = ts.split('.');
    let secs = parts.next()?.parse::<i64>().ok()?;
    let micro = parts.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
    // Slack microseconds are a non-negative 6-digit string;
    // `try_from` rejects negative or overflowing values and we then
    // multiply into nanoseconds with saturating arithmetic to keep
    // an out-of-range microsecond from wrapping around.
    let micro_u32 = u32::try_from(micro).unwrap_or(0);
    DateTime::<Utc>::from_timestamp(secs, micro_u32.saturating_mul(1_000))
}

/// Map a Slack message into a substrate-side connector event,
/// keyed by sync mode (initial = create, incremental = update,
/// `subtype == "message_deleted"` = delete).
fn message_to_event(msg: &SlackMessage, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = slack_ts_to_datetime(&msg.ts).unwrap_or_else(Utc::now);
    let id = SlackConnector::document_id(&msg.channel, &msg.ts);
    if msg.subtype.as_deref() == Some("message_deleted") {
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
        SyncMode::Incremental => {
            if msg.subtype.as_deref() == Some("message_changed") {
                ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                }
            } else {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            }
        }
    }
}

/// Which sync pass produced this event — Slack reuses the
/// `message` event type for both create and edit, so the connector
/// needs a side-channel signal to disambiguate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

impl Connector for SlackConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "slack authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let channels = self.list_channels(&base_url, token)?;
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = None;
        for channel in &channels {
            let mut messages = self.fetch_history(&base_url, token, &channel.id, None)?;
            // Slack returns history newest-first; emit chronologically.
            messages.reverse();
            for msg in messages {
                let ev = message_to_event(&msg, SyncMode::Initial);
                let occurred_at = ev.occurred_at();
                events.push(ev);
                watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
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
        let channels = self.list_channels(&base_url, token)?;
        // Convert the RFC-3339 watermark we wrote in `initial_sync`
        // back into a Slack-format `ts` for the `oldest` parameter.
        // Slack accepts both fractional and integer seconds, so we
        // emit Unix-seconds-with-fractional-microseconds.
        let oldest = state
            .cursor
            .as_deref()
            .and_then(rfc3339_to_slack_ts)
            .or_else(|| state.cursor.clone());
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = None;
        for channel in &channels {
            let mut messages =
                self.fetch_history(&base_url, token, &channel.id, oldest.as_deref())?;
            messages.reverse();
            for msg in messages {
                let ev = message_to_event(&msg, SyncMode::Incremental);
                let occurred_at = ev.occurred_at();
                events.push(ev);
                watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
            }
        }
        let next_cursor = watermark
            .map(|t| t.to_rfc3339())
            // If no new messages, keep the prior cursor so the next
            // run still asks for messages newer than the previous
            // watermark.
            .or_else(|| state.cursor.clone());
        Ok(SyncRunResult {
            events,
            next_cursor,
        })
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Slack's Events API is configured in the Slack app dashboard
        // (Features → Event Subscriptions → "Request URL"). There is
        // no `/subscriptions` REST endpoint to POST to. The substrate
        // therefore models `subscribe_webhook` as a metadata-only
        // construction step: it captures the signing secret from
        // `auth_config_json` so `handle_webhook_event` can verify
        // signatures, and records the callback URL so the runtime
        // can surface it back to the app dashboard for the operator
        // to paste in.
        //
        // The signing secret is a long-lived value provisioned at
        // app-install time. We read it out of `auth_config_json`
        // rather than from the OAuth2 token because Slack issues it
        // separately from the user OAuth grant — it is per-app, not
        // per-user.
        let signing_secret = config
            .auth_config_json
            .get("signing_secret")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "slack subscribe_webhook: auth_config_json.signing_secret is required to \
                     verify incoming Events API payloads"
                        .into(),
                )
            })?;
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(signing_secret),
            WebhookEventTypes {
                document_created: true,
                document_updated: true,
                document_deleted: true,
                permission_changed: false,
            },
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let env: SlackEventEnvelope = serde_json::from_slice(body)?;
        match env.envelope_type.as_str() {
            // Slack's URL-verification handshake. The substrate is
            // expected to echo `challenge` back; the connector just
            // surfaces an empty event list to indicate "nothing to
            // ingest, but the body parsed cleanly".
            "url_verification" => {
                if env.challenge.is_none() {
                    return Err(ConnectorError::Webhook(
                        "url_verification envelope missing challenge".into(),
                    ));
                }
                Ok(Vec::new())
            }
            "event_callback" => {
                let inner = env.event.ok_or_else(|| {
                    ConnectorError::Webhook("event_callback missing event".into())
                })?;
                let occurred_at = env
                    .event_time
                    .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0))
                    .unwrap_or_else(Utc::now);
                let event = match inner.event_type.as_str() {
                    "message" => {
                        let channel = inner.channel.unwrap_or_default();
                        let ts = inner.ts.unwrap_or_default();
                        if ts.is_empty() {
                            return Err(ConnectorError::Webhook("message event missing ts".into()));
                        }
                        let id = SlackConnector::document_id(&channel, &ts);
                        match inner.subtype.as_deref() {
                            Some("message_deleted") => ConnectorEvent::DocumentDeleted {
                                document_id: SlackConnector::document_id(
                                    &channel,
                                    inner.deleted_ts.as_deref().unwrap_or(&ts),
                                ),
                                occurred_at,
                            },
                            Some("message_changed") => ConnectorEvent::DocumentUpdated {
                                document_id: id,
                                occurred_at,
                            },
                            _ => ConnectorEvent::DocumentCreated {
                                document_id: id,
                                occurred_at,
                            },
                        }
                    }
                    "file_shared" => {
                        let file_id = inner.file_id.ok_or_else(|| {
                            ConnectorError::Webhook("file_shared event missing file_id".into())
                        })?;
                        ConnectorEvent::DocumentCreated {
                            document_id: SlackConnector::file_document_id(&file_id),
                            occurred_at,
                        }
                    }
                    "channel_archive" => {
                        let channel = inner.channel.ok_or_else(|| {
                            ConnectorError::Webhook("channel_archive event missing channel".into())
                        })?;
                        ConnectorEvent::DocumentDeleted {
                            document_id: SlackConnector::channel_document_id(&channel),
                            occurred_at,
                        }
                    }
                    other => {
                        return Err(ConnectorError::Webhook(format!(
                            "unknown Slack inner event type: {other}"
                        )));
                    }
                };
                Ok(vec![event])
            }
            other => Err(ConnectorError::Webhook(format!(
                "unknown Slack envelope type: {other}"
            ))),
        }
    }
}

/// Convert an RFC-3339 timestamp (the cursor format we write at the
/// end of each sync run) into a Slack `ts` (`"<unix_secs>.<micros>"`).
///
/// Returns `None` if the input is not a valid RFC-3339 timestamp;
/// callers fall through to using the raw cursor string in that case
/// (Slack accepts both forms).
fn rfc3339_to_slack_ts(s: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Utc);
    let secs = dt.timestamp();
    let micros = dt.timestamp_subsec_micros();
    Some(format!("{secs}.{micros:06}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;
    use serde_json::json;

    /// Tiny `OAuth2CodeExchange` fake used in unit tests — returns a
    /// fixed token without ever touching the HTTP transport. The
    /// connector's auth path is exercised separately in
    /// `authenticate_*` tests.
    struct FixedOAuthExchange;

    impl OAuth2CodeExchange for FixedOAuthExchange {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "test-access-token",
                "test-refresh-token",
                Utc::now() + Duration::hours(12),
                "channels:history channels:read files:read",
            ))
        }
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Slack, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(json!({
                "authorization_code": "ac-1",
                "signing_secret": "sec-xyz",
                "api_base_url": "https://api.test/slack",
            }))
    }

    fn connector_with(transport: Arc<MockHttpTransport>) -> SlackConnector {
        SlackConnector::new(
            ConnectorInstanceId::new_v4(),
            transport,
            Arc::new(FixedOAuthExchange),
        )
    }

    fn token() -> OAuth2Token {
        OAuth2Token::new(
            "test-access-token",
            "test-refresh-token",
            Utc::now() + Duration::hours(12),
            "channels:history channels:read files:read",
        )
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "test-access-token");
        assert!(tok.scope.contains("channels:history"));
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let mut config = cfg();
        config.auth_config_json = json!({"signing_secret": "sec-xyz"});
        let err = c.authenticate(&config).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_walks_channels_then_history_and_emits_chronologically() {
        let transport = Arc::new(MockHttpTransport::new());
        // conversations.list page 1 → two channels, no next cursor.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channels": [
                        {"id": "C-A", "name": "general", "is_archived": false},
                        {"id": "C-B", "name": "design", "is_archived": false},
                    ],
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        // C-A history — Slack returns newest-first.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.history?channel=C-A&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channel": "C-A",
                    "messages": [
                        {"ts": "1700000300.000000", "type": "message", "text": "third"},
                        {"ts": "1700000200.000000", "type": "message", "text": "second"},
                        {"ts": "1700000100.000000", "type": "message", "text": "first"},
                    ],
                    "has_more": false,
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        // C-B history — empty.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.history?channel=C-B&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channel": "C-B",
                    "messages": [],
                    "has_more": false,
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        let c = connector_with(Arc::clone(&transport));
        let res = c.initial_sync(&cfg(), &token()).unwrap();
        assert_eq!(res.events.len(), 3);
        // Substrate emits chronologically — first event must be the oldest ts.
        let first = res.events[0].document_id().as_str().to_string();
        assert!(
            first.ends_with("1700000100.000000"),
            "expected oldest first, got {first}",
        );
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        // Cursor advanced to the latest ts.
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn initial_sync_paginates_conversations_list_and_filters_archived() {
        let transport = Arc::new(MockHttpTransport::new());
        // List page 1 → one archived channel + one active, plus a
        // next_cursor.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channels": [
                        // Slack should already have filtered this
                        // because of exclude_archived=true, but the
                        // connector defends in depth and re-filters.
                        {"id": "C-ARCH", "name": "old", "is_archived": true},
                        {"id": "C-A", "name": "general", "is_archived": false},
                    ],
                    "response_metadata": {"next_cursor": "PAGE2"},
                }))
                .unwrap(),
            ),
        );
        // List page 2 → one more channel.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200&cursor=PAGE2",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channels": [
                        {"id": "C-B", "name": "design", "is_archived": false},
                    ],
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        // History for both active channels — empty.
        for ch in ["C-A", "C-B"] {
            transport.expect(
                HttpMethod::Get,
                format!("https://api.test/slack/conversations.history?channel={ch}&limit=200"),
                MockResponse::ok_json(
                    serde_json::to_vec(&json!({
                        "ok": true,
                        "channel": ch,
                        "messages": [],
                        "has_more": false,
                        "response_metadata": {"next_cursor": ""},
                    }))
                    .unwrap(),
                ),
            );
        }
        let c = connector_with(Arc::clone(&transport));
        let res = c.initial_sync(&cfg(), &token()).unwrap();
        assert!(res.events.is_empty(), "no messages → no events");
        // Recorded requests: 2 list calls + 2 history calls (no
        // history call for the archived channel).
        let recs = transport.recorded();
        let history_calls = recs
            .iter()
            .filter(|r| r.url.contains("conversations.history"))
            .count();
        assert_eq!(
            history_calls, 2,
            "archived channel must NOT trigger a history call: {recs:#?}"
        );
    }

    #[test]
    fn initial_sync_paginates_conversations_history() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channels": [{"id": "C-A", "name": "g", "is_archived": false}],
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        // History page 1 → has_more=true with a cursor.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.history?channel=C-A&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channel": "C-A",
                    "messages": [{"ts": "1700000200.000000", "type": "message"}],
                    "has_more": true,
                    "response_metadata": {"next_cursor": "HCURSOR"},
                }))
                .unwrap(),
            ),
        );
        // History page 2 → has_more=false, no cursor.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.history?channel=C-A&limit=200&cursor=HCURSOR",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channel": "C-A",
                    "messages": [{"ts": "1700000100.000000", "type": "message"}],
                    "has_more": false,
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        let c = connector_with(Arc::clone(&transport));
        let res = c.initial_sync(&cfg(), &token()).unwrap();
        assert_eq!(res.events.len(), 2);
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse {
                status: 401,
                headers: vec![],
                body: br#"{"error":"invalid_auth"}"#.to_vec(),
            },
        );
        let c = connector_with(Arc::clone(&transport));
        let err = c.initial_sync(&cfg(), &token()).unwrap_err();
        assert!(
            matches!(err, ConnectorError::Auth(_)),
            "401 must map to Auth, got {err:?}"
        );
    }

    #[test]
    fn initial_sync_maps_slack_ok_false_invalid_auth_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        // Slack's HTTP layer can return 200 with ok=false for
        // invalidated tokens.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                br#"{"ok":false,"error":"invalid_auth"}"#.to_vec(),
            ),
        );
        let c = connector_with(Arc::clone(&transport));
        let err = c.initial_sync(&cfg(), &token()).unwrap_err();
        assert!(
            matches!(err, ConnectorError::Auth(_)),
            "ok=false invalid_auth must map to Auth, got {err:?}"
        );
    }

    #[test]
    fn initial_sync_maps_slack_ok_false_other_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                br#"{"ok":false,"error":"ratelimited"}"#.to_vec(),
            ),
        );
        let c = connector_with(Arc::clone(&transport));
        let err = c.initial_sync(&cfg(), &token()).unwrap_err();
        assert!(
            matches!(err, ConnectorError::Sync(_)),
            "ok=false ratelimited must map to Sync, got {err:?}"
        );
    }

    #[test]
    fn incremental_sync_uses_cursor_as_oldest_parameter() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channels": [{"id": "C-A", "name": "g", "is_archived": false}],
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        // Cursor "1700000200.000000" → should appear in the oldest
        // parameter on the history call (already in Slack's native
        // format — no RFC-3339 round-trip).
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.history?channel=C-A&limit=200&oldest=1700000200.000000",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channel": "C-A",
                    "messages": [{"ts": "1700000300.000000", "type": "message"}],
                    "has_more": false,
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        let c = connector_with(Arc::clone(&transport));
        let state = SyncState {
            connector: c.instance,
            mode: connector_framework::SyncMode::Incremental,
            cursor: Some("1700000200.000000".into()),
            last_synced_at: None,
            status: connector_framework::SyncStatus::Succeeded,
            last_error: None,
        };
        let res = c.incremental_sync(&cfg(), &token(), &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn incremental_sync_keeps_prior_cursor_when_no_new_messages() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channels": [{"id": "C-A", "name": "g", "is_archived": false}],
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.history?channel=C-A&limit=200&oldest=1700000999.000000",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channel": "C-A",
                    "messages": [],
                    "has_more": false,
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        let c = connector_with(Arc::clone(&transport));
        let state = SyncState {
            connector: c.instance,
            mode: connector_framework::SyncMode::Incremental,
            cursor: Some("1700000999.000000".into()),
            last_synced_at: None,
            status: connector_framework::SyncStatus::Succeeded,
            last_error: None,
        };
        let res = c.incremental_sync(&cfg(), &token(), &state).unwrap();
        assert!(res.events.is_empty());
        // Cursor preserved so we don't slide back to the beginning.
        assert_eq!(res.next_cursor.as_deref(), Some("1700000999.000000"));
    }

    #[test]
    fn incremental_sync_converts_rfc3339_cursor_to_slack_ts() {
        // initial_sync writes the cursor as RFC-3339; incremental_sync
        // must round-trip it back into Slack's ts format for the
        // oldest parameter.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.list?exclude_archived=true&limit=200",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channels": [{"id": "C-A", "name": "g", "is_archived": false}],
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        // 2023-11-14T22:13:20Z → 1700000000.000000
        transport.expect(
            HttpMethod::Get,
            "https://api.test/slack/conversations.history?channel=C-A&limit=200&oldest=1700000000.000000",
            MockResponse::ok_json(
                serde_json::to_vec(&json!({
                    "ok": true,
                    "channel": "C-A",
                    "messages": [],
                    "has_more": false,
                    "response_metadata": {"next_cursor": ""},
                }))
                .unwrap(),
            ),
        );
        let c = connector_with(Arc::clone(&transport));
        let state = SyncState {
            connector: c.instance,
            mode: connector_framework::SyncMode::Incremental,
            cursor: Some("2023-11-14T22:13:20Z".into()),
            last_synced_at: None,
            status: connector_framework::SyncStatus::Succeeded,
            last_error: None,
        };
        let res = c.incremental_sync(&cfg(), &token(), &state).unwrap();
        assert!(res.events.is_empty());
    }

    #[test]
    fn subscribe_webhook_requires_signing_secret() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let mut config = cfg();
        config.auth_config_json = json!({"authorization_code": "ac"});
        let err = c
            .subscribe_webhook(&config, &token(), "https://cb.example/slack")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn subscribe_webhook_captures_signing_secret_from_config() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let sub = c
            .subscribe_webhook(&cfg(), &token(), "https://cb.example/slack")
            .unwrap();
        assert_eq!(sub.callback_url, "https://cb.example/slack");
        // Signing secret round-trips into the WebhookSubscription's secret.
        assert_eq!(sub.secret.expose(), "sec-xyz");
        assert!(sub.event_types.document_created);
        assert!(sub.event_types.document_updated);
        assert!(sub.event_types.document_deleted);
    }

    #[test]
    fn handle_webhook_url_verification_returns_no_events() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let body = serde_json::to_vec(&json!({
            "type": "url_verification",
            "challenge": "abc-123",
        }))
        .unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn handle_webhook_message_creates_event() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let body = serde_json::to_vec(&json!({
            "type": "event_callback",
            "event_time": 1_700_000_000_i64,
            "event": {
                "type": "message",
                "channel": "C-A",
                "ts": "1700000000.000123",
                "text": "hi",
            },
        }))
        .unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn handle_webhook_message_deleted_maps_to_delete() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let body = serde_json::to_vec(&json!({
            "type": "event_callback",
            "event_time": 1_700_000_000_i64,
            "event": {
                "type": "message",
                "subtype": "message_deleted",
                "channel": "C-A",
                "ts": "1700000000.000123",
                "deleted_ts": "1699999999.000000",
            },
        }))
        .unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::DocumentDeleted { document_id, .. } => {
                assert!(
                    document_id.as_str().ends_with("1699999999.000000"),
                    "delete must use deleted_ts: {document_id:?}"
                );
            }
            other => panic!("expected delete, got {other:?}"),
        }
    }

    #[test]
    fn handle_webhook_file_shared_maps_to_file_document() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let body = serde_json::to_vec(&json!({
            "type": "event_callback",
            "event_time": 1_700_000_000_i64,
            "event": {
                "type": "file_shared",
                "file_id": "F-abc",
            },
        }))
        .unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "slack:file:F-abc");
            }
            other => panic!("expected created, got {other:?}"),
        }
    }

    #[test]
    fn handle_webhook_channel_archive_emits_delete_on_channel_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = connector_with(transport);
        let body = serde_json::to_vec(&json!({
            "type": "event_callback",
            "event_time": 1_700_000_000_i64,
            "event": {
                "type": "channel_archive",
                "channel": "C-A",
            },
        }))
        .unwrap();
        let evs = c.handle_webhook_event(&body).unwrap();
        match &evs[0] {
            ConnectorEvent::DocumentDeleted { document_id, .. } => {
                assert_eq!(document_id.as_str(), "slack:channel:C-A");
            }
            other => panic!("expected delete, got {other:?}"),
        }
    }

    #[test]
    fn rfc3339_to_slack_ts_round_trips() {
        let t = "2023-11-14T22:13:20Z";
        let got = rfc3339_to_slack_ts(t).expect("must parse");
        assert_eq!(got, "1700000000.000000");
    }

    #[test]
    fn rfc3339_to_slack_ts_handles_fractional_seconds() {
        let t = "2023-11-14T22:13:20.123456Z";
        let got = rfc3339_to_slack_ts(t).expect("must parse");
        assert_eq!(got, "1700000000.123456");
    }

    #[test]
    fn slack_ts_to_datetime_handles_microseconds() {
        let dt = slack_ts_to_datetime("1700000000.000123").unwrap();
        assert_eq!(dt.timestamp(), 1_700_000_000);
        assert_eq!(dt.timestamp_subsec_micros(), 123);
    }
}
