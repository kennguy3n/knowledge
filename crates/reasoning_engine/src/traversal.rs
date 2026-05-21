//! Multi-hop typed-edge traversal over the concept graph.
//!
//! Per `docs/DESIGN.md` §11.1, the reasoning
//! engine traverses the sparse typed concept graph with explicit
//! budgets so the cost of any one query is bounded. Two modes
//! are supported:
//!
//! * Targeted (`A → B`) — find paths from a start node to a
//!   specific target node.
//! * Exploratory (`A → ?`) — fan out from a start node and
//!   return every node reached within budget.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use concept_graph::{ConceptEdge, ConceptGraph, EdgeId, NodeId, RelationType};
use evidence_store::ScopeId;
use serde::{Deserialize, Serialize};

/// Direction of traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TraversalDirection {
    /// Follow only `from -> to` edges (default).
    #[default]
    Outgoing,
    /// Follow only `to -> from` edges.
    Incoming,
    /// Follow either direction.
    Both,
}

/// Hard budgets enforced during traversal. Any breach short-
/// circuits the traversal and returns a partial result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TraversalBudget {
    /// Maximum number of hops from the start node.
    pub max_hops: usize,
    /// Maximum number of distinct nodes the traversal may
    /// dequeue.
    pub max_nodes_visited: usize,
    /// Wall-clock cap. Traversal returns after this many
    /// milliseconds even if the other budgets remain.
    pub max_time_ms: u64,
    /// Maximum edges followed *per hop*. Caps the fan-out at any
    /// single node; once exceeded the remaining edges at that
    /// node are dropped (this is a soft cap on the *expansion*,
    /// not on the total).
    pub max_edges_per_hop: usize,
}

impl Default for TraversalBudget {
    fn default() -> Self {
        Self {
            max_hops: 4,
            max_nodes_visited: 1_000,
            max_time_ms: 50,
            max_edges_per_hop: 64,
        }
    }
}

/// Definition of a single traversal request.
#[derive(Debug, Clone)]
pub struct TraversalQuery {
    /// Origin node.
    pub start: NodeId,
    /// Optional target — if `Some`, the traversal is targeted
    /// (`A → B`) and stops as soon as `target` is reached. If
    /// `None`, the traversal is exploratory (`A → ?`).
    pub target: Option<NodeId>,
    /// Restrict to edges of these types. Empty = no filter.
    pub edge_types: Vec<RelationType>,
    /// Restrict to edges within these scopes. Empty = no filter.
    pub scopes: Vec<ScopeId>,
    /// Direction.
    pub direction: TraversalDirection,
}

impl TraversalQuery {
    /// Construct an exploratory query.
    pub fn explore(start: NodeId) -> Self {
        Self {
            start,
            target: None,
            edge_types: Vec::new(),
            scopes: Vec::new(),
            direction: TraversalDirection::Outgoing,
        }
    }

    /// Construct a targeted query.
    pub fn between(start: NodeId, target: NodeId) -> Self {
        Self {
            start,
            target: Some(target),
            edge_types: Vec::new(),
            scopes: Vec::new(),
            direction: TraversalDirection::Outgoing,
        }
    }

    /// Constrain to one or more relation types.
    pub fn with_edge_types(mut self, types: Vec<RelationType>) -> Self {
        self.edge_types = types;
        self
    }

    /// Constrain to one or more scopes.
    pub fn with_scopes(mut self, scopes: Vec<ScopeId>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Override the traversal direction.
    pub fn with_direction(mut self, dir: TraversalDirection) -> Self {
        self.direction = dir;
        self
    }
}

/// One path the traversal materialised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraversedPath {
    /// Ordered list of node ids along the path, starting with
    /// the query's `start` node.
    pub nodes: Vec<NodeId>,
    /// Ordered list of edge ids traversed (length =
    /// `nodes.len() - 1`).
    pub edges: Vec<EdgeId>,
    /// Score in `[0.0, 1.0]` — higher is better. Populated by
    /// [`PathScorer`].
    pub score: f64,
}

/// Reasoning trace returned alongside the paths — useful for
/// audit, replays, and debugging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReasoningTrace {
    /// Number of nodes the traversal expanded.
    pub nodes_expanded: usize,
    /// Number of edges the traversal inspected.
    pub edges_inspected: usize,
    /// Number of hops actually taken (deepest level reached).
    pub hops_taken: usize,
    /// Wall-clock duration of the traversal.
    pub elapsed_ms: u64,
    /// Whether any budget was exhausted (`max_hops`,
    /// `max_nodes_visited`, `max_time_ms`).
    pub budget_exhausted: bool,
}

/// Output of a traversal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraversalResult {
    /// Paths returned by the traversal. For targeted queries,
    /// at most one path (the shortest). For exploratory queries,
    /// one path per reached node.
    pub paths: Vec<TraversedPath>,
    /// Distinct node ids visited (including `start`).
    pub visited: Vec<NodeId>,
    /// Reasoning trace.
    pub trace: ReasoningTrace,
    /// Wall-clock timestamp when the traversal completed.
    pub completed_at: DateTime<Utc>,
}

/// Scores a path on a `[0.0, 1.0]` scale. Higher is better.
#[derive(Debug, Clone)]
pub struct PathScorer {
    /// Per-relation weight in `[0.0, 1.0]`. Defaults to `1.0`
    /// for every relation; callers can downweight noisy
    /// relations.
    pub relation_weights: std::collections::HashMap<RelationType, f64>,
    /// Penalty applied per hop, in `[0.0, 1.0]`. The path's
    /// raw score is `relation_geom_mean * (1 - depth_penalty * len)`
    /// clamped to `[0, 1]`.
    pub depth_penalty: f64,
}

impl Default for PathScorer {
    fn default() -> Self {
        Self {
            relation_weights: std::collections::HashMap::new(),
            depth_penalty: 0.05,
        }
    }
}

impl PathScorer {
    /// Construct a scorer with default weights.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the weight for one relation type.
    pub fn with_weight(mut self, relation: RelationType, weight: f64) -> Self {
        self.relation_weights.insert(relation, weight);
        self
    }

    /// Override the per-hop depth penalty.
    pub fn with_depth_penalty(mut self, penalty: f64) -> Self {
        self.depth_penalty = penalty;
        self
    }

    /// Score one path. Caller must supply the relation of every
    /// edge in the path (`relations.len() == path.edges.len()`).
    pub fn score(&self, path_len: usize, relations: &[RelationType]) -> f64 {
        if relations.is_empty() {
            // A trivial single-node "path" — assign the maximum.
            return 1.0;
        }
        let mut prod = 1.0_f64;
        for r in relations {
            prod *= self.relation_weights.get(r).copied().unwrap_or(1.0);
        }
        let geom = prod.powf(1.0 / relations.len() as f64);
        let penalty = (self.depth_penalty * path_len as f64).min(1.0);
        (geom * (1.0 - penalty)).clamp(0.0, 1.0)
    }
}

/// Multi-hop typed-edge traversal driver.
#[derive(Debug, Clone)]
pub struct GraphTraversal<'g> {
    graph: &'g ConceptGraph,
    budget: TraversalBudget,
    scorer: PathScorer,
}

impl<'g> GraphTraversal<'g> {
    /// Construct a traversal with default budget and scorer.
    pub fn new(graph: &'g ConceptGraph) -> Self {
        Self {
            graph,
            budget: TraversalBudget::default(),
            scorer: PathScorer::default(),
        }
    }

    /// Override the budget.
    pub fn with_budget(mut self, budget: TraversalBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Override the path scorer.
    pub fn with_scorer(mut self, scorer: PathScorer) -> Self {
        self.scorer = scorer;
        self
    }

    fn edge_passes_filter(edge: &ConceptEdge, query: &TraversalQuery) -> bool {
        if !query.edge_types.is_empty() && !query.edge_types.contains(&edge.relation) {
            return false;
        }
        if !query.scopes.is_empty() && !query.scopes.contains(&edge.scope_id) {
            return false;
        }
        true
    }

    /// Run the traversal.
    pub fn run(&self, query: &TraversalQuery) -> TraversalResult {
        let start_time = Instant::now();
        let deadline = start_time + Duration::from_millis(self.budget.max_time_ms);

        let mut visited: HashSet<NodeId> = HashSet::from([query.start]);
        let mut visited_order: Vec<NodeId> = vec![query.start];
        let mut frontier: VecDeque<(NodeId, usize, Vec<NodeId>, Vec<EdgeId>, Vec<RelationType>)> =
            VecDeque::new();
        frontier.push_back((query.start, 0, vec![query.start], vec![], vec![]));

        let mut paths: Vec<TraversedPath> = Vec::new();
        let mut trace = ReasoningTrace::default();

        let nothing_to_visit = self.graph.get_node(query.start).is_none();
        if nothing_to_visit {
            trace.elapsed_ms = elapsed_ms_saturating(&start_time);
            return TraversalResult {
                paths,
                visited: visited_order,
                trace,
                completed_at: Utc::now(),
            };
        }

        let mut hit_target = false;

        while let Some((node, depth, node_path, edge_path, rel_path)) = frontier.pop_front() {
            trace.nodes_expanded += 1;
            trace.hops_taken = trace.hops_taken.max(depth);

            if Instant::now() >= deadline {
                trace.budget_exhausted = true;
                break;
            }
            if visited.len() >= self.budget.max_nodes_visited {
                trace.budget_exhausted = true;
                break;
            }
            if depth >= self.budget.max_hops {
                continue;
            }

            // For exploratory queries, every reached node is
            // also a "path" entry (excluding the start node
            // itself, which is implicit).
            if query.target.is_none() && node != query.start {
                let score = self.scorer.score(node_path.len() - 1, &rel_path);
                paths.push(TraversedPath {
                    nodes: node_path.clone(),
                    edges: edge_path.clone(),
                    score,
                });
            }

            let neighbours = self.expand(node, query);
            let mut edges_emitted = 0_usize;
            for (next, edge_id, relation) in neighbours {
                if edges_emitted >= self.budget.max_edges_per_hop {
                    break;
                }
                trace.edges_inspected += 1;
                if !visited.insert(next) {
                    continue;
                }
                visited_order.push(next);
                edges_emitted += 1;
                let mut np = node_path.clone();
                np.push(next);
                let mut ep = edge_path.clone();
                ep.push(edge_id);
                let mut rp = rel_path.clone();
                rp.push(relation);
                if Some(next) == query.target {
                    let score = self.scorer.score(np.len() - 1, &rp);
                    paths.push(TraversedPath {
                        nodes: np,
                        edges: ep,
                        score,
                    });
                    hit_target = true;
                    break;
                }
                frontier.push_back((next, depth + 1, np, ep, rp));
            }
            if hit_target {
                break;
            }
        }

        trace.elapsed_ms = elapsed_ms_saturating(&start_time);

        TraversalResult {
            paths,
            visited: visited_order,
            trace,
            completed_at: Utc::now(),
        }
    }

    fn expand(&self, node: NodeId, query: &TraversalQuery) -> Vec<(NodeId, EdgeId, RelationType)> {
        let edges = self.graph.get_edges(node);
        let mut out = Vec::new();
        for edge in edges {
            if !Self::edge_passes_filter(edge, query) {
                continue;
            }
            match query.direction {
                TraversalDirection::Outgoing => {
                    if edge.from == node {
                        out.push((edge.to, edge.id, edge.relation));
                    }
                }
                TraversalDirection::Incoming => {
                    if edge.to == node {
                        out.push((edge.from, edge.id, edge.relation));
                    }
                }
                TraversalDirection::Both => {
                    if edge.from == node {
                        out.push((edge.to, edge.id, edge.relation));
                    } else if edge.to == node {
                        out.push((edge.from, edge.id, edge.relation));
                    }
                }
            }
        }
        out
    }
}

/// `Instant::elapsed().as_millis()` is `u128`; the substrate's
/// reasoning budgets are sub-second so any value beyond `u64::MAX`
/// milliseconds (~585 million years) is impossible in practice.
/// `try_from` saturates defensively rather than wrapping if a
/// pathological caller (e.g. a stalled debug session) overflows.
fn elapsed_ms_saturating(start: &Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use concept_graph::{ConceptEdge, ConceptGraph, ConceptNode, NodeState, RelationType};
    use evidence_store::ScopeId;

    fn cn(scope: ScopeId, name: &str) -> ConceptNode {
        let mut n = ConceptNode::new_candidate(name.to_string(), String::new(), scope);
        n.state = NodeState::Canonical;
        n
    }

    fn link(g: &mut ConceptGraph, from: NodeId, to: NodeId, rel: RelationType, scope: ScopeId) {
        g.add_edge(ConceptEdge::new(from, to, rel, scope)).unwrap();
    }

    #[test]
    fn single_hop_targeted_traversal() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(cn(scope, "a")).unwrap();
        let b = g.add_node(cn(scope, "b")).unwrap();
        link(&mut g, a, b, RelationType::IsA, scope);

        let t = GraphTraversal::new(&g);
        let res = t.run(&TraversalQuery::between(a, b));
        assert_eq!(res.paths.len(), 1);
        assert_eq!(res.paths[0].nodes, vec![a, b]);
        assert_eq!(res.paths[0].edges.len(), 1);
        assert!(!res.trace.budget_exhausted);
    }

    #[test]
    fn multi_hop_targeted_traversal() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(cn(scope, "a")).unwrap();
        let b = g.add_node(cn(scope, "b")).unwrap();
        let c = g.add_node(cn(scope, "c")).unwrap();
        let d = g.add_node(cn(scope, "d")).unwrap();
        link(&mut g, a, b, RelationType::IsA, scope);
        link(&mut g, b, c, RelationType::IsA, scope);
        link(&mut g, c, d, RelationType::IsA, scope);

        let t = GraphTraversal::new(&g);
        let res = t.run(&TraversalQuery::between(a, d));
        assert_eq!(res.paths.len(), 1);
        assert_eq!(res.paths[0].nodes, vec![a, b, c, d]);
    }

    #[test]
    fn exploratory_traversal_returns_all_reachable() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(cn(scope, "a")).unwrap();
        let b = g.add_node(cn(scope, "b")).unwrap();
        let c = g.add_node(cn(scope, "c")).unwrap();
        link(&mut g, a, b, RelationType::IsA, scope);
        link(&mut g, a, c, RelationType::PartOf, scope);

        let t = GraphTraversal::new(&g);
        let res = t.run(&TraversalQuery::explore(a));
        let reached: HashSet<NodeId> = res.paths.iter().map(|p| *p.nodes.last().unwrap()).collect();
        assert_eq!(reached, HashSet::from([b, c]));
    }

    #[test]
    fn budget_max_hops_limits_depth() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(cn(scope, "a")).unwrap();
        let b = g.add_node(cn(scope, "b")).unwrap();
        let c = g.add_node(cn(scope, "c")).unwrap();
        link(&mut g, a, b, RelationType::IsA, scope);
        link(&mut g, b, c, RelationType::IsA, scope);

        let budget = TraversalBudget {
            max_hops: 1,
            ..TraversalBudget::default()
        };
        let t = GraphTraversal::new(&g).with_budget(budget);
        let res = t.run(&TraversalQuery::between(a, c));
        assert!(res.paths.is_empty());
        assert_eq!(res.trace.hops_taken, 1);
    }

    #[test]
    fn scope_filter_drops_other_scopes() {
        let scope_a = ScopeId::new_v4();
        let scope_b = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(cn(scope_a, "a")).unwrap();
        let b = g.add_node(cn(scope_a, "b")).unwrap();
        let c = g.add_node(cn(scope_b, "c")).unwrap();
        link(&mut g, a, b, RelationType::IsA, scope_a);
        link(&mut g, b, c, RelationType::IsA, scope_b);

        let q = TraversalQuery::between(a, c).with_scopes(vec![scope_a]);
        let t = GraphTraversal::new(&g);
        let res = t.run(&q);
        assert!(res.paths.is_empty());
    }

    #[test]
    fn typed_edge_filter_drops_other_types() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let a = g.add_node(cn(scope, "a")).unwrap();
        let b = g.add_node(cn(scope, "b")).unwrap();
        link(&mut g, a, b, RelationType::PartOf, scope);

        let q = TraversalQuery::between(a, b).with_edge_types(vec![RelationType::IsA]);
        let res = GraphTraversal::new(&g).run(&q);
        assert!(res.paths.is_empty());

        let q = TraversalQuery::between(a, b).with_edge_types(vec![RelationType::PartOf]);
        let res = GraphTraversal::new(&g).run(&q);
        assert_eq!(res.paths.len(), 1);
    }

    #[test]
    fn path_scorer_penalises_depth_and_relation_weights() {
        let s = PathScorer::new()
            .with_weight(RelationType::IsA, 1.0)
            .with_weight(RelationType::PartOf, 0.5)
            .with_depth_penalty(0.1);
        let high = s.score(1, &[RelationType::IsA]);
        let low = s.score(1, &[RelationType::PartOf]);
        let deep = s.score(
            3,
            &[RelationType::IsA, RelationType::IsA, RelationType::IsA],
        );
        assert!(high > low);
        assert!(high > deep);
    }
}
