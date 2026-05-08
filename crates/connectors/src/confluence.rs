//! Confluence connector — Confluence REST API.
//!
//! * `initial_sync` walks `/wiki/rest/api/content?expand=body.storage`
//!   with `start`/`limit` pagination.
//! * `incremental_sync` filters by `lastModified` (CQL
//!   `lastModified > "<cursor>"`).
//! * `subscribe_webhook` registers Confluence webhooks for
//!   `page_created`, `page_updated`, `page_removed`, and
//!   `space_permissions_updated`.
//! * `handle_webhook_event` parses Atlassian's webhook envelope.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Confluence content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    /// Wiki page.
    Page,
    /// Blog post.
    Blogpost,
    /// Comment.
    Comment,
    /// Attachment.
    Attachment,
}

/// Lifecycle status of a Confluence content row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentStatus {
    /// Live, indexable.
    Current,
    /// Soft-deleted (in trash).
    Trashed,
    /// Permanently deleted.
    Deleted,
    /// Draft, not published.
    Draft,
}

/// Confluence content metadata (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceContent {
    /// Content id.
    pub id: String,
    /// Content type.
    #[serde(rename = "type")]
    pub kind: ContentType,
    /// Title.
    #[serde(default)]
    pub title: String,
    /// Status.
    pub status: ContentStatus,
    /// History block — carries `createdDate`.
    #[serde(default)]
    pub history: Option<ConfluenceHistory>,
    /// Version block — carries `when` (last modified).
    #[serde(default)]
    pub version: Option<ConfluenceVersion>,
}

/// Confluence history sub-object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceHistory {
    /// Created date.
    #[serde(rename = "createdDate")]
    pub created_date: DateTime<Utc>,
}

/// Confluence version sub-object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceVersion {
    /// Last-modified timestamp.
    pub when: DateTime<Utc>,
    /// Version number.
    #[serde(default)]
    pub number: u32,
}

/// One page of `/content` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfluenceContentList {
    /// Results on this page.
    #[serde(default)]
    pub results: Vec<ConfluenceContent>,
    /// `start` echo (substrate-side cursor base).
    #[serde(default)]
    pub start: u32,
    /// `limit` echo.
    #[serde(default)]
    pub limit: u32,
    /// Total result count.
    #[serde(default)]
    pub size: u32,
}

/// One Confluence webhook envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceWebhookPayload {
    /// `page_created`, `page_updated`, `page_removed`,
    /// `space_permissions_updated`.
    #[serde(rename = "webhookEvent")]
    pub webhook_event: String,
    /// Affected content.
    #[serde(default)]
    pub page: Option<ConfluenceContent>,
    /// Wall-clock event time (millis since epoch).
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// User account id whose permission changed.
    #[serde(default, rename = "accountId")]
    pub account_id: Option<String>,
    /// New permission level.
    #[serde(default)]
    pub new_role: Option<String>,
    /// Affected content id (used on permission events).
    #[serde(default, rename = "contentId")]
    pub content_id: Option<String>,
}

/// Confluence connector.
#[derive(Debug, Clone)]
pub struct ConfluenceConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// Initial-sync fixture pages.
    pub initial_pages: Vec<ConfluenceContentList>,
    /// Incremental-sync fixture pages.
    pub incremental_pages: Vec<ConfluenceContentList>,
}

impl ConfluenceConnector {
    /// Construct an empty connector.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_pages: Vec::new(),
            incremental_pages: Vec::new(),
        }
    }

    /// Override initial-sync fixture pages.
    pub fn with_initial_pages(mut self, pages: Vec<ConfluenceContentList>) -> Self {
        self.initial_pages = pages;
        self
    }

    /// Override incremental-sync fixture pages.
    pub fn with_incremental_pages(mut self, pages: Vec<ConfluenceContentList>) -> Self {
        self.incremental_pages = pages;
        self
    }

    fn page_index(cursor: Option<&str>) -> usize {
        cursor
            .and_then(|c| c.strip_prefix("page-"))
            .and_then(|n| n.parse::<usize>().ok())
            .map_or(0, |n| n.saturating_sub(1))
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "view" | "read" | "viewer" => Some(SourcePermissionLevel::Read),
        "edit" | "write" | "contributor" => Some(SourcePermissionLevel::Write),
        "admin" | "administrator" | "owner" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

fn content_to_event(c: &ConfluenceContent) -> ConnectorEvent {
    let occurred_at = c
        .version
        .as_ref()
        .map(|v| v.when)
        .or_else(|| c.history.as_ref().map(|h| h.created_date))
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(c.id.clone());
    match c.status {
        ContentStatus::Trashed | ContentStatus::Deleted => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
        ContentStatus::Current | ContentStatus::Draft => {
            // First version → created; otherwise updated.
            let version_number = c.version.as_ref().map_or(1, |v| v.number);
            if version_number <= 1 {
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
    }
}

impl Connector for ConfluenceConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "confluence-access-token",
            "confluence-refresh-token",
            Utc::now() + Duration::hours(1),
            "read:confluence-content.all read:confluence-space.summary",
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
            for c in &page.results {
                events.push(content_to_event(c));
                let modified = c
                    .version
                    .as_ref()
                    .map(|v| v.when)
                    .or_else(|| c.history.as_ref().map(|h| h.created_date));
                if let Some(t) = modified {
                    watermark = Some(watermark.map_or(t, |w| w.max(t)));
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
        for c in &page.results {
            events.push(content_to_event(c));
            if let Some(v) = &c.version {
                watermark = Some(watermark.map_or(v.when, |w| w.max(v.when)));
            }
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
            WebhookSecret::new("confluence-webhook-secret"),
            WebhookEventTypes::all(),
            // Confluence webhooks have no provider-side TTL; we
            // refresh at most monthly.
            Some(Utc::now() + Duration::days(30)),
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<ConnectorEvent> {
        let p: ConfluenceWebhookPayload = serde_json::from_slice(body)?;
        let occurred_at = p
            .timestamp
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .unwrap_or_else(Utc::now);
        match p.webhook_event.as_str() {
            "page_created" => {
                let c = p
                    .page
                    .ok_or_else(|| ConnectorError::Webhook("missing page body".into()))?;
                Ok(ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new(c.id),
                    occurred_at,
                })
            }
            "page_updated" => {
                let c = p
                    .page
                    .ok_or_else(|| ConnectorError::Webhook("missing page body".into()))?;
                Ok(ConnectorEvent::DocumentUpdated {
                    document_id: SourceDocumentId::new(c.id),
                    occurred_at,
                })
            }
            "page_removed" | "page_trashed" => {
                let c = p
                    .page
                    .ok_or_else(|| ConnectorError::Webhook("missing page body".into()))?;
                Ok(ConnectorEvent::DocumentDeleted {
                    document_id: SourceDocumentId::new(c.id),
                    occurred_at,
                })
            }
            "space_permissions_updated" => {
                let id = p
                    .content_id
                    .or_else(|| p.page.as_ref().map(|c| c.id.clone()))
                    .ok_or_else(|| {
                        ConnectorError::Webhook(
                            "permission event missing contentId / page.id".into(),
                        )
                    })?;
                Ok(ConnectorEvent::PermissionChanged {
                    document_id: SourceDocumentId::new(id),
                    user_id: SourceUserId::new(p.account_id.unwrap_or_default()),
                    new_level: p.new_role.as_deref().and_then(parse_role),
                    occurred_at,
                })
            }
            other => Err(ConnectorError::Webhook(format!(
                "unknown Confluence webhookEvent: {other}"
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
        ConnectorConfig::new(
            ConnectorKind::Confluence,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
    }

    fn page(id: &str, version: u32, when: DateTime<Utc>) -> ConfluenceContent {
        ConfluenceContent {
            id: id.into(),
            kind: ContentType::Page,
            title: "Doc".into(),
            status: ContentStatus::Current,
            history: Some(ConfluenceHistory { created_date: when }),
            version: Some(ConfluenceVersion {
                when,
                number: version,
            }),
        }
    }

    #[test]
    fn authenticate_returns_confluence_scope() {
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("confluence-content"));
    }

    #[test]
    fn initial_sync_emits_created_for_v1() {
        let now = Utc::now();
        let pages = vec![ConfluenceContentList {
            results: vec![page("c1", 1, now)],
            start: 0,
            limit: 25,
            size: 1,
        }];
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn incremental_sync_emits_updated_for_v2_plus() {
        let now = Utc::now();
        let pages = vec![ConfluenceContentList {
            results: vec![page("c2", 3, now)],
            start: 0,
            limit: 25,
            size: 1,
        }];
        let c =
            ConfluenceConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn webhook_parses_page_removed() {
        let body = serde_json::json!({
            "webhookEvent": "page_removed",
            "timestamp": Utc::now().timestamp_millis(),
            "page": page("c9", 5, Utc::now()),
        });
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4());
        let ev = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert!(matches!(ev, ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_parses_space_permission_change() {
        let body = serde_json::json!({
            "webhookEvent": "space_permissions_updated",
            "timestamp": Utc::now().timestamp_millis(),
            "contentId": "c-12",
            "accountId": "acc-7",
            "new_role": "edit",
        });
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4());
        let ev = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        match ev {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_event_errors() {
        let body = serde_json::json!({"webhookEvent": "weird"});
        let c = ConfluenceConnector::new(ConnectorInstanceId::new_v4());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
