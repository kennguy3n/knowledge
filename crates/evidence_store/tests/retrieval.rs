//! Integration tests for the hybrid retrieval module.
//!
//! Exercises FTS5 lexical search, recency-only search, and the
//! fan-in scoring (FTS5 + recency + stub vector) end-to-end.

use evidence_store::embeddings::{
    EmbeddingError, EmbeddingModel, EmbeddingProbe, Result as EmbeddingResult,
};
use evidence_store::{
    EvidenceError, EvidenceStore, EvidenceStoreConfig, HybridRetriever, HybridWeights,
    ImportanceClass, RetrievalResult, ScopeId,
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

/// Embedding model that always fails. Used to assert
/// `rerank_with_embeddings` surfaces a typed
/// `EvidenceError::Embedding` (no `Box::leak`).
struct FailingEmbeddingModel;

impl EmbeddingModel for FailingEmbeddingModel {
    fn embed(&self, _text: &str) -> EmbeddingResult<Vec<f32>> {
        Err(EmbeddingError::InferenceFailure("boom".into()))
    }
    fn dimension(&self) -> usize {
        4
    }
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

/// Deterministic embedding model — vector is `[len, 0, 0, 0]` so two
/// equal-length texts cosine to 1.0 and otherwise non-zero.
struct LengthEmbeddingModel;

impl EmbeddingModel for LengthEmbeddingModel {
    fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbeddingError::EmptyInput);
        }
        Ok(vec![text.len() as f32, 0.0, 0.0, 0.0])
    }
    fn dimension(&self) -> usize {
        4
    }
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

/// Bug 1 regression: a failing embedding model in
/// `rerank_with_embeddings` must surface as `EvidenceError::Embedding`,
/// not as a leaked `&'static str`.
#[test]
fn rerank_with_embeddings_failure_returns_embedding_error_variant() {
    let (_dir, store) = fresh_store();
    let retriever = HybridRetriever::new(&store).with_embedding_model(FailingEmbeddingModel);

    let dummy_id = evidence_store::EvidenceId(uuid::Uuid::nil());
    let candidates = vec![RetrievalResult {
        evidence_id: dummy_id,
        score: 0.0,
        fts_score: 0.0,
        recency_score: 0.0,
        vector_score: 0.0,
    }];
    let bodies = vec![(dummy_id, String::from("placeholder body"))];
    let err = retriever
        .rerank_with_embeddings("anything", candidates, &bodies)
        .unwrap_err();
    assert!(
        matches!(err, EvidenceError::Embedding(_)),
        "expected EvidenceError::Embedding, got {err:?}",
    );
}

/// Bug 1 sibling: when no embedding model is wired up,
/// `rerank_with_embeddings` is a no-op (returns the input).
#[test]
fn rerank_with_embeddings_is_noop_when_no_model_attached() {
    let (_dir, store) = fresh_store();
    let retriever = HybridRetriever::new(&store);
    let dummy_id = evidence_store::EvidenceId(uuid::Uuid::nil());
    let input = vec![RetrievalResult {
        evidence_id: dummy_id,
        score: 0.42,
        fts_score: 0.42,
        recency_score: 0.0,
        vector_score: 0.0,
    }];
    let out = retriever
        .rerank_with_embeddings("query", input.clone(), &[])
        .unwrap();
    assert_eq!(out, input);
}

/// Bug 4 doc test: `search_hybrid` must not consult the embedding
/// model — only `rerank_with_embeddings` does. The contract is
/// documented on `with_embedding_model`; this test pins the
/// implementation to that contract.
#[test]
fn search_hybrid_keeps_vector_score_zero_even_with_embedding_model() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _r = store
        .ingest(
            scope,
            b"deadline very recent reminder",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let retriever = HybridRetriever::new(&store).with_embedding_model(LengthEmbeddingModel);
    assert!(retriever.has_embedding_model());
    let hits = retriever.search_hybrid(scope, "deadline", 5).unwrap();
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(hit.vector_score, 0.0);
    }
}

/// Bug 4 round-trip: `rerank_with_embeddings` *does* populate
/// `vector_score` and rewrites the final `score` from the configured
/// weights.
#[test]
fn rerank_with_embeddings_populates_vector_score_for_matching_body() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            b"deadline very recent reminder",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let retriever = HybridRetriever::new(&store)
        .with_weights(HybridWeights {
            fts: 0.0,
            recency: 0.0,
            vector: 1.0,
        })
        .with_embedding_model(LengthEmbeddingModel);
    let candidates = vec![RetrievalResult {
        evidence_id: r.evidence_id,
        score: 0.0,
        fts_score: 0.0,
        recency_score: 0.0,
        vector_score: 0.0,
    }];
    let bodies = vec![(r.evidence_id, String::from("deadline very recent reminder"))];
    let out = retriever
        .rerank_with_embeddings("deadline very recent reminder", candidates, &bodies)
        .unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].vector_score > 0.0, "{out:?}");
    assert!(out[0].score > 0.0, "{out:?}");
}
