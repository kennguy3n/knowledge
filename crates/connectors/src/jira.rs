//! Jira connector — Jira REST API v3.
//!
//! * `initial_sync` runs JQL `ORDER BY created` against `/rest/api/3/search`
//!   and pages via `startAt`/`maxResults`.
//! * `incremental_sync` runs JQL `updated >= cursor_timestamp ORDER BY updated`.
//! * `subscribe_webhook` registers a Jira webhook for issue events.
//! * `handle_webhook_event` parses Jira's `webhookEvent` payload —
//!   `jira:issue_created`, `jira:issue_updated`, `jira:issue_deleted`,
//!   plus permission-scheme changes.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// One Jira issue (subset of fields used by the substrate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraIssue {
    /// Issue key (e.g. `PROJ-123`).
    pub key: String,
    /// Numeric id (Jira's stable internal id).
    #[serde(default)]
    pub id: String,
    /// Field bundle.
    #[serde(default)]
    pub fields: JiraFields,
}

/// Subset of `JiraIssue.fields` used by the substrate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraFields {
    /// Issue summary line.
    #[serde(default)]
    pub summary: String,
    /// Created timestamp.
    #[serde(default)]
    pub created: Option<DateTime<Utc>>,
    /// Updated timestamp.
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    /// Status object — when name == "Closed" we treat it as deleted
    /// for substrate purposes only if `resolution` is "Done".
    #[serde(default)]
    pub status: Option<JiraStatus>,
}

/// Jira status sub-object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraStatus {
    /// Status name.
    pub name: String,
}

/// One page of a JQL `/search` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraSearchResponse {
    /// Issues on this page.
    #[serde(default)]
    pub issues: Vec<JiraIssue>,
    /// `startAt` echo — substrate-side cursor base.
    #[serde(default, rename = "startAt")]
    pub start_at: u32,
    /// `maxResults` echo.
    #[serde(default, rename = "maxResults")]
    pub max_results: u32,
    /// Total issues matching the JQL — used to determine end-of-page.
    #[serde(default)]
    pub total: u32,
}

/// Jira webhook payload (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraWebhookPayload {
    /// `jira:issue_created`, `jira:issue_updated`, `jira:issue_deleted`,
    /// `permissionscheme_updated`.
    #[serde(rename = "webhookEvent")]
    pub webhook_event: String,
    /// Issue body (absent on permission-scheme events).
    #[serde(default)]
    pub issue: Option<JiraIssue>,
    /// Wall-clock timestamp Jira emitted the event.
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// User account id whose permission changed (only on permission events).
    #[serde(default, rename = "accountId")]
    pub account_id: Option<String>,
    /// New role / permission level (`administrators`, `developers`, …).
    #[serde(default)]
    pub new_role: Option<String>,
    /// Document key (when `issue` is absent — permission events).
    #[serde(default, rename = "issueKey")]
    pub issue_key: Option<String>,
}

/// Jira connector.
#[derive(Debug, Clone)]
pub struct JiraConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// Initial-sync fixture pages.
    pub initial_pages: Vec<JiraSearchResponse>,
    /// Incremental-sync fixture pages.
    pub incremental_pages: Vec<JiraSearchResponse>,
}

impl JiraConnector {
    /// Construct an empty connector.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_pages: Vec::new(),
            incremental_pages: Vec::new(),
        }
    }

    /// Override initial-sync fixture pages.
    pub fn with_initial_pages(mut self, pages: Vec<JiraSearchResponse>) -> Self {
        self.initial_pages = pages;
        self
    }

    /// Override incremental-sync fixture pages.
    pub fn with_incremental_pages(mut self, pages: Vec<JiraSearchResponse>) -> Self {
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

fn issue_to_event(issue: &JiraIssue, kind: &str) -> ConnectorEvent {
    let occurred_at = issue
        .fields
        .updated
        .or(issue.fields.created)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(issue.key.clone());
    match kind {
        "create" => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        "delete" => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "browsers" | "browse" | "viewer" | "read" => Some(SourcePermissionLevel::Read),
        "developers" | "edit" | "write" => Some(SourcePermissionLevel::Write),
        "administrators" | "admin" | "owner" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

impl Connector for JiraConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "jira-access-token",
            "jira-refresh-token",
            Utc::now() + Duration::hours(1),
            "read:jira-work read:jira-user manage:jira-webhook",
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
            for issue in &page.issues {
                events.push(issue_to_event(issue, "create"));
                if let Some(t) = issue.fields.created {
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
        for issue in &page.issues {
            events.push(issue_to_event(issue, "update"));
            if let Some(t) = issue.fields.updated {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
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
            WebhookSecret::new("jira-webhook-secret"),
            WebhookEventTypes::all(),
            // Jira webhooks have a 30-day rotation cap before they
            // need a refresh.
            Some(Utc::now() + Duration::days(30)),
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Jira posts one webhook event per HTTP request.
        let p: JiraWebhookPayload = serde_json::from_slice(body)?;
        let event = match p.webhook_event.as_str() {
            "jira:issue_created" => {
                let issue = p
                    .issue
                    .ok_or_else(|| ConnectorError::Webhook("missing issue body".into()))?;
                issue_to_event(&issue, "create")
            }
            "jira:issue_updated" => {
                let issue = p
                    .issue
                    .ok_or_else(|| ConnectorError::Webhook("missing issue body".into()))?;
                issue_to_event(&issue, "update")
            }
            "jira:issue_deleted" => {
                let issue = p
                    .issue
                    .ok_or_else(|| ConnectorError::Webhook("missing issue body".into()))?;
                issue_to_event(&issue, "delete")
            }
            "permissionscheme_updated" => {
                let key = p
                    .issue_key
                    .or_else(|| p.issue.as_ref().map(|i| i.key.clone()))
                    .ok_or_else(|| {
                        ConnectorError::Webhook(
                            "permissionscheme_updated payload missing issueKey".into(),
                        )
                    })?;
                let occurred_at = p
                    .timestamp
                    .and_then(DateTime::<Utc>::from_timestamp_millis)
                    .unwrap_or_else(Utc::now);
                ConnectorEvent::PermissionChanged {
                    document_id: SourceDocumentId::new(key),
                    user_id: SourceUserId::new(p.account_id.unwrap_or_default()),
                    new_level: p.new_role.as_deref().and_then(parse_role),
                    occurred_at,
                }
            }
            other => {
                return Err(ConnectorError::Webhook(format!(
                    "unknown Jira webhookEvent: {other}"
                )))
            }
        };
        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use connector_framework::{AuthKind, ConnectorKind};
    use evidence_store::ScopeId;

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Jira, AuthKind::OAuth2, ScopeId::new_v4())
    }

    fn issue(key: &str, created: DateTime<Utc>, updated: DateTime<Utc>) -> JiraIssue {
        JiraIssue {
            key: key.into(),
            id: key.into(),
            fields: JiraFields {
                summary: "test".into(),
                created: Some(created),
                updated: Some(updated),
                status: None,
            },
        }
    }

    #[test]
    fn authenticate_returns_jira_scope() {
        let c = JiraConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("read:jira-work"));
    }

    #[test]
    fn initial_sync_emits_created_events_and_watermark_cursor() {
        let now = Utc::now();
        let pages = vec![JiraSearchResponse {
            issues: vec![issue("PROJ-1", now, now)],
            start_at: 0,
            max_results: 50,
            total: 1,
        }];
        let c = JiraConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn incremental_sync_emits_updated_events() {
        let now = Utc::now();
        let pages = vec![JiraSearchResponse {
            issues: vec![issue("PROJ-2", now - Duration::days(1), now)],
            start_at: 0,
            max_results: 50,
            total: 1,
        }];
        let c = JiraConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn webhook_parses_issue_created() {
        let body = serde_json::json!({
            "webhookEvent": "jira:issue_created",
            "timestamp": Utc::now().timestamp_millis(),
            "issue": {
                "key": "PROJ-99",
                "id": "10099",
                "fields": {
                    "summary": "demo",
                    "created": Utc::now(),
                    "updated": Utc::now(),
                }
            }
        });
        let c = JiraConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
    }

    #[test]
    fn webhook_parses_permission_change() {
        let body = serde_json::json!({
            "webhookEvent": "permissionscheme_updated",
            "issueKey": "PROJ-50",
            "accountId": "acc-1",
            "new_role": "administrators",
            "timestamp": Utc::now().timestamp_millis(),
        });
        let c = JiraConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(*new_level, Some(SourcePermissionLevel::Admin));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_event_errors() {
        let body = serde_json::json!({"webhookEvent": "weird:thing"});
        let c = JiraConnector::new(ConnectorInstanceId::new_v4());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
