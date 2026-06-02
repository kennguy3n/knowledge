//! End-to-end evidence-store → concept-graph promotion test.
//!
//! Exercises the substrate's promotion path from raw evidence rows
//! to typed concept-graph nodes:
//!
//! 1. Open both a SQLCipher-backed [`EvidenceStore`] and a
//!    SQLCipher-backed [`PersistentConceptGraph`] under the same
//!    master key.
//! 2. Ingest 6 evidence rows covering three "observations" worth of
//!    text (label + definition pairs).
//! 3. Promote those observations to candidate concept nodes via
//!    [`PersistentConceptGraph::add_node`], persist each
//!    `IsA`/`PartOf` edge through `add_edge`, then mark each node
//!    canonical.
//! 4. Verify the in-memory graph view matches the disk view by
//!    reopening the database and calling `load_scope`.
//! 5. Supersede a concept — assert the predecessor is marked
//!    `Superseded`, points at the successor, and has a `Supersedes`
//!    edge persisted alongside it.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::TempDir;

use concept_graph::{
    ConceptEdge, ConceptNode, NodeId, NodeState, PersistentConceptGraph, RelationType,
};

const MASTER_KEY: [u8; 32] = [0xA5; 32];

fn evidence_text(label: &str, definition: &str) -> Vec<u8> {
    // Pad up above the inline threshold so the body lives in the
    // body table — exercises the dedup + per-CEK path.
    let mut buf =
        format!("observation: {label} :: {definition} :: integration:promotion").into_bytes();
    buf.resize(DEFAULT_INLINE_THRESHOLD_BYTES * 4, b' ');
    buf
}

#[test]
fn evidence_promotion_and_supersession_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let evidence_path = dir.path().join("evidence.db");
    let graph_path = dir.path().join("concepts.db");

    let scope = ScopeId::new_v4();

    // Three observations × two evidence rows each = six evidence rows.
    let observations = [
        ("Project Atlas", "Q3 launch codename"),
        ("Migration Plan", "M2 → M3 cutover sequence"),
        ("Channel Recap", "weekly summary epoch"),
    ];

    // 1. Open both stores and ingest evidence rows.
    let mut store =
        EvidenceStore::open(&evidence_path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open evidence store");

    let mut evidence_ids = Vec::new();
    for (label, def) in observations {
        for variant in ["initial", "follow-up"] {
            let body = evidence_text(label, &format!("{def} ({variant})"));
            let res = store
                .ingest(scope,
                    &body,
                    Some("integration:promotion"),
                    ImportanceClass::Important,
                )
                .expect("ingest evidence");
            evidence_ids.push(res.evidence_id);
        }
    }
    assert_eq!(evidence_ids.len(), 6, "six evidence rows total");

    // 2. Promote each observation to a candidate concept node, then
    //    add IsA / PartOf edges between them and mark canonical.
    let mut graph =
        PersistentConceptGraph::open(&graph_path, &MASTER_KEY).expect("open concept graph");

    let mut concept_ids: Vec<NodeId> = Vec::new();
    for (label, def) in observations {
        let node = ConceptNode::new_candidate(label, def, scope);
        let id = graph.add_node(node).expect("add_node + persist");
        concept_ids.push(id);
    }
    // Channel Recap is a PartOf Project Atlas; Migration Plan IsA
    // Project Atlas. Two edges total.
    let edge_partof = graph
        .add_edge(ConceptEdge::new(concept_ids[2],
            concept_ids[0],
            RelationType::PartOf,
            scope,
        ))
        .expect("add PartOf edge");
    let edge_isa = graph
        .add_edge(ConceptEdge::new(concept_ids[1],
            concept_ids[0],
            RelationType::IsA,
            scope,
        ))
        .expect("add IsA edge");

    // Promote every node from Candidate → Canonical and persist.
    for &id in &concept_ids {
        graph
            .graph_mut()
            .get_node_mut(id)
            .expect("just-inserted node must be lookup-able")
            .mark_canonical();
        graph.save_node(id).expect("save_node");
    }

    // 3. In-memory view sees three canonical nodes + two typed edges.
    {
        let view = graph.graph();
        assert_eq!(view.node_count(), 3, "three candidate-then-canonical nodes");
        assert_eq!(view.edge_count(), 2, "PartOf + IsA edges");
        for &id in &concept_ids {
            let n = view.get_node(id).expect("node present");
            assert_eq!(n.state, NodeState::Canonical);
        }
        let partof_edges = view.get_edges(concept_ids[2]);
        assert!(partof_edges.iter().any(|e| e.id == edge_partof
                && e.relation == RelationType::PartOf
                && e.to == concept_ids[0]),
            "PartOf edge wired ChannelRecap → Atlas"
        );
        let isa_edges = view.get_edges(concept_ids[1]);
        assert!(isa_edges.iter().any(|e| e.id == edge_isa
                && e.relation == RelationType::IsA
                && e.to == concept_ids[0]),
            "IsA edge wired Migration → Atlas"
        );
    }

    // 4. Reopen + rehydrate from disk — disk view must match memory.
    drop(graph);
    let mut graph_reopened =
        PersistentConceptGraph::open(&graph_path, &MASTER_KEY).expect("reopen graph");
    let (loaded_nodes, loaded_edges) = graph_reopened.load_scope(scope).expect("load_scope");
    assert_eq!(loaded_nodes, 3, "3 nodes hydrated from SQLCipher");
    assert_eq!(loaded_edges, 2, "2 edges hydrated from SQLCipher");
    for &id in &concept_ids {
        let n = graph_reopened
            .graph()
            .get_node(id)
            .expect("rehydrated node present");
        assert_eq!(n.state, NodeState::Canonical);
    }

    // 5. Supersede the Migration Plan with a fresh node, then
    //    rehydrate again and verify the supersession edge survived.
    let new_migration = ConceptNode::new_candidate("Migration Plan v2", "M3 → M4 cutover", scope);
    let new_id = graph_reopened
        .add_node(new_migration)
        .expect("add successor");
    let supersedes_edge_id = graph_reopened
        .supersede_node(concept_ids[1], new_id)
        .expect("supersede");

    {
        let view = graph_reopened.graph();
        let pred = view.get_node(concept_ids[1]).expect("predecessor present");
        assert_eq!(pred.state, NodeState::Superseded);
        assert_eq!(pred.superseded_by, Some(new_id));
        let edges = view.get_edges(concept_ids[1]);
        assert!(edges.iter().any(|e| e.id == supersedes_edge_id
                && e.relation == RelationType::Supersedes
                && e.to == new_id),
            "supersession edge wired predecessor → successor"
        );
    }

    drop(graph_reopened);
    let mut graph_final =
        PersistentConceptGraph::open(&graph_path, &MASTER_KEY).expect("reopen for final check");
    let (final_nodes, final_edges) = graph_final.load_scope(scope).expect("load_scope final");
    assert_eq!(final_nodes, 4, "predecessor + 2 originals + successor");
    assert_eq!(final_edges, 3, "IsA + PartOf + Supersedes");
    let pred = graph_final
        .graph()
        .get_node(concept_ids[1])
        .expect("predecessor still present");
    assert_eq!(pred.state, NodeState::Superseded);
    assert_eq!(pred.superseded_by, Some(new_id));
}
