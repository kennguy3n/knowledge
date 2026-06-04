//! Zendesk connector — Zendesk Support API (`/api/v2`).
//!
//! * `initial_sync` and `incremental_sync` both drive the
//!   time-based **incremental export** endpoint
//!   (`/api/v2/incremental/tickets.json?start_time=<unix>`), which
//!   returns tickets ordered by `updated_at` plus an `end_time`
//!   high-water mark and an `end_of_stream` flag. The connector walks
//!   pages until `end_of_stream` is set, then stores `end_time` (unix
//!   seconds) as the cursor.
//! * `fetch_content` GETs the single ticket
//!   (`/api/v2/tickets/{id}.json`) and reconstructs Markdown from
//!   `subject` + `description`.
//! * `subscribe_webhook` POSTs `/api/v2/webhooks`; Zendesk returns a
//!   webhook id that the substrate persists for revocation.
//! * `handle_webhook_event` parses the trigger-delivered batch; a
//!   single POST may carry several ticket events, all emitted.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result,
    SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret,
    WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Zendesk base URL. Per-instance overrides go through
/// `auth_config_json.api_base_url` (Zendesk is per-subdomain:
/// `https://your-subdomain.zendesk.com`).
pub const DEFAULT_API_BASE_URL: &str = "https://your-subdomain.zendesk.com";

/// Safety ceiling on number of incremental-export pages a single
/// sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// One Zendesk ticket (subset of fields used by the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZendeskTicket {
    /// Numeric ticket id.
    #[serde(default)]
    pub id: u64,
    /// One-line subject.
    #[serde(default)]
    pub subject: Option<String>,
    /// First-comment description body.
    #[serde(default)]
    pub description: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// One page of an incremental-export response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZendeskExportResponse {
    /// Tickets on this page.
    #[serde(default)]
    pub tickets: Vec<ZendeskTicket>,
    /// Unix-seconds high-water mark to use as the next `start_time`.
    #[serde(default)]
    pub end_time: Option<i64>,
    /// `true` once the export has caught up to "now".
    #[serde(default)]
    pub end_of_stream: bool,
}

/// Single-ticket response (`GET /api/v2/tickets/{id}.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZendeskTicketResponse {
    /// The ticket body.
    #[serde(default)]
    pub ticket: ZendeskTicket,
}

/// `POST /api/v2/webhooks` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZendeskWebhookResponse {
    /// Created webhook.
    #[serde(default)]
    pub webhook: ZendeskWebhookHandle,
}

/// The id-bearing portion of a webhook response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZendeskWebhookHandle {
    /// Zendesk webhook id.
    #[serde(default)]
    pub id: String,
}

/// One ticket-change event delivered by a Zendesk trigger.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZendeskWebhookEvent {
    /// Affected ticket id (Zendesk triggers may emit it as a string
    /// or a number depending on the body template).
    #[serde(default)]
    pub ticket_id: serde_json::Value,
    /// Event type, e.g. `ticket.created`, `ticket.updated`.
    #[serde(default, rename = "type")]
    pub event_type: String,
    /// RFC-3339 timestamp of the change.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Zendesk connector.
pub struct ZendeskConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
}

impl std::fmt::Debug for ZendeskConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZendeskConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ZendeskConnector {
    /// Construct a Zendesk connector.
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
        }
    }

    /// Override the Zendesk base URL (the subdomain URL).
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

    /// Walk the incremental-export stream from `start_time` (unix
    /// seconds) until `end_of_stream`. Returns the collected tickets
    /// plus the final `end_time` to persist as the next cursor.
    fn export_from(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        start_time: i64,
    ) -> Result<(Vec<ZendeskTicket>, Option<i64>)> {
        let mut tickets = Vec::<ZendeskTicket>::new();
        let mut cursor = start_time;
        let mut last_end_time: Option<i64> = None;
        for _ in 0..MAX_PAGES {
            let url = format!("{base_url}/api/v2/incremental/tickets.json?start_time={cursor}");
            let resp: ZendeskExportResponse = bearer_get_json(
                &self.transport,
                "zendesk",
                "/api/v2/incremental/tickets.json",
                &url,
                token,
                &[],
            )?;
            tickets.extend(resp.tickets);
            if let Some(end) = resp.end_time {
                last_end_time = Some(end);
                cursor = end;
            }
            if resp.end_of_stream || resp.end_time.is_none() {
                return Ok((tickets, last_end_time));
            }
        }
        Err(ConnectorError::Sync(format!(
            "zendesk incremental export exceeded {MAX_PAGES} pages without end_of_stream"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn ticket_watermark(t: &ZendeskTicket) -> Option<DateTime<Utc>> {
    t.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| t.created_at.as_deref().and_then(parse_rfc3339))
}

fn ticket_id_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for ZendeskConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "zendesk authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let (tickets, end_time) = self.export_from(&base_url, token, 0)?;
        let mut events = Vec::with_capacity(tickets.len());
        for ticket in &tickets {
            let occurred_at = ticket_watermark(ticket).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(ticket.id.to_string()),
                occurred_at,
            });
        }
        Ok(SyncRunResult {
            events,
            next_cursor: end_time.map(|t| t.to_string()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let start_time = state
            .cursor
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let (tickets, end_time) = self.export_from(&base_url, token, start_time)?;
        let mut events = Vec::with_capacity(tickets.len());
        for ticket in &tickets {
            let occurred_at = ticket_watermark(ticket).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(ticket.id.to_string()),
                occurred_at,
            });
        }
        Ok(SyncRunResult {
            events,
            // Preserve the prior cursor if the export produced no new
            // `end_time` (caught up with zero rows).
            next_cursor: end_time
                .map(|t| t.to_string())
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
        let url = format!("{base_url}/api/v2/tickets/{id}.json");
        let resp: ZendeskTicketResponse = bearer_get_json(
            &self.transport,
            "zendesk",
            "/api/v2/tickets/{id}.json",
            &url,
            token,
            &[],
        )?;
        let ticket = resp.ticket;
        let subject = ticket.subject.clone().unwrap_or_default();
        let description = ticket.description.clone().unwrap_or_default();
        let mut md = String::new();
        if !subject.is_empty() {
            md.push_str("# ");
            md.push_str(&subject);
            md.push_str("\n\n");
        }
        if !description.is_empty() {
            md.push_str(&description);
        }
        let body = md.trim_end().to_string();
        let source_url = format!("{base_url}/agent/tickets/{id}");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(subject)
            .with_metadata(serde_json::json!({
                "provider": "zendesk",
                "ticket_id": id,
                "updated_at": ticket.updated_at,
            }))
            .with_source_url(source_url))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let url = format!("{base_url}/api/v2/webhooks");
        let request = serde_json::json!({
            "webhook": {
                "name": "knowledge-substrate",
                "endpoint": callback_url,
                "http_method": "POST",
                "request_format": "json",
                "status": "active",
                "subscriptions": ["conditional_ticket_events"],
            }
        });
        let resp: ZendeskWebhookResponse = bearer_post_json(
            &self.transport,
            "zendesk",
            "/api/v2/webhooks",
            &url,
            token,
            &[],
            &request,
        )?;
        let provider_id = if resp.webhook.id.is_empty() {
            None
        } else {
            Some(resp.webhook.id)
        };
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("zendesk-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = provider_id;
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let events: Vec<ZendeskWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<ZendeskWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<ZendeskWebhookEvent>(body)?]
            };
        if events.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty zendesk webhook batch".into(),
            ));
        }
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            let id_str = ticket_id_to_string(&event.ticket_id).ok_or_else(|| {
                ConnectorError::Webhook("zendesk webhook event missing ticket_id".into())
            })?;
            let occurred_at = event
                .updated_at
                .as_deref()
                .and_then(parse_rfc3339)
                .unwrap_or_else(Utc::now);
            let id = SourceDocumentId::new(id_str);
            let event = if event.event_type.contains("created") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if event.event_type.contains("deleted") {
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
            out.push(event);
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
                "zd-access",
                "zd-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Zendesk, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "demo-code",
                "api_base_url": "https://api.test/zd",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn export_url(start: i64) -> String {
        format!("https://api.test/zd/api/v2/incremental/tickets.json?start_time={start}")
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::Zendesk, AuthKind::OAuth2, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "zd-access"
        );
    }

    #[test]
    fn initial_sync_walks_export_until_end_of_stream() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            export_url(0),
            ok_json(&serde_json::json!({
                "tickets": [
                    {"id": 1, "subject": "a", "created_at": "2024-01-01T00:00:00Z", "updated_at": "2024-01-01T00:00:00Z"},
                    {"id": 2, "subject": "b", "created_at": "2024-01-01T01:00:00Z", "updated_at": "2024-01-01T01:00:00Z"}
                ],
                "end_time": 1000,
                "end_of_stream": false
            })),
        );
        transport.expect(
            HttpMethod::Get,
            export_url(1000),
            ok_json(&serde_json::json!({
                "tickets": [
                    {"id": 3, "subject": "c", "created_at": "2024-01-02T00:00:00Z", "updated_at": "2024-01-02T00:00:00Z"}
                ],
                "end_time": 2000,
                "end_of_stream": true
            })),
        );
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("2000"));
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_starts_from_cursor() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            export_url(2000),
            ok_json(&serde_json::json!({
                "tickets": [
                    {"id": 9, "subject": "z", "updated_at": "2024-02-01T00:00:00Z"}
                ],
                "end_time": 3000,
                "end_of_stream": true
            })),
        );
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2000".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("3000"));
    }

    #[test]
    fn initial_sync_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            export_url(0),
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zd/api/v2/tickets/42.json".to_string(),
            ok_json(&serde_json::json!({
                "ticket": {
                    "id": 42,
                    "subject": "Login fails",
                    "description": "User cannot sign in.",
                    "updated_at": "2024-03-01T00:00:00Z"
                }
            })),
        );
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("42"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Login fails"));
        assert!(body.contains("User cannot sign in."));
        assert_eq!(fc.title.as_deref(), Some("Login fails"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zd/api/v2/tickets/x.json".to_string(),
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("x")),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn subscribe_webhook_creates_and_captures_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/zd/api/v2/webhooks".to_string(),
            ok_json(&serde_json::json!({"webhook": {"id": "whk_123"}})),
        );
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/zd")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("whk_123"));
    }

    #[test]
    fn subscribe_webhook_propagates_failure() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/zd/api/v2/webhooks".to_string(),
            MockResponse::status(422, b"unprocessable".to_vec()),
        );
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/zd")
            .is_err());
    }

    #[test]
    fn webhook_parses_batched_events() {
        let body = serde_json::json!([
            {"ticket_id": "1", "type": "ticket.created", "updated_at": "2024-01-01T00:00:00Z"},
            {"ticket_id": 2, "type": "ticket.updated", "updated_at": "2024-01-02T00:00:00Z"},
            {"ticket_id": "3", "type": "ticket.deleted", "updated_at": "2024-01-03T00:00:00Z"}
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_parses_single_object() {
        let body = serde_json::json!({"ticket_id": 7, "type": "ticket.updated"});
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentUpdated { .. }));
    }

    #[test]
    fn webhook_missing_ticket_id_errors() {
        let body = serde_json::json!({"type": "ticket.updated"});
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZendeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap()),
            Err(ConnectorError::Webhook(_))
        ));
    }
}
