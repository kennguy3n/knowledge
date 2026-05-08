//! Google Drive connector — Drive API v3.
//!
//! * `initial_sync` walks the `files.list` endpoint with pagination
//!   keyed off `nextPageToken`.
//! * `incremental_sync` uses the Changes API
//!   (`changes.list?pageToken=…`) — `startPageToken` plays the role
//!   of the substrate-side cursor.
//! * `subscribe_webhook` installs a Drive *push notification channel*
//!   targeting `callback_url`.
//! * `handle_webhook_event` parses Drive's resource-state push
//!   payload (`X-Goog-Resource-State` body, plus a JSON resource
//!   description) into a [`ConnectorEvent`].
//!
//! The trait is synchronous; vendor I/O is mocked through fixture
//! JSON so the substrate can unit-test the contract end-to-end.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// One file as returned by Drive `files.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleDriveFile {
    /// Drive file id.
    pub id: String,
    /// File name (display).
    #[serde(default)]
    pub name: String,
    /// MIME type (e.g. `application/pdf`).
    #[serde(default, rename = "mimeType")]
    pub mime_type: String,
    /// Trashed flag — when `true` the file is in the user's trash.
    #[serde(default)]
    pub trashed: bool,
    /// RFC-3339 timestamp Drive last modified the file.
    #[serde(default, rename = "modifiedTime")]
    pub modified_time: Option<DateTime<Utc>>,
    /// RFC-3339 timestamp Drive created the file.
    #[serde(default, rename = "createdTime")]
    pub created_time: Option<DateTime<Utc>>,
}

/// One page of `files.list` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleDriveFileList {
    /// File records on this page.
    #[serde(default)]
    pub files: Vec<GoogleDriveFile>,
    /// Token for the next page; absent on the final page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    /// Token to seed `incremental_sync` with — Drive returns this on
    /// the final page so callers know what `startPageToken` to use.
    #[serde(default, rename = "newStartPageToken")]
    pub new_start_page_token: Option<String>,
}

/// One change record from Drive's Changes API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleDriveChange {
    /// File id of the changed resource.
    #[serde(rename = "fileId")]
    pub file_id: String,
    /// Change kind: `"file"` or `"drive"`.
    #[serde(default)]
    pub kind: String,
    /// `removed = true` is Drive's deletion signal.
    #[serde(default)]
    pub removed: bool,
    /// File body — present unless `removed`.
    #[serde(default)]
    pub file: Option<GoogleDriveFile>,
    /// RFC-3339 timestamp the change was recorded.
    #[serde(default, rename = "time")]
    pub time: Option<DateTime<Utc>>,
}

/// One page of `changes.list` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleDriveChangeList {
    /// Change records on this page, in source order.
    #[serde(default)]
    pub changes: Vec<GoogleDriveChange>,
    /// Token for the next page; absent on the final page.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    /// Token to seed the next incremental run with.
    #[serde(default, rename = "newStartPageToken")]
    pub new_start_page_token: Option<String>,
}

/// One Drive push-notification body. The relevant headers
/// (`X-Goog-Resource-State`, `X-Goog-Resource-Id`) are inlined into
/// the JSON body for substrate-side parity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleDrivePushNotification {
    /// Resource id the change concerns (Drive file id).
    #[serde(rename = "resourceId")]
    pub resource_id: String,
    /// `add`, `update`, `remove`, or `permission_change`.
    #[serde(rename = "resourceState")]
    pub resource_state: String,
    /// `targetUserEmail` or similar — only set for permission events.
    #[serde(default, rename = "userId")]
    pub user_id: Option<String>,
    /// New permission role (`reader`, `writer`, `owner`); `None` on
    /// revocation.
    #[serde(default, rename = "newRole")]
    pub new_role: Option<String>,
    /// RFC-3339 wall-clock time Drive emitted the notification.
    #[serde(default, rename = "occurredAt")]
    pub occurred_at: Option<DateTime<Utc>>,
}

/// Google Drive connector.
///
/// Holds fixture pages (initial + incremental) keyed by index — the
/// trait methods walk these in order and emit
/// [`ConnectorEvent`]s.
#[derive(Debug, Clone)]
pub struct GoogleDriveConnector {
    /// Connector instance id (used by `subscribe_webhook`).
    pub instance: ConnectorInstanceId,
    /// First-pass fixture pages. `initial_sync` walks every page in
    /// order and emits one event per file.
    pub initial_pages: Vec<GoogleDriveFileList>,
    /// Incremental fixture pages — `incremental_sync` reads exactly
    /// one page per call, using the substrate-side cursor
    /// `page-N` as a 1-based index.
    pub incremental_pages: Vec<GoogleDriveChangeList>,
}

impl GoogleDriveConnector {
    /// Construct a connector with no fixture data — every sync call
    /// will return zero events.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_pages: Vec::new(),
            incremental_pages: Vec::new(),
        }
    }

    /// Override the initial-sync fixture pages.
    pub fn with_initial_pages(mut self, pages: Vec<GoogleDriveFileList>) -> Self {
        self.initial_pages = pages;
        self
    }

    /// Override the incremental-sync fixture pages.
    pub fn with_incremental_pages(mut self, pages: Vec<GoogleDriveChangeList>) -> Self {
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
        "reader" | "viewer" => Some(SourcePermissionLevel::Read),
        "writer" | "commenter" => Some(SourcePermissionLevel::Write),
        "owner" | "organizer" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

impl Connector for GoogleDriveConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "drive-access-token",
            "drive-refresh-token",
            Utc::now() + Duration::hours(1),
            "https://www.googleapis.com/auth/drive.readonly",
        ))
    }

    fn initial_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
    ) -> Result<SyncRunResult> {
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut seed_token: Option<String> = None;
        for page in &self.initial_pages {
            for f in &page.files {
                if f.trashed {
                    events.push(ConnectorEvent::DocumentDeleted {
                        document_id: SourceDocumentId::new(f.id.clone()),
                        occurred_at: f.modified_time.unwrap_or_else(Utc::now),
                    });
                } else {
                    events.push(ConnectorEvent::DocumentCreated {
                        document_id: SourceDocumentId::new(f.id.clone()),
                        occurred_at: f.created_time.or(f.modified_time).unwrap_or_else(Utc::now),
                    });
                }
            }
            if page.new_start_page_token.is_some() {
                seed_token.clone_from(&page.new_start_page_token);
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: seed_token,
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
        for ch in page.changes {
            let occurred_at = ch.time.unwrap_or_else(Utc::now);
            let id = SourceDocumentId::new(ch.file_id);
            if ch.removed || ch.file.as_ref().is_some_and(|f| f.trashed) {
                events.push(ConnectorEvent::DocumentDeleted {
                    document_id: id,
                    occurred_at,
                });
            } else {
                events.push(ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                });
            }
        }
        let next_cursor = if idx + 1 < self.incremental_pages.len() {
            Some(format!("page-{}", idx + 2))
        } else {
            page.new_start_page_token
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
        // Drive push channels are valid for 7 days.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new("google-drive-channel-secret"),
            WebhookEventTypes::all(),
            Some(Utc::now() + Duration::days(7)),
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<ConnectorEvent> {
        let push: GoogleDrivePushNotification = serde_json::from_slice(body)?;
        let occurred_at = push.occurred_at.unwrap_or_else(Utc::now);
        let document_id = SourceDocumentId::new(push.resource_id);
        match push.resource_state.as_str() {
            "add" | "create" => Ok(ConnectorEvent::DocumentCreated {
                document_id,
                occurred_at,
            }),
            "update" | "change" => Ok(ConnectorEvent::DocumentUpdated {
                document_id,
                occurred_at,
            }),
            "remove" | "trash" => Ok(ConnectorEvent::DocumentDeleted {
                document_id,
                occurred_at,
            }),
            "permission_change" => Ok(ConnectorEvent::PermissionChanged {
                document_id,
                user_id: SourceUserId::new(push.user_id.unwrap_or_default()),
                new_level: push.new_role.as_deref().and_then(parse_role),
                occurred_at,
            }),
            other => Err(ConnectorError::Webhook(format!(
                "unknown drive resource state: {other}"
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
            ConnectorKind::GoogleDrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
    }

    #[test]
    fn authenticate_returns_drive_scope() {
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("drive"));
    }

    #[test]
    fn initial_sync_emits_created_events_and_seeds_cursor() {
        let pages = vec![GoogleDriveFileList {
            files: vec![GoogleDriveFile {
                id: "f1".into(),
                name: "Doc".into(),
                mime_type: "application/vnd.google-apps.document".into(),
                trashed: false,
                modified_time: Some(Utc::now()),
                created_time: Some(Utc::now()),
            }],
            next_page_token: None,
            new_start_page_token: Some("seed-1".into()),
        }];
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("seed-1"));
    }

    #[test]
    fn incremental_sync_handles_removed_changes() {
        let pages = vec![GoogleDriveChangeList {
            changes: vec![GoogleDriveChange {
                file_id: "f9".into(),
                kind: "file".into(),
                removed: true,
                file: None,
                time: Some(Utc::now()),
            }],
            next_page_token: None,
            new_start_page_token: Some("seed-2".into()),
        }];
        let c =
            GoogleDriveConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = None;
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("seed-2"));
    }

    #[test]
    fn webhook_parses_permission_change() {
        let body = serde_json::json!({
            "resourceId": "f1",
            "resourceState": "permission_change",
            "userId": "user-7",
            "newRole": "writer",
            "occurredAt": Utc::now(),
        });
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4());
        let ev = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        match ev {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_state_errors() {
        let body = serde_json::json!({"resourceId": "f1", "resourceState": "weird"});
        let c = GoogleDriveConnector::new(ConnectorInstanceId::new_v4());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
