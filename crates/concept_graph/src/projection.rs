//! Projection from per-scope memory observations into a
//! [`ConceptGraph`].
//!
//! The concept graph is *derived*, not separately persisted: the
//! authoritative source of truth is the per-scope user-memory plane
//! (`memory_manager`), which is what the substrate writes and the
//! decay state machine mutates. Maintaining a second persisted graph
//! that must be kept in lock-step with memory would invite
//! write-amplification and consistency drift across 5000 tenants, so
//! instead we project the live memory objects into a graph on read.
//! This guarantees the graph can never disagree with memory and needs
//! no extra encrypted store or CRDT delta sync to stay correct.
//!
//! The projection is intentionally *neutral*: it speaks in
//! [`NodeState`] and [`Uuid`] rather than `memory_manager` types, so
//! `concept_graph` keeps zero dependency on the memory layer. The FFI
//! tier — which already links both crates — maps each
//! `MemoryObject` into a [`MemoryProjection`] and calls
//! [`project_memory_graph`].
//!
//! What it produces today:
//!
//! * One node per live (non-`Deleted`) memory observation, carrying
//!   the observation's lifecycle state so a graph traversal reflects
//!   decay transitions immediately.
//! * One [`RelationType::Supersedes`] edge per resolved
//!   `superseded_by` pointer (`successor --supersedes--> predecessor`),
//!   skipping dangling pointers and self-loops.
//!
//! Richer typed relations (`is_a`, `part_of`, …) are emitted by the
//! synthesis / observation engines and are out of scope here; this
//! module wires the population path the visualization layer and UI
//! already type against.

use std::collections::HashSet;

use uuid::Uuid;

use evidence_store::ScopeId;

use crate::edge::{ConceptEdge, EdgeId, RelationType};
use crate::graph::ConceptGraph;
use crate::node::{ConceptNode, NodeId, NodeState};

/// Stable namespace for deterministic supersession-edge ids, so
/// re-projecting the same memory set yields a byte-identical graph
/// (idempotent — friendly to diffing and caching).
const SUPERSEDES_EDGE_NAMESPACE: Uuid = Uuid::from_u128(0x6b9c_2d10_7a44_4f1e_9c3b_5e8d_21a7_0f64);

/// A single memory observation flattened into the inputs the concept
/// graph needs. The FFI tier builds one of these per `MemoryObject`.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryProjection {
    /// Memory id — reused verbatim as the [`NodeId`] so the node is
    /// addressable by the same id the UI already holds.
    pub id: Uuid,
    /// Scope the memory (and therefore the node) is bound to.
    pub scope_id: ScopeId,
    /// Short human-readable label (typically the observation summary).
    pub label: String,
    /// Long-form definition / full observation content.
    pub definition: String,
    /// Lifecycle state, already mapped to the graph state machine.
    pub state: NodeState,
    /// If this observation was superseded, the id of the newer
    /// observation that replaced it.
    pub superseded_by: Option<Uuid>,
    /// Creation timestamp carried through verbatim.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last-update timestamp carried through verbatim.
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Freeform metadata (underlying memory state, retention score,
    /// observation type, …) preserved for downstream consumers.
    pub metadata: serde_json::Value,
}

impl MemoryProjection {
    fn into_node(self) -> ConceptNode {
        ConceptNode {
            id: NodeId::from_uuid(self.id),
            label: self.label,
            definition: self.definition,
            scope_id: self.scope_id,
            state: self.state,
            superseded_by: self.superseded_by.map(NodeId::from_uuid),
            created_at: self.created_at,
            updated_at: self.updated_at,
            metadata: self.metadata,
        }
    }
}

/// Deterministic supersession edge id derived from the (successor,
/// predecessor) pair, so projecting the same memory set is idempotent.
fn supersedes_edge_id(successor: NodeId, predecessor: NodeId) -> EdgeId {
    let mut name = Vec::with_capacity(32);
    name.extend_from_slice(successor.as_uuid().as_bytes());
    name.extend_from_slice(predecessor.as_uuid().as_bytes());
    EdgeId::from_uuid(Uuid::new_v5(&SUPERSEDES_EDGE_NAMESPACE, &name))
}

/// Project a set of memory observations into a [`ConceptGraph`].
///
/// Nodes are inserted first; supersession edges are added in a second
/// pass once every endpoint is known, so a `superseded_by` pointer to
/// a memory that was not included (e.g. already hard-deleted, or
/// filtered out) is silently dropped rather than producing a dangling
/// edge. Duplicate ids and self-supersession are skipped defensively —
/// the projection never panics on adversarial input.
pub fn project_memory_graph(items: impl IntoIterator<Item = MemoryProjection>) -> ConceptGraph {
    let mut graph = ConceptGraph::new();
    // Capture the supersession pairs before the nodes are moved into
    // the graph.
    let mut pending_edges: Vec<(NodeId, NodeId, ScopeId)> = Vec::new();
    let mut present: HashSet<NodeId> = HashSet::new();

    for item in items {
        let id = NodeId::from_uuid(item.id);
        if let Some(succ) = item.superseded_by {
            let succ = NodeId::from_uuid(succ);
            if succ != id {
                // `successor --supersedes--> predecessor`.
                pending_edges.push((succ, id, item.scope_id));
            }
        }
        // `add_node` rejects duplicates; on a duplicate id we keep the
        // first occurrence and drop the rest.
        if graph.add_node(item.into_node()).is_ok() {
            present.insert(id);
        }
    }

    for (successor, predecessor, scope_id) in pending_edges {
        if !present.contains(&successor) || !present.contains(&predecessor) {
            continue;
        }
        let mut edge = ConceptEdge::new(successor, predecessor, RelationType::Supersedes, scope_id);
        edge.id = supersedes_edge_id(successor, predecessor);
        // `add_edge` only fails on a dangling endpoint, which the
        // `present` guard above already excludes.
        let _ = graph.add_edge(edge);
    }

    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection(id: Uuid, scope: ScopeId, state: NodeState) -> MemoryProjection {
        let now = chrono::Utc::now();
        MemoryProjection {
            id,
            scope_id: scope,
            label: "label".into(),
            definition: "definition".into(),
            state,
            superseded_by: None,
            created_at: now,
            updated_at: now,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn empty_input_yields_empty_graph() {
        let g = project_memory_graph(std::iter::empty());
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn each_observation_becomes_a_node_keyed_by_memory_id() {
        let scope = ScopeId::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let g = project_memory_graph([
            projection(a, scope, NodeState::Candidate),
            projection(b, scope, NodeState::Canonical),
        ]);
        assert_eq!(g.node_count(), 2);
        assert!(g.get_node(NodeId::from_uuid(a)).is_some());
        assert_eq!(
            g.get_node(NodeId::from_uuid(b)).unwrap().state,
            NodeState::Canonical
        );
    }

    #[test]
    fn superseded_by_resolves_to_a_supersedes_edge() {
        let scope = ScopeId::new_v4();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let mut older = projection(old, scope, NodeState::Superseded);
        older.superseded_by = Some(new);
        let g = project_memory_graph([older, projection(new, scope, NodeState::Canonical)]);

        assert_eq!(g.edge_count(), 1);
        let edge = g.iter_edges().next().unwrap();
        assert_eq!(edge.relation, RelationType::Supersedes);
        assert_eq!(edge.from, NodeId::from_uuid(new));
        assert_eq!(edge.to, NodeId::from_uuid(old));
    }

    #[test]
    fn dangling_superseded_by_is_dropped() {
        let scope = ScopeId::new_v4();
        let old = Uuid::new_v4();
        let mut older = projection(old, scope, NodeState::Superseded);
        // Points at a memory that is not part of the projected set.
        older.superseded_by = Some(Uuid::new_v4());
        let g = project_memory_graph([older]);
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn self_supersession_is_skipped() {
        let scope = ScopeId::new_v4();
        let id = Uuid::new_v4();
        let mut node = projection(id, scope, NodeState::Candidate);
        node.superseded_by = Some(id);
        let g = project_memory_graph([node]);
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn duplicate_ids_keep_first_and_drop_rest() {
        let scope = ScopeId::new_v4();
        let id = Uuid::new_v4();
        let mut first = projection(id, scope, NodeState::Canonical);
        first.label = "first".into();
        let mut second = projection(id, scope, NodeState::Candidate);
        second.label = "second".into();
        let g = project_memory_graph([first, second]);
        assert_eq!(g.node_count(), 1);
        let node = g.get_node(NodeId::from_uuid(id)).unwrap();
        assert_eq!(node.label, "first");
        assert_eq!(node.state, NodeState::Canonical);
    }

    #[test]
    fn projection_is_idempotent_in_edge_ids() {
        let scope = ScopeId::new_v4();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let build = || {
            let mut older = projection(old, scope, NodeState::Superseded);
            older.superseded_by = Some(new);
            project_memory_graph([older, projection(new, scope, NodeState::Canonical)])
        };
        let first = build();
        let second = build();
        let e1 = first.iter_edges().next().unwrap().id;
        let e2 = second.iter_edges().next().unwrap().id;
        assert_eq!(e1, e2);
    }
}
