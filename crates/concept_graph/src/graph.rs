//! [`ConceptGraph`] — in-memory adjacency-backed concept graph.
//!
//! A pure in-memory `HashMap` adjacency list. Persistence to the
//! encrypted store and CRDT delta sync are not yet wired.

use std::collections::{HashMap, HashSet};

use crate::edge::{ConceptEdge, EdgeId, RelationType};
use crate::error::{GraphError, Result};
use crate::node::{ConceptNode, NodeId};

/// Sparse typed concept graph.
///
/// Nodes are stored in a single map; outgoing and incoming edges are
/// indexed in two adjacency `HashMap`s for `O(1)` neighbour lookup.
/// All mutating operations validate referential integrity (no
/// dangling edges, no double-insert).
#[derive(Debug, Default, Clone)]
pub struct ConceptGraph {
    nodes: HashMap<NodeId, ConceptNode>,
    edges: HashMap<EdgeId, ConceptEdge>,
    /// `from -> [edge_id]`
    outgoing: HashMap<NodeId, Vec<EdgeId>>,
    /// `to -> [edge_id]`
    incoming: HashMap<NodeId, Vec<EdgeId>>,
}

impl ConceptGraph {
    /// Construct a fresh empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of nodes currently in the graph (any state).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges currently in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Iterate every node in the graph in unspecified order.
    ///
    /// Order is *not* guaranteed (the underlying storage is a
    /// `HashMap`); callers that need a stable order must sort the
    /// result explicitly.
    pub fn iter_nodes(&self) -> impl Iterator<Item = &ConceptNode> {
        self.nodes.values()
    }

    /// Iterate every edge in the graph in unspecified order.
    pub fn iter_edges(&self) -> impl Iterator<Item = &ConceptEdge> {
        self.edges.values()
    }

    /// Insert `node` into the graph. Returns the node id.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::DuplicateNode`] if a node with the same
    /// id is already present.
    pub fn add_node(&mut self, node: ConceptNode) -> Result<NodeId> {
        if self.nodes.contains_key(&node.id) {
            return Err(GraphError::DuplicateNode(node.id.0));
        }
        let id = node.id;
        self.nodes.insert(id, node);
        self.outgoing.entry(id).or_default();
        self.incoming.entry(id).or_default();
        Ok(id)
    }

    /// Insert an edge between two existing nodes. Returns the edge id.
    ///
    /// # Errors
    ///
    /// * [`GraphError::DanglingEdge`] if either endpoint is missing.
    pub fn add_edge(&mut self, edge: ConceptEdge) -> Result<EdgeId> {
        if !self.nodes.contains_key(&edge.from) {
            return Err(GraphError::DanglingEdge(edge.from.0));
        }
        if !self.nodes.contains_key(&edge.to) {
            return Err(GraphError::DanglingEdge(edge.to.0));
        }
        let id = edge.id;
        self.outgoing.entry(edge.from).or_default().push(id);
        self.incoming.entry(edge.to).or_default().push(id);
        self.edges.insert(id, edge);
        Ok(id)
    }

    /// Remove `node` and every incident edge (both directions).
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeNotFound`] if no such node.
    pub fn remove_node(&mut self, id: NodeId) -> Result<ConceptNode> {
        let node = self
            .nodes
            .remove(&id)
            .ok_or(GraphError::node_not_found(id))?;

        let outgoing = self.outgoing.remove(&id).unwrap_or_default();
        let incoming = self.incoming.remove(&id).unwrap_or_default();
        let mut to_remove: HashSet<EdgeId> = HashSet::new();
        to_remove.extend(outgoing);
        to_remove.extend(incoming);
        for edge_id in &to_remove {
            if let Some(edge) = self.edges.remove(edge_id) {
                if let Some(v) = self.outgoing.get_mut(&edge.from) {
                    v.retain(|e| e != edge_id);
                }
                if let Some(v) = self.incoming.get_mut(&edge.to) {
                    v.retain(|e| e != edge_id);
                }
            }
        }
        Ok(node)
    }

    /// Remove a single edge by id without touching either endpoint.
    ///
    /// Used by [`crate::PersistentConceptGraph`] to roll back an
    /// in-memory `add_edge` whose mirror persistence call failed.
    /// Callers that want to drop a node's *incident* edges should
    /// use [`Self::remove_node`] instead — that variant is the
    /// usual "delete this node and everything pointing at it"
    /// operation.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::EdgeNotFound`] if no edge with that id
    /// exists.
    pub fn remove_edge(&mut self, id: EdgeId) -> Result<ConceptEdge> {
        let edge = self
            .edges
            .remove(&id)
            .ok_or_else(|| GraphError::edge_not_found(id))?;
        if let Some(v) = self.outgoing.get_mut(&edge.from) {
            v.retain(|e| *e != id);
        }
        if let Some(v) = self.incoming.get_mut(&edge.to) {
            v.retain(|e| *e != id);
        }
        Ok(edge)
    }

    /// Borrow a node by id.
    pub fn get_node(&self, id: NodeId) -> Option<&ConceptNode> {
        self.nodes.get(&id)
    }

    /// Mutable borrow of a node by id.
    pub fn get_node_mut(&mut self, id: NodeId) -> Option<&mut ConceptNode> {
        self.nodes.get_mut(&id)
    }

    /// All edges incident on `id` (outgoing + incoming).
    pub fn get_edges(&self, id: NodeId) -> Vec<&ConceptEdge> {
        let mut out = Vec::new();
        if let Some(ids) = self.outgoing.get(&id) {
            for e in ids {
                if let Some(edge) = self.edges.get(e) {
                    out.push(edge);
                }
            }
        }
        if let Some(ids) = self.incoming.get(&id) {
            for e in ids {
                if let Some(edge) = self.edges.get(e) {
                    out.push(edge);
                }
            }
        }
        out
    }

    /// Outgoing neighbours of `id` (the `to` end of every outgoing
    /// edge), de-duplicated. Pass `relation = Some(_)` to filter by a
    /// specific relation type.
    pub fn neighbors(&self, id: NodeId, relation: Option<RelationType>) -> Vec<NodeId> {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut out = Vec::new();
        let Some(edge_ids) = self.outgoing.get(&id) else {
            return out;
        };
        for e in edge_ids {
            if let Some(edge) = self.edges.get(e) {
                if relation.is_some_and(|r| r != edge.relation) {
                    continue;
                }
                if seen.insert(edge.to) {
                    out.push(edge.to);
                }
            }
        }
        out
    }

    /// Mark `predecessor` as superseded by `successor` and add an
    /// explicit `predecessor -[supersedes]-> successor` edge.
    ///
    /// Per `docs/DESIGN.md` §4: "supersession preferred over deletion".
    /// The predecessor is preserved with its `superseded_by` pointer
    /// set so audit and contradiction tracking can find it.
    pub fn supersede_node(&mut self, predecessor: NodeId, successor: NodeId) -> Result<EdgeId> {
        if predecessor == successor {
            return Err(GraphError::SelfSupersession(predecessor.0));
        }
        if !self.nodes.contains_key(&successor) {
            return Err(GraphError::node_not_found(successor));
        }
        let predecessor_scope = {
            let pred = self
                .nodes
                .get_mut(&predecessor)
                .ok_or(GraphError::node_not_found(predecessor))?;
            pred.mark_superseded_by(successor);
            pred.scope_id
        };
        let edge = ConceptEdge::new(
            predecessor,
            successor,
            RelationType::Supersedes,
            predecessor_scope,
        );
        self.add_edge(edge)
    }

    /// Mark `a` as contradicting `b`: stamps both nodes with the
    /// `Contradicted` state pointing at each other and adds a
    /// reciprocal pair of `contradicts` edges.
    pub fn mark_contradiction(&mut self, a: NodeId, b: NodeId) -> Result<(EdgeId, EdgeId)> {
        if a == b {
            return Err(GraphError::SelfContradiction(a.0));
        }
        // Validate both endpoints exist *before* mutating either —
        // mirrors the pattern in [`Self::supersede_node`]. Without
        // this check, a missing `b` would leave `a` partially
        // mutated (state = Contradicted, dangling `superseded_by`
        // pointer) before the error is surfaced to the caller.
        if !self.nodes.contains_key(&a) {
            return Err(GraphError::node_not_found(a));
        }
        if !self.nodes.contains_key(&b) {
            return Err(GraphError::node_not_found(b));
        }
        let (a_scope, b_scope) = {
            let na = self
                .nodes
                .get_mut(&a)
                .expect("checked above with contains_key");
            na.mark_contradicted_by(b);
            let a_scope = na.scope_id;
            let nb = self
                .nodes
                .get_mut(&b)
                .expect("checked above with contains_key");
            nb.mark_contradicted_by(a);
            (a_scope, nb.scope_id)
        };
        let e1 = self.add_edge(ConceptEdge::new(a, b, RelationType::Contradicts, a_scope))?;
        let e2 = self.add_edge(ConceptEdge::new(b, a, RelationType::Contradicts, b_scope))?;
        Ok((e1, e2))
    }

    /// Breadth-first typed-edge traversal starting at `start`,
    /// following only edges whose `relation` matches.
    ///
    /// `max_depth` caps the walk; pass `None` for unbounded. Returns
    /// the visited node ids in BFS order, **excluding** `start`
    /// itself (matching the convention of "neighbours within `n`
    /// hops").
    pub fn traverse_typed(
        &self,
        start: NodeId,
        relation: RelationType,
        max_depth: Option<usize>,
    ) -> Vec<NodeId> {
        let mut visited: HashSet<NodeId> = HashSet::from([start]);
        let mut frontier: Vec<(NodeId, usize)> = vec![(start, 0)];
        let mut out: Vec<NodeId> = Vec::new();
        while let Some((node, depth)) = frontier.pop() {
            if max_depth.is_some_and(|max| depth >= max) {
                continue;
            }
            let next_depth = depth + 1;
            for n in self.neighbors(node, Some(relation)) {
                if visited.insert(n) {
                    out.push(n);
                    frontier.insert(0, (n, next_depth));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::ConceptNode;
    use evidence_store::ScopeId;

    fn graph_with(scope: ScopeId, n: usize) -> (ConceptGraph, Vec<NodeId>) {
        let mut g = ConceptGraph::new();
        let mut ids = Vec::new();
        for i in 0..n {
            let node = ConceptNode::new_candidate(format!("n{i}"), format!("def {i}"), scope);
            ids.push(g.add_node(node).unwrap());
        }
        (g, ids)
    }

    #[test]
    fn add_and_get_node() {
        let scope = ScopeId::new_v4();
        let (g, ids) = graph_with(scope, 1);
        assert!(g.get_node(ids[0]).is_some());
        assert_eq!(g.node_count(), 1);
    }

    #[test]
    fn duplicate_node_is_rejected() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let n = ConceptNode::new_candidate("x", "y", scope);
        let id = n.id.0;
        g.add_node(n.clone()).unwrap();
        let err = g.add_node(n).unwrap_err();
        assert_eq!(err, GraphError::DuplicateNode(id));
    }

    #[test]
    fn dangling_edge_is_rejected() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let n = ConceptNode::new_candidate("x", "y", scope);
        g.add_node(n.clone()).unwrap();
        let phantom = NodeId::new_v4();
        let edge = ConceptEdge::new(n.id, phantom, RelationType::IsA, scope);
        let err = g.add_edge(edge).unwrap_err();
        assert!(matches!(err, GraphError::DanglingEdge(_)));
    }

    #[test]
    fn remove_node_drops_incident_edges() {
        let scope = ScopeId::new_v4();
        let (mut g, ids) = graph_with(scope, 3);
        g.add_edge(ConceptEdge::new(ids[0], ids[1], RelationType::IsA, scope))
            .unwrap();
        g.add_edge(ConceptEdge::new(
            ids[2],
            ids[1],
            RelationType::PartOf,
            scope,
        ))
        .unwrap();
        assert_eq!(g.edge_count(), 2);
        g.remove_node(ids[1]).unwrap();
        assert_eq!(g.edge_count(), 0);
    }
}
