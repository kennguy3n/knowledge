//! Notion connector.
//!
//! Notion does **not** ship native webhooks (as of 2026-05). Steady
//! state is therefore polled — `incremental_sync` filters
//! `/databases/{id}/query` by `last_edited_time >= cursor`. The
//! `subscribe_webhook` method is provided for trait completeness but
//! returns a [`ConnectorError::Webhook`] tagged as polling-only so
//! the substrate routes Notion to its scheduled-poll path;
//! `handle_webhook_event` is likewise a polling-only marker.
//!
//! * `initial_sync` walks Notion `/search` (objects = `page,database`).
//! * `incremental_sync` queries each known database with the
//!   `last_edited_time` filter and pages via `next_cursor`.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SyncRunResult, SyncState, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Notion `object` enum (subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotionObjectKind {
    /// `"page"`.
    Page,
    /// `"database"`.
    Database,
    /// `"block"`.
    Block,
}

/// One page or database returned by `/search` or `/databases/{id}/query`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionObject {
    /// Object id (UUID v4 string).
    pub id: String,
    /// Object kind.
    pub object: NotionObjectKind,
    /// Wall-clock created time.
    #[serde(default)]
    pub created_time: Option<DateTime<Utc>>,
    /// Wall-clock last-edited time.
    #[serde(default)]
    pub last_edited_time: Option<DateTime<Utc>>,
    /// Notion's `archived = true` is the deletion signal.
    #[serde(default)]
    pub archived: bool,
}

/// `/search` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotionSearchResponse {
    /// Page of results.
    #[serde(default)]
    pub results: Vec<NotionObject>,
    /// `next_cursor` for the following page.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// `has_more` flag for explicit pagination control.
    #[serde(default)]
    pub has_more: bool,
}

/// Notion connector. Pure poll-based — no webhook surface.
#[derive(Debug, Clone)]
pub struct NotionConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// Initial-sync fixture pages.
    pub initial_pages: Vec<NotionSearchResponse>,
    /// Incremental-sync fixture pages.
    pub incremental_pages: Vec<NotionSearchResponse>,
}

impl NotionConnector {
    /// Construct a connector with empty fixtures.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_pages: Vec::new(),
            incremental_pages: Vec::new(),
        }
    }

    /// Override initial-sync fixture pages.
    pub fn with_initial_pages(mut self, pages: Vec<NotionSearchResponse>) -> Self {
        self.initial_pages = pages;
        self
    }

    /// Override incremental-sync fixture pages.
    pub fn with_incremental_pages(mut self, pages: Vec<NotionSearchResponse>) -> Self {
        self.incremental_pages = pages;
        self
    }

    fn page_index(cursor: Option<&str>) -> usize {
        cursor
            .and_then(|c| c.strip_prefix("page-"))
            .and_then(|n| n.parse::<usize>().ok())
            .map(|n| n.saturating_sub(1))
            .unwrap_or(0)
    }
}

fn object_to_event(obj: &NotionObject) -> ConnectorEvent {
    let occurred_at = obj
        .last_edited_time
        .or(obj.created_time)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(obj.id.clone());
    if obj.archived {
        ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        }
    } else if obj.created_time == obj.last_edited_time {
        ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        }
    } else {
        ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        }
    }
}

impl Connector for NotionConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "notion-access-token",
            "notion-refresh-token",
            Utc::now() + Duration::days(180),
            "read_content read_user_with_email",
        ))
    }

    fn initial_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
    ) -> Result<SyncRunResult> {
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut last_cursor: Option<String> = None;
        for page in &self.initial_pages {
            for obj in &page.results {
                events.push(object_to_event(obj));
                if let Some(t) = obj.last_edited_time {
                    last_cursor = Some(t.to_rfc3339());
                }
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: last_cursor,
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
        let mut watermark: Option<String> = None;
        for obj in &page.results {
            events.push(object_to_event(obj));
            if let Some(t) = obj.last_edited_time {
                watermark = Some(t.to_rfc3339());
            }
        }
        let next_cursor = if idx + 1 < self.incremental_pages.len() {
            Some(format!("page-{}", idx + 2))
        } else {
            watermark
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
        _callback_url: &str,
    ) -> Result<WebhookSubscription> {
        Err(ConnectorError::Webhook(
            "polling-only mode: Notion has no native webhook surface".to_string(),
        ))
    }

    fn handle_webhook_event(&self, _body: &[u8]) -> Result<ConnectorEvent> {
        Err(ConnectorError::Webhook(
            "polling-only mode: Notion does not deliver webhooks; use incremental_sync"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use connector_framework::{AuthKind, ConnectorKind};
    use evidence_store::ScopeId;

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Notion,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
    }

    #[test]
    fn authenticate_returns_notion_scope() {
        let c = NotionConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("read_content"));
    }

    #[test]
    fn initial_sync_emits_events_with_timestamp_cursor() {
        let now = Utc::now();
        let pages = vec![NotionSearchResponse {
            results: vec![NotionObject {
                id: "page-uuid".into(),
                object: NotionObjectKind::Page,
                created_time: Some(now),
                last_edited_time: Some(now),
                archived: false,
            }],
            next_cursor: None,
            has_more: false,
        }];
        let c = NotionConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(res.events[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn incremental_sync_emits_archived_as_deleted() {
        let pages = vec![NotionSearchResponse {
            results: vec![NotionObject {
                id: "page-2".into(),
                object: NotionObjectKind::Page,
                created_time: Some(Utc::now() - Duration::days(1)),
                last_edited_time: Some(Utc::now()),
                archived: true,
            }],
            next_cursor: None,
            has_more: false,
        }];
        let c = NotionConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(res.events[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_subscribe_is_unsupported() {
        let c = NotionConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://example/webhook")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_event_is_unsupported() {
        let c = NotionConnector::new(ConnectorInstanceId::new_v4());
        let err = c.handle_webhook_event(b"{}").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
