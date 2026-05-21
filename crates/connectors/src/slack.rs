//! Slack connector — Slack Web API + Events API.
//!
//! Per `docs/DESIGN.md` §10.1 the substrate ingests Slack messages and
//! file shares as observation evidence. Slack ships **two**
//! integration surfaces:
//!
//! * **Web API** — `conversations.list`, `conversations.history`,
//!   `files.list`. Used for `initial_sync` (full pull of every
//!   channel the bot can read) and `incremental_sync` (delta pull
//!   keyed off the `oldest` timestamp cursor).
//! * **Events API** — push notifications. The substrate registers
//!   an HTTPS callback and Slack POSTs an event envelope per change.
//!   The first POST is a one-shot URL-verification challenge that
//!   must be echoed back before subscriptions activate.
//!
//! This module is fixture-driven so it can be exhaustively
//! unit-tested without touching the network. Production transport
//! (HTTP, retries, rate limits) is the responsibility of the Go
//! gateway.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
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
}

/// Slack `response_metadata` envelope (next-page cursor).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackResponseMetadata {
    /// Cursor for `conversations.history` / `conversations.list`.
    #[serde(default)]
    pub next_cursor: String,
}

/// One initial-sync page — list of channels + history per channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackInitialSyncPage {
    /// Channels surfaced by `conversations.list`.
    #[serde(default)]
    pub channels: Vec<SlackChannel>,
    /// Per-channel history responses keyed by channel id.
    #[serde(default)]
    pub history_by_channel: Vec<SlackHistoryResponse>,
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

/// Slack connector. Pure fixture-driven so the substrate can
/// unit-test it without hitting `slack.com`.
#[derive(Debug, Clone)]
pub struct SlackConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// Initial-sync fixture pages.
    pub initial_pages: Vec<SlackInitialSyncPage>,
    /// Incremental-sync fixture pages — one
    /// [`SlackHistoryResponse`] per `conversations.history` call.
    pub incremental_pages: Vec<SlackHistoryResponse>,
}

impl SlackConnector {
    /// Construct an empty connector.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_pages: Vec::new(),
            incremental_pages: Vec::new(),
        }
    }

    /// Override initial-sync fixture pages.
    pub fn with_initial_pages(mut self, pages: Vec<SlackInitialSyncPage>) -> Self {
        self.initial_pages = pages;
        self
    }

    /// Override incremental-sync fixture pages.
    pub fn with_incremental_pages(mut self, pages: Vec<SlackHistoryResponse>) -> Self {
        self.incremental_pages = pages;
        self
    }

    fn page_index(cursor: Option<&str>) -> usize {
        cursor
            .and_then(|c| c.strip_prefix("page-"))
            .and_then(|n| n.parse::<usize>().ok())
            .map_or(0, |n| n.saturating_sub(1))
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
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "slack-access-token",
            "slack-refresh-token",
            Utc::now() + Duration::hours(12),
            "channels:history channels:read files:read",
        ))
    }

    fn initial_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
    ) -> Result<SyncRunResult> {
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = None;
        for page in &self.initial_pages {
            for hist in &page.history_by_channel {
                // Slack returns history newest-first; the substrate
                // emits events in chronological order so downstream
                // consumers can rely on a monotonic stream.
                let mut messages = hist.messages.clone();
                messages.reverse();
                for mut msg in messages {
                    if msg.channel.is_empty() {
                        msg.channel.clone_from(&hist.channel);
                    }
                    let ev = message_to_event(&msg, SyncMode::Initial);
                    let occurred_at = ev.occurred_at();
                    events.push(ev);
                    watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
                }
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn incremental_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let idx = Self::page_index(state.cursor.as_deref());
        let page = self.incremental_pages.get(idx).cloned().unwrap_or_default();
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = None;
        let mut messages = page.messages.clone();
        messages.reverse();
        for mut msg in messages {
            if msg.channel.is_empty() {
                msg.channel.clone_from(&page.channel);
            }
            let ev = message_to_event(&msg, SyncMode::Incremental);
            let occurred_at = ev.occurred_at();
            events.push(ev);
            watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
        }
        let next_cursor = if idx + 1 < self.incremental_pages.len() {
            Some(format!("page-{}", idx + 2))
        } else {
            watermark.map(|t| t.to_rfc3339())
        };
        Ok(SyncRunResult {
            events,
            next_cursor,
        })
    }

    fn subscribe_webhook(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new("slack-signing-secret"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use connector_framework::{AuthKind, ConnectorKind};
    use evidence_store::ScopeId;

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Slack, AuthKind::OAuth2, ScopeId::new_v4())
    }

    fn message(channel: &str, ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.into(),
            message_type: "message".into(),
            subtype: None,
            channel: channel.into(),
            user: Some("U-1".into()),
            text: text.into(),
        }
    }

    #[test]
    fn authenticate_returns_slack_scopes() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("channels:history"));
        assert!(tok.scope.contains("files:read"));
    }

    #[test]
    fn initial_sync_emits_messages_in_chronological_order() {
        let pages = vec![SlackInitialSyncPage {
            channels: vec![SlackChannel {
                id: "C-A".into(),
                name: "general".into(),
                is_archived: false,
            }],
            // Slack returns newest-first.
            history_by_channel: vec![SlackHistoryResponse {
                ok: true,
                channel: "C-A".into(),
                messages: vec![
                    message("C-A", "1700000300.000000", "third"),
                    message("C-A", "1700000200.000000", "second"),
                    message("C-A", "1700000100.000000", "first"),
                ],
                has_more: false,
                response_metadata: SlackResponseMetadata::default(),
            }],
        }];
        let c = SlackConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        // Substrate emits chronologically — first event must be
        // the oldest `ts`.
        let first = res.events[0].document_id().as_str().to_string();
        assert!(
            first.ends_with("1700000100.000000"),
            "expected oldest first, got {first}",
        );
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn initial_sync_backfills_channel_when_message_omits_it() {
        let pages = vec![SlackInitialSyncPage {
            channels: vec![SlackChannel {
                id: "C-B".into(),
                name: "design".into(),
                is_archived: false,
            }],
            history_by_channel: vec![SlackHistoryResponse {
                ok: true,
                channel: "C-B".into(),
                messages: vec![SlackMessage {
                    ts: "1700001000.000000".into(),
                    channel: String::new(),
                    ..Default::default()
                }],
                has_more: false,
                response_metadata: SlackResponseMetadata::default(),
            }],
        }];
        let c = SlackConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            res.events[0].document_id().as_str(),
            "slack:C-B:1700001000.000000"
        );
    }

    #[test]
    fn incremental_sync_classifies_message_changed_as_updated() {
        let pages = vec![SlackHistoryResponse {
            ok: true,
            channel: "C-A".into(),
            messages: vec![SlackMessage {
                ts: "1700002000.000000".into(),
                message_type: "message".into(),
                subtype: Some("message_changed".into()),
                channel: "C-A".into(),
                user: Some("U-1".into()),
                text: "edited".into(),
            }],
            has_more: false,
            response_metadata: SlackResponseMetadata::default(),
        }];
        let c = SlackConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn incremental_sync_classifies_message_deleted_as_deleted() {
        let pages = vec![SlackHistoryResponse {
            ok: true,
            channel: "C-A".into(),
            messages: vec![SlackMessage {
                ts: "1700003000.000000".into(),
                message_type: "message".into(),
                subtype: Some("message_deleted".into()),
                channel: "C-A".into(),
                user: None,
                text: String::new(),
            }],
            has_more: false,
            response_metadata: SlackResponseMetadata::default(),
        }];
        let c = SlackConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn incremental_sync_pages_via_cursor() {
        let pages = vec![
            SlackHistoryResponse {
                ok: true,
                channel: "C-A".into(),
                messages: vec![message("C-A", "1700000100.000000", "p1")],
                has_more: true,
                response_metadata: SlackResponseMetadata::default(),
            },
            SlackHistoryResponse {
                ok: true,
                channel: "C-A".into(),
                messages: vec![message("C-A", "1700000200.000000", "p2")],
                has_more: false,
                response_metadata: SlackResponseMetadata::default(),
            },
        ];
        let c = SlackConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res1 = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res1.events.len(), 1);
        assert_eq!(res1.next_cursor.as_deref(), Some("page-2"));
        let mut state2 = state.clone();
        state2.cursor = res1.next_cursor.clone();
        let res2 = c.incremental_sync(&cfg(), &tok, &state2).unwrap();
        assert_eq!(res2.events.len(), 1);
        // Last page — cursor falls back to the watermark RFC3339
        // timestamp, not "page-3".
        let last = res2.next_cursor.as_deref().unwrap_or("");
        assert!(!last.starts_with("page-"), "got {last}");
    }

    #[test]
    fn subscribe_webhook_uses_slack_signing_secret() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/slack/webhook")
            .unwrap();
        assert_eq!(sub.connector, c.instance);
        assert!(sub.event_types.document_created);
        assert!(!sub.event_types.permission_changed);
    }

    #[test]
    fn webhook_url_verification_returns_empty_event_list() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "url_verification",
            "challenge": "abc123",
            "token": "verification-token",
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_url_verification_without_challenge_errors() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "url_verification",
        });
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_message_event_creates_document() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "event_callback",
            "event_time": 1700004000_i64,
            "event": {
                "type": "message",
                "channel": "C-A",
                "ts": "1700004000.000000",
                "user": "U-1",
                "text": "hello",
            }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "slack:C-A:1700004000.000000");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_message_changed_event_updates_document() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "subtype": "message_changed",
                "channel": "C-A",
                "ts": "1700005000.000000",
            }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_message_deleted_event_emits_deleted_with_original_ts() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "subtype": "message_deleted",
                "channel": "C-A",
                "ts": "1700006001.000000",
                "deleted_ts": "1700006000.000000",
            }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        match &evs[0] {
            ConnectorEvent::DocumentDeleted { document_id, .. } => {
                // The substrate keys deletes off the *original*
                // message ts, not the deletion-tombstone ts.
                assert_eq!(document_id.as_str(), "slack:C-A:1700006000.000000");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_file_shared_event_creates_file_document() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "file_shared",
                "file_id": "F-99",
            }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        match &evs[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "slack:file:F-99");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_channel_archive_emits_channel_deleted() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "channel_archive",
                "channel": "C-Z",
            }
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        match &evs[0] {
            ConnectorEvent::DocumentDeleted { document_id, .. } => {
                assert_eq!(document_id.as_str(), "slack:channel:C-Z");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_inner_type_errors() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "weird_event"}
        });
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_unknown_envelope_type_errors() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({"type": "block_actions"});
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_message_event_without_ts_errors() {
        let c = SlackConnector::new(ConnectorInstanceId::new_v4());
        let body = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "channel": "C-A",
            }
        });
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn document_id_helpers_pin_string_format() {
        assert_eq!(
            SlackConnector::document_id("C-1", "1700000000.000000").as_str(),
            "slack:C-1:1700000000.000000"
        );
        assert_eq!(
            SlackConnector::file_document_id("F-1").as_str(),
            "slack:file:F-1"
        );
        assert_eq!(
            SlackConnector::channel_document_id("C-1").as_str(),
            "slack:channel:C-1"
        );
    }

    #[test]
    fn slack_ts_to_datetime_parses_microsecond_suffix() {
        let dt = slack_ts_to_datetime("1700000000.123456").expect("parse");
        assert_eq!(dt.timestamp(), 1700000000);
    }

    #[test]
    fn slack_ts_to_datetime_handles_missing_suffix() {
        let dt = slack_ts_to_datetime("1700000000").expect("parse");
        assert_eq!(dt.timestamp(), 1700000000);
    }
}
