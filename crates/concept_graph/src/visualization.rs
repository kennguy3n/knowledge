//! Concept graph visualization query API (Phase 6 / Kanvas).
//!
//! Per `PROPOSAL.md` §10.2 and `PHASES.md` Phase 6, the substrate
//! ships a "Kanvas-style" exploration model on top of the concept
//! graph: graphs of nodes with typed edges that the front-end can
//! render with scope-aware filtering. This module provides the
//! query-side data model used by the API gateway and any future
//! interactive UI.
//!
//! The shape of the API is deliberately conservative — no embedded
//! layout engine, no streaming, no incremental subscriptions. Those
//! belong above the substrate. What this module *does* guarantee:
//!
//! * Every traversal is bounded by a [`ViewFilter`] (scopes,
//!   relations, node states, depth, node count) so the worst case
//!   is `O(filter.max_nodes)` regardless of the underlying graph
//!   size.
//! * Every result is filtered through a caller-supplied
//!   [`ScopeAccess`] predicate so that nodes / edges in scopes the
//!   caller cannot see never leak into the [`GraphView`].
//!   Concretely the production wiring will hand a closure backed by
//!   the `permission_service` (`check_permission(scope, viewer,
//!   subject)`); the trait keeps `concept_graph` from depending on
//!   `permission_service` directly so the dependency graph stays a
//!   DAG.
//!
//! Cross-references:
//!
//! * Module map: `ARCHITECTURE.md` §2.1.
//! * Phase 6 deliverables: `PHASES.md` Phase 6.
//! * Permission model: `PROPOSAL.md` §7.1 and the
//!   `permission_service` crate.

use std::collections::{BTreeSet, HashSet, VecDeque};

use evidence_store::ScopeId;
use serde::{Deserialize, Serialize};

use crate::edge::{ConceptEdge, EdgeId, RelationType};
use crate::graph::ConceptGraph;
use crate::node::{ConceptNode, NodeId, NodeState};

/// Default node-count cap when [`ViewFilter::max_nodes`] is unset.
///
/// Sized to keep the front-end responsive on a single render frame
/// (~16 ms on a desktop browser). Callers that need more should pass
/// an explicit override and accept the latency cost.
pub const DEFAULT_MAX_NODES: usize = 256;

/// Scope-access predicate used to gate every node / edge that
/// surfaces from a query.
///
/// The trait is intentionally minimal — one yes/no question per
/// scope — so wiring it to a Zanzibar-style reachability check or a
/// simple in-memory allow-list both fit. The production wiring lives
/// in the API gateway: it constructs a closure that asks
/// `permission_service::check_permission(scope, Relation::Viewer,
/// subject)` for the calling subject and feeds the closure here.
///
/// All built-in queries call [`Self::can_view`] *every* time they
/// surface a node or edge — there is no implicit caching. Callers
/// that want caching should wrap their predicate in one.
pub trait ScopeAccess {
    /// Returns `true` iff the current subject can read scope `id` at
    /// `viewer` level or higher. The substrate's permission lattice
    /// is `Owner ⇒ Admin ⇒ Editor ⇒ Member ⇒ Viewer`, so a `viewer`
    /// check is the lowest bar.
    fn can_view(&self, scope: ScopeId) -> bool;
}

/// `ScopeAccess` impl that allows every scope. Useful for tests
/// that want to exercise the visualization layer in isolation
/// without standing up a full permission registry.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllScopes;

impl ScopeAccess for AllowAllScopes {
    fn can_view(&self, _scope: ScopeId) -> bool {
        true
    }
}

/// `ScopeAccess` impl backed by an explicit allow-list of scope ids.
/// Mostly useful in tests; production callers should plug in a
/// permission-service-backed closure instead.
#[derive(Debug, Clone, Default)]
pub struct AllowedScopeSet {
    allowed: HashSet<ScopeId>,
}

impl AllowedScopeSet {
    /// Construct an allow-list from the given scope ids.
    pub fn new(scopes: impl IntoIterator<Item = ScopeId>) -> Self {
        Self {
            allowed: scopes.into_iter().collect(),
        }
    }

    /// Add `scope` to the allow-list.
    pub fn allow(&mut self, scope: ScopeId) {
        self.allowed.insert(scope);
    }
}

impl ScopeAccess for AllowedScopeSet {
    fn can_view(&self, scope: ScopeId) -> bool {
        self.allowed.contains(&scope)
    }
}

impl<F> ScopeAccess for F
where
    F: Fn(ScopeId) -> bool,
{
    fn can_view(&self, scope: ScopeId) -> bool {
        (self)(scope)
    }
}

/// Filter applied to every visualization query.
///
/// The filter is intentionally conservative: every field defaults
/// to "do not restrict" (`None` / empty), so callers that want a
/// raw, unbounded view explicitly opt in with an empty
/// [`ViewFilter::default()`]. The single hard-coded default is
/// [`Self::max_nodes`] = [`DEFAULT_MAX_NODES`], which still applies
/// when it is left as `None` to bound traversal cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewFilter {
    /// Restrict to these scopes. Empty = no scope restriction
    /// (still gated by `ScopeAccess`).
    #[serde(default)]
    pub scope_ids: Vec<ScopeId>,
    /// Restrict to these node lifecycle states. Empty = all states.
    #[serde(default)]
    pub node_states: Vec<NodeState>,
    /// Restrict to these typed relations on edges. Empty = all
    /// relations.
    #[serde(default)]
    pub relation_types: Vec<RelationType>,
    /// Maximum BFS depth from the seed node(s). `None` = unbounded
    /// (still bounded by `max_nodes`).
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// Hard cap on the number of nodes returned. `None` =
    /// [`DEFAULT_MAX_NODES`].
    #[serde(default)]
    pub max_nodes: Option<usize>,
}

impl ViewFilter {
    /// Effective node cap honouring the [`DEFAULT_MAX_NODES`]
    /// fallback.
    pub fn effective_max_nodes(&self) -> usize {
        self.max_nodes.unwrap_or(DEFAULT_MAX_NODES)
    }

    fn allows_scope(&self, scope: ScopeId) -> bool {
        self.scope_ids.is_empty() || self.scope_ids.contains(&scope)
    }

    fn allows_state(&self, state: NodeState) -> bool {
        self.node_states.is_empty() || self.node_states.contains(&state)
    }

    fn allows_relation(&self, rel: RelationType) -> bool {
        self.relation_types.is_empty() || self.relation_types.contains(&rel)
    }

    fn allows_node(&self, node: &ConceptNode) -> bool {
        self.allows_scope(node.scope_id) && self.allows_state(node.state)
    }
}

/// A node as exposed to the visualization API.
///
/// The shape mirrors `ConceptNode` but trims the long-form
/// `definition` and the freeform `metadata` blob — those are
/// fetched on demand when the user opens a detail panel. We keep
/// `connections_count` so the front-end can size nodes without
/// rendering every edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeVisual {
    /// Stable identifier.
    pub id: NodeId,
    /// Short human-readable label.
    pub label: String,
    /// Lifecycle state (so the renderer can dim superseded /
    /// contradicted nodes).
    pub state: NodeState,
    /// Scope this node is bound to.
    pub scope_id: ScopeId,
    /// Optional layout-engine hint persisted on the node, propagated
    /// from `metadata.position` if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_hint: Option<PositionHint>,
    /// Number of *visible* incident edges in the surrounding
    /// [`GraphView`] (after filtering and access-gating). Lets the
    /// renderer size nodes without a second pass.
    pub connections_count: usize,
}

/// An edge as exposed to the visualization API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeVisual {
    /// Stable identifier.
    pub id: EdgeId,
    /// Source node id.
    pub from: NodeId,
    /// Target node id.
    pub to: NodeId,
    /// Typed relation.
    pub relation_type: RelationType,
    /// Scope this edge is bound to.
    pub scope_id: ScopeId,
}

/// 2-D layout hint persisted on a node, in arbitrary (logical)
/// canvas coordinates. The renderer is free to ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PositionHint {
    /// X coordinate in logical canvas units.
    pub x: f64,
    /// Y coordinate in logical canvas units.
    pub y: f64,
}

/// Why a query terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// The query exhausted the graph within the bounds — every
    /// reachable, in-scope, in-state node is included.
    Complete,
    /// The query hit [`ViewFilter::max_nodes`] before exhausting
    /// the frontier. The view is partial.
    NodeLimitReached,
    /// The query hit [`ViewFilter::max_depth`] before exhausting
    /// the frontier. The view is partial.
    DepthLimitReached,
}

/// Response shape for every visualization query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphView {
    /// Nodes returned, deduped, in BFS-discovery order.
    pub nodes: Vec<NodeVisual>,
    /// Edges between nodes that are *both* in [`Self::nodes`].
    /// Edges to truncated/forbidden nodes are dropped.
    pub edges: Vec<EdgeVisual>,
    /// Echo of the scope filter applied (post-merge of
    /// [`ScopeAccess`] gating). Useful for the front-end to render
    /// the active filter chip.
    pub scope_filter: Vec<ScopeId>,
    /// Maximum traversal depth actually used.
    pub depth: usize,
    /// Why the traversal stopped.
    pub truncation: TruncationReason,
}

impl GraphView {
    fn empty(filter: &ViewFilter) -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            scope_filter: filter.scope_ids.clone(),
            depth: 0,
            truncation: TruncationReason::Complete,
        }
    }
}

/// Reify a [`ConceptNode`] into a [`NodeVisual`], extracting an
/// optional `metadata.position` hint if present. The hint is read
/// out of the freeform metadata blob so the schema stays optional.
fn to_node_visual(node: &ConceptNode, connections_count: usize) -> NodeVisual {
    let position_hint = node.metadata.get("position").and_then(|p| {
        let x = p.get("x").and_then(serde_json::Value::as_f64)?;
        let y = p.get("y").and_then(serde_json::Value::as_f64)?;
        Some(PositionHint { x, y })
    });
    NodeVisual {
        id: node.id,
        label: node.label.clone(),
        state: node.state,
        scope_id: node.scope_id,
        position_hint,
        connections_count,
    }
}

fn to_edge_visual(edge: &ConceptEdge) -> EdgeVisual {
    EdgeVisual {
        id: edge.id,
        from: edge.from,
        to: edge.to,
        relation_type: edge.relation,
        scope_id: edge.scope_id,
    }
}

fn node_passes(node: &ConceptNode, filter: &ViewFilter, access: &impl ScopeAccess) -> bool {
    filter.allows_node(node) && access.can_view(node.scope_id)
}

fn edge_passes(edge: &ConceptEdge, filter: &ViewFilter, access: &impl ScopeAccess) -> bool {
    filter.allows_relation(edge.relation)
        && filter.allows_scope(edge.scope_id)
        && access.can_view(edge.scope_id)
}

/// BFS-explore the graph starting at `start`, honouring `filter`
/// and `access`. Edges are followed in both directions (the
/// underlying graph is directed but exploration is undirected so
/// the front-end can render incoming relations the user navigates
/// to).
///
/// Returns an empty [`GraphView`] if `start` is missing or fails
/// the filter/access check. Truncation reason explains whether the
/// view is partial.
pub fn explore_from(
    graph: &ConceptGraph,
    start: NodeId,
    filter: &ViewFilter,
    access: &impl ScopeAccess,
) -> GraphView {
    let Some(seed) = graph.get_node(start) else {
        return GraphView::empty(filter);
    };
    if !node_passes(seed, filter, access) {
        return GraphView::empty(filter);
    }
    bfs_collect(graph, std::iter::once(start), filter, access)
}

/// All nodes in `scope_id` (after filtering / access-gating), and
/// every edge that lives in the same scope between them.
///
/// `filter.scope_ids` is *narrowed* to `scope_id` for this query —
/// passing extra scopes in the filter has no effect. This keeps
/// the call site easy to read: `subgraph_for_scope(g, my_scope,
/// &ViewFilter::default(), &access)` returns exactly the nodes /
/// edges in `my_scope`.
pub fn subgraph_for_scope(
    graph: &ConceptGraph,
    scope_id: ScopeId,
    filter: &ViewFilter,
    access: &impl ScopeAccess,
) -> GraphView {
    if !access.can_view(scope_id) {
        return GraphView::empty(filter);
    }
    let cap = filter.effective_max_nodes();

    let mut node_visuals: Vec<NodeVisual> = Vec::new();
    let mut included_ids: HashSet<NodeId> = HashSet::new();
    let mut connections: std::collections::HashMap<NodeId, usize> =
        std::collections::HashMap::new();
    let mut truncation = TruncationReason::Complete;

    // Collect candidate nodes in a stable order so the response is
    // deterministic across runs (HashMap iteration order is not).
    let mut candidates: Vec<&ConceptNode> = graph
        .iter_nodes()
        .filter(|n| n.scope_id == scope_id)
        .filter(|n| filter.allows_state(n.state))
        .collect();
    candidates.sort_by_key(|n| n.id);

    for node in candidates {
        if node_visuals.len() >= cap {
            truncation = TruncationReason::NodeLimitReached;
            break;
        }
        if !access.can_view(node.scope_id) {
            continue;
        }
        included_ids.insert(node.id);
        // We push placeholder zero connections counts and rewrite
        // them in a second pass after collecting edges.
        node_visuals.push(to_node_visual(node, 0));
    }

    let mut edges: Vec<EdgeVisual> = Vec::new();
    let mut edge_keys: Vec<&ConceptEdge> = graph
        .iter_edges()
        .filter(|e| e.scope_id == scope_id)
        .collect();
    edge_keys.sort_by_key(|e| e.id);
    for edge in edge_keys {
        // Edges have already been pre-filtered to `e.scope_id ==
        // scope_id`, so we deliberately skip `filter.allows_scope`
        // here — `subgraph_for_scope` narrows the scope filter to
        // `scope_id` regardless of what the caller put in
        // `filter.scope_ids`. Mirrors the node loop above which also
        // bypasses `filter.scope_ids`.
        if !filter.allows_relation(edge.relation) || !access.can_view(edge.scope_id) {
            continue;
        }
        if !included_ids.contains(&edge.from) || !included_ids.contains(&edge.to) {
            continue;
        }
        *connections.entry(edge.from).or_default() += 1;
        *connections.entry(edge.to).or_default() += 1;
        edges.push(to_edge_visual(edge));
    }

    for nv in &mut node_visuals {
        nv.connections_count = *connections.get(&nv.id).unwrap_or(&0);
    }

    GraphView {
        nodes: node_visuals,
        edges,
        scope_filter: vec![scope_id],
        depth: 0,
        truncation,
    }
}

/// N-hop neighbourhood centred on `center`. Equivalent to
/// [`explore_from`] but with `filter.max_depth` *forced* to
/// `depth`. If the caller already set a stricter `max_depth`, the
/// stricter value wins.
pub fn neighborhood(
    graph: &ConceptGraph,
    center: NodeId,
    depth: usize,
    filter: &ViewFilter,
    access: &impl ScopeAccess,
) -> GraphView {
    let mut local = filter.clone();
    let effective_depth = match local.max_depth {
        Some(prev) => prev.min(depth),
        None => depth,
    };
    local.max_depth = Some(effective_depth);
    explore_from(graph, center, &local, access)
}

/// Search nodes by case-insensitive substring match on `label` or
/// `definition`, returning at most [`ViewFilter::effective_max_nodes`]
/// results (after access-gating). Results are sorted by id for
/// determinism.
pub fn search_nodes(
    graph: &ConceptGraph,
    query_text: &str,
    filter: &ViewFilter,
    access: &impl ScopeAccess,
) -> Vec<NodeVisual> {
    let needle = query_text.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let cap = filter.effective_max_nodes();
    let mut hits: Vec<&ConceptNode> = graph
        .iter_nodes()
        .filter(|n| node_passes(n, filter, access))
        .filter(|n| {
            n.label.to_lowercase().contains(&needle)
                || n.definition.to_lowercase().contains(&needle)
        })
        .collect();
    hits.sort_by_key(|n| n.id);
    hits.into_iter()
        .take(cap)
        .map(|n| to_node_visual(n, 0))
        .collect()
}

fn bfs_collect(
    graph: &ConceptGraph,
    seeds: impl IntoIterator<Item = NodeId>,
    filter: &ViewFilter,
    access: &impl ScopeAccess,
) -> GraphView {
    let cap = filter.effective_max_nodes();
    let depth_cap = filter.max_depth;
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut included_ids: HashSet<NodeId> = HashSet::new();
    let mut node_visuals: Vec<NodeVisual> = Vec::new();
    let mut frontier: VecDeque<(NodeId, usize)> = VecDeque::new();
    let mut max_depth_seen: usize = 0;
    let mut truncation = TruncationReason::Complete;
    let mut scope_set: BTreeSet<ScopeId> = BTreeSet::new();

    for s in seeds {
        if visited.insert(s) {
            frontier.push_back((s, 0));
        }
    }

    while let Some((id, depth)) = frontier.pop_front() {
        max_depth_seen = max_depth_seen.max(depth);
        if node_visuals.len() >= cap {
            truncation = TruncationReason::NodeLimitReached;
            break;
        }
        let Some(node) = graph.get_node(id) else {
            continue;
        };
        if !node_passes(node, filter, access) {
            continue;
        }
        included_ids.insert(id);
        scope_set.insert(node.scope_id);
        node_visuals.push(to_node_visual(node, 0));

        if depth_cap.is_some_and(|max| depth >= max) {
            truncation = TruncationReason::DepthLimitReached;
            continue;
        }

        // Enqueue both outgoing and incoming neighbours so the
        // exploration is undirected from the front-end's POV.
        let mut next_ids: Vec<NodeId> = Vec::new();
        for edge in graph.get_edges(id) {
            if !edge_passes(edge, filter, access) {
                continue;
            }
            let other = if edge.from == id { edge.to } else { edge.from };
            next_ids.push(other);
        }
        // Sort for deterministic exploration order in tests.
        next_ids.sort();
        next_ids.dedup();
        for next in next_ids {
            if visited.insert(next) {
                frontier.push_back((next, depth + 1));
            }
        }
    }

    // Second pass: collect edges between included nodes only and
    // tally connection counts. We walk every edge in the graph
    // once, which is `O(|E|)` — acceptable since the BFS already
    // bounded the working set.
    let mut edges: Vec<EdgeVisual> = Vec::new();
    let mut connections: std::collections::HashMap<NodeId, usize> =
        std::collections::HashMap::new();
    let mut edge_seen: HashSet<EdgeId> = HashSet::new();
    let mut sorted_edges: Vec<&ConceptEdge> = graph.iter_edges().collect();
    sorted_edges.sort_by_key(|e| e.id);
    for edge in sorted_edges {
        if !included_ids.contains(&edge.from) || !included_ids.contains(&edge.to) {
            continue;
        }
        if !edge_passes(edge, filter, access) {
            continue;
        }
        if !edge_seen.insert(edge.id) {
            continue;
        }
        *connections.entry(edge.from).or_default() += 1;
        *connections.entry(edge.to).or_default() += 1;
        edges.push(to_edge_visual(edge));
    }

    for nv in &mut node_visuals {
        nv.connections_count = *connections.get(&nv.id).unwrap_or(&0);
    }

    GraphView {
        nodes: node_visuals,
        edges,
        scope_filter: if filter.scope_ids.is_empty() {
            scope_set.into_iter().collect()
        } else {
            filter.scope_ids.clone()
        },
        depth: max_depth_seen,
        truncation,
    }
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use crate::edge::{ConceptEdge, RelationType};
    use crate::graph::ConceptGraph;
    use crate::node::ConceptNode;
    use evidence_store::ScopeId;

    fn mk_node(scope: ScopeId, label: &str) -> ConceptNode {
        ConceptNode::new_candidate(label, format!("{label}-def"), scope)
    }

    fn promote(mut n: ConceptNode) -> ConceptNode {
        n.mark_canonical();
        n
    }

    #[test]
    fn empty_graph_returns_empty_view() {
        let g = ConceptGraph::new();
        let scope = ScopeId::new_v4();
        let v = explore_from(
            &g,
            NodeId::new_v4(),
            &ViewFilter::default(),
            &AllowAllScopes,
        );
        assert!(v.nodes.is_empty());
        assert!(v.edges.is_empty());
        assert_eq!(v.depth, 0);
        let s = subgraph_for_scope(&g, scope, &ViewFilter::default(), &AllowAllScopes);
        assert!(s.nodes.is_empty());
        assert_eq!(s.scope_filter, vec![scope]);
    }

    #[test]
    fn explore_from_visits_neighbours() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(scope, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(scope, "B"))).unwrap();
        let c = g.add_node(promote(mk_node(scope, "C"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(b, c, RelationType::PartOf, scope))
            .unwrap();
        let v = explore_from(&g, a, &ViewFilter::default(), &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, HashSet::from([a, b, c]));
        assert_eq!(v.edges.len(), 2);
        assert_eq!(v.depth, 2);
        assert_eq!(v.truncation, TruncationReason::Complete);
    }

    #[test]
    fn depth_limit_truncates_traversal() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(scope, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(scope, "B"))).unwrap();
        let c = g.add_node(promote(mk_node(scope, "C"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(b, c, RelationType::IsA, scope))
            .unwrap();
        let filter = ViewFilter {
            max_depth: Some(1),
            ..Default::default()
        };
        let v = explore_from(&g, a, &filter, &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
        assert!(!ids.contains(&c));
        assert_eq!(v.truncation, TruncationReason::DepthLimitReached);
    }

    #[test]
    fn node_limit_truncates_traversal() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(scope, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(scope, "B"))).unwrap();
        let _c = g.add_node(promote(mk_node(scope, "C"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
            .unwrap();
        let filter = ViewFilter {
            max_nodes: Some(1),
            ..Default::default()
        };
        let v = explore_from(&g, a, &filter, &AllowAllScopes);
        assert_eq!(v.nodes.len(), 1);
        assert_eq!(v.truncation, TruncationReason::NodeLimitReached);
    }

    #[test]
    fn scope_filter_excludes_other_scopes() {
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(s1, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(s2, "B"))).unwrap();
        // cross-scope edge — explicit
        g.add_edge(ConceptEdge::new(a, b, RelationType::DerivedFrom, s1))
            .unwrap();
        let filter = ViewFilter {
            scope_ids: vec![s1],
            ..Default::default()
        };
        let v = explore_from(&g, a, &filter, &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a));
        assert!(!ids.contains(&b));
    }

    #[test]
    fn permission_predicate_hides_inaccessible_scopes() {
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(s1, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(s2, "B"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::DerivedFrom, s1))
            .unwrap();
        // Caller can only see s1.
        let access = AllowedScopeSet::new([s1]);
        let v = explore_from(&g, a, &ViewFilter::default(), &access);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a));
        assert!(!ids.contains(&b));
    }

    #[test]
    fn permission_predicate_blocks_unauthorized_seed() {
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let _a = g.add_node(promote(mk_node(s1, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(s2, "B"))).unwrap();
        let access = AllowedScopeSet::new([s1]);
        let v = explore_from(&g, b, &ViewFilter::default(), &access);
        assert!(v.nodes.is_empty());
        assert!(v.edges.is_empty());
    }

    #[test]
    fn relation_filter_drops_other_relations() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(scope, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(scope, "B"))).unwrap();
        let c = g.add_node(promote(mk_node(scope, "C"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(a, c, RelationType::DerivedFrom, scope))
            .unwrap();
        let filter = ViewFilter {
            relation_types: vec![RelationType::IsA],
            ..Default::default()
        };
        let v = explore_from(&g, a, &filter, &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
        assert!(!ids.contains(&c));
        assert_eq!(v.edges.len(), 1);
        assert_eq!(v.edges[0].relation_type, RelationType::IsA);
    }

    #[test]
    fn node_state_filter_drops_candidates() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let canonical = g.add_node(promote(mk_node(scope, "Canon"))).unwrap();
        let candidate = g.add_node(mk_node(scope, "Cand")).unwrap();
        g.add_edge(ConceptEdge::new(
            canonical,
            candidate,
            RelationType::IsA,
            scope,
        ))
        .unwrap();
        let filter = ViewFilter {
            node_states: vec![NodeState::Canonical],
            ..Default::default()
        };
        let v = explore_from(&g, canonical, &filter, &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&canonical));
        assert!(!ids.contains(&candidate));
    }

    #[test]
    fn subgraph_for_scope_returns_nodes_and_edges_in_scope() {
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(s1, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(s1, "B"))).unwrap();
        let c = g.add_node(promote(mk_node(s2, "C"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, s1))
            .unwrap();
        g.add_edge(ConceptEdge::new(a, c, RelationType::DerivedFrom, s1))
            .unwrap();
        let v = subgraph_for_scope(&g, s1, &ViewFilter::default(), &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, HashSet::from([a, b]));
        // The cross-scope edge is dropped because `c` isn't in the view.
        assert_eq!(v.edges.len(), 1);
        let connections: std::collections::HashMap<_, _> = v
            .nodes
            .iter()
            .map(|n| (n.id, n.connections_count))
            .collect();
        assert_eq!(connections[&a], 1);
        assert_eq!(connections[&b], 1);
    }

    #[test]
    fn subgraph_for_scope_ignores_caller_supplied_scope_filter() {
        // Regression: `subgraph_for_scope` documents that it narrows
        // `filter.scope_ids` to the target scope, so a caller passing
        // an unrelated scope in the filter must not silently drop the
        // edges in the requested scope.
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(s1, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(s1, "B"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, s1))
            .unwrap();
        let filter = ViewFilter {
            scope_ids: vec![s2],
            ..Default::default()
        };
        let v = subgraph_for_scope(&g, s1, &filter, &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, HashSet::from([a, b]));
        assert_eq!(v.edges.len(), 1);
        assert_eq!(v.scope_filter, vec![s1]);
        let connections: std::collections::HashMap<_, _> = v
            .nodes
            .iter()
            .map(|n| (n.id, n.connections_count))
            .collect();
        assert_eq!(connections[&a], 1);
        assert_eq!(connections[&b], 1);
    }

    #[test]
    fn subgraph_for_scope_blocked_when_no_permission() {
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let _ = g.add_node(promote(mk_node(s1, "A"))).unwrap();
        let access = AllowedScopeSet::new([s2]);
        let v = subgraph_for_scope(&g, s1, &ViewFilter::default(), &access);
        assert!(v.nodes.is_empty());
        assert!(v.edges.is_empty());
    }

    #[test]
    fn neighborhood_clamps_max_depth_to_arg() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(scope, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(scope, "B"))).unwrap();
        let c = g.add_node(promote(mk_node(scope, "C"))).unwrap();
        let d = g.add_node(promote(mk_node(scope, "D"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(b, c, RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(c, d, RelationType::IsA, scope))
            .unwrap();
        // depth=2 from a: {a, b, c}, but not d.
        let v = neighborhood(&g, a, 2, &ViewFilter::default(), &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
        assert!(ids.contains(&c));
        assert!(!ids.contains(&d));
    }

    #[test]
    fn neighborhood_takes_min_with_filter_max_depth() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(scope, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(scope, "B"))).unwrap();
        let c = g.add_node(promote(mk_node(scope, "C"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(b, c, RelationType::IsA, scope))
            .unwrap();
        // filter says depth=0, arg says depth=5 -> min wins, only `a`.
        let filter = ViewFilter {
            max_depth: Some(0),
            ..Default::default()
        };
        let v = neighborhood(&g, a, 5, &filter, &AllowAllScopes);
        let ids: HashSet<_> = v.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, HashSet::from([a]));
    }

    #[test]
    fn search_nodes_matches_label_and_definition() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let mut a = mk_node(scope, "Atlas");
        a.mark_canonical();
        let mut b = ConceptNode::new_candidate("Project", "Atlas-related work", scope);
        b.mark_canonical();
        let mut c = mk_node(scope, "Unrelated");
        c.mark_canonical();
        let a_id = g.add_node(a).unwrap();
        let b_id = g.add_node(b).unwrap();
        let _ = g.add_node(c).unwrap();
        let hits = search_nodes(&g, "atlas", &ViewFilter::default(), &AllowAllScopes);
        let ids: HashSet<_> = hits.iter().map(|n| n.id).collect();
        assert_eq!(ids, HashSet::from([a_id, b_id]));
    }

    #[test]
    fn search_nodes_respects_permission_and_scope() {
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(s1, "Atlas-1"))).unwrap();
        let _b = g.add_node(promote(mk_node(s2, "Atlas-2"))).unwrap();
        let access = AllowedScopeSet::new([s1]);
        let hits = search_nodes(&g, "atlas", &ViewFilter::default(), &access);
        let ids: HashSet<_> = hits.iter().map(|n| n.id).collect();
        assert_eq!(ids, HashSet::from([a]));
    }

    #[test]
    fn search_nodes_empty_query_returns_nothing() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let _a = g.add_node(promote(mk_node(scope, "Atlas"))).unwrap();
        let hits = search_nodes(&g, "  ", &ViewFilter::default(), &AllowAllScopes);
        assert!(hits.is_empty());
    }

    #[test]
    fn position_hint_extracted_from_metadata() {
        let scope = ScopeId::new_v4();
        let mut node = mk_node(scope, "Pos");
        node.mark_canonical();
        node.metadata = serde_json::json!({"position": {"x": 12.5, "y": -3.0}});
        let mut g = ConceptGraph::new();
        let id = g.add_node(node).unwrap();
        let v = explore_from(&g, id, &ViewFilter::default(), &AllowAllScopes);
        let pos = v.nodes[0].position_hint.expect("hint");
        assert!((pos.x - 12.5).abs() < f64::EPSILON);
        assert!((pos.y - (-3.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn graph_view_round_trips_through_serde() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(promote(mk_node(scope, "A"))).unwrap();
        let b = g.add_node(promote(mk_node(scope, "B"))).unwrap();
        g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
            .unwrap();
        let v = explore_from(&g, a, &ViewFilter::default(), &AllowAllScopes);
        let json = serde_json::to_string(&v).expect("serialize");
        let back: GraphView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.nodes.len(), v.nodes.len());
        assert_eq!(back.edges.len(), v.edges.len());
    }
}
