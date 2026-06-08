//! Monday.com connector — Monday GraphQL API
//! (`https://api.monday.com/v2`).
//!
//! * `initial_sync` reads the configured board's `items_page`, then
//!   follows `next_items_page(cursor:)` until the cursor is null.
//! * `incremental_sync` re-walks the board the same way but emits
//!   only items whose `updated_at` is strictly newer than the stored
//!   watermark (Monday's `items_page` has no server-side
//!   updated-since filter, so the comparison is client-side; the
//!   strict `>` means no boundary dedup is required).
//! * `fetch_content` queries `items(ids:)` and reconstructs Markdown
//!   from the item name + its column values.
//! * `subscribe_webhook` runs the `create_webhook` mutation and
//!   persists Monday's returned webhook id.
//! * `handle_webhook_event` parses Monday's `{ "event": { … } }`
//!   delivery envelope (one item change per POST) and tolerates a
//!   batched array form.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_post_json, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result,
    SourceDocumentId, SyncRunResult, SyncState, WatermarkCursor, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
};
use serde::Deserialize;

/// Default Monday API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.monday.com";

/// Page size for `items_page` / `next_items_page`.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

#[derive(Debug, Clone, Deserialize)]
struct GraphQlError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQlError>>,
}

impl<T> GraphQlResponse<T> {
    fn into_data(self, ctx: &str) -> Result<T> {
        if let Some(errors) = self.errors.filter(|e| !e.is_empty()) {
            let joined = errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ConnectorError::Sync(format!(
                "monday {ctx} GraphQL error: {joined}"
            )));
        }
        self.data.ok_or_else(|| {
            ConnectorError::Sync(format!("monday {ctx}: GraphQL response had no data"))
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ItemsPage {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    items: Vec<MondayItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MondayItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    column_values: Vec<ColumnValue>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ColumnValue {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BoardsData {
    #[serde(default)]
    boards: Vec<BoardItemsPage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BoardItemsPage {
    #[serde(default)]
    items_page: ItemsPage,
}

#[derive(Debug, Clone, Deserialize)]
struct NextItemsPageData {
    next_items_page: ItemsPage,
}

#[derive(Debug, Clone, Deserialize)]
struct ItemsData {
    #[serde(default)]
    items: Vec<MondayItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct CreateWebhookData {
    create_webhook: WebhookHandle,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WebhookHandle {
    #[serde(default)]
    id: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MondayWebhookEnvelope {
    #[serde(default)]
    event: Option<MondayWebhookEvent>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MondayWebhookEvent {
    #[serde(rename = "type", default)]
    event_type: String,
    #[serde(rename = "pulseId", default)]
    pulse_id: serde_json::Value,
}

/// Monday.com connector.
pub struct MondayConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for MondayConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MondayConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl MondayConnector {
    /// Construct a Monday connector.
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

    /// Override the Monday API base URL.
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

    fn graphql_url(&self, config: &ConnectorConfig) -> String {
        format!("{}/v2", self.resolved_base_url(config))
    }

    fn board_id(config: &ConnectorConfig) -> Result<i64> {
        config
            .auth_config_json
            .get("board_id")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                ConnectorError::Sync(
                    "monday: auth_config_json.board_id (integer) is required".into(),
                )
            })
    }

    fn graphql<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        token: &OAuth2Token,
        query: &str,
        variables: &serde_json::Value,
        ctx: &str,
    ) -> Result<T> {
        let request = serde_json::json!({ "query": query, "variables": variables });
        let resp: GraphQlResponse<T> =
            bearer_post_json(&self.transport, "monday", "/v2", url, token, &[], &request)?;
        resp.into_data(ctx)
    }

    /// Walk a board's items via `items_page` + `next_items_page`.
    fn walk_board(&self, url: &str, token: &OAuth2Token, board_id: i64) -> Result<Vec<MondayItem>> {
        let first_query = "query Items($board: ID!, $limit: Int!) { boards(ids: [$board]) { items_page(limit: $limit) { cursor items { id name created_at updated_at column_values { title text } } } } }";
        let data: BoardsData = self.graphql(
            url,
            token,
            first_query,
            &serde_json::json!({ "board": board_id.to_string(), "limit": self.page_size }),
            "items_page",
        )?;
        let mut items = Vec::<MondayItem>::new();
        let mut cursor = data.boards.into_iter().next().and_then(|b| {
            let page = b.items_page;
            items.extend(page.items);
            page.cursor
        });

        let next_query = "query Next($cursor: String!, $limit: Int!) { next_items_page(limit: $limit, cursor: $cursor) { cursor items { id name created_at updated_at column_values { title text } } } }";
        for _ in 0..MAX_PAGES {
            let Some(c) = cursor.take() else {
                return Ok(items);
            };
            let data: NextItemsPageData = self.graphql(
                url,
                token,
                next_query,
                &serde_json::json!({ "cursor": c, "limit": self.page_size }),
                "next_items_page",
            )?;
            items.extend(data.next_items_page.items);
            cursor = data.next_items_page.cursor;
        }
        Err(ConnectorError::Sync(format!(
            "monday board walk exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn item_watermark(item: &MondayItem) -> Option<DateTime<Utc>> {
    item.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| item.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for MondayConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "monday authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let url = self.graphql_url(config);
        let board = Self::board_id(config)?;
        let items = self.walk_board(&url, token, board)?;
        let mut events = Vec::with_capacity(items.len());
        let mut cursor = WatermarkCursor::empty();
        for item in &items {
            let occurred_at = item_watermark(item).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(item.id.clone()),
                occurred_at,
            });
            if let Some(t) = item_watermark(item) {
                cursor.observe(t, &item.id);
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
        let url = self.graphql_url(config);
        let board = Self::board_id(config)?;
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let items = self.walk_board(&url, token, board)?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for item in &items {
            let Some(updated) = item_watermark(item) else {
                continue;
            };
            if !prior.should_emit(updated, &item.id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(item.id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &item.id);
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
        let url = self.graphql_url(config);
        let query = "query Item($ids: [ID!]) { items(ids: $ids) { id name updated_at column_values { title text } } }";
        let data: ItemsData = self.graphql(
            &url,
            token,
            query,
            &serde_json::json!({ "ids": [document_id.as_str()] }),
            "items",
        )?;
        let item = data.items.into_iter().next().ok_or_else(|| {
            ConnectorError::Sync(format!(
                "monday fetch_content: item {} not found",
                document_id.as_str()
            ))
        })?;
        let name = item.name.clone().unwrap_or_default();
        let mut md = String::new();
        if !name.is_empty() {
            md.push_str("# ");
            md.push_str(&name);
            md.push_str("\n\n");
        }
        for col in &item.column_values {
            let title = col.title.as_deref().unwrap_or_default();
            let text = col.text.as_deref().unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            if title.is_empty() {
                md.push_str(text);
            } else {
                md.push_str("**");
                md.push_str(title);
                md.push_str("**: ");
                md.push_str(text);
            }
            md.push('\n');
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(name)
            .with_metadata(serde_json::json!({
                "provider": "monday",
                "item_id": item.id,
                "updated_at": item.updated_at,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let url = self.graphql_url(config);
        let board = Self::board_id(config)?;
        let query = "mutation CreateWebhook($board: ID!, $url: String!) { create_webhook(board_id: $board, url: $url, event: change_column_value) { id board_id } }";
        let data: CreateWebhookData = self.graphql(
            &url,
            token,
            query,
            &serde_json::json!({ "board": board.to_string(), "url": callback_url }),
            "create_webhook",
        )?;
        let provider_id = id_value_to_string(&data.create_webhook.id);
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("monday-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let envelopes: Vec<MondayWebhookEnvelope> =
            if let Ok(batch) = serde_json::from_slice::<Vec<MondayWebhookEnvelope>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<MondayWebhookEnvelope>(body)?]
            };
        let mut events = Vec::new();
        for envelope in envelopes {
            let Some(event) = envelope.event else {
                continue;
            };
            let id_str = id_value_to_string(&event.pulse_id).ok_or_else(|| {
                ConnectorError::Webhook("monday webhook event missing pulseId".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if event.event_type.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if event.event_type.contains("delete") {
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
        if events.is_empty() {
            return Err(ConnectorError::Webhook(
                "monday webhook payload contained no events".into(),
            ));
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
                "mon-access",
                "mon-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Monday, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/mon",
                "board_id": 12345,
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    const GQL_URL: &str = "https://api.test/mon/v2";

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Monday, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "mon-access"
        );
    }

    #[test]
    fn initial_sync_walks_items_then_next_page() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "boards": [ { "items_page": {
                    "cursor": "CUR1",
                    "items": [ {"id": "1", "name": "a", "updated_at": "2024-01-01T00:00:00Z"} ]
                }}]}
            })),
        );
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "next_items_page": {
                    "cursor": null,
                    "items": [ {"id": "2", "name": "b", "updated_at": "2024-01-02T00:00:00Z"} ]
                }}
            })),
        );
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
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
    fn initial_sync_requires_board_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let mut config = cfg();
        config.auth_config_json = serde_json::json!({ "authorization_code": "x" });
        let tok = c.authenticate(&config).unwrap();
        assert!(matches!(
            c.initial_sync(&config, &tok),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn incremental_sync_filters_client_side() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "boards": [ { "items_page": {
                    "cursor": null,
                    "items": [
                        {"id": "old", "updated_at": "2024-01-01T00:00:00Z"},
                        {"id": "new", "updated_at": "2024-06-01T00:00:00Z"}
                    ]
                }}]}
            })),
        );
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-03-01T00:00:00Z".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|new")
        );
    }

    #[test]
    fn fetch_content_assembles_columns() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "items": [ {
                    "id": "1",
                    "name": "Launch plan",
                    "updated_at": "2024-01-01T00:00:00Z",
                    "column_values": [
                        {"title": "Status", "text": "Done"},
                        {"title": "Owner", "text": "Sam"}
                    ]
                }]}
            })),
        );
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("1"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Launch plan"));
        assert!(body.contains("**Status**: Done"));
        assert!(body.contains("**Owner**: Sam"));
    }

    #[test]
    fn fetch_content_missing_item_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({ "data": { "items": [] } })),
        );
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("404")),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn subscribe_webhook_creates_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            GQL_URL.to_string(),
            ok_json(&serde_json::json!({
                "data": { "create_webhook": { "id": 778, "board_id": 12345 } }
            })),
        );
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/mon")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("778"));
    }

    #[test]
    fn webhook_parses_event_envelope() {
        let body = serde_json::json!({
            "event": { "type": "create_pulse", "pulseId": 99, "boardId": 12345 }
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn webhook_maps_delete_and_update() {
        let body = serde_json::json!([
            {"event": {"type": "delete_pulse", "pulseId": 1}},
            {"event": {"type": "update_column_value", "pulseId": 2}}
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_missing_pulse_id_errors() {
        let body = serde_json::json!({"event": {"type": "update_column_value"}});
        let transport = Arc::new(MockHttpTransport::new());
        let c = MondayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap()),
            Err(ConnectorError::Webhook(_))
        ));
    }
}
