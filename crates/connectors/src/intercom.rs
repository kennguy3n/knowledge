//! Intercom connector — Intercom REST API (`https://api.intercom.io`).
//!
//! * Both syncs use the **Search API**
//!   (`POST /conversations/search`) because it supports an
//!   `updated_at` filter and cursor pagination
//!   (`pagination.starting_after` → `pages.next.starting_after`).
//!   `initial_sync` queries `updated_at > 0`; `incremental_sync`
//!   queries `updated_at > <cursor>` (Unix seconds, strict `>`).
//! * `fetch_content` GETs the single conversation
//!   (`/conversations/{id}`) and reconstructs Markdown from the title
//!   + the source message body.
//! * `subscribe_webhook` POSTs `/subscriptions` and persists
//!   Intercom's returned subscription id.
//! * `handle_webhook_event` parses Intercom's
//!   `notification_event` envelope (`data.item.id`), tolerating a
//!   batched array.
//!
//! Intercom timestamps are Unix **seconds** (integers).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Intercom API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.intercom.io";

/// Page size for conversation search.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// One Intercom conversation (subset of fields used by the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomConversation {
    /// Conversation id.
    #[serde(default)]
    pub id: String,
    /// Optional conversation title.
    #[serde(default)]
    pub title: Option<String>,
    /// Source message (first message of the thread).
    #[serde(default)]
    pub source: Option<IntercomSource>,
    /// Creation time (Unix seconds).
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Last-update time (Unix seconds).
    #[serde(default)]
    pub updated_at: Option<i64>,
}

/// The `source` block of a conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomSource {
    /// HTML/text body of the source message.
    #[serde(default)]
    pub body: Option<String>,
}

/// Cursor-pagination block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomPages {
    /// Next-page descriptor (null on the last page).
    #[serde(default)]
    pub next: Option<IntercomPageNext>,
}

/// The next-page descriptor carrying the cursor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomPageNext {
    /// Opaque cursor for the next request.
    #[serde(default)]
    pub starting_after: Option<String>,
}

/// A conversation search/list response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomConversationList {
    /// Conversations on this page.
    #[serde(default)]
    pub conversations: Vec<IntercomConversation>,
    /// Pagination block.
    #[serde(default)]
    pub pages: IntercomPages,
}

/// `POST /subscriptions` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomSubscriptionResponse {
    /// Subscription id.
    #[serde(default)]
    pub id: serde_json::Value,
}

/// Intercom `notification_event` webhook envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomWebhookEvent {
    /// Notification topic, e.g. `conversation.user.created`.
    #[serde(default)]
    pub topic: String,
    /// Event payload.
    #[serde(default)]
    pub data: IntercomWebhookData,
    /// Event time (Unix seconds).
    #[serde(default)]
    pub created_at: Option<i64>,
}

/// The `data` block of a notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomWebhookData {
    /// The affected item.
    #[serde(default)]
    pub item: IntercomWebhookItem,
}

/// The `data.item` block of a notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntercomWebhookItem {
    /// Item id.
    #[serde(default)]
    pub id: String,
}

/// Intercom connector.
pub struct IntercomConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for IntercomConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntercomConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl IntercomConnector {
    /// Construct an Intercom connector.
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

    /// Override the Intercom API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the search page size.
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

    /// Run `POST /conversations/search` for `updated_at > since`,
    /// following the cursor until exhausted.
    fn search_from(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        since: i64,
    ) -> Result<Vec<IntercomConversation>> {
        let url = format!("{base_url}/conversations/search");
        let mut conversations = Vec::<IntercomConversation>::new();
        let mut starting_after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut pagination = serde_json::json!({ "per_page": self.page_size });
            if let Some(cursor) = &starting_after {
                pagination["starting_after"] = serde_json::Value::String(cursor.clone());
            }
            let request = serde_json::json!({
                "query": {
                    "field": "updated_at",
                    "operator": ">",
                    "value": since,
                },
                "pagination": pagination,
            });
            let resp: IntercomConversationList = bearer_post_json(
                &self.transport,
                "intercom",
                "/conversations/search",
                &url,
                token,
                &[],
                &request,
            )?;
            conversations.extend(resp.conversations);
            match resp.pages.next.and_then(|n| n.starting_after) {
                Some(cursor) if !cursor.is_empty() => starting_after = Some(cursor),
                _ => return Ok(conversations),
            }
        }
        Err(ConnectorError::Sync(format!(
            "intercom conversation search exceeded {MAX_PAGES} pages"
        )))
    }
}

fn conversation_watermark(c: &IntercomConversation) -> Option<i64> {
    c.updated_at.or(c.created_at)
}

fn unix_to_datetime(secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for IntercomConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "intercom authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let conversations = self.search_from(&base_url, token, 0)?;
        let mut events = Vec::with_capacity(conversations.len());
        let mut watermark: Option<i64> = None;
        for conversation in &conversations {
            let occurred_at =
                conversation_watermark(conversation).map_or_else(Utc::now, unix_to_datetime);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(conversation.id.clone()),
                occurred_at,
            });
            if let Some(ts) = conversation_watermark(conversation) {
                watermark = Some(watermark.map_or(ts, |w| w.max(ts)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|ts| ts.to_string()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let prior: Option<i64> = state.cursor.as_deref().and_then(|s| s.parse::<i64>().ok());
        let conversations = self.search_from(&base_url, token, prior.unwrap_or(0))?;
        let mut events = Vec::with_capacity(conversations.len());
        let mut watermark = prior;
        for conversation in &conversations {
            let occurred_at =
                conversation_watermark(conversation).map_or_else(Utc::now, unix_to_datetime);
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(conversation.id.clone()),
                occurred_at,
            });
            if let Some(ts) = conversation_watermark(conversation) {
                watermark = Some(watermark.map_or(ts, |w| w.max(ts)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark
                .map(|ts| ts.to_string())
                .or_else(|| state.cursor.clone()),
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
        let url = format!("{base_url}/conversations/{id_enc}");
        let conversation: IntercomConversation = bearer_get_json(
            &self.transport,
            "intercom",
            "/conversations/{id}",
            &url,
            token,
            &[],
        )?;
        let title = conversation.title.clone().unwrap_or_default();
        let source_body = conversation
            .source
            .as_ref()
            .and_then(|s| s.body.clone())
            .unwrap_or_default();
        let mut md = String::new();
        if !title.is_empty() {
            md.push_str("# ");
            md.push_str(&title);
            md.push_str("\n\n");
        }
        if !source_body.is_empty() {
            md.push_str(&source_body);
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "intercom",
                "conversation_id": id,
                "updated_at": conversation.updated_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/subscriptions");
        let request = serde_json::json!({
            "service_type": "web",
            "url": callback_url,
            "topics": [
                "conversation.user.created",
                "conversation.user.replied",
                "conversation.admin.replied",
            ],
        });
        let resp: IntercomSubscriptionResponse = bearer_post_json(
            &self.transport,
            "intercom",
            "/subscriptions",
            &url,
            token,
            &[],
            &request,
        )?;
        let provider_id = id_value_to_string(&resp.id);
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("intercom-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<IntercomWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<IntercomWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<IntercomWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty intercom webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            if delivery.data.item.id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "intercom webhook event missing data.item.id".into(),
                ));
            }
            let occurred_at = delivery.created_at.map_or_else(Utc::now, unix_to_datetime);
            let id = SourceDocumentId::new(delivery.data.item.id);
            let event = if delivery.topic.contains("created") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.topic.contains("deleted") {
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
    use chrono::Duration;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "ic-access",
                "ic-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Intercom, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/ic",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const SEARCH_URL: &str = "https://api.test/ic/conversations/search";

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::Intercom, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "ic-access"
        );
    }

    #[test]
    fn initial_sync_follows_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            SEARCH_URL.to_string(),
            ok_json(&serde_json::json!({
                "conversations": [{"id": "c1", "updated_at": 1000}],
                "pages": {"next": {"starting_after": "CUR"}}
            })),
        );
        transport.expect(
            HttpMethod::Post,
            SEARCH_URL.to_string(),
            ok_json(&serde_json::json!({
                "conversations": [{"id": "c2", "updated_at": 2000}],
                "pages": {"next": null}
            })),
        );
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("2000"));
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_emits_updated() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            SEARCH_URL.to_string(),
            ok_json(&serde_json::json!({
                "conversations": [{"id": "c9", "updated_at": 5000}],
                "pages": {"next": null}
            })),
        );
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("3000".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("5000"));
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/ic/conversations/c1".to_string(),
            ok_json(&serde_json::json!({
                "id": "c1",
                "title": "Refund request",
                "source": {"body": "I want a refund."},
                "updated_at": 1000
            })),
        );
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("c1"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Refund request"));
        assert!(body.contains("I want a refund."));
        assert_eq!(fc.title.as_deref(), Some("Refund request"));
    }

    #[test]
    fn subscribe_webhook_creates_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/ic/subscriptions".to_string(),
            ok_json(&serde_json::json!({"id": "sub_1"})),
        );
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/ic")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("sub_1"));
    }

    #[test]
    fn webhook_parses_notification_envelope() {
        let body = serde_json::json!({
            "type": "notification_event",
            "topic": "conversation.user.created",
            "data": {"item": {"type": "conversation", "id": "c1"}},
            "created_at": 1000
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn webhook_maps_replied_to_update() {
        let body = serde_json::json!([
            {"topic": "conversation.user.replied", "data": {"item": {"id": "a"}}},
            {"topic": "conversation.admin.replied", "data": {"item": {"id": "b"}}}
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_missing_item_id_errors() {
        let body =
            serde_json::json!({"topic": "conversation.user.created", "data": {"item": {"id": ""}}});
        let transport = Arc::new(MockHttpTransport::new());
        let c = IntercomConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap()),
            Err(ConnectorError::Webhook(_))
        ));
    }
}
