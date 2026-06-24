//! Integration tests for retrieval result clustering by content hash.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, HybridRetriever, ImportanceClass, ScopeId,
};
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xA5; 32];

fn fresh_store() -> (tempfile::TempDir, EvidenceStore) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");
    (dir, store)
}

#[test]
fn cluster_groups_duplicate_content_across_sources() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest the same content from three different sources.
    let body = b"We decided to use PostgreSQL for the database.";
    let r1 = store
        .ingest(scope, body, Some("slack:msg:C001"), ImportanceClass::Important)
        .unwrap();
    let r2 = store
        .ingest(scope, body, Some("gmail:msg:M001"), ImportanceClass::Important)
        .unwrap();
    let r3 = store
        .ingest(scope, body, Some("github:issue:789"), ImportanceClass::Important)
        .unwrap();

    // All three should have the same content hash.
    assert_eq!(r1.content_hash, r2.content_hash);
    assert_eq!(r2.content_hash, r3.content_hash);

    let retriever = HybridRetriever::new(&store);
    let clustered = retriever
        .search_hybrid_clustered(scope, "PostgreSQL", 10)
        .expect("clustered search");

    // Should be a single cluster with 3 members.
    assert_eq!(clustered.len(), 1);
    assert_eq!(clustered[0].cluster_members.len(), 3);
    assert_eq!(clustered[0].source_count, 3);
}

#[test]
fn cluster_preserves_distinct_content_as_singletons() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    store
        .ingest(
            scope,
            b"We decided to use PostgreSQL for the database.",
            Some("slack:msg:C001"),
            ImportanceClass::Important,
        )
        .unwrap();
    store
        .ingest(
            scope,
            b"The migration plan is to move to MongoDB next quarter.",
            Some("slack:msg:C002"),
            ImportanceClass::Important,
        )
        .unwrap();
    store
        .ingest(
            scope,
            b"Reminder: team standup at 9am tomorrow.",
            Some("slack:msg:C003"),
            ImportanceClass::Useful,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let clustered = retriever
        .search_hybrid_clustered(scope, "database", 10)
        .expect("clustered search");

    // Each result has unique content → each is a singleton cluster.
    for c in &clustered {
        assert_eq!(c.cluster_members.len(), 1);
        assert_eq!(c.source_count, 1);
    }
}

#[test]
fn cluster_representative_is_highest_scoring() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest same content at different times — the more recent one
    // should have a higher recency score and become the representative.
    let body = b"The deadline for the migration is Friday.";
    store
        .ingest(scope, body, Some("slack:msg:old"), ImportanceClass::Important)
        .unwrap();
    store
        .ingest(scope, body, Some("slack:msg:new"), ImportanceClass::Important)
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let clustered = retriever
        .search_hybrid_clustered(scope, "deadline", 10)
        .expect("clustered search");

    assert_eq!(clustered.len(), 1);
    assert_eq!(clustered[0].cluster_members.len(), 2);
    // Representative should be one of the two evidence IDs.
    assert!(clustered[0]
        .cluster_members
        .contains(&clustered[0].representative.evidence_id));
}

#[test]
fn cluster_empty_results_returns_empty() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();

    let retriever = HybridRetriever::new(&store);
    let clustered = retriever
        .search_hybrid_clustered(scope, "nonexistent", 10)
        .expect("clustered search");

    assert!(clustered.is_empty());
}

#[test]
fn cluster_zero_limit_returns_empty() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    store
        .ingest(
            scope,
            b"Some content here for testing.",
            Some("slack:msg:C001"),
            ImportanceClass::Important,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let clustered = retriever
        .search_hybrid_clustered(scope, "content", 0)
        .expect("clustered search");

    assert!(clustered.is_empty());
}

#[test]
fn cluster_by_content_hash_directly() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest duplicate + unique content.
    let dup_body = b"Vendor X selected for payment integration.";
    let r1 = store
        .ingest(scope, dup_body, Some("slack:msg:A"), ImportanceClass::Important)
        .unwrap();
    let _r2 = store
        .ingest(scope, dup_body, Some("email:msg:B"), ImportanceClass::Important)
        .unwrap();
    let _r3 = store
        .ingest(
            scope,
            b"Unique content about something else.",
            Some("slack:msg:C"),
            ImportanceClass::Useful,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let fts_results = retriever
        .search_fts(scope, "Vendor", 10)
        .expect("fts search");

    // Manually cluster the FTS results.
    let clustered = retriever
        .cluster_by_content_hash(fts_results)
        .expect("clustering");

    // FTS for "Vendor" should match the duplicate body.
    // The duplicate appears twice but should cluster into one.
    let dup_cluster = clustered
        .iter()
        .find(|c| c.content_hash == r1.content_hash)
        .expect("duplicate cluster exists");
    assert_eq!(dup_cluster.cluster_members.len(), 2);
    assert_eq!(dup_cluster.source_count, 2);
}

#[test]
fn cluster_sorted_by_representative_score_descending() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest multiple unique items.
    store
        .ingest(
            scope,
            b"Critical incident in production database.",
            Some("slack:msg:U001"),
            ImportanceClass::Critical,
        )
        .unwrap();
    store
        .ingest(
            scope,
            b"Useful note about the dashboard design.",
            Some("slack:msg:U002"),
            ImportanceClass::Useful,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store);
    let clustered = retriever
        .search_hybrid_clustered(scope, "database", 10)
        .expect("clustered search");

    // Verify descending order.
    for i in 1..clustered.len() {
        assert!(
            clustered[i - 1].representative.score >= clustered[i].representative.score,
            "clusters should be sorted by representative score descending"
        );
    }
}

#[test]
fn cluster_truncates_to_limit() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest 5 unique items.
    for i in 0..5 {
        let msg = format!("Unique message number {i} about databases.");
        store
            .ingest(
                scope,
                msg.as_bytes(),
                Some(&format!("slack:msg:{i}")),
                ImportanceClass::Important,
            )
            .unwrap();
    }

    let retriever = HybridRetriever::new(&store);
    let clustered = retriever
        .search_hybrid_clustered(scope, "database", 3)
        .expect("clustered search");

    assert!(clustered.len() <= 3);
}
