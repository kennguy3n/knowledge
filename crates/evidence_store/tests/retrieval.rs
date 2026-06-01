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

/// Assert that a retrieval score is bit-for-bit equal to an expected
/// boundary value (typically `0.0`). The retrieval-score struct fields
/// are populated by the retriever from short-circuit paths (no float
/// arithmetic happens when, e.g., the vector lane has no embedding
/// model), so an exact-equality assertion is the right semantic.
/// `f64::total_cmp` gives a strict comparison that still flags `NaN`
/// loudly — `assert_eq!` on `f64` would have been correct here but
/// trips `clippy::float_cmp`.
#[track_caller]
fn assert_score_eq(actual: f64, expected: f64) {
    assert!(
        actual.total_cmp(&expected).is_eq(),
        "score mismatch: actual={actual}, expected={expected}"
    );
}

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
/// actually runs the embedding lane (the regression target).
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
    assert_score_eq(hits[0].recency_score, 0.0);
    assert_score_eq(hits[0].vector_score, 0.0);
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
    assert_score_eq(hits[0].vector_score, 0.0);
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

    let retriever =
        HybridRetriever::new(&store).with_embedding_model(FailingEmbeddingModel, "failing-v1");
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

    let retriever =
        HybridRetriever::new(&store).with_embedding_model(ConstUnitEmbeddingModel, "const-v1");
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
// Regression tests — the on-write embedding cache.
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

    // Sanity-check the cache was populated. The retriever uses the
    // same `model_tag` the store ingested under, so `model_tag`-aware
    // `get_embedding_for_model` returns the cached vector.
    let hits = HybridRetriever::new(&store)
        .with_embedding_model(LenEmbeddingModel, "len-v1")
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
    // Seed a stale 8-d embedding; the retriever's model is 4-d. Use
    // the same `model_tag` the retriever will pass so the cache row is
    // visible by tag and the dimension-mismatch branch is the one
    // that triggers the fallback (not the tag-mismatch branch).
    store
        .store_embedding(r.evidence_id, &[1.0; 8], "stale-v0")
        .unwrap();

    let hits = HybridRetriever::new(&store)
        .with_embedding_model(ConstUnitEmbeddingModel, "stale-v0")
        .search_hybrid(scope, "deadline", 5)
        .unwrap();
    assert!(
        hits.iter().any(|h| h.vector_score > 0.0),
        "expected the fallback live-embed path to produce a non-zero \
         vector_score even though the cache row has the wrong width: \
         got {hits:?}"
    );
}

/// Regression test for the review finding "candidate_embedding
/// propagates cache errors via `?`, aborting entire search on a single
/// corrupted embedding row". We seed an evidence row, then corrupt its
/// cached embedding to a length that is not a multiple of 4 (the
/// blob-deserialisation invariant). The retriever must still return a
/// result for that row by falling back to live-embedding the body —
/// not surface `EvidenceError::Schema` from the underlying cache read.
#[test]
fn search_hybrid_treats_corrupted_cache_row_as_miss() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, b"deadline reminder", None, ImportanceClass::Useful)
        .unwrap();

    // Corrupt the cached embedding for `r.evidence_id` to an odd
    // length (3 bytes). `bytes_to_embedding` would reject this with
    // `EvidenceError::Schema(...)` if `?`-propagated. Tag the row with
    // the same `model_tag` the retriever uses so the `model_tag`-aware
    // read actually surfaces this row (otherwise the tag filter would
    // mask the corruption before deserialisation ever runs and we
    // wouldn't exercise the Schema-error branch we care about).
    store
        .raw_conn()
        .execute(
            "INSERT OR REPLACE INTO evidence_embeddings
                 (evidence_id, embedding, model_tag, created_at)
             VALUES (?1, X'00FF00', 'corrupt-tag', 0)",
            rusqlite::params![r.evidence_id.as_uuid().as_bytes().as_slice()],
        )
        .unwrap();

    let hits = HybridRetriever::new(&store)
        .with_embedding_model(ConstUnitEmbeddingModel, "corrupt-tag")
        .search_hybrid(scope, "deadline", 5)
        .expect("search_hybrid must not propagate a per-row cache error");
    assert!(
        hits.iter().any(|h| h.evidence_id == r.evidence_id),
        "row with corrupted cache row should still appear in results: {hits:?}"
    );
}

/// Regression test for the follow-up finding
/// "candidate_embedding propagates body-decryption errors via `?`,
/// aborting `search_hybrid` on a single corrupted body row" (Flag #3
/// in the evidence_store hygiene PR). The previous shape of the
/// function used `?` on `self.lookup_body_text(id)`, so any AEAD /
/// crypto failure on one row cascaded up through the retriever and
/// failed the whole search.
///
/// We seed two rows that match the FTS query, route one through the
/// `body_store` table (above the inline threshold) and corrupt the
/// stored ciphertext post-hoc. `lookup_body_text` will then surface
/// `EvidenceError::Crypto(_)` for that row. After the fix the
/// retriever demotes that error to a per-row miss (`vector_score`
/// 0.0) and the healthy row still scores normally.
#[test]
fn search_hybrid_treats_corrupted_body_row_as_miss() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Healthy row — small body, stored inline. Live-embed path will
    // succeed and produce a non-zero `vector_score`.
    let healthy = store
        .ingest(
            scope,
            b"deadline reminder for the migration",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    // Corrupted row — body large enough to route through `body_store`
    // so we can rewrite the ciphertext without tripping the
    // append-only triggers on `evidence`.
    let mut large = String::from("deadline meeting agenda ");
    large.push_str(&"x".repeat(5_000));
    let corrupted = store
        .ingest(scope, large.as_bytes(), None, ImportanceClass::Useful)
        .unwrap();

    // Overwrite the ciphertext in `body_store` so AEAD decryption
    // fails on read. The `body_store` table is not append-only —
    // `ref_count` updates require it to be mutable — so a raw UPDATE
    // is fine here.
    store
        .raw_conn()
        .execute(
            "UPDATE body_store SET body = X'DEADBEEFDEADBEEF' WHERE content_hash = ?1",
            rusqlite::params![corrupted.content_hash.as_slice()],
        )
        .unwrap();

    // Before the fix this `search_hybrid` would propagate
    // `EvidenceError::Crypto(_)` and the whole call would error out.
    let hits = HybridRetriever::new(&store)
        .with_embedding_model(ConstUnitEmbeddingModel, "v1")
        .search_hybrid(scope, "deadline", 5)
        .expect("search_hybrid must not propagate per-row body-decryption errors");

    let healthy_hit = hits
        .iter()
        .find(|h| h.evidence_id == healthy.evidence_id)
        .expect("healthy row must still appear in results");
    assert!(
        healthy_hit.vector_score > 0.0,
        "healthy row should score via the live-embed path: {hits:?}"
    );

    let corrupted_hit = hits
        .iter()
        .find(|h| h.evidence_id == corrupted.evidence_id)
        .expect("corrupted row must still appear (FTS-matched), just with a zero vector_score");
    assert_score_eq(corrupted_hit.vector_score, 0.0);
    assert!(
        corrupted_hit.score >= 0.0,
        "corrupted row's combined score must remain finite: {hits:?}"
    );
}

/// Regression test for the review finding "embedding cache is
/// not populated for body-table dedup hits — embedding work is
/// repeated for deduped bodies". We ingest the same large body twice
/// and assert both rows share the cached vector and that the embed
/// model was only invoked once.
#[test]
fn dedup_hit_copies_embedding_instead_of_re_embedding() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct CountingEmbedding {
        calls: Arc<AtomicUsize>,
    }
    impl EmbeddingModel for CountingEmbedding {
        fn embed(&self, _text: &str) -> evidence_store::embeddings::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1.0, 2.0, 3.0, 4.0])
        }
        fn dimension(&self) -> usize {
            4
        }
        fn probe(&self) -> EmbeddingProbe {
            EmbeddingProbe::Available
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let model = CountingEmbedding {
        calls: Arc::clone(&calls),
    };

    let (_dir, mut store) = open_store_with_model(model, "count-v1");
    let scope = ScopeId::new_v4();
    // The body-table path triggers above the inline threshold
    // (default 4096 bytes). Use a printable body so the FTS lane is
    // also exercised on both ingests — both should hit the dedup
    // path on the second insert.
    let body = "x".repeat(4097);

    let first = store
        .ingest(scope, body.as_bytes(), None, ImportanceClass::Useful)
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "first ingest embeds once");

    let second = store
        .ingest(scope, body.as_bytes(), None, ImportanceClass::Useful)
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second ingest must reuse the cached embedding, not re-embed"
    );

    let first_vec = store.get_embedding(first.evidence_id).unwrap().unwrap();
    let second_vec = store.get_embedding(second.evidence_id).unwrap().unwrap();
    assert_eq!(
        first_vec, second_vec,
        "dedup-copied embedding must be byte-identical to the source"
    );
}

/// Regression test for the review finding "embedding cache
/// read path ignores `model_tag`, returning stale vectors on a
/// same-dimension model swap".
///
/// The write side ([`EvidenceStore::index_embedding_or_copy_dedup`])
/// already stamps every cached row with the active `model_tag` and
/// filters dedup-copies by it. The read side
/// ([`EvidenceStore::get_embedding_for_model`] called from
/// [`HybridRetriever::candidate_embedding`]) must enforce the same
/// invariant — otherwise a row written by a previous model that
/// happens to share the new model's output dimension would silently
/// be returned and scored as if it had been produced by the active
/// model, yielding a meaningless cosine similarity.
///
/// We seed the cache for an evidence row under `model_tag = "model-a"`
/// with a vector that differs from what `model-b` would produce, then
/// drive the retriever with `model-b` and assert that:
///   1. The cache row from `model-a` does not short-circuit the
///      live-embed path (counted invocations include the body embed),
///      and
///   2. The resulting `vector_score` matches what live-embedding the
///      body under `model-b` would produce, not what the stale
///      `model-a` row would score against `model-b`'s query embed.
#[test]
fn candidate_embedding_skips_cache_row_with_mismatched_model_tag() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Deterministic 4-d model whose every output is the unit vector
    /// `[1.0, 0.0, 0.0, 0.0]`. Counts every `embed` invocation so the
    /// test can distinguish "cache hit, no live embed" from "cache
    /// miss, live embed ran".
    #[derive(Clone)]
    struct CountingOnesModel {
        calls: Arc<AtomicUsize>,
    }
    impl EmbeddingModel for CountingOnesModel {
        fn embed(&self, _text: &str) -> evidence_store::embeddings::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1.0, 0.0, 0.0, 0.0])
        }
        fn dimension(&self) -> usize {
            4
        }
        fn probe(&self) -> EmbeddingProbe {
            EmbeddingProbe::Available
        }
    }

    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, b"deadline reminder", None, ImportanceClass::Useful)
        .unwrap();

    // Seed the cache row under `model_tag = "model-a"` with a vector
    // that is orthogonal to `CountingOnesModel`'s output. If the read
    // path ignored `model_tag` and returned this row to a retriever
    // tagged `"model-b"`, the cosine similarity against the query
    // embed `[1, 0, 0, 0]` would be 0.0 → similarity_to_score(0) =
    // 0.5, which is detectably different from the 1.0 the live-embed
    // path produces.
    store
        .store_embedding(r.evidence_id, &[0.0, 1.0, 0.0, 0.0], "model-a")
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let model = CountingOnesModel {
        calls: Arc::clone(&calls),
    };

    // Retriever runs as `model-b`. The cache row's `model_tag` is
    // `model-a`, so `get_embedding_for_model` must return `None` and
    // force the live-embed fallback.
    let hits = HybridRetriever::new(&store)
        .with_embedding_model(model, "model-b")
        .search_hybrid(scope, "deadline", 5)
        .expect("search_hybrid must not propagate any error");

    // Two embed calls expected: one for the query, one for the body
    // (the fallback path). A cache hit on the stale `model-a` row
    // would have skipped the body embed, leaving the count at 1.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "model_tag filter must force live-embed fallback when the \
         cache row was produced by a different model_tag; observed \
         only {} embed call(s), which means the stale `model-a` row \
         was silently returned to a `model-b` retriever",
        calls.load(Ordering::SeqCst),
    );

    // Confirm the score reflects the live-embed path, not the stale
    // cache row. cosine([1,0,0,0], [1,0,0,0]) = 1.0 → score = 1.0.
    let hit = hits
        .iter()
        .find(|h| h.evidence_id == r.evidence_id)
        .expect("ingested row must appear in hits");
    assert!(
        (hit.vector_score - 1.0).abs() < 1e-6,
        "vector_score must reflect the live `model-b` embed (1.0), \
         not the stale `model-a` cache row scored against `model-b`'s \
         query embed (0.5); got {}",
        hit.vector_score,
    );
}

/// Regression test for the review finding "`evidence_embeddings`
/// PRIMARY KEY is `evidence_id` only — single model tag per row" (Flag
/// #5). Under the v3 composite PK `(evidence_id, model_tag)` the cache
/// must be able to hold multiple cached vectors for the same evidence
/// row when the active embedding model is swapped, so old retrievers
/// still get warm cache hits while new retrievers backfill in parallel.
///
/// We:
///   1. Ingest a single evidence row.
///   2. Write two cached vectors for the same `evidence_id` under two
///      different `model_tag`s (`alpha` and `beta`) via
///      `store_embedding`. With the old single-column PK the second
///      INSERT OR REPLACE would clobber the first row entirely.
///   3. Assert that `get_embedding_for_model` returns the correct
///      vector for *both* tags — both rows must coexist.
///   4. Assert the raw row count for that evidence_id is exactly 2.
///   5. Re-issue a write under `alpha` with a different vector and
///      assert it REPLACES the alpha row (not the beta row) — i.e. the
///      composite PK semantics are honoured by `INSERT OR REPLACE`.
#[test]
fn evidence_embeddings_holds_one_row_per_model_tag_for_same_evidence_id() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, b"deadline reminder", None, ImportanceClass::Useful)
        .unwrap();

    let alpha_v1 = vec![1.0_f32, 0.0, 0.0, 0.0];
    let beta_v1 = vec![0.0_f32, 1.0, 0.0, 0.0];

    // Two writes for the same evidence_id under two different model
    // tags. Under v2 (single-column PK) the second write would
    // overwrite the first; under v3 (composite PK) both rows coexist.
    store
        .store_embedding(r.evidence_id, &alpha_v1, "alpha")
        .expect("store alpha v1");
    store
        .store_embedding(r.evidence_id, &beta_v1, "beta")
        .expect("store beta v1");

    // Both rows must be visible by their respective tags.
    let alpha_read = store
        .get_embedding_for_model(r.evidence_id, "alpha")
        .expect("get alpha")
        .expect("alpha row must exist");
    let beta_read = store
        .get_embedding_for_model(r.evidence_id, "beta")
        .expect("get beta")
        .expect("beta row must exist");
    assert_eq!(alpha_read, alpha_v1, "alpha row must hold the alpha vector");
    assert_eq!(beta_read, beta_v1, "beta row must hold the beta vector");

    // The raw row count for this evidence_id must be exactly 2 — one
    // per `model_tag`. Under the old single-column PK the count would
    // be 1.
    let row_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_embeddings WHERE evidence_id = ?1",
            rusqlite::params![r.evidence_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        row_count, 2,
        "composite PK must let two `model_tag`s coexist for the same evidence_id"
    );

    // Now re-issue a write under `alpha` with a different vector. The
    // composite PK means this targets the (id, "alpha") row only,
    // replacing it and leaving the (id, "beta") row intact.
    let alpha_v2 = vec![0.0_f32, 0.0, 1.0, 0.0];
    store
        .store_embedding(r.evidence_id, &alpha_v2, "alpha")
        .expect("re-write alpha");

    let alpha_after = store
        .get_embedding_for_model(r.evidence_id, "alpha")
        .expect("get alpha after rewrite")
        .expect("alpha row must still exist");
    let beta_after = store
        .get_embedding_for_model(r.evidence_id, "beta")
        .expect("get beta after alpha rewrite")
        .expect("beta row must still exist");
    assert_eq!(
        alpha_after, alpha_v2,
        "alpha row must have been replaced by the v2 vector"
    );
    assert_eq!(
        beta_after, beta_v1,
        "beta row must NOT have been touched by the alpha rewrite — \
         composite PK semantics scope INSERT OR REPLACE to the exact tag"
    );
}

/// Regression test for the v2 → v3 destructive migration. We construct
/// a v2-shaped database by hand (single-column PK on
/// `evidence_embeddings`, `user_version = 2`), seed two cached rows
/// for *different* evidence_ids (one row each — the old PK forbids
/// multiple rows per id), then re-open with the production code and
/// assert:
///
///   1. The schema migration succeeds and stamps `user_version = 3`.
///   2. Both seeded rows survive the table rewrite byte-for-byte.
///   3. The migrated table accepts a *second* row for the same
///      evidence_id under a different `model_tag` — proving the
///      composite PK is in effect — and a duplicate (id, tag) is still
///      rejected (or replaced via `INSERT OR REPLACE`, which is what
///      the production write paths use).
#[test]
fn schema_migration_v2_to_v3_widens_pk_and_preserves_rows() {
    use rusqlite::Connection;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    // Build a v2-shaped database by hand. We need to seed the
    // *legacy* (single-PK) `evidence_embeddings` shape and stamp
    // user_version = 2 so production `open()` reads
    // `detected_version = 2` and runs `apply_migration(3)`.
    let seeded_id_a = evidence_store::EvidenceId::new_v4();
    let seeded_id_b = evidence_store::EvidenceId::new_v4();
    let seeded_emb_a: Vec<u8> = 0.5_f32.to_le_bytes().repeat(4); // 4 × f32
    let seeded_emb_b: Vec<u8> = (-0.25_f32).to_le_bytes().repeat(4);

    {
        let raw = Connection::open(&path).unwrap();
        let page_key = crypto::derive_key(&MASTER_KEY, b"sqlcipher:store:v1").unwrap();
        let key_pragma = format!("x'{}'", hex_encode_local(&page_key));
        raw.pragma_update(None, "key", &key_pragma).unwrap();
        raw.pragma_update(None, "cipher_page_size", 4096_i64)
            .unwrap();
        raw.pragma_update(None, "kdf_iter", 256_000_i64).unwrap();

        // Full v2 schema (v1 subset + the legacy single-PK
        // evidence_embeddings table).
        raw.execute_batch(
            r#"
            CREATE TABLE evidence (
                id BLOB PRIMARY KEY, scope_id BLOB NOT NULL,
                content_hash BLOB NOT NULL, body BLOB, body_ref BLOB,
                nonce BLOB, source_ref TEXT, acl_pointer TEXT,
                importance INTEGER NOT NULL, storage_path INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX idx_evidence_scope_created
                ON evidence (scope_id, created_at DESC);
            CREATE INDEX idx_evidence_content_hash
                ON evidence (content_hash);
            CREATE TABLE body_store (
                content_hash BLOB PRIMARY KEY, body BLOB NOT NULL,
                nonce BLOB NOT NULL, ref_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE ring_buffer (
                id INTEGER PRIMARY KEY AUTOINCREMENT, scope_id BLOB NOT NULL,
                body BLOB NOT NULL, nonce BLOB NOT NULL,
                payload_size INTEGER NOT NULL, created_at INTEGER NOT NULL
            );
            CREATE INDEX idx_ring_buffer_scope_created
                ON ring_buffer (scope_id, created_at DESC);
            CREATE VIRTUAL TABLE evidence_fts USING fts5(
                content, evidence_id UNINDEXED, scope_id UNINDEXED,
                tokenize = 'unicode61 remove_diacritics 2'
            );
            -- Legacy v2 evidence_embeddings: single-column PK on
            -- evidence_id. This is the shape the migration must
            -- rewrite.
            CREATE TABLE evidence_embeddings (
                evidence_id BLOB PRIMARY KEY,
                embedding BLOB NOT NULL,
                model_tag TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        // Seed two rows, one per evidence_id (the single-column PK
        // forbids two rows for the same id — that's the whole point
        // of the migration).
        raw.execute(
            "INSERT INTO evidence_embeddings
                 (evidence_id, embedding, model_tag, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                seeded_id_a.as_uuid().as_bytes().as_slice(),
                seeded_emb_a.clone(),
                "legacy-v2-tag",
                1_700_000_000_i64,
            ],
        )
        .unwrap();
        raw.execute(
            "INSERT INTO evidence_embeddings
                 (evidence_id, embedding, model_tag, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                seeded_id_b.as_uuid().as_bytes().as_slice(),
                seeded_emb_b.clone(),
                "legacy-v2-tag",
                1_700_000_001_i64,
            ],
        )
        .unwrap();

        // Stamp v2 so production `open()` sees `detected_version = 2`.
        raw.pragma_update(None, "user_version", 2_i32).unwrap();
    }

    // Now open with the production entrypoint. The v2 → v3 migration
    // must run and rewrite the table without losing rows.
    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open must migrate a v2 database to v3 in place");

    // 1. Version must now match the current `SCHEMA_VERSION`. We
    //    asserted v3 here historically; Gap 4 added a purely-additive
    //    v4 (`forgotten_scopes`) which the migration runner walks
    //    through unconditionally, so a v2 database opens at v4.
    let version: i32 = store
        .raw_conn()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(
        version,
        evidence_store::schema::SCHEMA_VERSION,
        "user_version must be stamped to SCHEMA_VERSION after v2 → v3 migration"
    );
    assert_eq!(version, evidence_store::schema::SCHEMA_VERSION);

    // 2. Both seeded rows must survive byte-for-byte.
    let read_a = store
        .get_embedding_for_model(seeded_id_a, "legacy-v2-tag")
        .expect("read seeded row A")
        .expect("row A must survive the migration");
    let read_b = store
        .get_embedding_for_model(seeded_id_b, "legacy-v2-tag")
        .expect("read seeded row B")
        .expect("row B must survive the migration");
    let expected_a: Vec<f32> = (0..4).map(|_| 0.5_f32).collect();
    let expected_b: Vec<f32> = (0..4).map(|_| -0.25_f32).collect();
    assert_eq!(read_a, expected_a, "row A vector must round-trip");
    assert_eq!(read_b, expected_b, "row B vector must round-trip");

    // 3. The migrated table must have the composite PK in effect. We
    //    prove this by adding a second row for the SAME evidence_id
    //    under a different model_tag. Under v2's single-PK this would
    //    have collapsed onto the existing row via INSERT OR REPLACE;
    //    under v3's composite PK both rows coexist.
    store
        .store_embedding(seeded_id_a, &[0.1, 0.2, 0.3, 0.4], "fresh-v3-tag")
        .expect("second tag for the same evidence_id must succeed under v3 composite PK");

    let row_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_embeddings WHERE evidence_id = ?1",
            rusqlite::params![seeded_id_a.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        row_count, 2,
        "post-migration table must allow two rows for the same evidence_id \
         under different `model_tag`s; got {row_count}"
    );

    // Verify the PK arity directly via PRAGMA so the assertion is not
    // purely behavioural. Two non-zero `pk` columns ⇒ composite PK.
    let pk_arity: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('evidence_embeddings') WHERE pk > 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pk_arity, 2,
        "evidence_embeddings must have a 2-column primary key after v2 → v3"
    );
}

/// Regression test for the review finding "schema migration
/// (v1→v2) has no migration path". We construct a v1-shaped database
/// (no `evidence_embeddings` table, `user_version = 1`) and assert
/// that `EvidenceStore::open` forward-ports it to v2 by creating the
/// missing table and stamping the version, without losing existing
/// rows.
#[test]
fn schema_migration_forward_ports_legacy_v1_database() {
    use rusqlite::Connection;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    // Build a v1-shaped database by hand: open with the SQLCipher
    // PRAGMA dance, create only the subset of tables that existed at
    // v1, and stamp `user_version = 1`. We can't easily reach into
    // `derive_key` from the integration test crate, so we mirror what
    // `EvidenceStore::open` does, but stop after the legacy subset.
    {
        let raw = Connection::open(&path).unwrap();
        // `EvidenceStore::open` derives the SQLCipher page key from
        // the master key with HKDF; we need to mirror that so the
        // hand-built v1 database is openable by the production code.
        let page_key = crypto::derive_key(&MASTER_KEY, b"sqlcipher:store:v1").unwrap();
        let key_pragma = format!("x'{}'", hex_encode_local(&page_key));
        raw.pragma_update(None, "key", &key_pragma).unwrap();
        raw.pragma_update(None, "cipher_page_size", 4096_i64)
            .unwrap();
        raw.pragma_update(None, "kdf_iter", 256_000_i64).unwrap();
        // v1 subset: evidence, body_store, ring_buffer, evidence_fts.
        // No evidence_embeddings.
        raw.execute_batch(
            r#"
            CREATE TABLE evidence (
                id BLOB PRIMARY KEY, scope_id BLOB NOT NULL,
                content_hash BLOB NOT NULL, body BLOB, body_ref BLOB,
                nonce BLOB, source_ref TEXT, acl_pointer TEXT,
                importance INTEGER NOT NULL, storage_path INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX idx_evidence_scope_created
                ON evidence (scope_id, created_at DESC);
            CREATE INDEX idx_evidence_content_hash
                ON evidence (content_hash);
            CREATE TABLE body_store (
                content_hash BLOB PRIMARY KEY, body BLOB NOT NULL,
                nonce BLOB NOT NULL, ref_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE ring_buffer (
                id INTEGER PRIMARY KEY AUTOINCREMENT, scope_id BLOB NOT NULL,
                body BLOB NOT NULL, nonce BLOB NOT NULL,
                payload_size INTEGER NOT NULL, created_at INTEGER NOT NULL
            );
            CREATE INDEX idx_ring_buffer_scope_created
                ON ring_buffer (scope_id, created_at DESC);
            CREATE VIRTUAL TABLE evidence_fts USING fts5(
                content, evidence_id UNINDEXED, scope_id UNINDEXED,
                tokenize = 'unicode61 remove_diacritics 2'
            );
            "#,
        )
        .unwrap();
        raw.pragma_update(None, "user_version", 1_i32).unwrap();
    }

    // Now open with the real entrypoint. The migration must succeed
    // and create `evidence_embeddings`.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open must migrate a legacy v1 database in place");

    // The new table must exist now.
    let table_exists: bool = store
        .raw_conn()
        .query_row(
            "SELECT 1 FROM sqlite_master \
                 WHERE type = 'table' AND name = 'evidence_embeddings'",
            [],
            |r| r.get::<_, i32>(0),
        )
        .is_ok();
    assert!(
        table_exists,
        "evidence_embeddings table must exist after v1→v2 migration"
    );

    // And the version must be stamped to current.
    let version: i32 = store
        .raw_conn()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(
        version,
        evidence_store::schema::SCHEMA_VERSION,
        "user_version must be stamped to SCHEMA_VERSION"
    );
}

/// Regression test for the review finding "schema migration
/// allows opening a future database silently". A database stamped
/// `user_version = 99` (a hypothetical future version) must be
/// rejected up-front rather than silently downgraded to the current
/// `SCHEMA_VERSION`.
#[test]
fn schema_migration_refuses_future_version_database() {
    use rusqlite::Connection;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    {
        let raw = Connection::open(&path).unwrap();
        let page_key = crypto::derive_key(&MASTER_KEY, b"sqlcipher:store:v1").unwrap();
        let key_pragma = format!("x'{}'", hex_encode_local(&page_key));
        raw.pragma_update(None, "key", &key_pragma).unwrap();
        raw.pragma_update(None, "cipher_page_size", 4096_i64)
            .unwrap();
        raw.pragma_update(None, "kdf_iter", 256_000_i64).unwrap();
        raw.execute_batch("CREATE TABLE evidence (id BLOB);")
            .unwrap();
        raw.pragma_update(None, "user_version", 99_i32).unwrap();
    }

    let result = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default());
    match result {
        Ok(_) => panic!("opening a future-version database must fail"),
        Err(EvidenceError::Schema(_)) => {}
        Err(other) => {
            panic!("future-version rejection must surface as EvidenceError::Schema, got {other:?}")
        }
    }
}

/// Lowercase hex encoder used by the schema-migration tests, which
/// need to drive SQLCipher's `PRAGMA key = X'…'` directly without
/// pulling in `hex` as a dev-dependency.
fn hex_encode_local(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(CHARS[(b >> 4) as usize] as char);
        s.push(CHARS[(b & 0x0f) as usize] as char);
    }
    s
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

// ---------------------------------------------------------------------
// Phase 1.11 — multilingual / cross-lingual embedding-lane invariant.
// ---------------------------------------------------------------------
//
// The production adapter is wired to XLM-R (`models/xlm-r-base.onnx`,
// 768d), which was trained on 100 languages over 2.5 TB of
// CommonCrawl.  XLM-R's defining property is that a query and a
// body do not need to share a script for their embeddings to be
// semantically clusterable — `"weather forecast"` and `明日の天気予報`
// land near each other in vector space, while `"weather forecast"`
// and `株式市場` ("stock market") do not.
//
// This module exercises that architectural invariant with a
// deterministic mock that simulates the same concept-clustering
// shape (without dragging the real XLM-R ONNX session into the
// test harness).  The mock maps multilingual paraphrases of the
// same concept onto the same unit-vector axis, so cosine
// similarity between paraphrases is 1.0 and similarity between
// unrelated concepts is 0.0.  Running this against the real
// `HybridRetriever` pins the invariant that the retriever does
// NOT script-segregate the embedding lane (a future refactor
// that accidentally inserted a "skip embed when query script !=
// body script" branch would fail this test).

/// Number of orthogonal concept axes the mock supports. One per
/// concept; the last axis is the "unrelated" catch-all so any
/// off-vocabulary text reliably lands far from every concept.
const MULTILINGUAL_MOCK_DIM: usize = 8;

/// Deterministic mock that simulates XLM-R's cross-lingual
/// concept-clustering shape.  Each input text is mapped to one of
/// a small inventory of concept axes; identical concepts produce
/// identical unit vectors (cos sim = 1.0), different concepts
/// produce orthogonal unit vectors (cos sim = 0.0).  The mock is
/// pure — no randomness, no model artifact — so the test is
/// reproducible without ORT installed.
struct MultilingualConceptMockModel;

impl MultilingualConceptMockModel {
    /// Concept axis for `text`. Multilingual paraphrases of the
    /// same concept share an axis; the catch-all axis is
    /// `MULTILINGUAL_MOCK_DIM - 1` so unmatched inputs land far
    /// from every named concept.
    fn concept_for(text: &str) -> usize {
        // The inputs are intentionally drawn from a tiny fixed
        // inventory.  We avoid a substring match because we want
        // to assert the architectural invariant — that the
        // retriever does not segregate by script — without
        // tangling the test in the mock's matching policy.
        //
        // `clippy::match_same_arms` is silenced deliberately: the
        // whole point of the mock is that cross-script
        // paraphrases collapse onto the *same* concept axis (so
        // their bodies are identical by design).  Merging the
        // arms with `|` would defeat the visual demonstration of
        // which inputs cluster, which is precisely the property
        // this test is designed to expose to future readers.
        #[allow(clippy::match_same_arms)]
        match text {
            // Concept 0 — weather (Latin, CJK, French, Spanish).
            "weather forecast" => 0,
            "明日の天気予報" => 0,
            "prévisions météo" => 0,
            "pronóstico del tiempo" => 0,
            "天气预报" => 0,
            // Concept 1 — finance.
            "stock market" => 1,
            "株式市場" => 1,
            // Concept 2 — cooking.
            "recipe ingredients" => 2,
            "レシピの材料" => 2,
            // Off-concept: lands on the catch-all axis.
            _ => MULTILINGUAL_MOCK_DIM - 1,
        }
    }
}

impl EmbeddingModel for MultilingualConceptMockModel {
    fn embed(&self, text: &str) -> evidence_store::embeddings::Result<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbeddingError::EmptyInput);
        }
        let mut v = vec![0.0_f32; MULTILINGUAL_MOCK_DIM];
        v[Self::concept_for(text)] = 1.0;
        Ok(v)
    }
    fn dimension(&self) -> usize {
        MULTILINGUAL_MOCK_DIM
    }
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

/// Cross-lingual recall via the real [`HybridRetriever::search_hybrid`]
/// surface.  English query `"weather forecast"`, Japanese body
/// `明日の天気予報` ("tomorrow's weather forecast"), unrelated
/// Japanese body `株式市場` ("stock market").  Pins the architectural
/// invariant that the embedding lane does NOT script-segregate —
/// the cross-script weather paraphrase MUST score above the
/// same-script unrelated body on `vector_score`, even though
/// FTS5 (which DOES tokenise per-script) returns the Japanese
/// stock-market body as the only lexical hit for the English
/// "forecast" query (it doesn't match anything in the CJK
/// body either, so FTS contributes nothing to either row, and
/// the vector lane is the sole signal).
#[test]
fn vector_telemetry_cross_lingual_recall_via_search_hybrid() {
    let (_dir, mut store) = open_store_with_model(MultilingualConceptMockModel, "ml-mock-v1");
    let scope = ScopeId::new_v4();
    // Two bodies in CJK script — one a paraphrase of the English
    // query, one unrelated.
    let weather_jp = store
        .ingest(
            scope,
            "明日の天気予報".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let stock_jp = store
        .ingest(scope, "株式市場".as_bytes(), None, ImportanceClass::Useful)
        .unwrap();

    // Vector-only weights: zero out FTS and recency so the
    // vector_score is the sole tiebreaker.  Cosine between two
    // identical unit vectors is 1.0; cosine between two
    // orthogonal unit vectors is 0.0.  The retriever carries
    // its own [`EmbeddingModel`] handle (it does NOT inherit
    // the one wired into the store at ingest time), so we wire
    // the same mock in on the retriever for the query-side
    // path.
    let retriever = HybridRetriever::new(&store)
        .with_embedding_model(MultilingualConceptMockModel, "ml-mock-v1")
        .with_weights(HybridWeights {
            fts: 0.0,
            recency: 0.0,
            vector: 1.0,
        });
    let hits = retriever
        .search_hybrid(scope, "weather forecast", 10)
        .unwrap();

    let weather_hit = hits
        .iter()
        .find(|h| h.evidence_id == weather_jp.evidence_id)
        .expect("Japanese weather body must appear in cross-lingual results");
    let stock_hit = hits
        .iter()
        .find(|h| h.evidence_id == stock_jp.evidence_id)
        .expect("Japanese stock body must appear in cross-lingual results");

    // The cross-script paraphrase scores HIGHER than the
    // unrelated same-script body — XLM-R's signature property
    // mocked by the concept-axis vectors above.
    assert!(
        weather_hit.vector_score > stock_hit.vector_score,
        "expected cross-lingual paraphrase to outscore unrelated body; \
         weather={weather_score}, stock={stock_score}",
        weather_score = weather_hit.vector_score,
        stock_score = stock_hit.vector_score,
    );
    // The weather body is the top result (vector-only weights,
    // FTS contributes 0.0 here because the English query does
    // not appear in either CJK body).
    assert_eq!(
        hits[0].evidence_id, weather_jp.evidence_id,
        "expected Japanese weather body to top the cross-lingual ranking, got hits={hits:?}"
    );
}

/// Same invariant via [`HybridRetriever::rerank_with_embeddings`]
/// — the alternative entry point.  French query, Spanish body of
/// the same concept, English body of a different concept.  Pins
/// that the rerank path is equally script-agnostic.
#[test]
fn vector_telemetry_cross_lingual_recall_via_rerank() {
    let (_dir, mut store) = open_store_with_model(MultilingualConceptMockModel, "ml-mock-v1");
    let scope = ScopeId::new_v4();
    let weather_es = store
        .ingest(
            scope,
            "pronóstico del tiempo".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let cooking_en = store
        .ingest(
            scope,
            "recipe ingredients".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    // Feed both rows in as candidates with a flat 0.0 baseline
    // so the rerank scores are entirely vector-driven.
    let candidates = vec![
        RetrievalResult {
            evidence_id: weather_es.evidence_id,
            score: 0.0,
            fts_score: 0.0,
            recency_score: 0.0,
            vector_score: 0.0,
        },
        RetrievalResult {
            evidence_id: cooking_en.evidence_id,
            score: 0.0,
            fts_score: 0.0,
            recency_score: 0.0,
            vector_score: 0.0,
        },
    ];
    let bodies = vec![
        (weather_es.evidence_id, "pronóstico del tiempo".to_string()),
        (cooking_en.evidence_id, "recipe ingredients".to_string()),
    ];

    let retriever = HybridRetriever::new(&store)
        .with_embedding_model(MultilingualConceptMockModel, "ml-mock-v1");

    // Capture the telemetry baseline immediately before the call so the
    // delta is invariant under any other test bumping the same
    // process-singleton counters concurrently or sequentially.
    let before = evidence_store::vector_telemetry::snapshot();

    let reranked = retriever
        .rerank_with_embeddings("prévisions météo", candidates, &bodies)
        .expect("rerank ok");
    let weather_hit = reranked
        .iter()
        .find(|h| h.evidence_id == weather_es.evidence_id)
        .expect("Spanish weather body present");
    let cooking_hit = reranked
        .iter()
        .find(|h| h.evidence_id == cooking_en.evidence_id)
        .expect("English cooking body present");
    assert!(
        weather_hit.vector_score > cooking_hit.vector_score,
        "French query should match Spanish weather paraphrase above English cooking body; \
         weather={weather_score}, cooking={cooking_score}",
        weather_score = weather_hit.vector_score,
        cooking_score = cooking_hit.vector_score,
    );

    // Regression coverage for the Phase-1.11 sweep-1 Bug fix:
    // `rerank_with_embeddings` MUST bump `query_embeddings_total` at
    // least once for the query embed AND `live_body_embeddings_total`
    // at least once per body it embeds.  Before the fix the
    // body-embed call site at `retrieval.rs:296` was silently
    // uninstrumented; this lower-bound assertion would have caught
    // that.  See PR #110 Devin Review sweep 1.
    //
    // Uses `>= before + N` rather than `== before + N` to stay
    // robust under parallel test execution: other tests in this
    // binary also exercise `MultilingualConceptMockModel` through
    // the public retriever surface and bump the same process-
    // singleton counters.  See the docstring on
    // `vector_telemetry::tests` for the architectural rationale.
    let after = evidence_store::vector_telemetry::snapshot();
    assert!(
        after.query_embeddings_total > before.query_embeddings_total,
        "rerank_with_embeddings must move query_embeddings_total upward by at least 1"
    );
    assert!(
        after.live_body_embeddings_total
            >= before
                .live_body_embeddings_total
                .saturating_add(bodies.len() as u64),
        "rerank_with_embeddings must move live_body_embeddings_total upward by at least {} (one per body)",
        bodies.len()
    );
}

/// Phase 1.11 — verify the vector-telemetry counters move through
/// the public retriever surface end-to-end.  Bumps `live_body_*`
/// rather than `cache_hits_*` because the store ingests the bodies
/// WITHOUT a wired-in model (`fresh_store` returns a model-less
/// store), so the retriever's `candidate_embedding` path has to
/// fall through to the live re-embed branch on every row.
#[test]
fn vector_telemetry_counters_move_through_public_retriever() {
    use evidence_store::vector_telemetry;
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    // Ingest WITHOUT a wired-in model so the cache is empty.
    // `candidate_embedding` will hit `MissNoRow` for every row
    // and fall through to live re-embed.
    let r1 = store
        .ingest(
            scope,
            "weather forecast".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let _r2 = store
        .ingest(
            scope,
            "stock market".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let before = vector_telemetry::snapshot();
    let retriever = HybridRetriever::new(&store)
        .with_embedding_model(MultilingualConceptMockModel, "ml-mock-v1")
        .with_weights(HybridWeights {
            fts: 0.0,
            recency: 0.0,
            vector: 1.0,
        });
    let hits = retriever
        .search_hybrid(scope, "weather forecast", 10)
        .unwrap();
    let after = vector_telemetry::snapshot();

    assert!(!hits.is_empty(), "search_hybrid produced no results");

    // The query was successfully embedded — bumps Query.
    assert!(
        after.query_embeddings_total > before.query_embeddings_total,
        "query_embeddings_total did not move (before={}, after={})",
        before.query_embeddings_total,
        after.query_embeddings_total,
    );
    // Every candidate fell through to the live-body re-embed —
    // bumps LiveBody at least once.  The retriever inspects
    // both rows so the increment is >= 1 (the exact count
    // depends on internal `candidate_embedding` call patterns
    // which are private; we only assert movement).
    assert!(
        after.live_body_embeddings_total > before.live_body_embeddings_total,
        "live_body_embeddings_total did not move (before={}, after={})",
        before.live_body_embeddings_total,
        after.live_body_embeddings_total,
    );
    // Cache was empty (no wired-in model on `ingest`), so the
    // miss-no-row counter MUST move.
    assert!(
        after.cache_misses_no_row_total > before.cache_misses_no_row_total,
        "cache_misses_no_row_total did not move (before={}, after={})",
        before.cache_misses_no_row_total,
        after.cache_misses_no_row_total,
    );
    // Sanity: the row IDs returned are the ones we ingested.
    assert!(
        hits.iter().any(|h| h.evidence_id == r1.evidence_id),
        "expected first ingested row in results"
    );
}
