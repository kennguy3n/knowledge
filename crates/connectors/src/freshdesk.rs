//! Freshdesk connector — Freshdesk REST API v2 (`/api/v2`).
//!
//! * `initial_sync` pages `/api/v2/tickets?page=N&per_page=100`,
//!   stopping when a short page (fewer than `per_page` rows) is
//!   returned.
//! * `incremental_sync` adds Freshdesk's `updated_since` filter keyed
//!   off the stored RFC-3339 watermark; `updated_since` is inclusive,
//!   so the boundary row is deduped client-side.
//! * `fetch_content` GETs the single ticket
//!   (`/api/v2/tickets/{id}`) and reconstructs Markdown from
//!   `subject` + `description_text`.
//! * Freshdesk has no REST endpoint to create webhooks (they are
//!   configured through UI automations), so `subscribe_webhook`
//!   records a polling-only subscription with no provider id.
//! * `handle_webhook_event` parses the automation-delivered payload
//!   (single object or batched array).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WatermarkCursor,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Freshdesk base URL. Freshdesk is per-domain
/// (`https://yourdomain.freshdesk.com`); override via
/// `auth_config_json.api_base_url`.
pub const DEFAULT_API_BASE_URL: &str = "https://yourdomain.freshdesk.com";

/// Page size for ticket listing (`per_page`).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk.
pub const MAX_PAGES: usize = 100_000;

/// One Freshdesk ticket (subset of fields used by the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FreshdeskTicket {
    /// Numeric ticket id.
    #[serde(default)]
    pub id: u64,
    /// One-line subject.
    #[serde(default)]
    pub subject: Option<String>,
    /// Plain-text description.
    #[serde(default)]
    pub description_text: Option<String>,
    /// RFC-3339 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Freshdesk webhook delivery (automation-defined payload).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FreshdeskWebhookEvent {
    /// Affected ticket id (Freshdesk automations may emit it as a
    /// string or a number).
    #[serde(default)]
    pub ticket_id: serde_json::Value,
    /// Optional event label, e.g. `ticket_created`, `ticket_updated`.
    #[serde(default)]
    pub event: String,
}

/// Freshdesk connector.
pub struct FreshdeskConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for FreshdeskConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FreshdeskConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl FreshdeskConnector {
    /// Construct a Freshdesk connector.
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

    /// Override the Freshdesk base URL.
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

    /// Walk the ticket list page-by-page, stopping on a short page.
    fn paginate_tickets(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        updated_since: Option<&str>,
    ) -> Result<Vec<FreshdeskTicket>> {
        let mut tickets = Vec::<FreshdeskTicket>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!(
                "{base_url}/api/v2/tickets?per_page={}&page={page}",
                self.page_size
            );
            if let Some(since) = updated_since {
                url.push_str("&updated_since=");
                url.push_str(&percent_encode_path_component(since));
            }
            let page_rows: Vec<FreshdeskTicket> = bearer_get_json(
                &self.transport,
                "freshdesk",
                "/api/v2/tickets",
                &url,
                token,
                &[],
            )?;
            let count = page_rows.len();
            tickets.extend(page_rows);
            if count < self.page_size as usize {
                return Ok(tickets);
            }
        }
        Err(ConnectorError::Sync(format!(
            "freshdesk /tickets exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn ticket_watermark(t: &FreshdeskTicket) -> Option<DateTime<Utc>> {
    t.updated_at
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| t.created_at.as_deref().and_then(parse_rfc3339))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for FreshdeskConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "freshdesk authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let tickets = self.paginate_tickets(&base_url, token, None)?;
        let mut events = Vec::with_capacity(tickets.len());
        let mut cursor = WatermarkCursor::empty();
        for ticket in &tickets {
            let occurred_at = ticket_watermark(ticket).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(ticket.id.to_string()),
                occurred_at,
            });
            if let Some(t) = ticket_watermark(ticket) {
                cursor.observe(t, &ticket.id.to_string());
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
        let base_url = self.resolved_base_url(config);
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let since = prior.query_since();
        let tickets = self.paginate_tickets(&base_url, token, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for ticket in &tickets {
            let Some(updated) = ticket_watermark(ticket) else {
                continue;
            };
            if !prior.should_emit(updated, &ticket.id.to_string()) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(ticket.id.to_string()),
                occurred_at: updated,
            });
            cursor.observe(updated, &ticket.id.to_string());
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
        let base_url = self.resolved_base_url(config);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base_url}/api/v2/tickets/{id_enc}");
        let ticket: FreshdeskTicket = bearer_get_json(
            &self.transport,
            "freshdesk",
            "/api/v2/tickets/{id}",
            &url,
            token,
            &[],
        )?;
        let subject = ticket.subject.clone().unwrap_or_default();
        let description = ticket.description_text.clone().unwrap_or_default();
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
        let source_url = format!("{base_url}/a/tickets/{id}");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(subject)
            .with_metadata(serde_json::json!({
                "provider": "freshdesk",
                "ticket_id": id,
                "updated_at": ticket.updated_at,
            }))
            .with_source_url(source_url))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Freshdesk exposes no API to create webhooks — they are set
        // up as UI automations. Record a polling-only subscription so
        // the runtime falls back to incremental_sync.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("freshdesk-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<FreshdeskWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<FreshdeskWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<FreshdeskWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty freshdesk webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.ticket_id).ok_or_else(|| {
                ConnectorError::Webhook("freshdesk webhook event missing ticket_id".into())
            })?;
            let occurred_at = Utc::now();
            let id = SourceDocumentId::new(id_str);
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("delete") {
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
                "fd-access",
                "fd-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Freshdesk,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/fd",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    // Small page size keeps pagination tests compact.
    fn small(c: FreshdeskConnector) -> FreshdeskConnector {
        c.with_page_size(2)
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FreshdeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(
            ConnectorKind::Freshdesk,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        );
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FreshdeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "fd-access"
        );
    }

    #[test]
    fn initial_sync_paginates_until_short_page() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/fd/api/v2/tickets?per_page=2&page=1".to_string(),
            ok_json(&serde_json::json!([
                {"id": 1, "subject": "a", "updated_at": "2024-01-01T00:00:00Z"},
                {"id": 2, "subject": "b", "updated_at": "2024-01-02T00:00:00Z"}
            ])),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/fd/api/v2/tickets?per_page=2&page=2".to_string(),
            ok_json(&serde_json::json!([
                {"id": 3, "subject": "c", "updated_at": "2024-01-03T00:00:00Z"}
            ])),
        );
        let c = small(FreshdeskConnector::new(
            ConnectorInstanceId::new_v4(),
            transport.clone(),
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|3")
        );
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_uses_updated_since_and_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/fd/api/v2/tickets?per_page=2&page=1&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!([
                {"id": 10, "updated_at": "2024-03-01T00:00:00Z"},
                {"id": 13, "updated_at": "2024-03-01T00:00:00Z"}
            ])),
        );
        // Page 1 came back full (== per_page), so pagination requests
        // a second page; a short/empty page terminates the walk.
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/fd/api/v2/tickets?per_page=2&page=2&updated_since={}",
                percent_encode_path_component(since)
            ),
            ok_json(&serde_json::json!([ {"id": 11, "updated_at": "2024-06-01T00:00:00Z"} ])),
        );
        let c = small(FreshdeskConnector::new(
            ConnectorInstanceId::new_v4(),
            transport,
            oauth(),
        ));
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must NOT
        // re-emit `10`, still surface the brand-new `13` at the same second,
        // and advance past the later row.
        state.cursor = Some(format!("{since}|10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["13", "11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|11")
        );
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/fd/api/v2/tickets/55".to_string(),
            ok_json(&serde_json::json!({
                "id": 55,
                "subject": "Cannot log in",
                "description_text": "Password reset loops.",
                "updated_at": "2024-03-01T00:00:00Z"
            })),
        );
        let c = FreshdeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Cannot log in"));
        assert!(body.contains("Password reset loops."));
        assert_eq!(fc.title.as_deref(), Some("Cannot log in"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FreshdeskConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/fd")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        // No HTTP call is made for a polling-only subscription.
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FreshdeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(
                &serde_json::to_vec(
                    &serde_json::json!({"ticket_id": 7, "event": "ticket_updated"}),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(single.len(), 1);
        assert!(matches!(single[0], ConnectorEvent::DocumentUpdated { .. }));
        let batch = c
            .handle_webhook_event(
                &serde_json::to_vec(&serde_json::json!([
                    {"ticket_id": "1", "event": "ticket_created"},
                    {"ticket_id": "2", "event": "ticket_deleted"}
                ]))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(batch[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_missing_ticket_id_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FreshdeskConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert!(matches!(
            c.handle_webhook_event(
                &serde_json::to_vec(&serde_json::json!({"event": "ticket_updated"})).unwrap()
            ),
            Err(ConnectorError::Webhook(_))
        ));
    }
}
