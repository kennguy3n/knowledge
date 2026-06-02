//! Integration tests for the sparse typed concept graph.

use concept_graph::{ConceptEdge, ConceptGraph, ConceptNode, GraphError, NodeState, RelationType};
use evidence_store::ScopeId;

fn build(scope: ScopeId, count: usize) -> (ConceptGraph, Vec<concept_graph::NodeId>) {
    let mut g = ConceptGraph::new();
    let mut ids = Vec::new();
    for i in 0..count {
        let n = ConceptNode::new_candidate(format!("node-{i}"), format!("def-{i}"), scope);
        ids.push(g.add_node(n).unwrap());
    }
    (g, ids)
}

#[test]
fn add_node_and_lookup() {
    let scope = ScopeId::new_v4();
    let (g, ids) = build(scope, 1);
    let n = g.get_node(ids[0]).expect("node present");
    assert_eq!(n.label, "node-0");
}

#[test]
fn add_edge_indexed_in_both_directions() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 2);
    let edge = ConceptEdge::new(ids[0], ids[1], RelationType::IsA, scope);
    g.add_edge(edge).unwrap();
    assert_eq!(g.get_edges(ids[0]).len(), 1);
    assert_eq!(g.get_edges(ids[1]).len(), 1);
}

#[test]
fn neighbors_filters_by_relation_type() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 3);
    g.add_edge(ConceptEdge::new(ids[0], ids[1], RelationType::IsA, scope))
        .unwrap();
    g.add_edge(ConceptEdge::new(ids[0],
        ids[2],
        RelationType::PartOf,
        scope,
    ))
    .unwrap();
    let is_a = g.neighbors(ids[0], Some(RelationType::IsA));
    assert_eq!(is_a, vec![ids[1]]);
    let any = g.neighbors(ids[0], None);
    assert_eq!(any.len(), 2);
}

#[test]
fn remove_node_clears_dangling_edges() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 3);
    g.add_edge(ConceptEdge::new(ids[0], ids[1], RelationType::IsA, scope))
        .unwrap();
    g.add_edge(ConceptEdge::new(ids[2],
        ids[0],
        RelationType::PartOf,
        scope,
    ))
    .unwrap();
    assert_eq!(g.edge_count(), 2);
    let removed = g.remove_node(ids[0]).unwrap();
    assert_eq!(removed.id, ids[0]);
    assert!(g.get_node(ids[0]).is_none());
    assert_eq!(g.edge_count(), 0);
    let err = g.remove_node(ids[0]).unwrap_err();
    assert!(matches!(err, GraphError::NodeNotFound(_)));
}

#[test]
fn supersede_marks_predecessor_and_creates_supersedes_edge() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 2);
    g.supersede_node(ids[0], ids[1]).unwrap();
    let pred = g.get_node(ids[0]).unwrap();
    assert_eq!(pred.state, NodeState::Superseded);
    assert_eq!(pred.superseded_by, Some(ids[1]));
    let edges = g.get_edges(ids[0]);
    assert!(edges
        .iter()
        .any(|e| e.relation == RelationType::Supersedes && e.from == ids[0] && e.to == ids[1]));
}

#[test]
fn supersession_rejects_self_pointer() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 1);
    let err = g.supersede_node(ids[0], ids[0]).unwrap_err();
    assert!(matches!(err, GraphError::SelfSupersession(_)));
}

#[test]
fn mark_contradiction_does_not_partially_mutate_when_b_missing() {
    // Regression test for the PR #6 review finding: previously
    // `mark_contradiction(a, missing_b)` mutated node `a` *before*
    // looking up `b`, leaving `a` in a corrupted state
    // (`state = Contradicted`, dangling `superseded_by` pointer)
    // when the call returned `Err(NodeNotFound(b))`. Mirrors the
    // pattern used by `supersede_node`.
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 1); // only `a` exists
    let missing_b = concept_graph::NodeId::new_v4();

    let err = g.mark_contradiction(ids[0], missing_b).unwrap_err();
    assert!(matches!(err, GraphError::NodeNotFound(uuid) if uuid == missing_b.0),
        "expected NodeNotFound({}), got {err:?}",
        missing_b.0,
    );

    // Node `a` must be untouched: no state change, no dangling
    // `superseded_by` pointer, no edges.
    let a = g.get_node(ids[0]).expect("node a still present");
    assert_eq!(a.state, NodeState::Candidate, "node a state changed");
    assert_eq!(a.superseded_by, None, "node a has dangling pointer");
    assert!(g.get_edges(ids[0]).is_empty(), "edges leaked for node a");
}

#[test]
fn mark_contradiction_does_not_create_edges_when_a_missing() {
    // Sibling test: missing `a` should never touch `b` either.
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 1); // only one node, used as `b`
    let missing_a = concept_graph::NodeId::new_v4();

    let err = g.mark_contradiction(missing_a, ids[0]).unwrap_err();
    assert!(matches!(err, GraphError::NodeNotFound(uuid) if uuid == missing_a.0),
        "expected NodeNotFound({}), got {err:?}",
        missing_a.0,
    );

    let b = g.get_node(ids[0]).expect("node b still present");
    assert_eq!(b.state, NodeState::Candidate);
    assert_eq!(b.superseded_by, None);
    assert!(g.get_edges(ids[0]).is_empty());
}

#[test]
fn mark_contradiction_rejects_self_with_dedicated_variant() {
    // Regression test for the PR #6 review finding: previously
    // `mark_contradiction(n, n)` returned `GraphError::SelfSupersession`,
    // which is misleading for callers pattern-matching on the error
    // variant.
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 1);
    let err = g.mark_contradiction(ids[0], ids[0]).unwrap_err();
    assert!(matches!(err, GraphError::SelfContradiction(_)),
        "expected SelfContradiction, got {err:?}"
    );
    // And it must NOT be the supersession variant.
    assert!(!matches!(err, GraphError::SelfSupersession(_)),
        "must not reuse SelfSupersession for contradiction failures"
    );
}

#[test]
fn mark_contradiction_creates_reciprocal_edges() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 2);
    let (e1, e2) = g.mark_contradiction(ids[0], ids[1]).unwrap();
    assert_ne!(e1, e2);
    for id in &ids {
        let n = g.get_node(*id).unwrap();
        assert_eq!(n.state, NodeState::Contradicted);
    }
    let edges = g.get_edges(ids[0]);
    let contradicts: Vec<_> = edges
        .iter()
        .filter(|e| e.relation == RelationType::Contradicts)
        .collect();
    assert_eq!(contradicts.len(), 2);
}

#[test]
fn traverse_typed_walks_only_matching_relations() {
    let scope = ScopeId::new_v4();
    // a -is_a-> b -is_a-> c, b -part_of-> d
    let (mut g, ids) = build(scope, 4);
    g.add_edge(ConceptEdge::new(ids[0], ids[1], RelationType::IsA, scope))
        .unwrap();
    g.add_edge(ConceptEdge::new(ids[1], ids[2], RelationType::IsA, scope))
        .unwrap();
    g.add_edge(ConceptEdge::new(ids[1],
        ids[3],
        RelationType::PartOf,
        scope,
    ))
    .unwrap();
    let visited = g.traverse_typed(ids[0], RelationType::IsA, None);
    assert!(visited.contains(&ids[1]));
    assert!(visited.contains(&ids[2]));
    assert!(!visited.contains(&ids[3]));
}

#[test]
fn traverse_typed_respects_max_depth() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 3);
    g.add_edge(ConceptEdge::new(ids[0], ids[1], RelationType::IsA, scope))
        .unwrap();
    g.add_edge(ConceptEdge::new(ids[1], ids[2], RelationType::IsA, scope))
        .unwrap();
    let visited = g.traverse_typed(ids[0], RelationType::IsA, Some(1));
    assert!(visited.contains(&ids[1]));
    assert!(!visited.contains(&ids[2]));
}

#[test]
fn get_edges_returns_both_outgoing_and_incoming() {
    let scope = ScopeId::new_v4();
    let (mut g, ids) = build(scope, 3);
    g.add_edge(ConceptEdge::new(ids[0], ids[1], RelationType::IsA, scope))
        .unwrap();
    g.add_edge(ConceptEdge::new(ids[2],
        ids[1],
        RelationType::PartOf,
        scope,
    ))
    .unwrap();
    let edges = g.get_edges(ids[1]);
    assert_eq!(edges.len(), 2);
}
