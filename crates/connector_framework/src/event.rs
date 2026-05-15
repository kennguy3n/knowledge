//! Connector event types — the canonical change events a connector
//! emits into the substrate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Identifier for a document in the *source* system (Google Drive
/// file id, Notion block id, Jira issue key, …). Kept opaque to the
/// substrate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceDocumentId(pub String);

impl SourceDocumentId {
    /// Wrap a string id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceDocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifier for a user in the *source* system (Google Workspace
/// account, Notion user id, Jira account id, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceUserId(pub String);

impl SourceUserId {
    /// Wrap a string id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceUserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One change event from a connector / webhook.
///
/// Per `docs/DESIGN.md` §10.2 connectors emit `(document, action,
/// permission)` deltas; this enum is the substrate's normalised
/// shape for those deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ConnectorEvent {
    /// A new document appeared in the source system.
    DocumentCreated {
        /// Source-side document identifier.
        document_id: SourceDocumentId,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
    /// An existing document was edited.
    DocumentUpdated {
        /// Source-side document identifier.
        document_id: SourceDocumentId,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
    /// A document was removed (or trashed) on the source side.
    DocumentDeleted {
        /// Source-side document identifier.
        document_id: SourceDocumentId,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
    /// A permission grant changed on the source side. The substrate
    /// uses these to keep its `permission_service` relation graph in
    /// sync with the source ACL — see `acl_sync` for the projection.
    PermissionChanged {
        /// Source-side document identifier whose ACL changed.
        document_id: SourceDocumentId,
        /// Source-side user whose permission changed.
        user_id: SourceUserId,
        /// New permission level (or `None` when revoked).
        new_level: Option<crate::acl_sync::SourcePermissionLevel>,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
}

impl ConnectorEvent {
    /// Stable string tag for the event variant — used for routing
    /// and metrics.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::DocumentCreated { .. } => "document_created",
            Self::DocumentUpdated { .. } => "document_updated",
            Self::DocumentDeleted { .. } => "document_deleted",
            Self::PermissionChanged { .. } => "permission_changed",
        }
    }

    /// The source-side document id this event refers to.
    pub fn document_id(&self) -> &SourceDocumentId {
        match self {
            Self::DocumentCreated { document_id, .. }
            | Self::DocumentUpdated { document_id, .. }
            | Self::DocumentDeleted { document_id, .. }
            | Self::PermissionChanged { document_id, .. } => document_id,
        }
    }

    /// Wall-clock occurrence time as reported by the source.
    pub fn occurred_at(&self) -> DateTime<Utc> {
        match self {
            Self::DocumentCreated { occurred_at, .. }
            | Self::DocumentUpdated { occurred_at, .. }
            | Self::DocumentDeleted { occurred_at, .. }
            | Self::PermissionChanged { occurred_at, .. } => *occurred_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl_sync::SourcePermissionLevel;

    #[test]
    fn event_round_trips_through_json() {
        let ev = ConnectorEvent::DocumentCreated {
            document_id: SourceDocumentId::new("doc-1"),
            occurred_at: Utc::now(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: ConnectorEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn permission_changed_carries_level() {
        let ev = ConnectorEvent::PermissionChanged {
            document_id: SourceDocumentId::new("doc-9"),
            user_id: SourceUserId::new("u-7"),
            new_level: Some(SourcePermissionLevel::Write),
            occurred_at: Utc::now(),
        };
        assert_eq!(ev.kind(), "permission_changed");
        assert_eq!(ev.document_id().as_str(), "doc-9");
    }

    #[test]
    fn revocation_serialises_with_null_level() {
        let ev = ConnectorEvent::PermissionChanged {
            document_id: SourceDocumentId::new("doc-9"),
            user_id: SourceUserId::new("u-7"),
            new_level: None,
            occurred_at: Utc::now(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"new_level\":null"));
    }
}
