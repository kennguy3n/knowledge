//! Incremental concept-graph updates.
//!
//! Per `PHASES.md` Phase 6, when an observation is promoted or
//! superseded the substrate must recompute *only the touched
//! branch* of the concept graph rather than re-running synthesis
//! end-to-end. This module models that behaviour as three small
//! pieces:
//!
//! * [`ChangeEvent`] — the kinds of mutations the engine knows
//!   how to react to.
//! * [`AffectedSubgraph`] — the closure of nodes and edges
//!   touched by a change. Direct neighbours are always
//!   included; transitive `derived_from` dependents are walked
//!   so that downstream synthesised concepts can be recomputed.
//! * [`UpdatePropagation`] — the bookkeeping result of pushing
//!   a change through the affected subgraph: which nodes ended
//!   up superseded / contradicted, which edges were dropped,
//!   how deep the walk went.
//!
//! The actual graph mutation primitives live on
//! [`crate::ConceptGraph`]. The engine is a thin coordinator on
//! top — it figures out what needs to change, asks the graph to
//! change it, and reports back.
//!
//! Intentionally in-memory only — persistence and CRDT delta
//! sync land in later phases (`PHASES.md` Phase 6 / Phase 3).

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::edge::{EdgeId, RelationType};
use crate::error::{GraphError, Result};
use crate::graph::ConceptGraph;
use crate::node::{NodeId, NodeState};

/// One mutation the [`IncrementalUpdateEngine`] knows how to
/// react to. These mirror the surface of the graph's mutation
/// API rather than the full set of internal state changes —
/// they are what an outside caller (synthesis pipeline,
/// observation promoter) emits when it wants the graph to
/// settle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeEvent {
    /// A candidate has been promoted to canonical.
    NodePromoted {
        /// The node whose state moved Candidate → Canonical.
        node: NodeId,
    },
    /// A canonical node has been replaced by a newer one.
    NodeSuperseded {
        /// The older node.
        predecessor: NodeId,
        /// The newer node that supersedes it.
        successor: NodeId,
    },
    /// Two canonical claims were declared contradictory.
    NodeContradicted {
        /// First half of the pair.
        a: NodeId,
        /// Second half of the pair.
        b: NodeId,
    },
    /// A typed edge was added.
    EdgeAdded {
        /// The edge id.
        edge: EdgeId,
    },
    /// A typed edge was removed.
    EdgeRemoved {
        /// The (since-removed) edge id.
        edge: EdgeId,
    },
}

impl ChangeEvent {
    /// The "anchor" nodes a [`ChangeEvent`] talks about, used
    /// as the seeds for affected-subgraph walks.
    pub fn anchors(&self) -> Vec<NodeId> {
        match self {
            Self::NodePromoted { node } => vec![*node],
            Self::NodeSuperseded {
                predecessor,
                successor,
            } => vec![*predecessor, *successor],
            Self::NodeContradicted { a, b } => vec![*a, *b],
            Self::EdgeAdded { .. } | Self::EdgeRemoved { .. } => Vec::new(),
        }
    }
}

/// The set of nodes / edges touched by a change.
///
/// "Touched" means *directly incident* (the change's anchor
/// nodes plus their immediate neighbours) **plus** the
/// transitive closure following `derived_from` edges in the
/// reverse direction — i.e. any concept whose provenance
/// includes one of the touched nodes is also touched, because
/// it may need to be re-synthesised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AffectedSubgraph {
    /// Nodes touched by the change.
    pub nodes: HashSet<NodeId>,
    /// Edges incident on those nodes.
    pub edges: HashSet<EdgeId>,
    /// Maximum walk depth that was actually reached. `0` for
    /// edge-only changes that don't seed a node walk.
    pub max_depth_reached: usize,
}

impl AffectedSubgraph {
    /// Number of touched nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True iff no nodes are touched.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// True iff `node` ended up in the affected set.
    pub fn contains_node(&self, node: NodeId) -> bool {
        self.nodes.contains(&node)
    }

    /// True iff `edge` ended up in the affected edge set.
    pub fn contains_edge(&self, edge: EdgeId) -> bool {
        self.edges.contains(&edge)
    }

    /// Sorted node ids — handy for deterministic test
    /// assertions and stable downstream output.
    pub fn sorted_nodes(&self) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self.nodes.iter().copied().collect();
        v.sort_by_key(|n| n.0);
        v
    }
}

/// The minimal set of nodes the caller needs to re-evaluate.
///
/// The recompute scope is a strict subset of
/// [`AffectedSubgraph::nodes`] — only nodes whose state
/// *actually depends on the change* are returned. For now that
/// means: the change anchors plus any node that derived from a
/// touched node via `derived_from`. Nodes that are merely
/// "near" the change in the graph (e.g. siblings under the
/// same parent) are not re-evaluated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecomputeScope {
    /// Nodes that need re-evaluation, in BFS-discovery order.
    pub nodes: Vec<NodeId>,
}

impl RecomputeScope {
    /// Empty scope.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Number of nodes flagged for recomputation.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True iff no nodes need recomputation.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// True iff `node` is flagged for recomputation.
    pub fn contains(&self, node: NodeId) -> bool {
        self.nodes.contains(&node)
    }
}

/// Bookkeeping returned by
/// [`IncrementalUpdateEngine::propagate`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdatePropagation {
    /// The originating event.
    pub event: Option<ChangeEvent>,
    /// What ended up in scope after the walk.
    pub affected: AffectedSubgraph,
    /// Subset that needs re-evaluation.
    pub recompute: RecomputeScope,
    /// Nodes whose state actually changed (e.g.
    /// Candidate → Canonical).
    pub state_transitions: HashMap<NodeId, NodeState>,
    /// Edges that the propagation removed (e.g. dangling
    /// `derived_from` edges from a tombstoned source).
    pub removed_edges: Vec<EdgeId>,
}

impl UpdatePropagation {
    /// Convenience: did anything actually change?
    pub fn is_noop(&self) -> bool {
        self.state_transitions.is_empty()
            && self.removed_edges.is_empty()
            && self.recompute.is_empty()
    }
}

/// Coordinator that translates [`ChangeEvent`]s into concrete
/// [`ConceptGraph`] mutations, only touching the affected
/// branch.
#[derive(Debug, Clone)]
pub struct IncrementalUpdateEngine {
    /// Maximum number of `derived_from` hops to walk when
    /// expanding the affected subgraph. Walks beyond this
    /// depth stop.
    pub max_propagation_depth: usize,
}

impl Default for IncrementalUpdateEngine {
    fn default() -> Self {
        Self {
            max_propagation_depth: 8,
        }
    }
}

impl IncrementalUpdateEngine {
    /// Construct an engine with a custom propagation depth.
    pub fn with_depth(max_propagation_depth: usize) -> Self {
        Self {
            max_propagation_depth,
        }
    }

    /// Compute the set of nodes / edges touched by `event`.
    pub fn affected_subgraph(&self, graph: &ConceptGraph, event: &ChangeEvent) -> AffectedSubgraph {
        let mut subgraph = AffectedSubgraph::default();
        let anchors = event.anchors();
        if anchors.is_empty() {
            // Edge-only event — nothing to walk.
            if let ChangeEvent::EdgeAdded { edge } | ChangeEvent::EdgeRemoved { edge } = event {
                subgraph.edges.insert(*edge);
            }
            return subgraph;
        }
        // BFS over the anchors. Direct neighbours always count;
        // we additionally walk the *reverse* direction of
        // `derived_from` so that synthesised concepts which
        // depend on a touched anchor are pulled in.
        let mut frontier: VecDeque<(NodeId, usize)> = VecDeque::new();
        for a in &anchors {
            if graph.get_node(*a).is_some() {
                subgraph.nodes.insert(*a);
                frontier.push_back((*a, 0));
            }
        }
        while let Some((id, depth)) = frontier.pop_front() {
            if depth > subgraph.max_depth_reached {
                subgraph.max_depth_reached = depth;
            }
            if depth >= self.max_propagation_depth {
                // Don't expand further — but still record this
                // node's incident edges so callers see the
                // boundary.
                for e in graph.get_edges(id) {
                    subgraph.edges.insert(e.id);
                }
                continue;
            }
            for e in graph.get_edges(id) {
                subgraph.edges.insert(e.id);
                let other = if e.from == id { e.to } else { e.from };
                if other == id {
                    continue;
                }
                let is_provenance = matches!(e.relation, RelationType::DerivedFrom);
                let in_provenance = is_provenance && e.to == id;
                let direct_neighbour = depth == 0;
                if (direct_neighbour || in_provenance) && subgraph.nodes.insert(other) {
                    frontier.push_back((other, depth + 1));
                }
            }
        }
        subgraph
    }

    /// Compute the minimal set of nodes that need
    /// re-evaluation after `event`.
    pub fn recompute_scope(
        &self,
        graph: &ConceptGraph,
        event: &ChangeEvent,
        affected: &AffectedSubgraph,
    ) -> RecomputeScope {
        let mut scope = RecomputeScope::empty();
        let mut seen = HashSet::new();
        let anchors = event.anchors();
        for a in &anchors {
            if graph.get_node(*a).is_some() && seen.insert(*a) {
                scope.nodes.push(*a);
            }
        }
        // BFS only along incoming `derived_from` edges from the
        // anchors, restricted to the affected set so we never
        // pull in nodes the affected-subgraph walk excluded.
        let mut frontier: VecDeque<NodeId> = scope.nodes.iter().copied().collect();
        while let Some(id) = frontier.pop_front() {
            for e in graph.get_edges(id) {
                if e.relation != RelationType::DerivedFrom {
                    continue;
                }
                if e.to != id {
                    continue;
                }
                let dependent = e.from;
                if !affected.contains_node(dependent) {
                    continue;
                }
                if seen.insert(dependent) {
                    scope.nodes.push(dependent);
                    frontier.push_back(dependent);
                }
            }
        }
        scope
    }

    /// Drive the change through the graph and report what
    /// changed.
    ///
    /// Mutations are minimal:
    ///
    /// * `NodePromoted` → flips the anchor's state to
    ///   `Canonical` if it was `Candidate`. No-op otherwise.
    /// * `NodeSuperseded` → invokes
    ///   [`ConceptGraph::supersede_node`].
    /// * `NodeContradicted` → invokes
    ///   [`ConceptGraph::mark_contradiction`].
    /// * `EdgeAdded` → bookkeeping only; adding edges is the
    ///   caller's job. The variant exists so callers can record
    ///   the change in [`UpdatePropagation`] without a separate
    ///   call.
    /// * `EdgeRemoved` → invokes
    ///   [`ConceptGraph::remove_edge`] for the named edge and
    ///   records it in [`UpdatePropagation::removed_edges`] on
    ///   success. Callers should *not* remove the edge first —
    ///   if the edge has already been dropped, the propagation
    ///   silently no-ops the removal step.
    ///
    /// In all cases the affected-subgraph walk runs first so
    /// the caller gets a consistent view of what was touched.
    pub fn propagate(
        &self,
        graph: &mut ConceptGraph,
        event: ChangeEvent,
    ) -> Result<UpdatePropagation> {
        let affected = self.affected_subgraph(graph, &event);
        let recompute = self.recompute_scope(graph, &event, &affected);
        let mut state_transitions: HashMap<NodeId, NodeState> = HashMap::new();
        let mut removed_edges: Vec<EdgeId> = Vec::new();

        match &event {
            ChangeEvent::NodePromoted { node } => {
                let n = graph
                    .get_node_mut(*node)
                    .ok_or(GraphError::node_not_found(*node))?;
                if n.state == NodeState::Candidate {
                    n.mark_canonical();
                    state_transitions.insert(*node, NodeState::Canonical);
                }
            }
            ChangeEvent::NodeSuperseded {
                predecessor,
                successor,
            } => {
                graph.supersede_node(*predecessor, *successor)?;
                state_transitions.insert(*predecessor, NodeState::Superseded);
            }
            ChangeEvent::NodeContradicted { a, b } => {
                graph.mark_contradiction(*a, *b)?;
                state_transitions.insert(*a, NodeState::Contradicted);
                state_transitions.insert(*b, NodeState::Contradicted);
            }
            ChangeEvent::EdgeAdded { .. } => {}
            ChangeEvent::EdgeRemoved { edge } => {
                if graph.remove_edge(*edge).is_ok() {
                    removed_edges.push(*edge);
                }
            }
        }

        Ok(UpdatePropagation {
            event: Some(event),
            affected,
            recompute,
            state_transitions,
            removed_edges,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::ConceptEdge;
    use crate::node::ConceptNode;
    use evidence_store::ScopeId;

    fn fresh_graph() -> (ConceptGraph, ScopeId) {
        (ConceptGraph::new(), ScopeId::new_v4())
    }

    fn add_canonical(g: &mut ConceptGraph, label: &str, scope: ScopeId) -> NodeId {
        let mut n = ConceptNode::new_candidate(label, format!("def {label}"), scope);
        n.mark_canonical();
        let id = n.id;
        g.add_node(n).unwrap();
        id
    }

    fn add_candidate(g: &mut ConceptGraph, label: &str, scope: ScopeId) -> NodeId {
        let n = ConceptNode::new_candidate(label, format!("def {label}"), scope);
        let id = n.id;
        g.add_node(n).unwrap();
        id
    }

    #[test]
    fn promotion_only_flips_anchor_state() {
        let (mut g, scope) = fresh_graph();
        let n = add_candidate(&mut g, "atlas", scope);
        let other = add_canonical(&mut g, "q3", scope);
        g.add_edge(ConceptEdge::new(n, other, RelationType::PartOf, scope))
            .unwrap();

        let engine = IncrementalUpdateEngine::default();
        let prop = engine
            .propagate(&mut g, ChangeEvent::NodePromoted { node: n })
            .unwrap();

        assert_eq!(g.get_node(n).unwrap().state, NodeState::Canonical);
        assert_eq!(prop.state_transitions.get(&n), Some(&NodeState::Canonical));
        assert!(g.get_node(other).unwrap().state == NodeState::Canonical);
        assert!(prop.affected.contains_node(n));
        assert!(prop.affected.contains_node(other));
        assert!(prop.recompute.contains(n));
    }

    #[test]
    fn promotion_of_already_canonical_is_noop_for_state() {
        let (mut g, scope) = fresh_graph();
        let n = add_canonical(&mut g, "atlas", scope);

        let engine = IncrementalUpdateEngine::default();
        let prop = engine
            .propagate(&mut g, ChangeEvent::NodePromoted { node: n })
            .unwrap();

        assert!(prop.state_transitions.is_empty());
        assert!(!prop.recompute.is_empty()); // anchor still in scope
    }

    #[test]
    fn supersession_cascades_through_derived_from() {
        let (mut g, scope) = fresh_graph();
        // A is the canonical anchor. B and C derive from A
        // (transitively): B derives directly from A, C derives
        // from B.
        let a = add_canonical(&mut g, "a", scope);
        let b = add_canonical(&mut g, "b", scope);
        let c = add_canonical(&mut g, "c", scope);
        let a2 = add_canonical(&mut g, "a2", scope);
        // derived_from edges point dependent → source.
        g.add_edge(ConceptEdge::new(b, a, RelationType::DerivedFrom, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(c, b, RelationType::DerivedFrom, scope))
            .unwrap();

        let engine = IncrementalUpdateEngine::default();
        let prop = engine
            .propagate(
                &mut g,
                ChangeEvent::NodeSuperseded {
                    predecessor: a,
                    successor: a2,
                },
            )
            .unwrap();

        assert_eq!(g.get_node(a).unwrap().state, NodeState::Superseded);
        assert!(prop.recompute.contains(a));
        assert!(prop.recompute.contains(b));
        assert!(prop.recompute.contains(c));
        assert!(prop.affected.contains_node(b));
        assert!(prop.affected.contains_node(c));
    }

    #[test]
    fn contradiction_propagation_marks_both_anchors() {
        let (mut g, scope) = fresh_graph();
        let a = add_canonical(&mut g, "a", scope);
        let b = add_canonical(&mut g, "b", scope);

        let engine = IncrementalUpdateEngine::default();
        let prop = engine
            .propagate(&mut g, ChangeEvent::NodeContradicted { a, b })
            .unwrap();

        assert_eq!(g.get_node(a).unwrap().state, NodeState::Contradicted);
        assert_eq!(g.get_node(b).unwrap().state, NodeState::Contradicted);
        assert_eq!(
            prop.state_transitions.get(&a),
            Some(&NodeState::Contradicted)
        );
        assert_eq!(
            prop.state_transitions.get(&b),
            Some(&NodeState::Contradicted)
        );
    }

    #[test]
    fn unrelated_neighbour_is_not_recomputed() {
        let (mut g, scope) = fresh_graph();
        // a and b are siblings under parent `p` via `is_a`,
        // but b does NOT derive from a → a change to a should
        // not mark b for recompute.
        let p = add_canonical(&mut g, "parent", scope);
        let a = add_canonical(&mut g, "a", scope);
        let b = add_canonical(&mut g, "b", scope);
        g.add_edge(ConceptEdge::new(a, p, RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(b, p, RelationType::IsA, scope))
            .unwrap();

        let engine = IncrementalUpdateEngine::default();
        let prop = engine
            .propagate(&mut g, ChangeEvent::NodePromoted { node: a })
            .unwrap();

        assert!(prop.recompute.contains(a));
        assert!(!prop.recompute.contains(b));
        assert!(!prop.recompute.contains(p));
    }

    #[test]
    fn missing_anchor_returns_node_not_found() {
        let (mut g, _scope) = fresh_graph();
        let phantom = NodeId::new_v4();
        let engine = IncrementalUpdateEngine::default();
        let err = engine
            .propagate(&mut g, ChangeEvent::NodePromoted { node: phantom })
            .unwrap_err();
        assert!(matches!(err, GraphError::NodeNotFound(_)));
    }

    #[test]
    fn edge_added_event_records_no_state_changes() {
        let (mut g, scope) = fresh_graph();
        let a = add_canonical(&mut g, "a", scope);
        let b = add_canonical(&mut g, "b", scope);
        let edge = ConceptEdge::new(a, b, RelationType::IsA, scope);
        let edge_id = edge.id;
        g.add_edge(edge).unwrap();

        let engine = IncrementalUpdateEngine::default();
        let prop = engine
            .propagate(&mut g, ChangeEvent::EdgeAdded { edge: edge_id })
            .unwrap();

        assert!(prop.state_transitions.is_empty());
        assert!(prop.affected.contains_edge(edge_id));
    }

    #[test]
    fn edge_removed_event_drops_edge_from_graph() {
        let (mut g, scope) = fresh_graph();
        let a = add_canonical(&mut g, "a", scope);
        let b = add_canonical(&mut g, "b", scope);
        let edge = ConceptEdge::new(a, b, RelationType::IsA, scope);
        let edge_id = edge.id;
        g.add_edge(edge).unwrap();
        assert_eq!(g.edge_count(), 1);

        let engine = IncrementalUpdateEngine::default();
        let prop = engine
            .propagate(&mut g, ChangeEvent::EdgeRemoved { edge: edge_id })
            .unwrap();

        assert_eq!(g.edge_count(), 0);
        assert!(prop.removed_edges.contains(&edge_id));
    }

    #[test]
    fn affected_subgraph_respects_max_depth() {
        let (mut g, scope) = fresh_graph();
        // Chain: c4 →(derived_from)→ c3 → c2 → c1 → a
        let a = add_canonical(&mut g, "a", scope);
        let c1 = add_canonical(&mut g, "c1", scope);
        let c2 = add_canonical(&mut g, "c2", scope);
        let c3 = add_canonical(&mut g, "c3", scope);
        let c4 = add_canonical(&mut g, "c4", scope);
        g.add_edge(ConceptEdge::new(c1, a, RelationType::DerivedFrom, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(c2, c1, RelationType::DerivedFrom, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(c3, c2, RelationType::DerivedFrom, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(c4, c3, RelationType::DerivedFrom, scope))
            .unwrap();

        let engine = IncrementalUpdateEngine::with_depth(2);
        let affected = engine.affected_subgraph(&g, &ChangeEvent::NodePromoted { node: a });
        // Depth-2 walk pulls in c1 (depth 1) and c2 (depth 2)
        // but stops there.
        assert!(affected.contains_node(a));
        assert!(affected.contains_node(c1));
        assert!(affected.contains_node(c2));
        assert!(!affected.contains_node(c3));
        assert!(!affected.contains_node(c4));
        assert!(affected.max_depth_reached >= 2);
    }
}
