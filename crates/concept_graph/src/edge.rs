//! [`ConceptEdge`] — one typed edge in the sparse concept graph.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::node::NodeId;

/// Identifier for a [`ConceptEdge`] (UUID v4 newtype).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeId(pub Uuid);

impl EdgeId {
    /// Generate a fresh random edge id.
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

impl std::fmt::Display for EdgeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The seven typed relations from `PROPOSAL.md` §3.3.
///
/// Every edge in the concept graph carries one of these tags. The
/// graph is intentionally sparse — most observations never enter the
/// graph; only high-value, reinforced, cross-source claims do — so
/// the relation taxonomy is small and stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    /// Subtype / instance-of relation (`Atlas is_a Project`).
    IsA,
    /// Mereological "is a part of" (`Atlas part_of Q3 Launch`).
    PartOf,
    /// Decision attribution (`Approval decided_by @Sara`).
    DecidedBy,
    /// Supersession (`v2 supersedes v1`).
    Supersedes,
    /// Contradiction marker (`Claim A contradicts Claim B`).
    Contradicts,
    /// Provenance link (`Concept derived_from EvidenceRow`).
    DerivedFrom,
    /// Task assignment (`Task assigned_to @Eng`).
    AssignedTo,
}

impl RelationType {
    /// Stable string tag used for serialisation and queries.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IsA => "is_a",
            Self::PartOf => "part_of",
            Self::DecidedBy => "decided_by",
            Self::Supersedes => "supersedes",
            Self::Contradicts => "contradicts",
            Self::DerivedFrom => "derived_from",
            Self::AssignedTo => "assigned_to",
        }
    }

    /// Inverse of [`Self::as_str`].
    pub fn parse_tag(s: &str) -> Option<Self> {
        match s {
            "is_a" => Some(Self::IsA),
            "part_of" => Some(Self::PartOf),
            "decided_by" => Some(Self::DecidedBy),
            "supersedes" => Some(Self::Supersedes),
            "contradicts" => Some(Self::Contradicts),
            "derived_from" => Some(Self::DerivedFrom),
            "assigned_to" => Some(Self::AssignedTo),
            _ => None,
        }
    }
}

/// One typed edge in the concept graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConceptEdge {
    /// Unique id (UUID v4).
    pub id: EdgeId,
    /// Source node.
    pub from: NodeId,
    /// Target node.
    pub to: NodeId,
    /// Typed relation.
    pub relation: RelationType,
    /// Scope this edge is bound to (typically the source node's
    /// scope — kept on the edge so cross-scope edges are explicit).
    pub scope_id: ScopeId,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Free-form metadata (provenance ref, confidence, …).
    pub metadata: serde_json::Value,
}

impl ConceptEdge {
    /// Construct a fresh edge.
    pub fn new(from: NodeId, to: NodeId, relation: RelationType, scope_id: ScopeId) -> Self {
        Self {
            id: EdgeId::new_v4(),
            from,
            to,
            relation,
            scope_id,
            created_at: Utc::now(),
            metadata: serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_round_trips_through_string_tag() {
        for r in [
            RelationType::IsA,
            RelationType::PartOf,
            RelationType::DecidedBy,
            RelationType::Supersedes,
            RelationType::Contradicts,
            RelationType::DerivedFrom,
            RelationType::AssignedTo,
        ] {
            assert_eq!(RelationType::parse_tag(r.as_str()), Some(r));
        }
    }
}
