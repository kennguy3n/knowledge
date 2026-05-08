//! [`ConceptNode`] — one node in the sparse concept graph.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

/// Identifier for a [`ConceptNode`] (UUID v4 newtype, mirroring
/// `EvidenceId` / `ScopeId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub Uuid);

impl NodeId {
    /// Generate a fresh random node id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a raw [`Uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// State of a concept node — mirrors the memory-manager state machine
/// at the graph layer so a graph traversal can filter on lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeState {
    /// Newly proposed — not yet promoted to canonical.
    Candidate,
    /// Canonical concept. Most queries default to this state.
    Canonical,
    /// A newer canonical concept has superseded this one. The node
    /// is preserved (with `superseded_by`) for audit and contradiction
    /// tracking.
    Superseded,
    /// Marked as contradicting another node — kept for audit, not
    /// retrieved by default.
    Contradicted,
    /// The scope DEK has been destroyed; the node is retained as a
    /// tombstone.
    Deleted,
}

impl NodeState {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Canonical => "canonical",
            Self::Superseded => "superseded",
            Self::Contradicted => "contradicted",
            Self::Deleted => "deleted",
        }
    }
}

/// One typed node in the concept graph.
///
/// Per `PROPOSAL.md` §3.3 each node is "scope-aware: bound to a scope
/// (user, channel, domain, tenant) and inherits its access policy".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConceptNode {
    /// Unique id (UUID v4).
    pub id: NodeId,
    /// Short human-readable label (e.g. `"Project Atlas"`).
    pub label: String,
    /// Long-form definition of the concept.
    pub definition: String,
    /// Scope this node is bound to.
    pub scope_id: ScopeId,
    /// Lifecycle state.
    pub state: NodeState,
    /// If the node is in [`NodeState::Superseded`] / [`NodeState::Contradicted`],
    /// the id of the newer / contradicting node.
    pub superseded_by: Option<NodeId>,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock last update time.
    pub updated_at: DateTime<Utc>,
    /// Free-form metadata (provenance refs, observation type, …) kept
    /// schema-flexible so callers can extend without a migration.
    pub metadata: serde_json::Value,
}

impl ConceptNode {
    /// Construct a fresh candidate node.
    pub fn new_candidate(
        label: impl Into<String>,
        definition: impl Into<String>,
        scope_id: ScopeId,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: NodeId::new_v4(),
            label: label.into(),
            definition: definition.into(),
            scope_id,
            state: NodeState::Candidate,
            superseded_by: None,
            created_at: now,
            updated_at: now,
            metadata: serde_json::Value::Null,
        }
    }

    /// Mark this node as canonical and stamp `updated_at`.
    pub fn mark_canonical(&mut self) {
        self.state = NodeState::Canonical;
        self.updated_at = Utc::now();
    }

    /// Mark this node as superseded by `successor` and stamp
    /// `updated_at`.
    pub fn mark_superseded_by(&mut self, successor: NodeId) {
        self.state = NodeState::Superseded;
        self.superseded_by = Some(successor);
        self.updated_at = Utc::now();
    }

    /// Mark this node as contradicting `other` and stamp `updated_at`.
    pub fn mark_contradicted_by(&mut self, other: NodeId) {
        self.state = NodeState::Contradicted;
        self.superseded_by = Some(other);
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_candidate_has_expected_defaults() {
        let scope = ScopeId::new_v4();
        let n = ConceptNode::new_candidate("Atlas", "Project codename for Q3 launch", scope);
        assert_eq!(n.state, NodeState::Candidate);
        assert!(n.superseded_by.is_none());
        assert_eq!(n.scope_id, scope);
    }

    #[test]
    fn mark_superseded_records_pointer() {
        let scope = ScopeId::new_v4();
        let mut n = ConceptNode::new_candidate("a", "b", scope);
        let successor = NodeId::new_v4();
        n.mark_superseded_by(successor);
        assert_eq!(n.state, NodeState::Superseded);
        assert_eq!(n.superseded_by, Some(successor));
    }
}
