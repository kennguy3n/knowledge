//! Email connector — Gmail (Google Workspace) + Microsoft Graph (Outlook).
//!
//! Per `docs/DESIGN.md` §10.1 the substrate ingests business email as
//! observation evidence. This connector multiplexes the two
//! production providers behind a single [`Connector`] surface:
//!
//! * **Gmail** — `users.messages.list` for paged listing, push
//!   notifications via Cloud Pub/Sub for change events.
//! * **Microsoft Graph** — `/me/messages` (or `/users/{id}/messages`)
//!   with `$top` paging, change notifications via `/subscriptions`
//!   with `changeType: "created"`.
//!
//! Both providers ship subtly different cursors and webhook
//! envelopes; the [`EmailProvider`] enum dispatches per-provider
//! logic without forking the trait surface.
//!
//! Like the other connectors in this crate the module is fixture
//! driven — production HTTP transport, retries, and rate limits are
//! the responsibility of the Go gateway.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Which mail provider this connector is bound to. Persisted with
/// the connector config so the runtime can dispatch correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailProvider {
    /// Gmail / Google Workspace via the Gmail API.
    Gmail,
    /// Outlook / Microsoft 365 via Microsoft Graph.
    MicrosoftGraph,
}

impl EmailProvider {
    /// Stable string tag — used for source ids and metadata.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::MicrosoftGraph => "msgraph",
        }
    }
}

/// One Gmail message (subset returned by `users.messages.get`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailMessage {
    /// Stable Gmail message id (RFC822 message-id derivative).
    pub id: String,
    /// Thread id this message belongs to.
    #[serde(default, rename = "threadId")]
    pub thread_id: String,
    /// Internal Gmail timestamp (epoch ms, Gmail-side).
    #[serde(default, rename = "internalDate")]
    pub internal_date: String,
    /// History id for the mailbox at the time of this message —
    /// used by Gmail's push notifications to bound delta queries.
    #[serde(default, rename = "historyId")]
    pub history_id: String,
}

/// One page of Gmail `users.messages.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailMessagesListPage {
    /// Messages on this page.
    #[serde(default)]
    pub messages: Vec<GmailMessage>,
    /// Cursor for the next page; absent on the last page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
}

/// Microsoft Graph mail message (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphMessage {
    /// Graph-side message id.
    pub id: String,
    /// Created datetime (RFC3339).
    #[serde(default, rename = "createdDateTime")]
    pub created_date_time: Option<DateTime<Utc>>,
    /// Last-modified datetime (RFC3339).
    #[serde(default, rename = "lastModifiedDateTime")]
    pub last_modified_date_time: Option<DateTime<Utc>>,
    /// Conversation id (thread).
    #[serde(default, rename = "conversationId")]
    pub conversation_id: String,
}

/// One page of Microsoft Graph `/me/messages`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphMessagesPage {
    /// Messages returned by Graph.
    #[serde(default)]
    pub value: Vec<GraphMessage>,
    /// `@odata.nextLink` cursor — absent on the last page.
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    /// `@odata.deltaLink` cursor — used to seed the next
    /// `incremental_sync` once the initial walk completes.
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// Gmail Cloud Pub/Sub push notification body. Gmail wraps a JSON
/// blob inside a base64-encoded `data` field on the Pub/Sub
/// envelope; this connector accepts either the raw inner blob or a
/// minimal envelope that already carries the parsed `emailAddress`
/// + `historyId`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailPushNotification {
    /// Mailbox the change applies to.
    #[serde(default, rename = "emailAddress")]
    pub email_address: String,
    /// Gmail history id watermark.
    #[serde(default, rename = "historyId")]
    pub history_id: u64,
    /// Optional list of message ids that triggered the
    /// notification — when present the connector emits one event
    /// per id; otherwise the runtime is expected to issue a
    /// `history.list` follow-up using `history_id`.
    #[serde(default, rename = "messageIds")]
    pub message_ids: Vec<String>,
}

/// One Microsoft Graph change-notification entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphChangeNotification {
    /// `subscriptionId` echoed by Graph.
    #[serde(default, rename = "subscriptionId")]
    pub subscription_id: String,
    /// `clientState` shared secret echoed by Graph.
    #[serde(default, rename = "clientState")]
    pub client_state: String,
    /// `changeType` — `created`, `updated`, `deleted`.
    #[serde(default, rename = "changeType")]
    pub change_type: String,
    /// Resource path that changed (e.g. `/me/messages/AAMk...`).
    #[serde(default)]
    pub resource: String,
    /// Resource id parsed out of `resource` (Graph also surfaces it
    /// in `resourceData.id`, which is what production code reads).
    #[serde(default, rename = "resourceData")]
    pub resource_data: GraphResourceData,
    /// Tenant id the subscription belongs to.
    #[serde(default, rename = "tenantId")]
    pub tenant_id: String,
}

/// Microsoft Graph `resourceData` envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphResourceData {
    /// Graph-side message id.
    #[serde(default)]
    pub id: String,
}

/// Microsoft Graph webhook batch envelope — one HTTP POST may
/// carry multiple notifications.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphChangeNotificationBatch {
    /// Notifications in the batch.
    #[serde(default)]
    pub value: Vec<GraphChangeNotification>,
    /// Validation token (only present on the initial subscription
    /// validation handshake; mutually exclusive with `value`).
    #[serde(default, rename = "validationToken")]
    pub validation_token: Option<String>,
}

/// Email connector. Pure fixture-driven so the substrate can
/// unit-test it without hitting Google or Microsoft.
#[derive(Debug, Clone)]
pub struct EmailConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// Provider this connector is bound to.
    pub provider: EmailProvider,
    /// Gmail initial-sync fixture pages — paged via
    /// `nextPageToken`.
    pub gmail_initial_pages: Vec<GmailMessagesListPage>,
    /// Gmail incremental-sync fixture pages.
    pub gmail_incremental_pages: Vec<GmailMessagesListPage>,
    /// Graph initial-sync fixture pages.
    pub graph_initial_pages: Vec<GraphMessagesPage>,
    /// Graph incremental-sync fixture pages.
    pub graph_incremental_pages: Vec<GraphMessagesPage>,
}

impl EmailConnector {
    /// Construct a fresh connector bound to `provider`.
    pub fn new(instance: ConnectorInstanceId, provider: EmailProvider) -> Self {
        Self {
            instance,
            provider,
            gmail_initial_pages: Vec::new(),
            gmail_incremental_pages: Vec::new(),
            graph_initial_pages: Vec::new(),
            graph_incremental_pages: Vec::new(),
        }
    }

    /// Override Gmail initial-sync pages.
    pub fn with_gmail_initial_pages(mut self, pages: Vec<GmailMessagesListPage>) -> Self {
        self.gmail_initial_pages = pages;
        self
    }

    /// Override Gmail incremental-sync pages.
    pub fn with_gmail_incremental_pages(mut self, pages: Vec<GmailMessagesListPage>) -> Self {
        self.gmail_incremental_pages = pages;
        self
    }

    /// Override Graph initial-sync pages.
    pub fn with_graph_initial_pages(mut self, pages: Vec<GraphMessagesPage>) -> Self {
        self.graph_initial_pages = pages;
        self
    }

    /// Override Graph incremental-sync pages.
    pub fn with_graph_incremental_pages(mut self, pages: Vec<GraphMessagesPage>) -> Self {
        self.graph_incremental_pages = pages;
        self
    }

    fn page_index(cursor: Option<&str>) -> usize {
        cursor
            .and_then(|c| c.strip_prefix("page-"))
            .and_then(|n| n.parse::<usize>().ok())
            .map_or(0, |n| n.saturating_sub(1))
    }

    /// Compose the substrate-side document id for an email message.
    /// Format is `"<provider>:msg:<id>"` so audit logs can attribute
    /// observations back to the source provider.
    pub fn document_id(provider: EmailProvider, message_id: &str) -> SourceDocumentId {
        SourceDocumentId::new(format!("{}:msg:{}", provider.as_str(), message_id))
    }

    fn parse_gmail_internal_date(s: &str) -> DateTime<Utc> {
        s.parse::<i64>()
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .unwrap_or_else(Utc::now)
    }

    fn graph_message_id_from_resource(resource: &str, fallback: &str) -> String {
        if !fallback.is_empty() {
            return fallback.to_string();
        }
        // Graph resource paths are `/me/messages/{id}` or
        // `/users/{user-id}/messages/{id}`.
        resource
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(resource)
            .to_string()
    }
}

impl Connector for EmailConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        let scope = match self.provider {
            EmailProvider::Gmail => {
                "https://www.googleapis.com/auth/gmail.readonly \
                 https://www.googleapis.com/auth/gmail.metadata"
            }
            EmailProvider::MicrosoftGraph => "Mail.Read offline_access",
        };
        Ok(OAuth2Token::new(
            format!("{}-access-token", self.provider.as_str()),
            format!("{}-refresh-token", self.provider.as_str()),
            Utc::now() + Duration::hours(1),
            scope,
        ))
    }

    fn initial_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
    ) -> Result<SyncRunResult> {
        match self.provider {
            EmailProvider::Gmail => {
                let mut events: Vec<ConnectorEvent> = Vec::new();
                let mut watermark: Option<DateTime<Utc>> = None;
                for page in &self.gmail_initial_pages {
                    for msg in &page.messages {
                        let occurred_at = Self::parse_gmail_internal_date(&msg.internal_date);
                        events.push(ConnectorEvent::DocumentCreated {
                            document_id: Self::document_id(self.provider, &msg.id),
                            occurred_at,
                        });
                        watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
                    }
                }
                Ok(SyncRunResult {
                    events,
                    next_cursor: watermark.map(|t| t.to_rfc3339()),
                })
            }
            EmailProvider::MicrosoftGraph => {
                let mut events: Vec<ConnectorEvent> = Vec::new();
                let mut watermark: Option<DateTime<Utc>> = None;
                let mut delta_link: Option<String> = None;
                for page in &self.graph_initial_pages {
                    for msg in &page.value {
                        let occurred_at = msg
                            .created_date_time
                            .or(msg.last_modified_date_time)
                            .unwrap_or_else(Utc::now);
                        events.push(ConnectorEvent::DocumentCreated {
                            document_id: Self::document_id(self.provider, &msg.id),
                            occurred_at,
                        });
                        watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
                    }
                    if page.delta_link.is_some() {
                        delta_link.clone_from(&page.delta_link);
                    }
                }
                let next_cursor = delta_link.or_else(|| watermark.map(|t| t.to_rfc3339()));
                Ok(SyncRunResult {
                    events,
                    next_cursor,
                })
            }
        }
    }

    fn incremental_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let idx = Self::page_index(state.cursor.as_deref());
        match self.provider {
            EmailProvider::Gmail => {
                let page = self
                    .gmail_incremental_pages
                    .get(idx)
                    .cloned()
                    .unwrap_or_default();
                let mut events: Vec<ConnectorEvent> = Vec::new();
                let mut watermark: Option<DateTime<Utc>> = None;
                for msg in &page.messages {
                    let occurred_at = Self::parse_gmail_internal_date(&msg.internal_date);
                    events.push(ConnectorEvent::DocumentCreated {
                        document_id: Self::document_id(self.provider, &msg.id),
                        occurred_at,
                    });
                    watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
                }
                let next_cursor = if idx + 1 < self.gmail_incremental_pages.len() {
                    Some(format!("page-{}", idx + 2))
                } else {
                    watermark.map(|t| t.to_rfc3339())
                };
                Ok(SyncRunResult {
                    events,
                    next_cursor,
                })
            }
            EmailProvider::MicrosoftGraph => {
                let page = self
                    .graph_incremental_pages
                    .get(idx)
                    .cloned()
                    .unwrap_or_default();
                let mut events: Vec<ConnectorEvent> = Vec::new();
                let mut watermark: Option<DateTime<Utc>> = None;
                for msg in &page.value {
                    let occurred_at = msg
                        .last_modified_date_time
                        .or(msg.created_date_time)
                        .unwrap_or_else(Utc::now);
                    events.push(ConnectorEvent::DocumentUpdated {
                        document_id: Self::document_id(self.provider, &msg.id),
                        occurred_at,
                    });
                    watermark = Some(watermark.map_or(occurred_at, |w| w.max(occurred_at)));
                }
                let next_cursor = if idx + 1 < self.graph_incremental_pages.len() {
                    Some(format!("page-{}", idx + 2))
                } else {
                    page.delta_link
                        .clone()
                        .or_else(|| watermark.map(|t| t.to_rfc3339()))
                };
                Ok(SyncRunResult {
                    events,
                    next_cursor,
                })
            }
        }
    }

    fn subscribe_webhook(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let secret = match self.provider {
            EmailProvider::Gmail => "gmail-pubsub-secret",
            EmailProvider::MicrosoftGraph => "msgraph-client-state",
        };
        let expires_at = match self.provider {
            // Gmail's `users.watch` lasts 7 days.
            EmailProvider::Gmail => Some(Utc::now() + Duration::days(7)),
            // Graph mail subscriptions max out at 4230 minutes
            // (~70.5h); the substrate refreshes well before.
            EmailProvider::MicrosoftGraph => Some(Utc::now() + Duration::hours(70)),
        };
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes {
                document_created: true,
                document_updated: matches!(self.provider, EmailProvider::MicrosoftGraph),
                document_deleted: matches!(self.provider, EmailProvider::MicrosoftGraph),
                permission_changed: false,
            },
            expires_at,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        match self.provider {
            EmailProvider::Gmail => {
                let push: GmailPushNotification = serde_json::from_slice(body)?;
                if push.email_address.is_empty() && push.history_id == 0 {
                    return Err(ConnectorError::Webhook(
                        "Gmail push notification missing emailAddress / historyId".into(),
                    ));
                }
                if push.message_ids.is_empty() {
                    // Gmail's normal flow: the push notification
                    // only carries `historyId` and the runtime is
                    // expected to follow up with `history.list`.
                    // The connector returns no events here so the
                    // runtime can fan-out the history call.
                    return Ok(Vec::new());
                }
                let occurred_at = Utc::now();
                let events = push
                    .message_ids
                    .into_iter()
                    .map(|id| ConnectorEvent::DocumentCreated {
                        document_id: Self::document_id(EmailProvider::Gmail, &id),
                        occurred_at,
                    })
                    .collect();
                Ok(events)
            }
            EmailProvider::MicrosoftGraph => {
                let batch: GraphChangeNotificationBatch = serde_json::from_slice(body)?;
                // Graph subscription validation: when the validation
                // token is present, no events should be emitted.
                if batch.validation_token.is_some() && batch.value.is_empty() {
                    return Ok(Vec::new());
                }
                if batch.value.is_empty() {
                    return Err(ConnectorError::Webhook(
                        "Graph change notification batch is empty".into(),
                    ));
                }
                let mut events: Vec<ConnectorEvent> = Vec::with_capacity(batch.value.len());
                for note in batch.value {
                    let id = Self::graph_message_id_from_resource(
                        &note.resource,
                        &note.resource_data.id,
                    );
                    if id.is_empty() {
                        return Err(ConnectorError::Webhook(
                            "Graph notification missing resource id".into(),
                        ));
                    }
                    let occurred_at = Utc::now();
                    let document_id = Self::document_id(EmailProvider::MicrosoftGraph, &id);
                    let ev = match note.change_type.as_str() {
                        "created" => ConnectorEvent::DocumentCreated {
                            document_id,
                            occurred_at,
                        },
                        "updated" => ConnectorEvent::DocumentUpdated {
                            document_id,
                            occurred_at,
                        },
                        "deleted" => ConnectorEvent::DocumentDeleted {
                            document_id,
                            occurred_at,
                        },
                        other => {
                            return Err(ConnectorError::Webhook(format!(
                                "unknown Graph changeType: {other}"
                            )));
                        }
                    };
                    events.push(ev);
                }
                Ok(events)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use connector_framework::{AuthKind, ConnectorKind};
    use evidence_store::ScopeId;

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Email, AuthKind::OAuth2, ScopeId::new_v4())
    }

    fn gmail_message(id: &str, internal_ms: i64) -> GmailMessage {
        GmailMessage {
            id: id.into(),
            thread_id: format!("t-{id}"),
            internal_date: internal_ms.to_string(),
            history_id: "1".into(),
        }
    }

    fn graph_message(id: &str, created: DateTime<Utc>, updated: DateTime<Utc>) -> GraphMessage {
        GraphMessage {
            id: id.into(),
            created_date_time: Some(created),
            last_modified_date_time: Some(updated),
            conversation_id: format!("c-{id}"),
        }
    }

    #[test]
    fn gmail_authenticate_uses_gmail_scopes() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::Gmail);
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("gmail.readonly"));
    }

    #[test]
    fn graph_authenticate_uses_mail_read_scope() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("Mail.Read"));
    }

    #[test]
    fn gmail_initial_sync_emits_created_events() {
        let pages = vec![GmailMessagesListPage {
            messages: vec![
                gmail_message("g1", 1_700_000_000_000),
                gmail_message("g2", 1_700_000_500_000),
            ],
            next_page_token: None,
        }];
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::Gmail)
            .with_gmail_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.events[0].document_id().as_str(), "gmail:msg:g1");
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn graph_initial_sync_uses_delta_link_when_present() {
        let now = Utc::now();
        let pages = vec![GraphMessagesPage {
            value: vec![graph_message("m1", now, now)],
            next_link: None,
            delta_link: Some("https://graph.microsoft.com/.../delta?$deltaToken=abc".into()),
        }];
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph)
            .with_graph_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "msgraph:msg:m1");
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("https://graph.microsoft.com/.../delta?$deltaToken=abc")
        );
    }

    #[test]
    fn gmail_incremental_sync_pages_via_cursor() {
        let pages = vec![
            GmailMessagesListPage {
                messages: vec![gmail_message("g1", 1_700_000_000_000)],
                next_page_token: Some("tok-2".into()),
            },
            GmailMessagesListPage {
                messages: vec![gmail_message("g2", 1_700_000_500_000)],
                next_page_token: None,
            },
        ];
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::Gmail)
            .with_gmail_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        let res1 = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res1.events.len(), 1);
        assert_eq!(res1.next_cursor.as_deref(), Some("page-2"));
        state.cursor = res1.next_cursor.clone();
        let res2 = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res2.events.len(), 1);
        assert!(!res2
            .next_cursor
            .as_deref()
            .unwrap_or_default()
            .starts_with("page-"));
    }

    #[test]
    fn graph_incremental_sync_emits_updated_events() {
        let now = Utc::now();
        let pages = vec![GraphMessagesPage {
            value: vec![graph_message("m1", now, now)],
            next_link: None,
            delta_link: Some("https://graph.microsoft.com/.../delta?$deltaToken=def".into()),
        }];
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph)
            .with_graph_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("https://graph.microsoft.com/.../delta?$deltaToken=def")
        );
    }

    #[test]
    fn gmail_subscribe_webhook_emits_pubsub_secret() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::Gmail);
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/email/gmail")
            .unwrap();
        assert!(sub.expires_at.is_some());
        assert!(sub.event_types.document_created);
        // Gmail push notifications cannot distinguish updates vs
        // creates without a follow-up `history.list`; the connector
        // therefore claims only `created`.
        assert!(!sub.event_types.document_updated);
    }

    #[test]
    fn graph_subscribe_webhook_supports_full_event_set() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/email/msgraph")
            .unwrap();
        assert!(sub.event_types.document_created);
        assert!(sub.event_types.document_updated);
        assert!(sub.event_types.document_deleted);
    }

    #[test]
    fn gmail_webhook_with_message_ids_emits_per_message_events() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::Gmail);
        let body = serde_json::json!({
            "emailAddress": "user@example.com",
            "historyId": 12345,
            "messageIds": ["g1", "g2", "g3"],
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].document_id().as_str(), "gmail:msg:g1");
        assert_eq!(evs[2].document_id().as_str(), "gmail:msg:g3");
    }

    #[test]
    fn gmail_webhook_without_message_ids_returns_empty_event_list() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::Gmail);
        let body = serde_json::json!({
            "emailAddress": "user@example.com",
            "historyId": 12345,
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        // No message ids = the runtime must follow up with
        // `history.list`. Connector returns no events.
        assert!(evs.is_empty());
    }

    #[test]
    fn gmail_webhook_without_email_or_history_errors() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::Gmail);
        let body = serde_json::json!({});
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn graph_validation_token_short_circuits_to_empty_event_list() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let body = serde_json::json!({
            "validationToken": "x",
            "value": [],
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn graph_change_notification_emits_created_event() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let body = serde_json::json!({
            "value": [{
                "subscriptionId": "sub-1",
                "clientState": "shared",
                "changeType": "created",
                "resource": "/me/messages/AAMkABCDE",
                "resourceData": {"id": "AAMkABCDE"},
                "tenantId": "tenant-1",
            }]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "msgraph:msg:AAMkABCDE");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn graph_change_notification_falls_back_to_resource_path_when_data_missing() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let body = serde_json::json!({
            "value": [{
                "changeType": "updated",
                "resource": "/users/abc/messages/MSG-99",
            }]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        match &evs[0] {
            ConnectorEvent::DocumentUpdated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "msgraph:msg:MSG-99");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn graph_change_notification_emits_deleted_event() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let body = serde_json::json!({
            "value": [{
                "changeType": "deleted",
                "resource": "/me/messages/MSG-1",
                "resourceData": {"id": "MSG-1"},
            }]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn graph_unknown_change_type_errors() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let body = serde_json::json!({
            "value": [{
                "changeType": "weird",
                "resource": "/me/messages/MSG-1",
                "resourceData": {"id": "MSG-1"},
            }]
        });
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn graph_empty_batch_errors() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let body = serde_json::json!({"value": []});
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn graph_notification_without_resource_id_errors() {
        let c = EmailConnector::new(ConnectorInstanceId::new_v4(), EmailProvider::MicrosoftGraph);
        let body = serde_json::json!({
            "value": [{
                "changeType": "created",
                "resource": "",
                "resourceData": {"id": ""},
            }]
        });
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn provider_string_tags_are_pinned() {
        assert_eq!(EmailProvider::Gmail.as_str(), "gmail");
        assert_eq!(EmailProvider::MicrosoftGraph.as_str(), "msgraph");
    }

    #[test]
    fn document_id_format_is_pinned() {
        assert_eq!(
            EmailConnector::document_id(EmailProvider::Gmail, "g1").as_str(),
            "gmail:msg:g1"
        );
        assert_eq!(
            EmailConnector::document_id(EmailProvider::MicrosoftGraph, "m1").as_str(),
            "msgraph:msg:m1"
        );
    }
}
