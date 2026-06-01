//! Process-singleton observability counters for the multilingual
//! embedding / vector-retrieval path (Phase 1.11).
//!
//! Phases 1.3 – 1.10 closed the multilingual gaps on the lexical
//! ([`crate::fts_telemetry`]) and classifier
//! ([`observation_engine::lexicon_telemetry`]) lanes — but the
//! third leg of [`crate::retrieval::HybridRetriever`]'s fan-in,
//! the semantic-vector lane, was running blind: zero metrics on
//! adapter health, on how often the `evidence_embeddings` cache
//! actually paid off vs. the live-embed fallback, on
//! [`crate::store::EvidenceStore::index_embedding_or_copy_dedup`]
//! short-circuits, or on whether the `model_tag` rotation
//! discipline (a single tag MUST map to a single output
//! dimension) was holding in production. This module closes
//! that observability gap with the same atomic-counter shape
//! used by [`crate::fts_telemetry`].
//!
//! # Counter taxonomy
//!
//! Every counter is monotonic and process-singleton. The
//! taxonomy mirrors the read / write decision points in
//! [`crate::store::EvidenceStore`] and
//! [`crate::retrieval::HybridRetriever`]:
//!
//! * **Live embeddings successfully computed** — three sibling
//!   counters, one per call site:
//!
//!   * [`EmbedSite::Query`] — query-side embed in
//!     [`crate::retrieval::HybridRetriever::search_hybrid`] or
//!     [`crate::retrieval::HybridRetriever::rerank_with_embeddings`].
//!     One increment per *successful* `model.embed(query)` call.
//!   * [`EmbedSite::IndexWrite`] — body-side embed in
//!     [`crate::store::EvidenceStore::index_embedding`] (the
//!     fresh-embed path inside ingest). Counts only the cases
//!     where the dedup-copy short-circuit was NOT taken — i.e.
//!     when a brand-new vector actually got computed.
//!   * [`EmbedSite::LiveBody`] — body-side embed run live by
//!     the retriever rather than the index-writer. Bumped by
//!     two distinct call sites that share the same operational
//!     meaning (the model embedded a body's plaintext on the
//!     read path, NOT the write path):
//!
//!     * [`crate::retrieval::HybridRetriever::candidate_embedding`]'s
//!       cache-miss fallback (the `evidence_embeddings` cache
//!       had no usable row for the active `model_tag`, so the
//!       retriever re-embedded the plaintext body inline).
//!     * [`crate::retrieval::HybridRetriever::rerank_with_embeddings`]'s
//!       caller-supplied-body branch (the caller passed an
//!       explicit `body_lookup` map; the cache is not consulted
//!       at all, but the embed still happens at retrieval time
//!       rather than ingest time).
//!
//!     The counter aggregates both because the operator question
//!     it answers — "how many body embeds did we run live on
//!     the read path?" — is the same in both cases.
//!
//!   These three are mutually exclusive *per call site*: a
//!   single decision point bumps exactly one of them on a
//!   successful embed and nothing on a failed embed (errors are
//!   counted separately, see below).
//!
//! * **Embedding-cache outcomes (read path)** — four sibling
//!   counters covering the disposition of every cache lookup
//!   in [`crate::store::EvidenceStore::get_embedding_for_model`]:
//!
//!   * [`CacheOutcome::Hit`] — the cached row was returned and
//!     its dimension matched the query embedding's
//!     dimension. No live re-embed needed — fast path.
//!   * [`CacheOutcome::MissNoRow`] — no cached row exists for
//!     the active `(evidence_id, model_tag)`. The retriever
//!     falls through to the live-embed path.
//!   * [`CacheOutcome::MissDimension`] — a row existed but its
//!     dimension did NOT match. Defensive — this should never
//!     happen when the `model_tag` rotation rule is followed
//!     (one tag ⇒ one dimension), so a non-zero count here is
//!     an operator-visible warning that the rule was violated
//!     somewhere in history. Mutually exclusive with
//!     [`CacheOutcome::Hit`].
//!   * [`CacheOutcome::MissReadError`] — the cache `SELECT`
//!     itself returned `Err` (corrupted blob, transient
//!     SQLite I/O hiccup). The retriever demotes this to a
//!     miss so a flaky cache table cannot abort an otherwise-
//!     valid search.
//!
//!   The four outcomes are mutually exclusive by construction:
//!   every call to `get_embedding_for_model` bumps exactly one
//!   of them. Summing the four gives the total cache-lookup
//!   attempt count.
//!
//! * **Dedup-copy hits (write path)** — single counter
//!   [`Counters::dedup_copy_hits_total`] for the cases where
//!   [`crate::store::EvidenceStore::index_embedding_or_copy_dedup`]
//!   found a prior evidence row with the same `content_hash`
//!   AND the same active `model_tag`, and reused that row's
//!   cached vector instead of running the ONNX runtime again.
//!   This is the key win for high-dedup workloads (mailing-list
//!   threads, replayed payloads); dividing this counter by the
//!   total ingest count gives the dedup-copy hit rate.
//!
//! * **Adapter error breakdown** — three sibling counters, one
//!   per [`crate::embeddings::EmbeddingError`] variant that can
//!   surface from a production call. Every failed embed bumps
//!   exactly one of them; success bumps none. The
//!   [`crate::embeddings::EmbeddingError::EmptyInput`] variant
//!   is intentionally NOT counted: every production call site
//!   short-circuits on empty input before reaching the adapter,
//!   so any `EmptyInput` count would reflect a programmer error
//!   on the call-site side rather than an adapter issue.
//!
//!   * [`EmbeddingErrorKind::RuntimeUnavailable`] — adapter
//!     reported the ONNX runtime / dynamic library is missing
//!     (e.g. a host whose `ORT_DYLIB_PATH` is unset). A non-zero
//!     count means the substrate is running in stub-fallback
//!     mode for that fraction of calls; the vector lane is
//!     producing `0.0` scores instead of meaningful cosines.
//!   * [`EmbeddingErrorKind::ModelLoad`] — the runtime tried to
//!     open the model file and failed. Usually filesystem
//!     (missing artifact, permissions); the operator-visible
//!     remediation is to verify the `OnnxModelConfig::model_path`
//!     points at the shipped artifact.
//!   * [`EmbeddingErrorKind::InferenceFailure`] — the runtime
//!     loaded but the inference call itself failed (OOM,
//!     kernel error, tokenizer issue, dimension mismatch
//!     between runtime output and `OnnxModelConfig::dimension`).
//!
//! * **`model_tag` rotation invariant** — single counter
//!   [`Counters::model_tag_dimension_violations_total`] bumped
//!   by [`record_observed_dimension`] when the same `model_tag`
//!   is observed at *different* output dimensions over the life
//!   of the process. The first observation registers the
//!   `(tag, dim)` pair; every subsequent observation with a
//!   matching tag but a mismatching dimension bumps the
//!   violation counter and emits a `tracing::warn!`. The check
//!   is purely advisory — it does NOT fail the surrounding
//!   operation. This intentionally mirrors the fail-open
//!   philosophy of the rest of the embedding code (cache
//!   inserts are best-effort, body decryption errors are
//!   demoted to per-row misses, etc.); the goal is to make
//!   the rotation-rule violation operator-visible without
//!   adding a new failure mode that didn't exist before
//!   Phase 1.11.
//!
//! # Wire-format stability
//!
//! [`VectorTelemetrySnapshot`] is the wire-flat read-out
//! structure platform hosts deserialize via the FFI. New
//! counters must be added as additional fields with
//! `#[serde(default)]` on the FFI-mirror struct in
//! `crates/ffi/src/metrics.rs` — see the sibling
//! [`crate::fts_telemetry`] module doc for the full
//! wire-evolution rationale.
//!
//! # Performance
//!
//! Same model as [`crate::fts_telemetry`]: one
//! [`AtomicU64::fetch_add`] with [`Ordering::Relaxed`] per
//! decision point. No allocations, no locks on the hot path.
//! The single exception is
//! [`record_observed_dimension`], which acquires a
//! [`std::sync::Mutex`] around a small `HashMap<String, usize>`
//! the first time each `model_tag` is observed; for the steady
//! state of a single active tag, the lookup is one hash + one
//! integer comparison under the lock and the lock holds for
//! microseconds at most. The mutex sits behind a `OnceLock` so
//! processes that never wire in an embedding model pay zero
//! cost.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Process-singleton bag of atomic counters. Internal — callers
/// touch via [`record_embedding_computed`], [`record_cache_outcome`],
/// [`record_dedup_copy_hit`], [`record_embedding_error`],
/// [`record_observed_dimension`], and read via [`snapshot`].
///
/// The `_total` field suffix is the Prometheus-canonical naming
/// convention for monotonic counter metrics — preserved through
/// the FFI [`crate::ffi::metrics::VectorTelemetry`] mirror and
/// into any downstream metric exporter.
#[derive(Default, Debug)]
#[allow(clippy::struct_field_names)]
pub(crate) struct Counters {
    // ─── Live embeddings successfully computed ──────────────────
    pub(crate) query_embeddings_total: AtomicU64,
    pub(crate) index_write_embeddings_total: AtomicU64,
    pub(crate) live_body_embeddings_total: AtomicU64,

    // ─── Embedding-cache outcomes (read path) ───────────────────
    pub(crate) cache_hits_total: AtomicU64,
    pub(crate) cache_misses_no_row_total: AtomicU64,
    pub(crate) cache_misses_dimension_total: AtomicU64,
    pub(crate) cache_misses_read_error_total: AtomicU64,

    // ─── Dedup-copy hits (write path) ───────────────────────────
    pub(crate) dedup_copy_hits_total: AtomicU64,

    // ─── Adapter error breakdown ────────────────────────────────
    pub(crate) runtime_unavailable_total: AtomicU64,
    pub(crate) model_load_errors_total: AtomicU64,
    pub(crate) inference_failures_total: AtomicU64,

    // ─── model_tag rotation invariant ──────────────────────────
    pub(crate) model_tag_dimension_violations_total: AtomicU64,
}

static COUNTERS: OnceLock<Counters> = OnceLock::new();

/// Borrow the process-singleton counter block. Internal.
#[inline]
fn counters() -> &'static Counters {
    COUNTERS.get_or_init(Counters::default)
}

/// `model_tag -> first-observed dimension` registry used by
/// [`record_observed_dimension`] to enforce the rotation rule.
/// Sits behind a `OnceLock` so the allocation only happens for
/// processes that wire in an embedding model.
static OBSERVED_TAG_DIMS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

#[inline]
fn observed_tag_dims() -> &'static Mutex<HashMap<String, usize>> {
    OBSERVED_TAG_DIMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Which call site produced a successful `model.embed()` call.
/// Each variant maps to a distinct counter in [`Counters`]; the
/// three variants are mutually exclusive per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedSite {
    /// Query-side embed in
    /// [`crate::retrieval::HybridRetriever::search_hybrid`] or
    /// [`crate::retrieval::HybridRetriever::rerank_with_embeddings`].
    Query,
    /// Body-side embed in
    /// [`crate::store::EvidenceStore::index_embedding`] (the
    /// fresh-embed path inside ingest).
    IndexWrite,
    /// Body-side embed run live by the retriever. Bumped by
    /// two distinct call sites that share the same operational
    /// meaning — see the module-level doc on
    /// [`crate::vector_telemetry`] for the full enumeration
    /// ([`crate::retrieval::HybridRetriever::candidate_embedding`]'s
    /// cache-miss fallback AND
    /// [`crate::retrieval::HybridRetriever::rerank_with_embeddings`]'s
    /// caller-supplied-body branch).
    LiveBody,
}

/// Record a successful `model.embed()` call at `site`. No-op
/// for any failure path — callers bump [`record_embedding_error`]
/// in that case (the success / error counters never both fire
/// for a single embed attempt).
#[inline]
pub fn record_embedding_computed(site: EmbedSite) {
    let c = counters();
    let counter = match site {
        EmbedSite::Query => &c.query_embeddings_total,
        EmbedSite::IndexWrite => &c.index_write_embeddings_total,
        EmbedSite::LiveBody => &c.live_body_embeddings_total,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Disposition of a single
/// [`crate::store::EvidenceStore::get_embedding_for_model`]
/// call. The four variants are mutually exclusive by
/// construction — every cache lookup bumps exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// The cached row was returned and its dimension matched
    /// the query embedding's dimension. Fast path — no live
    /// re-embed needed.
    Hit,
    /// No cached row exists for the active
    /// `(evidence_id, model_tag)`. The retriever falls through
    /// to the live-embed path.
    MissNoRow,
    /// A row existed but its dimension did NOT match the
    /// query embedding's dimension. Defensive: should never
    /// happen when the `model_tag` rotation rule (one tag ⇒
    /// one dimension) is followed; a non-zero count signals
    /// the rule was violated somewhere in history. Mutually
    /// exclusive with [`Self::Hit`].
    MissDimension,
    /// The cache `SELECT` itself returned `Err` (corrupted
    /// blob, transient SQLite I/O hiccup). The retriever
    /// demotes this to a miss to preserve the fail-open
    /// contract on the read path.
    MissReadError,
}

/// Record the outcome of a single embedding-cache lookup.
#[inline]
pub fn record_cache_outcome(outcome: CacheOutcome) {
    let c = counters();
    let counter = match outcome {
        CacheOutcome::Hit => &c.cache_hits_total,
        CacheOutcome::MissNoRow => &c.cache_misses_no_row_total,
        CacheOutcome::MissDimension => &c.cache_misses_dimension_total,
        CacheOutcome::MissReadError => &c.cache_misses_read_error_total,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Record a dedup-copy hit in
/// [`crate::store::EvidenceStore::index_embedding_or_copy_dedup`]
/// — i.e. a prior evidence row with the same `content_hash` AND
/// the same active `model_tag` was found and its cached vector
/// was reused (skipping the ONNX runtime invocation).
#[inline]
pub fn record_dedup_copy_hit() {
    counters()
        .dedup_copy_hits_total
        .fetch_add(1, Ordering::Relaxed);
}

/// Which [`crate::embeddings::EmbeddingError`] variant a failed
/// embed call returned. The
/// [`crate::embeddings::EmbeddingError::EmptyInput`] variant is
/// intentionally absent — production call sites short-circuit
/// on empty input before reaching the adapter, so any
/// `EmptyInput` would be a programmer error on the call-site
/// side rather than an adapter issue worth reporting through
/// telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingErrorKind {
    /// [`crate::embeddings::EmbeddingError::RuntimeUnavailable`].
    /// Adapter reported the ONNX runtime / dynamic library is
    /// missing — the vector lane is degraded for this call.
    RuntimeUnavailable,
    /// [`crate::embeddings::EmbeddingError::ModelLoad`]. The
    /// runtime tried to open the model file and failed.
    ModelLoad,
    /// [`crate::embeddings::EmbeddingError::InferenceFailure`].
    /// Runtime loaded but the inference call itself failed.
    InferenceFailure,
}

/// Record a failed `model.embed()` attempt. The
/// success / error counters never both fire for a single embed
/// attempt: callers bump [`record_embedding_computed`] on `Ok`
/// and one of these variants on `Err`.
#[inline]
pub fn record_embedding_error(err: EmbeddingErrorKind) {
    let c = counters();
    let counter = match err {
        EmbeddingErrorKind::RuntimeUnavailable => &c.runtime_unavailable_total,
        EmbeddingErrorKind::ModelLoad => &c.model_load_errors_total,
        EmbeddingErrorKind::InferenceFailure => &c.inference_failures_total,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Convenience wrapper around [`record_embedding_error`] that
/// translates from the upstream [`crate::embeddings::EmbeddingError`]
/// variant tag at the call site. Keeps the variant ↔ counter
/// mapping in this module so every call site does not need to
/// match on the full error enum.
///
/// [`crate::embeddings::EmbeddingError::EmptyInput`] is silently
/// dropped (no counter is bumped) because every production call
/// site short-circuits on empty input before reaching the
/// adapter — see the [`EmbeddingErrorKind`] doc for the full
/// rationale.
#[inline]
pub fn record_embedding_error_from(err: &crate::embeddings::EmbeddingError) {
    use crate::embeddings::EmbeddingError as E;
    let kind = match err {
        E::RuntimeUnavailable { .. } => EmbeddingErrorKind::RuntimeUnavailable,
        E::ModelLoad { .. } => EmbeddingErrorKind::ModelLoad,
        E::InferenceFailure(_) => EmbeddingErrorKind::InferenceFailure,
        // `EmptyInput` is a call-site bug per the
        // `EmbeddingErrorKind` doc — not part of the adapter
        // health signal. Skip.
        E::EmptyInput => return,
    };
    record_embedding_error(kind);
}

/// Register the dimension observed for `model_tag`. The first
/// call with a given tag records the `(tag, dim)` pair; every
/// subsequent call with the same tag but a *different*
/// dimension bumps [`Counters::model_tag_dimension_violations_total`]
/// and emits a `tracing::warn!` so the operator sees the rotation-
/// rule violation in both metrics and logs.
///
/// The check is purely advisory: it never fails the surrounding
/// operation. Callers should invoke this helper at
/// [`crate::retrieval::HybridRetriever::with_embedding_model`]
/// wire-in time and inside the live-embed paths so a violation
/// is caught the moment it appears, not only when the cached row
/// happens to be inspected.
///
/// `model_tag` MUST be non-empty — empty-tag wirings are skipped
/// because the
/// [`crate::store::EvidenceStore::with_embedding_model`] helper
/// allows empty tags as a "no model wired in" sentinel, and we
/// don't want to register an arbitrary dimension under the empty
/// key.
pub fn record_observed_dimension(model_tag: &str, dim: usize) {
    if model_tag.is_empty() {
        return;
    }
    let mut map = match observed_tag_dims().lock() {
        Ok(g) => g,
        // A poisoned mutex means a previous holder panicked. The
        // counter operation is best-effort observability — recover
        // the map via `into_inner()` and proceed with the
        // observation rather than propagating panics into the
        // embedding hot path. The recovered map is structurally
        // valid: the only mutation in this function is a single
        // `insert` of a `(String, usize)` pair, which is atomic at
        // the Rust level, so the worst case is that the prior
        // panic dropped one half-inserted pair (the rotation
        // counter would miss at most one violation).
        Err(poisoned) => poisoned.into_inner(),
    };
    match map.get(model_tag).copied() {
        None => {
            map.insert(model_tag.to_string(), dim);
        }
        Some(seen) if seen == dim => {
            // Consistent — no-op.
        }
        Some(seen) => {
            counters()
                .model_tag_dimension_violations_total
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                model_tag,
                first_observed_dim = seen,
                conflicting_dim = dim,
                "vector_telemetry: model_tag rotation rule violated — a single model_tag must map to a single output dimension; bump the tag on any model change"
            );
        }
    }
}

/// Wire-flat read-out of every counter at the moment of the
/// [`snapshot`] call.
///
/// `_total` suffix is preserved on every field for the same
/// Prometheus-convention reason documented on [`Counters`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(clippy::struct_field_names)]
#[serde(default)]
pub struct VectorTelemetrySnapshot {
    /// Successful query-side embeds in
    /// [`crate::retrieval::HybridRetriever::search_hybrid`] or
    /// [`crate::retrieval::HybridRetriever::rerank_with_embeddings`].
    pub query_embeddings_total: u64,
    /// Successful fresh body embeds in
    /// [`crate::store::EvidenceStore::index_embedding`] (the
    /// path NOT short-circuited by dedup-copy).
    pub index_write_embeddings_total: u64,
    /// Successful body embeds run live by the retriever — both
    /// [`crate::retrieval::HybridRetriever::candidate_embedding`]'s
    /// cache-miss fallback AND
    /// [`crate::retrieval::HybridRetriever::rerank_with_embeddings`]'s
    /// caller-supplied-body branch. See [`EmbedSite::LiveBody`]
    /// for the rationale on aggregating both.
    pub live_body_embeddings_total: u64,
    /// Embedding-cache lookups that returned a dimension-
    /// matching row for the active `(evidence_id, model_tag)`.
    pub cache_hits_total: u64,
    /// Embedding-cache lookups that found no row for the
    /// active `(evidence_id, model_tag)`.
    pub cache_misses_no_row_total: u64,
    /// Embedding-cache lookups that returned a row whose
    /// dimension did NOT match. Defensive — non-zero means the
    /// `model_tag` rotation rule (one tag ⇒ one dimension) was
    /// violated somewhere in history.
    pub cache_misses_dimension_total: u64,
    /// Embedding-cache lookups whose `SELECT` itself errored.
    /// Demoted to a miss to preserve the fail-open read-path
    /// contract.
    pub cache_misses_read_error_total: u64,
    /// Dedup-copy hits in
    /// [`crate::store::EvidenceStore::index_embedding_or_copy_dedup`]
    /// — the dominant write-path optimisation for high-dedup
    /// workloads.
    pub dedup_copy_hits_total: u64,
    /// Failed embeds with
    /// [`crate::embeddings::EmbeddingError::RuntimeUnavailable`].
    pub runtime_unavailable_total: u64,
    /// Failed embeds with
    /// [`crate::embeddings::EmbeddingError::ModelLoad`].
    pub model_load_errors_total: u64,
    /// Failed embeds with
    /// [`crate::embeddings::EmbeddingError::InferenceFailure`].
    pub inference_failures_total: u64,
    /// Number of times [`record_observed_dimension`] observed
    /// the same `model_tag` at a different output dimension
    /// than the first observation — a rotation-rule violation.
    pub model_tag_dimension_violations_total: u64,
}

/// Return a wire-flat snapshot of every vector-telemetry
/// counter.
#[must_use]
pub fn snapshot() -> VectorTelemetrySnapshot {
    let c = counters();
    VectorTelemetrySnapshot {
        query_embeddings_total: c.query_embeddings_total.load(Ordering::Relaxed),
        index_write_embeddings_total: c.index_write_embeddings_total.load(Ordering::Relaxed),
        live_body_embeddings_total: c.live_body_embeddings_total.load(Ordering::Relaxed),
        cache_hits_total: c.cache_hits_total.load(Ordering::Relaxed),
        cache_misses_no_row_total: c.cache_misses_no_row_total.load(Ordering::Relaxed),
        cache_misses_dimension_total: c.cache_misses_dimension_total.load(Ordering::Relaxed),
        cache_misses_read_error_total: c.cache_misses_read_error_total.load(Ordering::Relaxed),
        dedup_copy_hits_total: c.dedup_copy_hits_total.load(Ordering::Relaxed),
        runtime_unavailable_total: c.runtime_unavailable_total.load(Ordering::Relaxed),
        model_load_errors_total: c.model_load_errors_total.load(Ordering::Relaxed),
        inference_failures_total: c.inference_failures_total.load(Ordering::Relaxed),
        model_tag_dimension_violations_total: c
            .model_tag_dimension_violations_total
            .load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests in this module mutate the process-singleton counter
    // block in `COUNTERS`. Integration tests in
    // `crates/evidence_store/tests/retrieval.rs` and any future
    // unit test that drives `EvidenceStore::ingest` or
    // `HybridRetriever::search_hybrid` through a wired-in
    // embedding model bump the same counters, and `cargo test`'s
    // default scheduler runs tests within the same test binary
    // in parallel. Exact-delta assertions (`after == before + N`)
    // are therefore inherently flaky against process-singleton
    // counters — a concurrent test bumping any of these between
    // our two snapshots would break the equality.
    //
    // The architecturally correct assertion for a singleton-state
    // counter under parallel test execution is a monotonic
    // lower bound: "my call incremented the counter by at least
    // N". That property catches every real wiring bug — a
    // `record_*` writing to the wrong field, a counter that's
    // accidentally a no-op, a snapshot reader that dropped a
    // field — without depending on the absence of concurrent
    // activity. The unrelated-counter invariant ("a `Query` bump
    // must NOT cross into `IndexWrite`") is implicitly preserved:
    // if `Query` were misrouted to `IndexWrite`, the
    // `query_embeddings_total > before` assertion would itself
    // fail. See `crates/ffi/src/metrics.rs:1402-1419` for the
    // precedent docstring this mirrors.

    /// Pin successful-embed accounting: every per-site
    /// `record_embedding_computed` call moves its own counter
    /// upward by at least one.
    #[test]
    fn record_embedding_computed_routes_to_correct_site() {
        let before = snapshot();
        record_embedding_computed(EmbedSite::Query);
        record_embedding_computed(EmbedSite::IndexWrite);
        record_embedding_computed(EmbedSite::LiveBody);
        let after = snapshot();
        assert!(
            after.query_embeddings_total > before.query_embeddings_total,
            "Query bump must move query_embeddings_total upward by at least 1"
        );
        assert!(
            after.index_write_embeddings_total > before.index_write_embeddings_total,
            "IndexWrite bump must move index_write_embeddings_total upward by at least 1"
        );
        assert!(
            after.live_body_embeddings_total > before.live_body_embeddings_total,
            "LiveBody bump must move live_body_embeddings_total upward by at least 1"
        );
    }

    /// Pin cache-outcome accounting: each variant call moves
    /// its own counter upward by at least one.
    #[test]
    fn record_cache_outcome_routes_to_correct_variant() {
        let before = snapshot();
        record_cache_outcome(CacheOutcome::Hit);
        record_cache_outcome(CacheOutcome::MissNoRow);
        record_cache_outcome(CacheOutcome::MissDimension);
        record_cache_outcome(CacheOutcome::MissReadError);
        let after = snapshot();
        assert!(
            after.cache_hits_total > before.cache_hits_total,
            "Hit bump must move cache_hits_total upward by at least 1"
        );
        assert!(
            after.cache_misses_no_row_total > before.cache_misses_no_row_total,
            "MissNoRow bump must move cache_misses_no_row_total upward by at least 1"
        );
        assert!(
            after.cache_misses_dimension_total > before.cache_misses_dimension_total,
            "MissDimension bump must move cache_misses_dimension_total upward by at least 1"
        );
        assert!(
            after.cache_misses_read_error_total > before.cache_misses_read_error_total,
            "MissReadError bump must move cache_misses_read_error_total upward by at least 1"
        );
    }

    /// Pin error-kind accounting: each variant call moves its
    /// own counter upward by at least one.
    #[test]
    fn record_embedding_error_routes_to_correct_kind() {
        let before = snapshot();
        record_embedding_error(EmbeddingErrorKind::RuntimeUnavailable);
        record_embedding_error(EmbeddingErrorKind::ModelLoad);
        record_embedding_error(EmbeddingErrorKind::InferenceFailure);
        let after = snapshot();
        assert!(
            after.runtime_unavailable_total > before.runtime_unavailable_total,
            "RuntimeUnavailable bump must move runtime_unavailable_total upward by at least 1"
        );
        assert!(
            after.model_load_errors_total > before.model_load_errors_total,
            "ModelLoad bump must move model_load_errors_total upward by at least 1"
        );
        assert!(
            after.inference_failures_total > before.inference_failures_total,
            "InferenceFailure bump must move inference_failures_total upward by at least 1"
        );
    }

    /// Pin dedup-copy counter: each call moves
    /// `dedup_copy_hits_total` upward by at least one.
    #[test]
    fn record_dedup_copy_hit_bumps_counter() {
        let before = snapshot();
        record_dedup_copy_hit();
        let after = snapshot();
        assert!(
            after.dedup_copy_hits_total > before.dedup_copy_hits_total,
            "record_dedup_copy_hit must move dedup_copy_hits_total upward by at least 1"
        );
    }

    /// Pin the `model_tag` rotation invariant via the
    /// lower-bound assertion pattern: registering a unique tag,
    /// re-observing it at the same dim, and then observing a
    /// dimension change must move
    /// `model_tag_dimension_violations_total` upward by at least
    /// one between the pre- and post-violation snapshots.
    ///
    /// Using `>= before + 1` rather than `== before + 1` keeps
    /// the test correct under parallel execution: any other test
    /// in the same binary that exercises
    /// [`record_observed_dimension`] with conflicting dims would
    /// otherwise race the equality assertion.
    #[test]
    fn record_observed_dimension_detects_rotation_violation() {
        // Use a unique tag per test invocation so the FIRST
        // observation is guaranteed to register cleanly under
        // the process-singleton registry (no other test can
        // collide on this exact tag). A monotonic AtomicU64 is
        // enough — PID is identical across all tests in the
        // binary, so we add a per-call discriminator to
        // distinguish call sites within the same process.
        static TAG_COUNTER: AtomicU64 = AtomicU64::new(0);
        let tag = format!(
            "rotation-test-pid{}-n{}",
            std::process::id(),
            TAG_COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        // First observation registers; second identical
        // observation is a no-op; third (different dim) is the
        // violation.
        let before_violation = snapshot();
        record_observed_dimension(&tag, 768);
        record_observed_dimension(&tag, 768);
        record_observed_dimension(&tag, 384);
        let after_violation = snapshot();

        assert!(
            after_violation.model_tag_dimension_violations_total
                > before_violation.model_tag_dimension_violations_total,
            "Dimension change for same tag MUST move violation counter upward by at least 1"
        );
    }

    /// Pin empty-tag short-circuit: an empty `model_tag` is
    /// treated as the no-model-wired-in sentinel and skipped.
    ///
    /// Asserts directly on the `OBSERVED_TAG_DIMS` registry
    /// rather than on the singleton counter. The counter-based
    /// shape (`after_violations == before_violations`) would be
    /// racy under parallel test execution: the sibling
    /// [`record_observed_dimension_detects_rotation_violation`]
    /// bumps the same `model_tag_dimension_violations_total`,
    /// and if it interleaves between this test's two snapshots
    /// the equality would fail even though the empty-tag short-
    /// circuit itself never contributed. The registry-state
    /// shape is race-free: the empty-key insert (or absence
    /// thereof) is a property of the local call, not a global
    /// counter that other tests share.
    #[test]
    fn record_observed_dimension_skips_empty_tag() {
        // The empty-tag short-circuit's invariant: a call with
        // `model_tag == ""` must NEVER insert the empty key into
        // the registry, regardless of `dim`. We assert that
        // directly under the registry's lock, which is the only
        // state the empty-tag branch could mutate. Counter state
        // is shared with sibling tests and not checked here — its
        // correctness is implied by the registry state (an empty-
        // key insert would precede any counter bump on
        // re-observation).
        record_observed_dimension("", 768);
        record_observed_dimension("", 384); // would be a violation if not skipped
        let map = observed_tag_dims().lock().expect("observed_tag_dims lock");
        assert!(
            !map.contains_key(""),
            "Empty model_tag must NOT be inserted into the OBSERVED_TAG_DIMS registry (would have allowed a future re-observation to bump the violation counter); registry currently has {} keys",
            map.len()
        );
    }
}
