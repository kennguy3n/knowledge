//! Integration tests for the hybrid retrieval module.
//!
//! Exercises FTS5 lexical search, recency-only search, and the
//! fan-in scoring (FTS5 + recency + stub vector) end-to-end.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, HybridRetriever, HybridWeights, ImportanceClass, ScopeId,
};
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xC7; 32];

fn fresh_store() -> (tempfile::TempDir, EvidenceStore) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");
    (dir, store)
}

#[test]
fn fts_search_finds_matching_text() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r1 = store
        .ingest(
            scope,
            b"Friday is the deadline for the migration",
            None,
            ImportanceClass::Important,
        )
        .unwrap();
    let _ = store
        .ingest(
            scope,
            b"unrelated content about lunch",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let hits = retriever.search_fts(scope, "deadline", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].evidence_id, r1.evidence_id);
    assert!(hits[0].fts_score > 0.0);
    assert_eq!(hits[0].recency_score, 0.0);
    assert_eq!(hits[0].vector_score, 0.0);
}

#[test]
fn recency_search_orders_by_created_at_desc() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r1 = store
        .ingest(scope, b"first message", None, ImportanceClass::Useful)
        .unwrap();
    // Sleep so the second row's created_at is strictly greater.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let r2 = store
        .ingest(scope, b"second message", None, ImportanceClass::Useful)
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let hits = retriever.search_recency(scope, 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].evidence_id, r2.evidence_id);
    assert_eq!(hits[1].evidence_id, r1.evidence_id);
    assert!(hits[0].recency_score >= hits[1].recency_score);
}

#[test]
fn hybrid_search_combines_fts_and_recency() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    // Two rows both match the query; the more recent row should win
    // overall after the fan-in.
    let _r_old = store
        .ingest(
            scope,
            b"deadline early in the project",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let r_new = store
        .ingest(
            scope,
            b"deadline very recent reminder",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let hits = retriever.search_hybrid(scope, "deadline", 5).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].evidence_id, r_new.evidence_id);
    // The hybrid score must be positive and incorporate both
    // components.
    assert!(hits[0].score > 0.0);
    assert!(hits[0].fts_score > 0.0);
    assert!(hits[0].recency_score > 0.0);
    assert_eq!(hits[0].vector_score, 0.0);
}

#[test]
fn hybrid_search_respects_custom_weights() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _r1 = store
        .ingest(
            scope,
            b"deadline early reminder",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let r_new = store
        .ingest(
            scope,
            b"deadline very recent",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store).with_weights(HybridWeights {
        fts: 0.0,
        recency: 1.0,
        vector: 0.0,
    });
    let hits = retriever.search_hybrid(scope, "deadline", 5).unwrap();
    assert_eq!(hits[0].evidence_id, r_new.evidence_id);
    assert!(hits[0].score >= hits.last().unwrap().score);
}

#[test]
fn empty_limit_returns_empty() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let retriever = HybridRetriever::new(&store);
    assert!(retriever
        .search_fts(scope, "anything", 0)
        .unwrap()
        .is_empty());
    assert!(retriever.search_recency(scope, 0).unwrap().is_empty());
    assert!(retriever
        .search_hybrid(scope, "anything", 0)
        .unwrap()
        .is_empty());
}
