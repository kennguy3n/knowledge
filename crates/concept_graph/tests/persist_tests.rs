//! Integration tests for [`PersistentConceptGraph`].

use tempfile::TempDir;

use concept_graph::{ConceptEdge, ConceptNode, NodeState, PersistentConceptGraph, RelationType};
use crypto::{MasterKey, MASTER_KEY_LEN};
use evidence_store::ScopeId;

fn fixture_master_key() -> MasterKey {
    let mut k = [0u8; MASTER_KEY_LEN];
    for (i, b) in k.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    k
}

#[test]
fn persist_and_reload_roundtrips_nodes_and_edges() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("concepts.db");
    let key = fixture_master_key();

    let scope = ScopeId::new_v4();
    let (node_id_a, node_id_b, edge_id) = {
        let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
        let a = ConceptNode::new_candidate("Atlas", "Project codename", scope);
        let b = ConceptNode::new_candidate("Q3 Launch", "Roadmap epoch", scope);
        let a_id = g.add_node(a).unwrap();
        let b_id = g.add_node(b).unwrap();
        let edge = ConceptEdge::new(a_id, b_id, RelationType::PartOf, scope);
        let e_id = g.add_edge(edge).unwrap();
        (a_id, b_id, e_id)
    };

    // Reopen and rehydrate.
    let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
    let (n, e) = g.load_scope(scope).unwrap();
    assert_eq!(n, 2);
    assert_eq!(e, 1);

    let inner = g.graph();
    assert!(inner.get_node(node_id_a).is_some());
    assert!(inner.get_node(node_id_b).is_some());
    let edges = inner.get_edges(node_id_a);
    assert!(edges.iter().any(|e| e.id == edge_id));
}

#[test]
fn load_scope_filters_by_scope() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("concepts.db");
    let key = fixture_master_key();

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    {
        let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
        for label in ["a1", "a2", "a3"] {
            g.add_node(ConceptNode::new_candidate(label, "in scope A", scope_a))
                .unwrap();
        }
        for label in ["b1", "b2"] {
            g.add_node(ConceptNode::new_candidate(label, "in scope B", scope_b))
                .unwrap();
        }
    }

    let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
    let (n_a, _) = g.load_scope(scope_a).unwrap();
    assert_eq!(n_a, 3);
    assert_eq!(g.graph().node_count(), 3);

    let (n_b, _) = g.load_scope(scope_b).unwrap();
    assert_eq!(n_b, 2);
    assert_eq!(g.graph().node_count(), 2);
}

#[test]
fn supersession_is_persisted() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("concepts.db");
    let key = fixture_master_key();

    let scope = ScopeId::new_v4();
    let (pred_id, succ_id) = {
        let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
        let pred = g
            .add_node(ConceptNode::new_candidate("v1", "old", scope))
            .unwrap();
        let succ = g
            .add_node(ConceptNode::new_candidate("v2", "new", scope))
            .unwrap();
        g.supersede_node(pred, succ).unwrap();
        (pred, succ)
    };

    let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
    g.load_scope(scope).unwrap();
    let pred = g.graph().get_node(pred_id).unwrap();
    assert_eq!(pred.state, NodeState::Superseded);
    assert_eq!(pred.superseded_by, Some(succ_id));

    let edges = g.graph().get_edges(pred_id);
    assert!(edges
        .iter()
        .any(|e| e.relation == RelationType::Supersedes && e.from == pred_id && e.to == succ_id));
}

#[test]
fn wrong_master_key_cannot_decrypt() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("concepts.db");
    let key1 = fixture_master_key();
    let scope = ScopeId::new_v4();
    {
        let mut g = PersistentConceptGraph::open(&path, &key1).unwrap();
        g.add_node(ConceptNode::new_candidate("secret", "hidden", scope))
            .unwrap();
    }

    // Different master key — SQLCipher should refuse to open.
    let mut bad_key = key1;
    bad_key[0] ^= 0xff;
    let err = PersistentConceptGraph::open(&path, &bad_key).err();
    assert!(err.is_some(), "wrong key must not unlock the database");
}

#[test]
fn persisted_counts_track_inserts() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("concepts.db");
    let key = fixture_master_key();

    let scope = ScopeId::new_v4();
    let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
    assert_eq!(g.persisted_node_count(scope).unwrap(), 0);
    let a = g
        .add_node(ConceptNode::new_candidate("a", "x", scope))
        .unwrap();
    let b = g
        .add_node(ConceptNode::new_candidate("b", "y", scope))
        .unwrap();
    assert_eq!(g.persisted_node_count(scope).unwrap(), 2);

    g.add_edge(ConceptEdge::new(a, b, RelationType::IsA, scope))
        .unwrap();
    assert_eq!(g.persisted_edge_count(scope).unwrap(), 1);
}

#[test]
fn raw_sqlite_view_is_encrypted() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("concepts.db");
    let key = fixture_master_key();

    let scope = ScopeId::new_v4();
    let plaintext_label = "Sensitive Project Phoenix";
    {
        let mut g = PersistentConceptGraph::open(&path, &key).unwrap();
        g.add_node(ConceptNode::new_candidate(
            plaintext_label,
            "should not appear in plaintext on disk",
            scope,
        ))
        .unwrap();
    }

    // Read the raw bytes off disk and confirm the plaintext label
    // does not appear anywhere — both SQLCipher page encryption and
    // the per-scope AEAD ciphertext should hide it.
    let bytes = std::fs::read(&path).unwrap();
    let needle = plaintext_label.as_bytes();
    assert!(
        !bytes.windows(needle.len()).any(|w| w == needle),
        "plaintext label leaked into the on-disk database"
    );
}
