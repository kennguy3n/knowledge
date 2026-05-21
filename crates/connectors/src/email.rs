//! Email connector — multiplexes Gmail (Google Workspace) and
//! Microsoft Graph (Outlook / Microsoft 365) behind a single
//! [`Connector`] surface.
//!
//! Production wiring contract (mirrors the other connectors in this
//! crate): the constructor takes an `Arc<dyn HttpTransport>` and an
//! `Arc<dyn OAuth2CodeExchange>`. Production binds the
//! `BlockingHttpTransport` + `OAuth2Client` pair; tests bind
//! `MockHttpTransport` + a fixed-token exchange.
//!
//! Provider dispatch is driven by `auth_config_json.provider`
//! (`"gmail"` or `"msgraph"`) on the connector config — there is
//! exactly one [`EmailConnector`] per instance bound to exactly one
//! provider for its lifetime. The provider determines which API
//! surface the sync, watch, and webhook handlers exercise:
//!
//! * **Gmail** — REST endpoints under
//!   `https://gmail.googleapis.com/gmail/v1`:
//!   * `users.messages.list` (paged via `nextPageToken`),
//!   * `users.getProfile` (anchors the historyId watermark after
//!     the initial walk so incremental polls can resume),
//!   * `users.history.list?startHistoryId=<id>` (incremental),
//!   * `users.watch` (registers a Cloud Pub/Sub topic for change
//!     notifications),
//!   * Cloud Pub/Sub push notifications (parsed in
//!     `handle_webhook_event`).
//! * **Microsoft Graph** — REST endpoints under
//!   `https://graph.microsoft.com/v1.0`:
//!   * `/me/messages/delta` (paged via `@odata.nextLink`, final
//!     cursor surfaced as `@odata.deltaLink`),
//!   * `/subscriptions` (POST to install a change-notification
//!     subscription targeting the substrate callback URL),
//!   * Graph delivers webhook batches as
//!     `changeNotificationCollection` envelopes (parsed in
//!     `handle_webhook_event`).
//!
//! Both providers carry the same overall lifecycle: token exchange
//! via [`OAuth2CodeExchange::exchange_code`] in `authenticate`, an
//! initial walk in `initial_sync` that anchors the watermark, an
//! incremental walk in `incremental_sync` that consumes the
//! watermark, a watch subscription in `subscribe_webhook`, and
//! parsing of inbound push notifications in
//! `handle_webhook_event`.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, encode_form, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result,
    SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Gmail REST base URL. Override via
/// `auth_config_json.gmail_api_base_url` for sandboxes.
pub const GMAIL_DEFAULT_API_BASE_URL: &str = "https://gmail.googleapis.com";

/// Default Graph REST base URL. Override via
/// `auth_config_json.graph_api_base_url` for sovereign clouds.
pub const GRAPH_DEFAULT_API_BASE_URL: &str = "https://graph.microsoft.com";

/// Default Graph API version segment. Override via
/// `auth_config_json.graph_api_version` (`"/v1.0"` or `"/beta"`).
pub const GRAPH_DEFAULT_API_VERSION: &str = "/v1.0";

/// Gmail's `users.watch` subscription TTL is bounded at 7 days by
/// Google; we set ourselves one minute under the limit to leave room
/// for clock skew between the substrate scheduler and Google.
pub const DEFAULT_GMAIL_WATCH_TTL_MINUTES: i64 = 7 * 24 * 60 - 1;

/// Graph mail subscription TTL — Graph caps at 4230 minutes
/// (≈70.5h) for inbox notifications; we sit one minute under.
pub const DEFAULT_GRAPH_SUBSCRIPTION_TTL_MINUTES: i64 = 4_229;

/// Safety ceiling on number of pages a single sync will walk —
/// catches mis-shaped server responses that return a non-empty page
/// without ever clearing the cursor.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Page size we request from Gmail / Graph list endpoints.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Which mail provider this connector instance is bound to.
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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::MicrosoftGraph => "msgraph",
        }
    }

    fn from_config(config: &ConnectorConfig) -> Result<Self> {
        let raw = config
            .auth_config_json
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "email: auth_config_json.provider is required \
                     (\"gmail\" or \"msgraph\")"
                        .into(),
                )
            })?;
        match raw {
            "gmail" | "google" => Ok(Self::Gmail),
            "msgraph" | "microsoft_graph" | "ms_graph" | "outlook" => Ok(Self::MicrosoftGraph),
            other => Err(ConnectorError::Auth(format!(
                "email: unknown provider {other:?} (expected \"gmail\" or \"msgraph\")"
            ))),
        }
    }
}

// =====================================================================
// Gmail wire types
// =====================================================================

/// One reference to a Gmail message — what `users.messages.list`
/// returns per entry. `internalDate` is only populated when callers
/// request `format=metadata` / `format=full` (not the default for
/// list); we accept it optionally to support both shapes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailMessageRef {
    /// Stable Gmail message id.
    pub id: String,
    /// Thread id this message belongs to.
    #[serde(default, rename = "threadId")]
    pub thread_id: String,
    /// Epoch-ms wall-clock from the originating MTA (Gmail-side).
    #[serde(default, rename = "internalDate")]
    pub internal_date: Option<String>,
    /// History id at the time of the message — used by Gmail's push
    /// notifications to bound delta queries.
    #[serde(default, rename = "historyId")]
    pub history_id: Option<String>,
}

/// One page of Gmail `users.messages.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailMessagesListPage {
    /// Messages on this page.
    #[serde(default)]
    pub messages: Vec<GmailMessageRef>,
    /// Cursor for the next page; absent on the last page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    /// Server-side estimate of total result count.
    #[serde(default, rename = "resultSizeEstimate")]
    pub result_size_estimate: Option<u64>,
}

/// `users.getProfile` response (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailProfile {
    /// Mailbox address.
    #[serde(default, rename = "emailAddress")]
    pub email_address: String,
    /// Current history id (monotonic counter — substrate uses this
    /// as the watermark for incremental history.list polls).
    #[serde(default, rename = "historyId")]
    pub history_id: Option<String>,
    /// Total message count.
    #[serde(default, rename = "messagesTotal")]
    pub messages_total: Option<u64>,
}

/// One inner `messageAdded` / `messageDeleted` envelope inside a
/// history entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailHistoryMessageEnvelope {
    /// Message reference (`{id, threadId}`).
    #[serde(default)]
    pub message: GmailMessageRef,
}

/// One entry from `users.history.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailHistoryEntry {
    /// History entry id (monotonic counter).
    pub id: String,
    /// Messages newly visible to the substrate at this history step
    /// — Gmail's `historyTypes=messageAdded` view.
    #[serde(default, rename = "messagesAdded")]
    pub messages_added: Vec<GmailHistoryMessageEnvelope>,
    /// Messages deleted at this history step.
    #[serde(default, rename = "messagesDeleted")]
    pub messages_deleted: Vec<GmailHistoryMessageEnvelope>,
}

/// One page of `users.history.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailHistoryPage {
    /// History entries.
    #[serde(default)]
    pub history: Vec<GmailHistoryEntry>,
    /// Forward cursor; absent on the last page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    /// Server's high-water history id at the time of this page.
    /// Used to seed the next `incremental_sync`.
    #[serde(default, rename = "historyId")]
    pub history_id: Option<String>,
}

/// `users.watch` create response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailWatchResponse {
    /// History id at the time of the watch registration.
    #[serde(default, rename = "historyId")]
    pub history_id: Option<String>,
    /// Expiration time — Gmail returns this as an epoch-ms string.
    #[serde(default)]
    pub expiration: Option<String>,
}

/// Gmail Cloud Pub/Sub push notification body. Gmail's normal flow
/// delivers only `{emailAddress, historyId}` and the runtime is
/// expected to follow up with `history.list`; some integrations
/// pre-resolve and include a `messageIds` array so the connector
/// can emit per-message events directly.
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

// =====================================================================
// Microsoft Graph wire types
// =====================================================================

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
    /// Graph delta marks deletes with `@removed`.
    #[serde(default, rename = "@removed")]
    pub removed: Option<GraphRemoved>,
}

/// Graph "removed" envelope returned by delta queries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphRemoved {
    /// `reason` (`"changed"` for soft-deleted, `"deleted"` for hard).
    #[serde(default)]
    pub reason: String,
}

/// One page of Microsoft Graph `/me/messages/delta`.
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

/// `/subscriptions` create response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphSubscriptionResponse {
    /// Subscription id Graph assigned (needed for revoke).
    #[serde(default)]
    pub id: Option<String>,
    /// Echoed expiration (RFC-3339) — Graph caps mail subs at
    /// ~70.5 h.
    #[serde(default, rename = "expirationDateTime")]
    pub expiration_date_time: Option<DateTime<Utc>>,
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

// =====================================================================
// Connector
// =====================================================================

/// Email connector.
///
/// Holds the wired [`HttpTransport`] + [`OAuth2CodeExchange`] used to
/// drive every Gmail / Graph REST call (token exchange, pagination,
/// watch / subscription create).
pub struct EmailConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    gmail_api_base_url: String,
    graph_api_base_url: String,
    graph_api_version: String,
}

impl std::fmt::Debug for EmailConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailConnector")
            .field("instance", &self.instance)
            .field("gmail_api_base_url", &self.gmail_api_base_url)
            .field("graph_api_base_url", &self.graph_api_base_url)
            .field("graph_api_version", &self.graph_api_version)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl EmailConnector {
    /// Construct an Email connector.
    ///
    /// `transport` carries every REST call; `oauth` drives the
    /// `authorization_code` exchange against the provider's token
    /// endpoint (Google / Microsoft).
    pub fn new(
        instance: ConnectorInstanceId,
        transport: Arc<dyn HttpTransport>,
        oauth: Arc<dyn OAuth2CodeExchange>,
    ) -> Self {
        Self {
            instance,
            transport,
            oauth,
            gmail_api_base_url: GMAIL_DEFAULT_API_BASE_URL.to_string(),
            graph_api_base_url: GRAPH_DEFAULT_API_BASE_URL.to_string(),
            graph_api_version: GRAPH_DEFAULT_API_VERSION.to_string(),
        }
    }

    /// Override the Gmail REST base URL.
    #[must_use]
    pub fn with_gmail_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.gmail_api_base_url = url.into();
        self
    }

    /// Override the Graph REST base URL.
    #[must_use]
    pub fn with_graph_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.graph_api_base_url = url.into();
        self
    }

    /// Override the Graph API version segment.
    #[must_use]
    pub fn with_graph_api_version(mut self, version: impl Into<String>) -> Self {
        self.graph_api_version = version.into();
        self
    }

    fn resolved_gmail_base(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("gmail_api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.gmail_api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    fn resolved_graph_base(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("graph_api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.graph_api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    fn resolved_graph_version(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("graph_api_version")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.graph_api_version.clone(),
                std::string::ToString::to_string,
            )
    }

    /// Where the substrate Graph mailbox lives. Defaults to
    /// `/me/messages/delta` (current user); override via
    /// `auth_config_json.graph_messages_path` to target a different
    /// mailbox (e.g. `/users/{id}/messages/delta` for an app-scoped
    /// tenant install or `/me/mailFolders/Inbox/messages/delta` to
    /// scope to the inbox).
    fn resolved_graph_messages_path(config: &ConnectorConfig) -> &str {
        config
            .auth_config_json
            .get("graph_messages_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("/me/messages/delta")
    }

    /// Where the Graph subscription's `resource` points. Defaults
    /// to `/me/messages`. Override via
    /// `auth_config_json.graph_subscription_resource`.
    fn resolved_graph_subscription_resource(config: &ConnectorConfig) -> &str {
        config
            .auth_config_json
            .get("graph_subscription_resource")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("/me/messages")
    }

    /// Where the substrate Gmail mailbox lives. Defaults to
    /// `users/me`; override via `auth_config_json.gmail_user_id` to
    /// target a different user (`users/{user-id}`).
    fn resolved_gmail_user_path(config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("gmail_user_id")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "users/me".to_string(), |uid| format!("users/{uid}"))
    }

    /// Cloud Pub/Sub topic Gmail's `users.watch` publishes change
    /// notifications to. Required when subscribing on the Gmail
    /// path. Read from `auth_config_json.gmail_topic_name`.
    fn resolved_gmail_topic(config: &ConnectorConfig) -> Result<&str> {
        config
            .auth_config_json
            .get("gmail_topic_name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "gmail subscribe: auth_config_json.gmail_topic_name is required \
                     (Cloud Pub/Sub topic name, e.g. \"projects/my-proj/topics/gmail\")"
                        .into(),
                )
            })
    }

    /// Compose the substrate-side document id for an email message.
    /// Format is `"<provider>:msg:<id>"` so audit logs can attribute
    /// observations back to the source provider.
    #[must_use]
    pub fn document_id(provider: EmailProvider, message_id: &str) -> SourceDocumentId {
        SourceDocumentId::new(format!("{}:msg:{}", provider.as_str(), message_id))
    }

    fn parse_gmail_internal_date(s: &str) -> DateTime<Utc> {
        s.parse::<i64>()
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .unwrap_or_else(Utc::now)
    }

    fn parse_gmail_expiration(raw: Option<&str>) -> Option<DateTime<Utc>> {
        raw.and_then(|s| s.parse::<i64>().ok())
            .and_then(DateTime::<Utc>::from_timestamp_millis)
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

    // -----------------------------------------------------------------
    // Gmail pagination helpers
    // -----------------------------------------------------------------

    /// Walk every page of `users.messages.list` until either
    /// `nextPageToken` is absent, an empty page lands mid-stream
    /// (defence against mis-shaped servers), or [`MAX_LIST_PAGES`]
    /// is hit. Returns the merged message refs.
    fn paginate_gmail_messages(
        &self,
        base: &str,
        user_path: &str,
        token: &OAuth2Token,
    ) -> Result<Vec<GmailMessageRef>> {
        let mut messages = Vec::<GmailMessageRef>::new();
        let mut next: Option<String> = None;
        let mut prev_token: Option<String> = None;
        let page_size = DEFAULT_PAGE_SIZE.to_string();
        for _ in 0..MAX_LIST_PAGES {
            let mut params: Vec<(&str, &str)> = Vec::with_capacity(2);
            params.push(("maxResults", page_size.as_str()));
            if let Some(t) = &next {
                params.push(("pageToken", t.as_str()));
            }
            let qs = encode_form(&params);
            let url = format!("{base}/gmail/v1/{user_path}/messages?{qs}");
            let page: GmailMessagesListPage =
                bearer_get_json(&self.transport, "gmail", "/messages", &url, token, &[])?;
            let returned = page.messages.len();
            messages.extend(page.messages);
            let Some(next_token) = page.next_page_token else {
                return Ok(messages);
            };
            // Loop guard — a mis-shaped server that echoes the same
            // pageToken would otherwise spin forever.
            if prev_token.as_deref() == Some(next_token.as_str()) {
                return Ok(messages);
            }
            // Empty page mid-stream → end-of-list defensively.
            if returned == 0 {
                return Ok(messages);
            }
            prev_token = Some(next_token.clone());
            next = Some(next_token);
        }
        Err(ConnectorError::Sync(format!(
            "gmail users.messages.list exceeded {MAX_LIST_PAGES} pages \
             without exhausting cursor"
        )))
    }

    /// Fetch the mailbox profile so the substrate can seed
    /// `historyId` as the incremental watermark.
    fn fetch_gmail_profile(
        &self,
        base: &str,
        user_path: &str,
        token: &OAuth2Token,
    ) -> Result<GmailProfile> {
        let url = format!("{base}/gmail/v1/{user_path}/profile");
        bearer_get_json(&self.transport, "gmail", "/profile", &url, token, &[])
    }

    /// Walk every page of `users.history.list` until either
    /// `nextPageToken` is absent, an empty page lands mid-stream,
    /// or [`MAX_LIST_PAGES`] is hit. Returns the merged history
    /// entries + the final `historyId` watermark.
    fn paginate_gmail_history(
        &self,
        base: &str,
        user_path: &str,
        token: &OAuth2Token,
        start_history_id: &str,
    ) -> Result<(Vec<GmailHistoryEntry>, Option<String>)> {
        let mut entries = Vec::<GmailHistoryEntry>::new();
        let mut next: Option<String> = None;
        let mut prev_token: Option<String> = None;
        let mut latest_history_id: Option<String> = None;
        let page_size = DEFAULT_PAGE_SIZE.to_string();
        for _ in 0..MAX_LIST_PAGES {
            let mut params: Vec<(&str, &str)> = Vec::with_capacity(3);
            params.push(("startHistoryId", start_history_id));
            params.push(("maxResults", page_size.as_str()));
            if let Some(t) = &next {
                params.push(("pageToken", t.as_str()));
            }
            let qs = encode_form(&params);
            let url = format!("{base}/gmail/v1/{user_path}/history?{qs}");
            let page: GmailHistoryPage =
                bearer_get_json(&self.transport, "gmail", "/history", &url, token, &[])?;
            let returned = page.history.len();
            if let Some(h) = page.history_id.clone() {
                latest_history_id = Some(h);
            }
            entries.extend(page.history);
            let Some(next_token) = page.next_page_token else {
                return Ok((entries, latest_history_id));
            };
            if prev_token.as_deref() == Some(next_token.as_str()) {
                return Ok((entries, latest_history_id));
            }
            if returned == 0 {
                return Ok((entries, latest_history_id));
            }
            prev_token = Some(next_token.clone());
            next = Some(next_token);
        }
        Err(ConnectorError::Sync(format!(
            "gmail users.history.list exceeded {MAX_LIST_PAGES} pages \
             without exhausting cursor"
        )))
    }

    // -----------------------------------------------------------------
    // Graph pagination helpers
    // -----------------------------------------------------------------

    /// Walk every `@odata.nextLink` page until either the link is
    /// absent, the server returns an empty page with no link, or
    /// [`MAX_LIST_PAGES`] is hit. Returns the merged messages +
    /// the final `@odata.deltaLink`.
    fn paginate_graph_delta(
        &self,
        first_url: &str,
        token: &OAuth2Token,
    ) -> Result<(Vec<GraphMessage>, Option<String>)> {
        let mut items = Vec::<GraphMessage>::new();
        let mut url = first_url.to_string();
        let mut prev_url: Option<String> = None;
        let mut delta_link: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let resp: GraphMessagesPage = bearer_get_json(
                &self.transport,
                "msgraph",
                "/messages/delta",
                &url,
                token,
                &[],
            )?;
            let returned = resp.value.len();
            items.extend(resp.value);
            if resp.delta_link.is_some() {
                delta_link = resp.delta_link;
            }
            let Some(next) = resp.next_link else {
                return Ok((items, delta_link));
            };
            if prev_url.as_deref() == Some(next.as_str()) {
                return Ok((items, delta_link));
            }
            if returned == 0 {
                return Ok((items, delta_link));
            }
            prev_url = Some(next.clone());
            url = next;
        }
        Err(ConnectorError::Sync(format!(
            "msgraph /me/messages/delta exceeded {MAX_LIST_PAGES} pages \
             without exhausting cursor"
        )))
    }
}

fn graph_message_to_event(msg: &GraphMessage, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = msg
        .last_modified_date_time
        .or(msg.created_date_time)
        .unwrap_or_else(Utc::now);
    let document_id = EmailConnector::document_id(EmailProvider::MicrosoftGraph, &msg.id);
    if msg.removed.is_some() {
        return ConnectorEvent::DocumentDeleted {
            document_id,
            occurred_at,
        };
    }
    match mode {
        SyncMode::Initial => ConnectorEvent::DocumentCreated {
            document_id,
            occurred_at,
        },
        SyncMode::Incremental => ConnectorEvent::DocumentUpdated {
            document_id,
            occurred_at,
        },
    }
}

/// Which sync pass produced this item — mirror of the enum in the
/// OneDrive / HubSpot / Notion connectors. During `initial_sync` the
/// substrate is seeing every non-deleted item for the first time and
/// must classify it as `DocumentCreated` regardless of whether the
/// upstream message has been edited.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

impl Connector for EmailConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        // Dispatch is keyed by `provider` so the substrate can't
        // accidentally exchange a Gmail code against Microsoft.
        let _provider = EmailProvider::from_config(config)?;
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "email authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        match EmailProvider::from_config(config)? {
            EmailProvider::Gmail => {
                let base = self.resolved_gmail_base(config);
                let user_path = Self::resolved_gmail_user_path(config);
                let messages = self.paginate_gmail_messages(&base, &user_path, token)?;
                let mut events: Vec<ConnectorEvent> = Vec::with_capacity(messages.len());
                for msg in &messages {
                    let occurred_at = msg
                        .internal_date
                        .as_deref()
                        .map_or_else(Utc::now, Self::parse_gmail_internal_date);
                    events.push(ConnectorEvent::DocumentCreated {
                        document_id: Self::document_id(EmailProvider::Gmail, &msg.id),
                        occurred_at,
                    });
                }
                // Anchor the historyId watermark via getProfile so
                // subsequent incremental polls have a real cursor.
                let profile = self.fetch_gmail_profile(&base, &user_path, token)?;
                Ok(SyncRunResult {
                    events,
                    next_cursor: profile.history_id,
                })
            }
            EmailProvider::MicrosoftGraph => {
                let base = self.resolved_graph_base(config);
                let version = self.resolved_graph_version(config);
                let messages_path = Self::resolved_graph_messages_path(config);
                let url = format!("{base}{version}{messages_path}");
                let (items, delta_link) = self.paginate_graph_delta(&url, token)?;
                let mut events: Vec<ConnectorEvent> = Vec::with_capacity(items.len());
                for msg in &items {
                    events.push(graph_message_to_event(msg, SyncMode::Initial));
                }
                Ok(SyncRunResult {
                    events,
                    next_cursor: delta_link,
                })
            }
        }
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let cursor = state.cursor.as_deref().ok_or_else(|| {
            ConnectorError::Sync(
                "email incremental_sync: missing cursor; \
                 initial_sync must populate the watermark first"
                    .into(),
            )
        })?;
        match EmailProvider::from_config(config)? {
            EmailProvider::Gmail => {
                let base = self.resolved_gmail_base(config);
                let user_path = Self::resolved_gmail_user_path(config);
                let (history, latest) =
                    self.paginate_gmail_history(&base, &user_path, token, cursor)?;
                let mut events: Vec<ConnectorEvent> = Vec::new();
                let occurred_at = Utc::now();
                for entry in history {
                    for added in entry.messages_added {
                        if added.message.id.is_empty() {
                            continue;
                        }
                        events.push(ConnectorEvent::DocumentCreated {
                            document_id: Self::document_id(EmailProvider::Gmail, &added.message.id),
                            occurred_at,
                        });
                    }
                    for deleted in entry.messages_deleted {
                        if deleted.message.id.is_empty() {
                            continue;
                        }
                        events.push(ConnectorEvent::DocumentDeleted {
                            document_id: Self::document_id(
                                EmailProvider::Gmail,
                                &deleted.message.id,
                            ),
                            occurred_at,
                        });
                    }
                }
                // Gmail returns `historyId` on the final page. If
                // the server omitted it (unlikely but possible),
                // fall back to the existing cursor so we don't lose
                // our place.
                let next_cursor = latest.or_else(|| Some(cursor.to_string()));
                Ok(SyncRunResult {
                    events,
                    next_cursor,
                })
            }
            EmailProvider::MicrosoftGraph => {
                // Graph's `@odata.deltaLink` is a fully-qualified
                // URL with the server-state cursor baked in — we
                // GET it verbatim.
                let (items, new_delta_link) = self.paginate_graph_delta(cursor, token)?;
                let mut events: Vec<ConnectorEvent> = Vec::with_capacity(items.len());
                for msg in &items {
                    events.push(graph_message_to_event(msg, SyncMode::Incremental));
                }
                let next_cursor = new_delta_link.or_else(|| Some(cursor.to_string()));
                Ok(SyncRunResult {
                    events,
                    next_cursor,
                })
            }
        }
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        match EmailProvider::from_config(config)? {
            EmailProvider::Gmail => {
                let base = self.resolved_gmail_base(config);
                let user_path = Self::resolved_gmail_user_path(config);
                let topic = Self::resolved_gmail_topic(config)?;
                let url = format!("{base}/gmail/v1/{user_path}/watch");
                let body = serde_json::json!({
                    "topicName": topic,
                    // Default to surface notifications for the
                    // INBOX; the substrate can override via
                    // `auth_config_json.gmail_label_ids`.
                    "labelIds": config
                        .auth_config_json
                        .get("gmail_label_ids")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!(["INBOX"])),
                    "labelFilterAction": "include",
                });
                let resp: GmailWatchResponse =
                    bearer_post_json(&self.transport, "gmail", "/watch", &url, token, &[], &body)?;
                let local_ttl = Utc::now() + Duration::minutes(DEFAULT_GMAIL_WATCH_TTL_MINUTES);
                let expires_at =
                    Self::parse_gmail_expiration(resp.expiration.as_deref()).unwrap_or(local_ttl);
                // Gmail's webhook channel is keyed by the Pub/Sub
                // topic — the substrate's "secret" is the
                // verification token Cloud Pub/Sub adds to push
                // requests. Stamp the resource pointer in the
                // subscription so the revoke path knows what to
                // call `stop` on.
                let secret = config
                    .auth_config_json
                    .get("gmail_pubsub_verification_token")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("gmail-pubsub-secret")
                    .to_string();
                let mut subscription = WebhookSubscription::new(
                    self.instance,
                    callback_url,
                    WebhookSecret::new(secret),
                    WebhookEventTypes {
                        document_created: true,
                        // Gmail push notifications cannot
                        // distinguish updates vs. creates without a
                        // follow-up `history.list`; the connector
                        // therefore claims only `created` directly.
                        // Updates / deletes still surface via the
                        // incremental `history.list` poll.
                        document_updated: false,
                        document_deleted: false,
                        permission_changed: false,
                    },
                    Some(expires_at),
                );
                // Gmail's `users.watch` does NOT return a discrete
                // subscription / channel id — revocation calls
                // `users.{userPath}.stop`. The substrate's webhook
                // lifecycle manager needs a stable handle to that
                // resource; stamp it with a structured `gmail-watch:`
                // marker so the manager can split on `:` and
                // dispatch the stop call. We intentionally do NOT
                // store `resp.history_id` here — that value is the
                // watermark for the next `history.list` call, which
                // is the responsibility of `initial_sync` (it
                // anchors the cursor via `users.getProfile`), not
                // of the webhook subscription.
                subscription.provider_subscription_id =
                    Some(format!("gmail-watch:{user_path}"));
                Ok(subscription)
            }
            EmailProvider::MicrosoftGraph => {
                let base = self.resolved_graph_base(config);
                let version = self.resolved_graph_version(config);
                let resource = Self::resolved_graph_subscription_resource(config);
                let url = format!("{base}{version}/subscriptions");
                let client_state = config
                    .auth_config_json
                    .get("graph_client_state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("graph-clientstate-secret")
                    .to_string();
                let expires_at =
                    Utc::now() + Duration::minutes(DEFAULT_GRAPH_SUBSCRIPTION_TTL_MINUTES);
                let body = serde_json::json!({
                    "changeType": "created,updated,deleted",
                    "notificationUrl": callback_url,
                    "resource": resource,
                    "expirationDateTime": expires_at.to_rfc3339(),
                    "clientState": client_state,
                });
                let resp: GraphSubscriptionResponse = bearer_post_json(
                    &self.transport,
                    "msgraph",
                    "/subscriptions",
                    &url,
                    token,
                    &[],
                    &body,
                )?;
                let mut subscription = WebhookSubscription::new(
                    self.instance,
                    callback_url,
                    WebhookSecret::new(client_state),
                    WebhookEventTypes {
                        document_created: true,
                        document_updated: true,
                        document_deleted: true,
                        permission_changed: false,
                    },
                    Some(resp.expiration_date_time.unwrap_or(expires_at)),
                );
                subscription.provider_subscription_id = resp.id;
                Ok(subscription)
            }
        }
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Webhook decoding doesn't have a `provider` field on the
        // wire (Gmail Pub/Sub vs Graph batches are
        // distinguishable by shape), so we try Graph first (its
        // envelope is more strict) and fall back to Gmail.
        if let Ok(batch) = serde_json::from_slice::<GraphChangeNotificationBatch>(body) {
            if batch.validation_token.is_some() && batch.value.is_empty() {
                return Ok(Vec::new());
            }
            if !batch.value.is_empty() {
                return Self::decode_graph_batch(batch);
            }
            // Empty batch with no validationToken — could be a
            // Gmail Pub/Sub envelope that lacked `value`; fall
            // through to Gmail decode.
        }
        let push: GmailPushNotification = serde_json::from_slice(body)?;
        if push.email_address.is_empty() && push.history_id == 0 {
            return Err(ConnectorError::Webhook(
                "email webhook: payload matched neither Graph \
                 changeNotificationCollection nor Gmail Pub/Sub envelope"
                    .into(),
            ));
        }
        if push.message_ids.is_empty() {
            // Gmail's normal flow — only `historyId` is delivered.
            // The runtime is expected to follow up with
            // `history.list`. Connector returns no events here.
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
}

impl EmailConnector {
    fn decode_graph_batch(batch: GraphChangeNotificationBatch) -> Result<Vec<ConnectorEvent>> {
        if batch.value.is_empty() {
            return Err(ConnectorError::Webhook(
                "msgraph: empty change notification batch".into(),
            ));
        }
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(batch.value.len());
        for note in batch.value {
            let id = Self::graph_message_id_from_resource(&note.resource, &note.resource_data.id);
            if id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "msgraph: change notification missing resource id".into(),
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
                // Unknown changeType strings are skipped rather
                // than aborting the whole batch — a single new
                // lifecycle value can't retroactively discard every
                // well-formed event queued behind it. Mirrors the
                // policy in the OneDrive handler.
                _ => continue,
            };
            events.push(ev);
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

    struct GmailOAuth;
    impl OAuth2CodeExchange for GmailOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "gmail-access",
                "gmail-refresh",
                Utc::now() + Duration::hours(1),
                "https://www.googleapis.com/auth/gmail.readonly",
            ))
        }
    }

    struct GraphOAuth;
    impl OAuth2CodeExchange for GraphOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "graph-access",
                "graph-refresh",
                Utc::now() + Duration::hours(1),
                "Mail.Read offline_access",
            ))
        }
    }

    fn gmail_oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(GmailOAuth)
    }
    fn graph_oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(GraphOAuth)
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const GMAIL_BASE: &str = "https://api.test/gmail";
    const GRAPH_BASE: &str = "https://api.test/graph";

    fn gmail_cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Email, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "provider": "gmail",
                "authorization_code": "demo-code",
                "gmail_api_base_url": GMAIL_BASE,
                "gmail_topic_name": "projects/p/topics/gmail-demo",
            }))
    }

    fn graph_cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Email, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "provider": "msgraph",
                "authorization_code": "demo-code",
                "graph_api_base_url": GRAPH_BASE,
            }))
    }

    fn gmail_list_url(page_token: Option<&str>) -> String {
        let page_size = DEFAULT_PAGE_SIZE.to_string();
        let mut params: Vec<(&str, &str)> = vec![("maxResults", page_size.as_str())];
        if let Some(t) = page_token {
            params.push(("pageToken", t));
        }
        let qs = encode_form(&params);
        format!("{GMAIL_BASE}/gmail/v1/users/me/messages?{qs}")
    }

    fn gmail_profile_url() -> String {
        format!("{GMAIL_BASE}/gmail/v1/users/me/profile")
    }

    fn gmail_history_url(start_history_id: &str, page_token: Option<&str>) -> String {
        let page_size = DEFAULT_PAGE_SIZE.to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("startHistoryId", start_history_id),
            ("maxResults", page_size.as_str()),
        ];
        if let Some(t) = page_token {
            params.push(("pageToken", t));
        }
        let qs = encode_form(&params);
        format!("{GMAIL_BASE}/gmail/v1/users/me/history?{qs}")
    }

    fn graph_delta_url() -> String {
        format!("{GRAPH_BASE}/v1.0/me/messages/delta")
    }

    #[test]
    fn authenticate_requires_provider() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = ConnectorConfig::new(ConnectorKind::Email, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({"authorization_code": "x"}));
        let err = c.authenticate(&cfg).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn authenticate_rejects_unknown_provider() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = ConnectorConfig::new(ConnectorKind::Email, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({"provider": "yahoo", "authorization_code": "x"}));
        let err = c.authenticate(&cfg).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn gmail_authenticate_dispatches_to_oauth_exchange() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let tok = c.authenticate(&gmail_cfg()).unwrap();
        assert!(tok.scope.contains("gmail.readonly"));
        assert_eq!(tok.access_token.expose(), "gmail-access");
    }

    #[test]
    fn graph_authenticate_dispatches_to_oauth_exchange() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let tok = c.authenticate(&graph_cfg()).unwrap();
        assert!(tok.scope.contains("Mail.Read"));
        assert_eq!(tok.access_token.expose(), "graph-access");
    }

    #[test]
    fn gmail_initial_sync_walks_pages_and_anchors_history_id() {
        let transport = MockHttpTransport::new();
        // Page 1 → page 2.
        transport.expect(
            HttpMethod::Get,
            gmail_list_url(None),
            ok_json(&serde_json::json!({
                "messages": [
                    {"id": "g1", "threadId": "t1", "internalDate": "1700000000000"},
                    {"id": "g2", "threadId": "t1", "internalDate": "1700000500000"}
                ],
                "nextPageToken": "tok-2"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            gmail_list_url(Some("tok-2")),
            ok_json(&serde_json::json!({
                "messages": [{"id": "g3", "threadId": "t2", "internalDate": "1700001000000"}],
            })),
        );
        // getProfile to anchor watermark.
        transport.expect(
            HttpMethod::Get,
            gmail_profile_url(),
            ok_json(&serde_json::json!({
                "emailAddress": "user@example.com",
                "historyId": "5005",
                "messagesTotal": 3
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = gmail_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let res = c.initial_sync(&cfg, &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(res.next_cursor.as_deref(), Some("5005"));
    }

    #[test]
    fn gmail_paginate_loop_guard_breaks_repeated_token() {
        // Server mis-shaped: echoes the same nextPageToken on every
        // page. Without the loop guard this would spin forever.
        let transport = MockHttpTransport::new();
        transport.with_default_response(ok_json(&serde_json::json!({
            "messages": [{"id": "g-loop", "threadId": "t"}],
            "nextPageToken": "same-token"
        })));
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        // Use a custom base URL to ensure we don't hammer real
        // endpoints in CI.
        let cfg = gmail_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        // Profile fetch must succeed too, so default-response also
        // satisfies it.
        let res = c.initial_sync(&cfg, &tok).unwrap();
        // Guard kicks in on second hit of the same token.
        assert!(res.events.len() <= 2, "loop guard must bound iteration");
    }

    #[test]
    fn gmail_incremental_sync_walks_history_and_emits_added_and_deleted() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            gmail_history_url("5005", None),
            ok_json(&serde_json::json!({
                "history": [
                    {
                        "id": "6001",
                        "messagesAdded": [
                            {"message": {"id": "h-new-1", "threadId": "t9"}},
                            {"message": {"id": "h-new-2", "threadId": "t9"}}
                        ],
                        "messagesDeleted": [
                            {"message": {"id": "h-rm-1", "threadId": "t-old"}}
                        ]
                    }
                ],
                "historyId": "6042"
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = gmail_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("5005".into());
        let res = c.incremental_sync(&cfg, &tok, &state).unwrap();
        assert_eq!(res.events.len(), 3);
        let created = res
            .events
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentCreated { .. }))
            .count();
        let deleted = res
            .events
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }))
            .count();
        assert_eq!(created, 2);
        assert_eq!(deleted, 1);
        assert_eq!(res.next_cursor.as_deref(), Some("6042"));
    }

    #[test]
    fn gmail_incremental_sync_requires_cursor() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = gmail_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let state = SyncState::new(c.instance);
        let err = c.incremental_sync(&cfg, &tok, &state).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn gmail_subscribe_webhook_posts_watch_and_stamps_user_path() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            format!("{GMAIL_BASE}/gmail/v1/users/me/watch"),
            ok_json(&serde_json::json!({
                "historyId": "7777",
                "expiration": "1900000000000"
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = gmail_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let sub = c
            .subscribe_webhook(&cfg, &tok, "https://substrate.example/email/gmail")
            .unwrap();
        // The substrate's revoke path needs a structured handle to
        // call `users/{userPath}/stop` on — the historyId returned
        // by `users.watch` is a watermark, not a subscription id,
        // so we stamp a `gmail-watch:` marker carrying the user
        // path. The historyId itself belongs in sync state (set by
        // `initial_sync` via `users.getProfile`).
        assert_eq!(
            sub.provider_subscription_id.as_deref(),
            Some("gmail-watch:users/me")
        );
        assert!(sub.event_types.document_created);
        // Direct push only carries history id; updates/deletes
        // surface via the incremental history.list poll.
        assert!(!sub.event_types.document_updated);
        assert!(!sub.event_types.document_deleted);
        // Expiration parsed from epoch-ms.
        let expected = DateTime::<Utc>::from_timestamp_millis(1_900_000_000_000).unwrap();
        assert_eq!(sub.expires_at, Some(expected));
    }

    #[test]
    fn gmail_subscribe_webhook_stamps_user_path_with_custom_user_id() {
        // When `auth_config_json.gmail_user_id` overrides the
        // default `me`, the synthetic subscription id must carry
        // the new path so revocation lines up with the actual
        // watched mailbox.
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            format!("{GMAIL_BASE}/gmail/v1/users/team@example.com/watch"),
            ok_json(&serde_json::json!({
                "historyId": "8888",
                "expiration": "1900000000000"
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let mut cfg = gmail_cfg();
        let auth = cfg
            .auth_config_json
            .as_object_mut()
            .expect("auth_config_json object");
        auth.insert(
            "gmail_user_id".to_string(),
            serde_json::Value::String("team@example.com".to_string()),
        );
        let tok = c.authenticate(&cfg).unwrap();
        let sub = c
            .subscribe_webhook(&cfg, &tok, "https://substrate.example/email/gmail")
            .unwrap();
        assert_eq!(
            sub.provider_subscription_id.as_deref(),
            Some("gmail-watch:users/team@example.com")
        );
    }

    #[test]
    fn gmail_subscribe_webhook_falls_back_to_local_ttl_when_server_omits_expiration() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            format!("{GMAIL_BASE}/gmail/v1/users/me/watch"),
            ok_json(&serde_json::json!({"historyId": "9000"})),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = gmail_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let sub = c
            .subscribe_webhook(&cfg, &tok, "https://substrate.example/email/gmail")
            .unwrap();
        let exp = sub.expires_at.expect("local TTL must populate expires_at");
        let now = Utc::now();
        let delta = exp - now;
        assert!(delta > Duration::days(6) && delta <= Duration::days(7));
    }

    #[test]
    fn gmail_subscribe_webhook_requires_topic() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let cfg = ConnectorConfig::new(ConnectorKind::Email, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "provider": "gmail",
                "authorization_code": "demo",
                "gmail_api_base_url": GMAIL_BASE,
            }));
        let tok = c.authenticate(&cfg).unwrap();
        let err = c
            .subscribe_webhook(&cfg, &tok, "https://substrate.example/email/gmail")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn graph_initial_sync_walks_delta_and_advances_watermark() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            graph_delta_url(),
            ok_json(&serde_json::json!({
                "value": [
                    {"id": "m1", "createdDateTime": "2024-01-01T00:00:00Z", "lastModifiedDateTime": "2024-01-02T00:00:00Z"},
                    {"id": "m2", "createdDateTime": "2024-01-03T00:00:00Z", "lastModifiedDateTime": "2024-01-03T00:00:00Z"}
                ],
                "@odata.nextLink": "https://api.test/graph/v1.0/me/messages/delta?$skiptoken=NEXT"
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/graph/v1.0/me/messages/delta?$skiptoken=NEXT",
            ok_json(&serde_json::json!({
                "value": [
                    {"id": "m3", "createdDateTime": "2024-01-05T00:00:00Z"}
                ],
                "@odata.deltaLink": "https://api.test/graph/v1.0/me/messages/delta?$deltatoken=42"
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let cfg = graph_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let res = c.initial_sync(&cfg, &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(res
            .events
            .iter()
            .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("https://api.test/graph/v1.0/me/messages/delta?$deltatoken=42")
        );
    }

    #[test]
    fn graph_initial_sync_emits_deleted_for_removed_envelope() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            graph_delta_url(),
            ok_json(&serde_json::json!({
                "value": [
                    {"id": "m1", "createdDateTime": "2024-01-01T00:00:00Z"},
                    {"id": "m-gone", "@removed": {"reason": "deleted"}}
                ],
                "@odata.deltaLink": "https://api.test/graph/v1.0/me/messages/delta?$deltatoken=99"
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let cfg = graph_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let res = c.initial_sync(&cfg, &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        let deleted = res
            .events
            .iter()
            .filter(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }))
            .count();
        assert_eq!(deleted, 1);
    }

    #[test]
    fn graph_incremental_sync_walks_delta_link_verbatim() {
        let cursor = "https://api.test/graph/v1.0/me/messages/delta?$deltatoken=42";
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            cursor,
            ok_json(&serde_json::json!({
                "value": [{"id": "m9", "lastModifiedDateTime": "2024-02-01T00:00:00Z"}],
                "@odata.deltaLink": "https://api.test/graph/v1.0/me/messages/delta?$deltatoken=99"
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let cfg = graph_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor.to_string());
        let res = c.incremental_sync(&cfg, &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("https://api.test/graph/v1.0/me/messages/delta?$deltatoken=99")
        );
    }

    #[test]
    fn graph_incremental_sync_falls_back_to_existing_cursor_when_no_new_delta_link() {
        let cursor = "https://api.test/graph/v1.0/me/messages/delta?$deltatoken=42";
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            cursor,
            ok_json(&serde_json::json!({
                "value": []
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let cfg = graph_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor.to_string());
        let res = c.incremental_sync(&cfg, &tok, &state).unwrap();
        assert!(res.events.is_empty());
        assert_eq!(res.next_cursor.as_deref(), Some(cursor));
    }

    #[test]
    fn graph_subscribe_webhook_posts_subscription_and_captures_id() {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Post,
            format!("{GRAPH_BASE}/v1.0/subscriptions"),
            ok_json(&serde_json::json!({
                "id": "sub-graph-1",
                "expirationDateTime": "2030-01-01T00:00:00Z"
            })),
        );
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let cfg = graph_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let sub = c
            .subscribe_webhook(&cfg, &tok, "https://substrate.example/email/graph")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("sub-graph-1"));
        assert!(sub.event_types.document_created);
        assert!(sub.event_types.document_updated);
        assert!(sub.event_types.document_deleted);
    }

    #[test]
    fn unauthorized_status_maps_to_auth_error() {
        let transport = MockHttpTransport::new();
        transport.with_default_response(MockResponse::status(
            401,
            b"{\"error\":\"invalid_grant\"}".to_vec(),
        ));
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let cfg = graph_cfg();
        let tok = c.authenticate(&cfg).unwrap();
        let err = c.initial_sync(&cfg, &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn gmail_webhook_with_message_ids_emits_per_message_events() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let body = serde_json::json!({
            "emailAddress": "user@example.com",
            "historyId": 12345,
            "messageIds": ["g1", "g2", "g3"]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].document_id().as_str(), "gmail:msg:g1");
    }

    #[test]
    fn gmail_webhook_without_message_ids_returns_empty_event_list() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let body = serde_json::json!({
            "emailAddress": "user@example.com",
            "historyId": 12345
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn webhook_unrecognised_payload_errors() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            gmail_oauth(),
        );
        let body = serde_json::json!({});
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn graph_validation_token_short_circuits_to_empty_event_list() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let body = serde_json::json!({"validationToken": "x", "value": []});
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn graph_change_notification_batch_emits_all_events() {
        // Regression: every notification in a Graph batch must
        // surface. A single batch routinely carries multiple
        // notifications and the handler must not stop at index 0.
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let body = serde_json::json!({
            "value": [
                {"changeType": "created", "resource": "/me/messages/A", "resourceData": {"id": "A"}},
                {"changeType": "updated", "resource": "/me/messages/B", "resourceData": {"id": "B"}},
                {"changeType": "deleted", "resource": "/me/messages/C", "resourceData": {"id": "C"}}
            ]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn graph_unknown_change_type_is_skipped_not_errored() {
        // A single unknown changeType cannot drop the rest of a
        // valid batch — mirror of the OneDrive policy.
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let body = serde_json::json!({
            "value": [
                {"changeType": "created", "resource": "/me/messages/A", "resourceData": {"id": "A"}},
                {"changeType": "weird", "resource": "/me/messages/B", "resourceData": {"id": "B"}},
                {"changeType": "deleted", "resource": "/me/messages/C", "resourceData": {"id": "C"}}
            ]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn graph_change_notification_falls_back_to_resource_path_when_data_missing() {
        let transport = MockHttpTransport::new();
        let c = EmailConnector::new(
            ConnectorInstanceId::new_v4(),
            Arc::new(transport),
            graph_oauth(),
        );
        let body = serde_json::json!({
            "value": [{"changeType": "updated", "resource": "/users/abc/messages/MSG-99"}]
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
    fn provider_string_tags_and_document_id_format_are_pinned() {
        assert_eq!(EmailProvider::Gmail.as_str(), "gmail");
        assert_eq!(EmailProvider::MicrosoftGraph.as_str(), "msgraph");
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
