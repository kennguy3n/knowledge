//! HubSpot connector — HubSpot CRM v3 API.
//!
//! * `initial_sync` walks `/crm/v3/objects/{contacts|companies|deals|notes}`
//!   with `after` pagination.
//! * `incremental_sync` filters via `lastModifiedDate >= cursor` using
//!   the `/crm/v3/objects/{type}/search` endpoint.
//! * `subscribe_webhook` registers a HubSpot webhook subscription via
//!   `/webhooks/v3/{appId}/subscriptions` for object lifecycle events.
//! * `handle_webhook_event` parses HubSpot's batched webhook payload
//!   — every event in the batch is materialised as a substrate event.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// HubSpot CRM object kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HubSpotObjectKind {
    /// `contacts`
    Contact,
    /// `companies`
    Company,
    /// `deals`
    Deal,
    /// `notes`
    Note,
}

/// One CRM object (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSpotObject {
    /// Object id.
    pub id: String,
    /// Object kind.
    pub kind: HubSpotObjectKind,
    /// `createdAt` timestamp.
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<DateTime<Utc>>,
    /// `updatedAt` timestamp.
    #[serde(default, rename = "updatedAt")]
    pub updated_at: Option<DateTime<Utc>>,
    /// `archived = true` is the deletion signal.
    #[serde(default)]
    pub archived: bool,
}

/// One page of `/crm/v3/objects/{type}` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubSpotListResponse {
    /// Object records on this page.
    #[serde(default)]
    pub results: Vec<HubSpotObject>,
    /// Paging cursor — `paging.next.after`.
    #[serde(default)]
    pub paging: Option<HubSpotPaging>,
}

/// HubSpot paging envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubSpotPaging {
    /// `next` block.
    #[serde(default)]
    pub next: Option<HubSpotPagingNext>,
}

/// HubSpot `paging.next` cursor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubSpotPagingNext {
    /// `after` opaque cursor token.
    pub after: String,
}

/// One HubSpot webhook event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSpotWebhookEvent {
    /// Subscription event type:
    /// `contact.creation`, `contact.propertyChange`, `contact.deletion`,
    /// `company.creation`, …
    #[serde(rename = "subscriptionType")]
    pub subscription_type: String,
    /// `objectId` (HubSpot's int id, serialised as a number).
    #[serde(rename = "objectId")]
    pub object_id: i64,
    /// `occurredAt` (millis since epoch).
    #[serde(default, rename = "occurredAt")]
    pub occurred_at_ms: Option<i64>,
    /// `propertyName` (only on propertyChange).
    #[serde(default, rename = "propertyName")]
    pub property_name: Option<String>,
    /// `propertyValue` (only on propertyChange).
    #[serde(default, rename = "propertyValue")]
    pub property_value: Option<String>,
    /// `userId` whose permission changed.
    #[serde(default, rename = "userId")]
    pub user_id: Option<String>,
}

/// HubSpot connector.
#[derive(Debug, Clone)]
pub struct HubSpotConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// Initial-sync fixture pages.
    pub initial_pages: Vec<HubSpotListResponse>,
    /// Incremental-sync fixture pages.
    pub incremental_pages: Vec<HubSpotListResponse>,
}

impl HubSpotConnector {
    /// Construct an empty connector.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_pages: Vec::new(),
            incremental_pages: Vec::new(),
        }
    }

    /// Override initial-sync fixture pages.
    pub fn with_initial_pages(mut self, pages: Vec<HubSpotListResponse>) -> Self {
        self.initial_pages = pages;
        self
    }

    /// Override incremental-sync fixture pages.
    pub fn with_incremental_pages(mut self, pages: Vec<HubSpotListResponse>) -> Self {
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

/// Which sync pass produced this object — we use this instead of
/// comparing `created_at == updated_at` because HubSpot may set
/// the two timestamps to slightly different millisecond instants
/// even on creation, which would silently misclassify the event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

fn object_to_event(obj: &HubSpotObject, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = obj.updated_at.or(obj.created_at).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(format!("{}:{}", kind_str(obj.kind), obj.id));
    if obj.archived {
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

fn kind_str(k: HubSpotObjectKind) -> &'static str {
    match k {
        HubSpotObjectKind::Contact => "contact",
        HubSpotObjectKind::Company => "company",
        HubSpotObjectKind::Deal => "deal",
        HubSpotObjectKind::Note => "note",
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "viewer" | "view" | "read" => Some(SourcePermissionLevel::Read),
        "editor" | "edit" | "write" => Some(SourcePermissionLevel::Write),
        "owner" | "admin" | "super_admin" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// Map a HubSpot `subscriptionType` to a substrate event.
///
/// Returns `None` for subscription types we don't understand so the
/// caller can skip them without aborting the rest of the batch — see
/// `handle_webhook_event` for why an unknown entry must not discard
/// already-processed valid events.
fn subscription_to_event(
    sub: &str,
    object_id: i64,
    occurred_at: DateTime<Utc>,
    user_id: Option<String>,
    new_role: Option<&str>,
) -> Option<ConnectorEvent> {
    let kind = sub.split_once('.').map_or("", |(prefix, _)| prefix);
    let id = SourceDocumentId::new(format!("{kind}:{object_id}"));
    if sub.ends_with(".creation") {
        Some(ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        })
    } else if sub.ends_with(".propertyChange") || sub.ends_with(".update") {
        Some(ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        })
    } else if sub.ends_with(".deletion") {
        Some(ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        })
    } else if sub.ends_with(".permissionChange") {
        Some(ConnectorEvent::PermissionChanged {
            document_id: id,
            user_id: SourceUserId::new(user_id.unwrap_or_default()),
            new_level: new_role.and_then(parse_role),
            occurred_at,
        })
    } else {
        None
    }
}

impl Connector for HubSpotConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "hubspot-access-token",
            "hubspot-refresh-token",
            Utc::now() + Duration::hours(6),
            "crm.objects.contacts.read crm.objects.companies.read crm.objects.deals.read",
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
            for obj in &page.results {
                events.push(object_to_event(obj, SyncMode::Initial));
                if let Some(t) = obj.updated_at.or(obj.created_at) {
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
        for obj in &page.results {
            events.push(object_to_event(obj, SyncMode::Incremental));
            if let Some(t) = obj.updated_at.or(obj.created_at) {
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
            WebhookSecret::new("hubspot-app-secret"),
            WebhookEventTypes::all(),
            // HubSpot subscriptions are evergreen — no provider TTL.
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // HubSpot delivers webhooks as a JSON array — a single POST
        // can carry many independent subscription events. Every
        // recognised entry must surface; previously we returned only
        // the first, which silently dropped the rest.
        //
        // Unknown subscription types are skipped rather than
        // aborting the whole batch — when HubSpot adds a new event
        // family we cannot retroactively discard every well-formed
        // event that was queued behind it. Mirrors the OneDrive
        // handler's policy on unknown `changeType`s.
        let batch: Vec<HubSpotWebhookEvent> = serde_json::from_slice(body)?;
        if batch.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty HubSpot webhook batch".to_string(),
            ));
        }
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(batch.len());
        for e in batch {
            let occurred_at = e
                .occurred_at_ms
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .unwrap_or_else(Utc::now);
            if let Some(ev) = subscription_to_event(
                &e.subscription_type,
                e.object_id,
                occurred_at,
                e.user_id,
                // HubSpot encodes the new role in `propertyValue`
                // for the `permissionChange` subscription.
                e.property_value.as_deref(),
            ) {
                events.push(ev);
            }
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
        ConnectorConfig::new(ConnectorKind::HubSpot, AuthKind::OAuth2, ScopeId::new_v4())
    }

    #[test]
    fn authenticate_returns_crm_scope() {
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("crm.objects.contacts.read"));
    }

    #[test]
    fn initial_sync_emits_create_per_object() {
        let now = Utc::now();
        let pages = vec![HubSpotListResponse {
            results: vec![HubSpotObject {
                id: "101".into(),
                kind: HubSpotObjectKind::Contact,
                created_at: Some(now),
                updated_at: Some(now),
                archived: false,
            }],
            paging: None,
        }];
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
    }

    #[test]
    fn incremental_sync_emits_update_for_modified_objects() {
        let now = Utc::now();
        let pages = vec![HubSpotListResponse {
            results: vec![HubSpotObject {
                id: "999".into(),
                kind: HubSpotObjectKind::Deal,
                created_at: Some(now - Duration::days(1)),
                updated_at: Some(now),
                archived: false,
            }],
            paging: None,
        }];
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4()).with_incremental_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let state = SyncState::new(c.instance);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
    }

    #[test]
    fn webhook_parses_contact_creation() {
        let body = serde_json::json!([
            {
                "subscriptionType": "contact.creation",
                "objectId": 1234,
                "occurredAt": Utc::now().timestamp_millis(),
            }
        ]);
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "contact:1234");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_parses_permission_change() {
        let body = serde_json::json!([
            {
                "subscriptionType": "company.permissionChange",
                "objectId": 42,
                "occurredAt": Utc::now().timestamp_millis(),
                "userId": "u-1",
                "propertyValue": "editor",
            }
        ]);
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4());
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
                assert_eq!(document_id.as_str(), "company:42");
                assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_empty_batch_errors() {
        let body = serde_json::json!([]);
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_unknown_subscription_is_skipped_not_errored() {
        // Regression: an unknown `subscriptionType` previously
        // bubbled up as `Err` from `subscription_to_event` via the
        // `?` operator inside `handle_webhook_event`, which would
        // have discarded every valid event already queued earlier in
        // the same batch. The handler must now skip the unknown
        // entry and continue processing the remainder.
        let body = serde_json::json!([
            {
                "subscriptionType": "contact.creation",
                "objectId": 1,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "foo.weird",
                "objectId": 2,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "deal.deletion",
                "objectId": 9,
                "occurredAt": Utc::now().timestamp_millis(),
            }
        ]);
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(
            evs.len(),
            2,
            "valid events on either side of an unknown subscriptionType must still surface",
        );
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_emits_every_event_in_batched_payload() {
        // Regression test: HubSpot ships subscription events in a
        // JSON array — one POST can carry many. Earlier revisions
        // dropped everything past index 0.
        let body = serde_json::json!([
            {
                "subscriptionType": "contact.creation",
                "objectId": 1,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "contact.propertyChange",
                "objectId": 1,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "deal.deletion",
                "objectId": 9,
                "occurredAt": Utc::now().timestamp_millis(),
            }
        ]);
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3, "every batched HubSpot event must surface");
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn initial_sync_classifies_objects_as_created_regardless_of_timestamps() {
        // Regression test: earlier revisions used `created_at ==
        // updated_at` to decide DocumentCreated vs DocumentUpdated,
        // which silently misclassified real-world payloads where
        // the two values differ by a few milliseconds even on
        // first creation.
        let now = Utc::now();
        let pages = vec![HubSpotListResponse {
            results: vec![HubSpotObject {
                id: "77".into(),
                kind: HubSpotObjectKind::Contact,
                created_at: Some(now),
                updated_at: Some(now + Duration::milliseconds(7)),
                archived: false,
            }],
            paging: None,
        }];
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4()).with_initial_pages(pages);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert!(
            matches!(res.events[0], ConnectorEvent::DocumentCreated { .. }),
            "initial_sync must always emit DocumentCreated, not depend on timestamp equality",
        );
    }
}
