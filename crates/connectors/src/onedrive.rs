//! OneDrive connector — Microsoft Graph `/drive/root/delta`.
//!
//! * `initial_sync` walks `/drive/root/delta` from scratch and emits
//!   one [`ConnectorEvent`] per `DriveItem` change. The cursor is the
//!   `@odata.deltaLink` token Graph returns on the final page.
//! * `incremental_sync` polls `/drive/root/delta?token={cursor}` to
//!   stream new changes since the last call.
//! * `subscribe_webhook` installs a Graph
//!   [subscription](https://learn.microsoft.com/en-us/graph/webhooks)
//!   targeting `callback_url` (max ~3-day TTL).
//! * `handle_webhook_event` parses Graph's
//!   [`changeNotificationCollection`](https://learn.microsoft.com/en-us/graph/api/resources/changenotification)
//!   payload — every notification in the batch is materialised
//!   into a substrate event.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// One Graph `DriveItem` (subset relevant to substrate ingestion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveItem {
    /// Item id.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Wall-clock created date.
    #[serde(default, rename = "createdDateTime")]
    pub created_date_time: Option<DateTime<Utc>>,
    /// Wall-clock last-modified date.
    #[serde(default, rename = "lastModifiedDateTime")]
    pub last_modified_date_time: Option<DateTime<Utc>>,
    /// Marker indicating Graph reports the item as soft-deleted.
    #[serde(default)]
    pub deleted: Option<DeletedFacet>,
}

/// Graph "deleted" facet — its presence means the item is gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedFacet {
    /// Reason string (e.g. `"deleted"`).
    #[serde(default)]
    pub state: String,
}

/// One page of `/drive/root/delta` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeltaResponse {
    /// Items that changed in this page.
    #[serde(default)]
    pub value: Vec<DriveItem>,
    /// Forward link to the next page (mid-walk).
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    /// Final-state cursor — present on the last page only.
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// One `changeNotification` in a Graph subscription batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeNotification {
    /// Substrate-side resource id (`drive/items/{id}`).
    pub resource: String,
    /// Lifecycle string: `created`, `updated`, `deleted`,
    /// `shared` (== permission change).
    #[serde(rename = "changeType")]
    pub change_type: String,
    /// Subscription id Graph uses for delivery routing.
    #[serde(default, rename = "subscriptionId")]
    pub subscription_id: String,
    /// Wall-clock event time.
    #[serde(default, rename = "eventTime")]
    pub event_time: Option<DateTime<Utc>>,
    /// User id whose permission changed (only on `shared`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// New role string (`read`, `write`, `owner`).
    #[serde(default)]
    pub new_role: Option<String>,
}

/// `changeNotificationCollection` payload — what Graph POSTs to the
/// `notificationUrl` of an active subscription.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeNotificationCollection {
    /// Notification batch.
    #[serde(default)]
    pub value: Vec<ChangeNotification>,
}

/// OneDrive connector.
#[derive(Debug, Clone)]
pub struct OneDriveConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// Initial-sync fixture pages.
    pub initial_pages: Vec<DeltaResponse>,
    /// Incremental-sync fixture pages.
    pub incremental_pages: Vec<DeltaResponse>,
}

impl OneDriveConnector {
    /// Construct a connector with empty fixtures.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_pages: Vec::new(),
            incremental_pages: Vec::new(),
        }
    }

    /// Override the initial-sync fixture pages.
    pub fn with_initial_pages(mut self, pages: Vec<DeltaResponse>) -> Self {
        self.initial_pages = pages;
        self
    }

    /// Override the incremental-sync fixture pages.
    pub fn with_incremental_pages(mut self, pages: Vec<DeltaResponse>) -> Self {
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
        "read" | "viewer" => Some(SourcePermissionLevel::Read),
        "write" | "editor" | "contribute" => Some(SourcePermissionLevel::Write),
        "owner" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// Which sync pass produced this item — we use this instead of
/// comparing `createdDateTime == lastModifiedDateTime` because
/// during `initial_sync` the substrate is seeing every non-deleted
/// item for the first time and must classify it as
/// `DocumentCreated` regardless of whether the file has been edited
/// upstream. Mirror of the `SyncMode` enum in the Notion and
/// HubSpot connectors.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

fn item_to_event(item: &DriveItem, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = item
        .last_modified_date_time
        .or(item.created_date_time)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(item.id.clone());
    if item.deleted.is_some() {
        return ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        };
    }
    match mode {
        SyncMode::Initial => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        SyncMode::Incremental => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for OneDriveConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "graph-access-token",
            "graph-refresh-token",
            Utc::now() + Duration::hours(1),
            "Files.Read.All Sites.Read.All",
        ))
    }

    fn initial_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
    ) -> Result<SyncRunResult> {
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut delta_link: Option<String> = None;
        for page in &self.initial_pages {
            for item in &page.value {
                events.push(item_to_event(item, SyncMode::Initial));
            }
            if page.delta_link.is_some() {
                delta_link.clone_from(&page.delta_link);
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: delta_link,
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
        for item in &page.value {
            events.push(item_to_event(item, SyncMode::Incremental));
        }
        let next_cursor = if idx + 1 < self.incremental_pages.len() {
            Some(format!("page-{}", idx + 2))
        } else {
            page.delta_link
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
        // Microsoft Graph drive subscriptions max out at ~3 days.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new("graph-clientstate-secret"),
            WebhookEventTypes::all(),
            Some(Utc::now() + Duration::days(3)),
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Microsoft Graph delivers batched `changeNotification`
        // payloads under a top-level `value` array. Every entry
        // must be materialised — returning only the first one
        // would silently drop concurrent file changes.
        let batch: ChangeNotificationCollection = serde_json::from_slice(body)?;
        if batch.value.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty Graph changeNotification batch".to_string(),
            ));
        }
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(batch.value.len());
        for n in batch.value {
            let occurred_at = n.event_time.unwrap_or_else(Utc::now);
            let document_id = SourceDocumentId::new(
                n.resource
                    .rsplit('/')
                    .next()
                    .unwrap_or(&n.resource)
                    .to_string(),
            );
            let event = match n.change_type.as_str() {
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
                "shared" | "permission_changed" => ConnectorEvent::PermissionChanged {
                    document_id,
                    user_id: SourceUserId::new(n.user_id.unwrap_or_default()),
                    new_level: n.new_role.as_deref().and_then(parse_role),
                    occurred_at,
                },
                other => {
                    return Err(ConnectorError::Webhook(format!(
                        "unknown Graph changeType: {other}"
                    )))
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
    use connector_framework::{AuthKind, ConnectorKind};
    use evidence_store::ScopeId;

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::OneDrive, AuthKind::OAuth2, ScopeId::new_v4())
    }

    #[test]
    fn authenticate_returns_graph_scopes() {
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("Files.Read.All"));
    }

    #[test]
    fn initial_sync_emits_create_then_seeds_delta_cursor() {
        let pages = vec![DeltaResponse {
            value: vec![DriveItem {
                id: "item-1".into(),
                name: "Spec.docx".into(),
                created_date_time: Some(Utc::now()),
                last_modified_date_time: None,
                deleted: None,
            }],
            next_link: None,
            delta_link: Some("delta-token-1".into()),
        }];
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("delta-token-1"));
    }

    #[test]
    fn incremental_sync_handles_deleted_facet() {
        let pages = vec![DeltaResponse {
            value: vec![DriveItem {
                id: "x".into(),
                name: String::new(),
                created_date_time: None,
                last_modified_date_time: Some(Utc::now()),
                deleted: Some(DeletedFacet {
                    state: "deleted".into(),
                }),
            }],
            next_link: None,
            delta_link: Some("delta-2".into()),
        }];
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn webhook_parses_shared_as_permission_change() {
        let body = serde_json::json!({
            "value": [{
                "resource": "drive/items/item-7",
                "changeType": "shared",
                "subscriptionId": "sub-1",
                "eventTime": Utc::now(),
                "user_id": "u-3",
                "new_role": "write",
            }]
        });
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged {
                document_id,
                new_level,
                ..
            } => {
                assert_eq!(document_id.as_str(), "item-7");
                assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_empty_batch_errors() {
        let body = serde_json::json!({"value": []});
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn initial_sync_classifies_items_as_created_regardless_of_timestamps() {
        // Regression test for the Devin Review finding that
        // OneDrive's `item_to_event` previously distinguished
        // `DocumentCreated` from `DocumentUpdated` by comparing
        // `createdDateTime == lastModifiedDateTime`. Real-world
        // files routinely have those timestamps differ even though
        // the substrate is seeing the file for the first time —
        // every non-deleted item in `initial_sync` must surface as
        // `DocumentCreated`.
        let created = Utc::now() - Duration::days(7);
        let modified = Utc::now();
        let pages = vec![DeltaResponse {
            value: vec![
                DriveItem {
                    id: "item-edited".into(),
                    name: "Edited.docx".into(),
                    created_date_time: Some(created),
                    last_modified_date_time: Some(modified),
                    deleted: None,
                },
                DriveItem {
                    id: "item-untouched".into(),
                    name: "Untouched.docx".into(),
                    created_date_time: Some(created),
                    last_modified_date_time: Some(created),
                    deleted: None,
                },
            ],
            next_link: None,
            delta_link: Some("delta-token-2".into()),
        }];
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        for ev in &res.events {
            assert!(
                matches!(ev, ConnectorEvent::DocumentCreated { .. }),
                "initial_sync must emit DocumentCreated for every non-deleted item, got {ev:?}"
            );
        }
    }

    #[test]
    fn webhook_emits_every_event_in_batched_payload() {
        // Regression test: Microsoft Graph batches multiple
        // `changeNotification`s into one POST. Earlier revisions
        // dropped everything past index 0.
        let body = serde_json::json!({
            "value": [
                {
                    "resource": "drive/items/file-a",
                    "changeType": "created",
                    "subscriptionId": "sub-1",
                    "eventTime": Utc::now(),
                },
                {
                    "resource": "drive/items/file-b",
                    "changeType": "updated",
                    "subscriptionId": "sub-1",
                    "eventTime": Utc::now(),
                },
                {
                    "resource": "drive/items/file-c",
                    "changeType": "deleted",
                    "subscriptionId": "sub-1",
                    "eventTime": Utc::now(),
                }
            ]
        });
        let c = OneDriveConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3, "every changeNotification must surface");
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }
}
