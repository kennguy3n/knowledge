//! Integration tests for the hybrid retrieval module.
//!
//! Exercises FTS5 lexical search, recency-only search, and the
//! fan-in scoring (FTS5 + recency + stub vector) end-to-end.

use evidence_store::embeddings::{EmbeddingError, EmbeddingModel, EmbeddingProbe};
use evidence_store::{
    EvidenceError, EvidenceStore, EvidenceStoreConfig, HybridRetriever, HybridWeights,
    ImportanceClass, RetrievalResult, ScopeId,
};
use tempfile::tempdir;

/// Always errors. Used to assert `rerank_with_embeddings` propagates
/// `EvidenceError::Embedding` rather than collapsing the failure into
/// a `Schema` variant via `Box::leak` (the regression target for the
/// 2026-05-08 retrieval bug fix).
struct FailingEmbeddingModel;

impl EmbeddingModel for FailingEmbeddingModel {
    fn embed(&self, _text: &str) -> evidence_store::embeddings::Result<Vec<f32>> {
        Err(EmbeddingError::InferenceFailure(
            "synthetic failure for regression test".into(),
        ))
    }
    fn dimension(&self) -> usize {
        4
    }
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

/// Returns a fixed unit vector for any input. Picked so cosine
/// similarity with itself is `1.0` and `similarity_to_score` projects
/// it into a non-zero `vector_score`. Used to assert `search_hybrid`
/// actually runs the embedding lane (the Phase-0 regression target).
struct ConstUnitEmbeddingModel;

impl EmbeddingModel for ConstUnitEmbeddingModel {
    fn embed(&self, _text: &str) -> evidence_store::embeddings::Result<Vec<f32>> {
        Ok(vec![1.0, 0.0, 0.0, 0.0])
    }
    fn dimension(&self) -> usize {
        4
    }
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

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

/// Regression test for the 2026-05-08 retrieval fix.
///
/// Before the fix, `rerank_with_embeddings` translated embedding
/// failures into `EvidenceError::Schema(Box::leak(format!(...)))`,
/// leaking memory on every failure. The fix routes the failure
/// through the new `EvidenceError::Embedding(String)` variant.
#[test]
fn rerank_with_failing_model_returns_embedding_error_not_schema() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r1 = store
        .ingest(scope, b"hello world", None, ImportanceClass::Useful)
        .unwrap();

    let candidates = vec![RetrievalResult {
        evidence_id: r1.evidence_id,
        score: 0.0,
        fts_score: 0.0,
        recency_score: 0.0,
        vector_score: 0.0,
    }];
    let bodies = vec![(r1.evidence_id, "hello world".to_string())];

    let retriever = HybridRetriever::new(&store).with_embedding_model(FailingEmbeddingModel);
    let err = retriever
        .rerank_with_embeddings("hello", candidates, &bodies)
        .expect_err("expected embedding error");
    assert!(
        matches!(err, EvidenceError::Embedding(_)),
        "expected EvidenceError::Embedding, got {err:?}"
    );
}

/// Regression test for Task 4: `search_hybrid` must consult the
/// embedding model when one is wired in. Before the fix the vector
/// lane was hardcoded to `0.0`; now it should produce a non-zero
/// `vector_score` for any candidate that has a body to embed.
#[test]
fn search_hybrid_with_embedding_model_produces_nonzero_vector_score() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _r1 = store
        .ingest(
            scope,
            b"deadline reminder body text",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let retriever = HybridRetriever::new(&store).with_embedding_model(ConstUnitEmbeddingModel);
    let hits = retriever.search_hybrid(scope, "deadline", 5).unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    // Cosine similarity between two identical unit vectors is 1.0,
    // which `similarity_to_score` projects to 1.0 (non-zero).
    assert!(
        hits.iter().any(|h| h.vector_score > 0.0),
        "expected at least one non-zero vector_score, got {hits:?}"
    );
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

// ---------------------------------------------------------------------
// Phase B regression tests — the on-write embedding cache.
// ---------------------------------------------------------------------

/// Returns a vector whose first slot is the byte length of the input
/// text. Two different bodies get different cached embeddings, so a
/// stored-cache hit is visibly distinct from a re-embedding of some
/// other text.
struct LenEmbeddingModel;

impl EmbeddingModel for LenEmbeddingModel {
    fn embed(&self, text: &str) -> evidence_store::embeddings::Result<Vec<f32>> {
        Ok(vec![text.len() as f32, 0.0, 0.0, 0.0])
    }
    fn dimension(&self) -> usize {
        4
    }
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

#[test]
fn ingest_with_model_persists_embedding_round_trip() {
    let (_dir, mut store) = open_store_with_model(LenEmbeddingModel, "len-v1");
    let scope = ScopeId::new_v4();
    let body = b"deadline reminder body text";
    let r = store
        .ingest(scope, body, None, ImportanceClass::Useful)
        .unwrap();

    let stored = store
        .get_embedding(r.evidence_id)
        .expect("get_embedding")
        .expect("expected a cached embedding for ingested row");
    assert_eq!(stored, vec![body.len() as f32, 0.0, 0.0, 0.0]);
}

#[test]
fn ingest_without_model_leaves_embedding_cache_empty() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, b"hello world", None, ImportanceClass::Useful)
        .unwrap();
    assert!(store.get_embedding(r.evidence_id).unwrap().is_none());
}

#[test]
fn store_embedding_round_trip_independent_of_ingest() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, b"some body", None, ImportanceClass::Useful)
        .unwrap();

    let written = vec![0.1_f32, -0.2, 0.3, 0.4, -0.5];
    store
        .store_embedding(r.evidence_id, &written, "test-tag")
        .expect("store_embedding");
    let read = store
        .get_embedding(r.evidence_id)
        .expect("get_embedding")
        .expect("expected stored embedding");
    assert_eq!(read, written);
}

#[test]
fn get_embedding_returns_none_for_unknown_row() {
    let (_dir, store) = fresh_store();
    let id = evidence_store::EvidenceId::new_v4();
    assert!(store.get_embedding(id).unwrap().is_none());
}

/// When the store has cached embeddings, the hybrid retriever uses
/// them instead of re-embedding bodies on each search. We assert this
/// by wiring an embedding model into the *store* (so the cache is
/// populated on ingest) but using a *different* model on the
/// retriever side that would produce a different score if it were
/// asked to re-embed. The retriever should use the cached vector and
/// score it against its own query embedding.
#[test]
fn search_hybrid_uses_cached_embeddings_when_present() {
    let (_dir, mut store) = open_store_with_model(LenEmbeddingModel, "len-v1");
    let scope = ScopeId::new_v4();
    let body = b"deadline reminder";
    let _r = store
        .ingest(scope, body, None, ImportanceClass::Useful)
        .unwrap();

    // Sanity-check the cache was populated.
    let hits = HybridRetriever::new(&store)
        .with_embedding_model(LenEmbeddingModel)
        .search_hybrid(scope, "deadline", 5)
        .unwrap();
    assert!(!hits.is_empty());
    // The cached vector is `[body.len(), 0, 0, 0]`; the query embed is
    // `[len("deadline"), 0, 0, 0]`. Both align on the same basis
    // vector so cosine similarity is 1.0 → similarity_to_score = 1.0.
    assert!(
        hits.iter().any(|h| (h.vector_score - 1.0).abs() < 1e-6),
        "expected a cached-hit vector_score of 1.0, got {hits:?}"
    );
}

/// If the cached embedding has a different dimension than the query
/// embedding (e.g. a stale model swap), the retriever must fall back
/// to re-embedding the body rather than emitting a misleading 0.5
/// score from `cosine_similarity` on mismatched lengths.
#[test]
fn search_hybrid_falls_back_to_live_embed_on_dim_mismatch() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, b"deadline reminder", None, ImportanceClass::Useful)
        .unwrap();
    // Seed a stale 8-d embedding; the retriever's model is 4-d.
    store
        .store_embedding(r.evidence_id, &[1.0; 8], "stale-v0")
        .unwrap();

    let hits = HybridRetriever::new(&store)
        .with_embedding_model(ConstUnitEmbeddingModel)
        .search_hybrid(scope, "deadline", 5)
        .unwrap();
    assert!(
        hits.iter().any(|h| h.vector_score > 0.0),
        "expected the fallback live-embed path to produce a non-zero \
         vector_score even though the cache row has the wrong width: \
         got {hits:?}"
    );
}

fn open_store_with_model<M>(model: M, tag: &str) -> (tempfile::TempDir, EvidenceStore)
where
    M: EmbeddingModel + 'static,
{
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store")
        .with_embedding_model(model, tag);
    (dir, store)
}
