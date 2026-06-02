//! Hybrid retrieval over the evidence plane.
//!
//! Per `docs/DESIGN.md` §3.2: "Hybrid retrieval — FTS5 + semantic
//! vector + recency". This module implements all three components:
//! the FTS5 (lexical) and recency lanes draw straight from the
//! evidence schema, and the semantic-vector lane is wired through
//! whichever [`EmbeddingModel`] the caller plumbed in via
//! [`HybridRetriever::with_embedding_model`]. When no model is
//! present the vector component contributes `0.0` and the retriever
//! degrades to FTS + recency only.
//!
//! The fan-in scoring is intentionally simple: each component
//! produces a `0.0 ..= 1.0` score, and the final score is a weighted
//! sum. Callers can override the weights via [`HybridWeights`].

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::embeddings::{cosine_similarity, similarity_to_score, EmbeddingModel};
use crate::error::{EvidenceError, Result};
use crate::ids::{EvidenceId, ScopeId};
use crate::store::{clamp_limit_to_sqlite, merged_fts_search, EvidenceStore};

/// One row in a hybrid retrieval result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetrievalResult {
    /// The evidence row id.
    pub evidence_id: EvidenceId,
    /// Final fan-in score in `0.0 ..= 1.0`.
    pub score: f64,
    /// FTS5 (lexical) score in `0.0 ..= 1.0`.
    pub fts_score: f64,
    /// Recency score in `0.0 ..= 1.0`.
    pub recency_score: f64,
    /// Semantic-vector score in `0.0 ..= 1.0`. Computed from the
    /// configured [`EmbeddingModel`] when one is plumbed in;
    /// otherwise contributes `0.0`.
    pub vector_score: f64,
}

/// Weights for the [`HybridRetriever`] fan-in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HybridWeights {
    /// Weight on the FTS5 component.
    pub fts: f64,
    /// Weight on the recency component.
    pub recency: f64,
    /// Weight on the semantic-vector component (stub).
    pub vector: f64,
}

impl Default for HybridWeights {
    fn default() -> Self {
        // 0.6 + 0.3 + 0.1 = 1.0. The vector component contributes
        // when an [`EmbeddingModel`] is plumbed into the retriever
        // and otherwise stays at zero, leaving FTS + recency as the
        // only signals.
        Self {
            fts: 0.6,
            recency: 0.3,
            vector: 0.1,
        }
    }
}

/// Hybrid retriever — combines FTS5 + recency + semantic vector
/// similarity over the encrypted evidence plane. The semantic
/// component is plumbed via an [`EmbeddingModel`]; when none is
/// supplied the retriever degrades to the FTS5-only behaviour
/// (vector_score = 0.0).
pub struct HybridRetriever<'a> {
    store: &'a EvidenceStore,
    weights: HybridWeights,
    /// Decay constant (seconds) for the recency score. Defaults to 7
    /// days; smaller values bias toward "what just happened", larger
    /// values produce a flatter recency curve.
    recency_half_life_seconds: f64,
    embedding_model: Option<Box<dyn EmbeddingModel>>,
    /// Free-form tag (e.g. `"xlm-r-v1"`) identifying the active
    /// embedding model. Empty when no model is wired in. Mirrors
    /// [`EvidenceStore`]'s `embedding_model_tag` so the read path can
    /// scope `evidence_embeddings` lookups by `model_tag` and avoid
    /// returning stale rows produced by a previous, dimension-matching
    /// model.
    embedding_model_tag: String,
}

impl<'a> HybridRetriever<'a> {
    /// Build a retriever with the default weights and a 7-day
    /// recency half-life.
    pub fn new(store: &'a EvidenceStore) -> Self {
        Self {
            store,
            weights: HybridWeights::default(),
            recency_half_life_seconds: 7.0 * 86_400.0,
            embedding_model: None,
            embedding_model_tag: String::new(),
        }
    }

    /// Override the fan-in weights.
    pub fn with_weights(mut self, weights: HybridWeights) -> Self {
        self.weights = weights;
        self
    }

    /// Override the recency half-life (in seconds).
    pub fn with_recency_half_life_seconds(mut self, seconds: f64) -> Self {
        self.recency_half_life_seconds = seconds;
        self
    }

    /// Plumb an [`EmbeddingModel`] in for the semantic-vector
    /// component of the hybrid score. Calling [`Self::search_hybrid`]
    /// after this will compute cosine similarity between the query
    /// embedding and the per-row stored embedding (when present).
    ///
    /// `model_tag` must match the tag the [`EvidenceStore`] was
    /// configured with when the rows were ingested — the retriever
    /// only consumes cache rows whose `model_tag` equals this string,
    /// so a stale row produced by a previous model (even one with the
    /// same output dimension) falls through to the live-embed path
    /// rather than producing a semantically meaningless cosine score.
    pub fn with_embedding_model<M: EmbeddingModel + 'static>(
        mut self,
        model: M,
        model_tag: impl Into<String>,
    ) -> Self {
        let dim = model.dimension();
        self.embedding_model = Some(Box::new(model));
        self.embedding_model_tag = model_tag.into();
        // Register the wired-in (tag, dimension) pair so a same-tag /
        // different-dimension rotation violation is flagged in
        // `model_tag_dimension_violations_total` the moment the second
        // retriever instance is constructed, not only when the cache
        // happens to be consulted. Skipped for the empty-tag sentinel
        // (see `record_observed_dimension`).
        crate::vector_telemetry::record_observed_dimension(&self.embedding_model_tag, dim);
        self
    }

    /// `true` iff the retriever has been wired to an embedding model.
    pub fn has_embedding_model(&self) -> bool {
        self.embedding_model.is_some()
    }

    /// Lexical search via SQLite FTS5.
    ///
    /// FTS5's built-in `rank` is a negative score (smaller is more
    /// relevant); we project it into `0.0 ..= 1.0` via
    /// `1 / (1 + (-rank))` so that the most relevant row scores
    /// closest to `1.0`.
    ///
    /// Per  / schema v14 the search fans out across **both**
    /// lexical indexes — `evidence_fts` (unicode61) for whitespace-
    /// segmented scripts and `evidence_fts_cjk` (trigram) for CJK
    /// Han / Hiragana / Katakana / Thai content — and de-duplicates
    /// on `evidence_id` taking the best (smallest, i.e. most
    /// relevant) rank across the two tokenisers. The two ranks are
    /// each table's own BM25 score and are not strictly comparable
    /// across tokenisers, but both are negative-and-smaller-is-
    /// better so `MIN(rank)` is the correct dedupe rule. A row that
    /// matches both indexes (mixed-script body with a query term
    /// findable in either tokenisation) appears once with its best
    /// rank rather than twice with separate scores. The
    /// `fts_score` field surfaced on
    /// [`RetrievalResult`] is the unified projected score derived
    /// from that best rank.
    ///
    /// Both branches accept the same FTS5 query *grammar*
    /// (bareword / `"phrase"` / `term OR term` / `NEAR(…)` /
    /// column-filter / prefix-star), but the `trigram` tokeniser
    /// rejects more shapes than `unicode61` does: per the
    /// [`trigram` tokeniser documentation][trigram-doc] it
    /// returns a SQLite error — not an empty result — when a
    /// query term is shorter than 3 characters, when the query
    /// is a `NEAR(…)` expression, when it uses a column filter,
    /// or when it asks for a prefix-star match shorter than 3
    /// codepoints.
    ///
    /// To preserve the architectural invariant that a
    /// syntactically valid `unicode61` query never breaks hybrid
    /// retrieval, the two branches run as two independent
    /// prepared statements and are merged in Rust:
    ///
    /// * `unicode61` is the source of truth for validity — its
    ///   errors propagate.
    /// * `trigram` is additive recall — its errors are silently
    ///   treated as an empty contribution.
    ///
    /// The shared [`crate::store::merged_fts_search`] helper
    /// implements both halves so this method and
    /// [`crate::EvidenceStore::search_fts`] cannot drift in their
    /// dedupe + error-containment semantics.
    ///
    /// [trigram-doc]: <https://www.sqlite.org/fts5.html#the_trigram_tokenizer>
    pub fn search_fts(
        &self,
        scope_id: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RetrievalResult>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let merged = merged_fts_search(self.store.raw_conn(), scope_id, query, limit)?;
        let out = merged
            .into_iter()
            .map(|(id, rank)| {
                let fts_score = 1.0 / (1.0 + (-rank).max(0.0));
                RetrievalResult {
                    evidence_id: id,
                    score: fts_score,
                    fts_score,
                    recency_score: 0.0,
                    vector_score: 0.0,
                }
            })
            .collect();
        Ok(out)
    }

    /// Recency search — most recent evidence rows in `scope_id`.
    pub fn search_recency(&self, scope_id: ScopeId, limit: usize) -> Result<Vec<RetrievalResult>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.store.raw_conn().prepare(
            "SELECT id, created_at FROM evidence
             WHERE scope_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                clamp_limit_to_sqlite(limit),
            ],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let now = chrono::Utc::now().timestamp();
        let mut out = Vec::new();
        for row in rows {
            let (id_bytes, created_at) = row?;
            let id = EvidenceId(slice_to_uuid(&id_bytes)?);
            let recency_score = self.recency_decay(now, created_at);
            out.push(RetrievalResult {
                evidence_id: id,
                score: recency_score,
                fts_score: 0.0,
                recency_score,
                vector_score: 0.0,
            });
        }
        Ok(out)
    }

    /// Re-rank an existing set of candidates by semantic similarity
    /// to `query`. Each candidate is `(EvidenceId, body_text)` — the
    /// caller is responsible for decrypting bodies via
    /// [`EvidenceStore::read_body`] (the retriever borrows the store
    /// immutably and so cannot read bodies itself).
    ///
    /// Returns the candidates with `vector_score` populated and the
    /// final `score` re-computed from the configured weights.
    pub fn rerank_with_embeddings(
        &self,
        query: &str,
        candidates: Vec<RetrievalResult>,
        bodies: &[(EvidenceId, String)],
    ) -> Result<Vec<RetrievalResult>> {
        let Some(model) = self.embedding_model.as_ref() else {
            return Ok(candidates);
        };
        // pre-embedding routing gate. A noise-only
        // rerank query (pure punctuation / emoji / digits)
        // would burn an ONNX call to produce a near-zero vector
        // that scores every candidate body uniformly — worse
        // than just returning the FTS+recency candidates
        // unchanged. Skip the lane wholesale in that case.
        let query_route = crate::embedding_routing::classify_for_embedding(query);
        crate::vector_telemetry::record_pre_embed_decision(query_route);
        if matches!(
            query_route,
            crate::embedding_routing::EmbeddingRoute::Skip(_)
        ) {
            return Ok(candidates);
        }
        let query_vec = match model.embed(query) {
            Ok(v) => {
                crate::vector_telemetry::record_embedding_computed(
                    crate::vector_telemetry::EmbedSite::Query,
                );
                v
            }
            Err(err) => {
                crate::vector_telemetry::record_embedding_error_from(&err);
                return Err(EvidenceError::Embedding(format!(
                    "embedding query failed: {err}"
                )));
            }
        };
        let body_lookup: std::collections::HashMap<EvidenceId, &str> = bodies
            .iter()
            .map(|(id, body)| (*id, body.as_str()))
            .collect();
        let mut out = Vec::with_capacity(candidates.len());
        for mut hit in candidates {
            let vector_score = match body_lookup.get(&hit.evidence_id) {
                Some(body) => {
                    // per-body pre-embed gate. A
                    // noise-only body would otherwise have its
                    // `vector_score` set from a near-zero embed
                    // that drags the fan-in score toward 0.5
                    // (the `cos == 0` projection per
                    // `similarity_to_score`). Skipping the
                    // embed and leaving `vector_score = 0.0`
                    // matches the existing "no body" branch
                    // below.
                    let body_route = crate::embedding_routing::classify_for_embedding(body);
                    crate::vector_telemetry::record_pre_embed_decision(body_route);
                    if matches!(
                        body_route,
                        crate::embedding_routing::EmbeddingRoute::Skip(_),
                    ) {
                        0.0
                    } else {
                        match model.embed(body) {
                            Ok(v) => {
                                crate::vector_telemetry::record_embedding_computed(
                                    crate::vector_telemetry::EmbedSite::LiveBody,
                                );
                                similarity_to_score(cosine_similarity(&query_vec, &v))
                            }
                            Err(err) => {
                                crate::vector_telemetry::record_embedding_error_from(&err);
                                0.0
                            }
                        }
                    }
                }
                None => 0.0,
            };
            hit.vector_score = vector_score;
            hit.score = self.weights.fts * hit.fts_score
                + self.weights.recency * hit.recency_score
                + self.weights.vector * hit.vector_score;
            out.push(hit);
        }
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }

    /// Hybrid search — fan-in over FTS5 + recency + semantic
    /// vector. Returns the top `limit` rows by combined score.
    ///
    /// When an [`EmbeddingModel`] is wired in via
    /// [`Self::with_embedding_model`], every fan-in candidate has its
    /// `vector_score` populated by embedding the candidate's body
    /// text and projecting `cosine_similarity(query, body)` into
    /// `[0.0, 1.0]` via [`crate::embeddings::similarity_to_score`].
    /// When no model is plumbed in (or the query embed itself fails)
    /// the vector component falls through to `0.0`.
    pub fn search_hybrid(
        &self,
        scope_id: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RetrievalResult>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Pull FTS hits with a wider net so recency tie-breakers can
        // surface candidates the FTS index might rank low.
        let widened = limit.saturating_mul(4).max(16);
        let fts_hits = self.search_fts(scope_id, query, widened)?;
        let mut by_id: std::collections::HashMap<EvidenceId, RetrievalResult> =
            fts_hits.into_iter().map(|h| (h.evidence_id, h)).collect();

        // Layer recency scores onto every fan-in candidate. We only
        // need recency for the rows we already collected from FTS,
        // plus the most-recent N to give the recency component a
        // chance to introduce new candidates.
        let recency_hits = self.search_recency(scope_id, widened)?;
        for r in &recency_hits {
            by_id
                .entry(r.evidence_id)
                .and_modify(|existing| existing.recency_score = r.recency_score)
                .or_insert(*r);
        }

        // Fill in created_at-based recency for any FTS-only hits the
        // recency window did not cover.
        for entry in by_id.values_mut() {
            if entry.recency_score == 0.0 {
                if let Some(score) = self.lookup_recency(entry.evidence_id)? {
                    entry.recency_score = score;
                }
            }
        }

        // Compute the semantic-vector lane when an embedding model is
        // plumbed in. We embed the query once, then walk every
        // candidate. The candidate body vector is sourced from the
        // store's `evidence_embeddings` cache when present;
        // otherwise we fall back to decrypting + re-embedding the body
        // on the fly. Failures are localised: if the query embed
        // fails we skip the lane wholesale (vector_score = 0.0);
        // per-row failures (missing body, runtime hiccup, dimension
        // mismatch with a stale cached row) just leave that single
        // row at 0.0.
        // Query-side embed for the semantic-vector lane. We do not
        // propagate failures here: a query embed error degrades the
        // single search to lexical-only (vector_score = 0.0) but must
        // not abort the whole `search_hybrid` call (the lexical hits
        // are still useful and the caller cannot do anything about an
        // adapter outage anyway). The success / error counters cover
        // both branches so the operator can see degraded mode in
        // metrics.
        // pre-embedding routing gate. A noise-only
        // query (pure punctuation / emoji / digits) would have
        // its near-zero embedding score every candidate body at
        // roughly the same (low) cosine similarity, dragging
        // the fan-in score toward `0.5` and effectively
        // randomising the top-k. Skipping the lane wholesale
        // is strictly better — the FTS+recency lanes still rank
        // the candidates by lexical signal.
        //
        // The gate is guarded behind `self.embedding_model.is_some()`
        // so the `pre_embed_*_total` counters reflect only call
        // sites where the model is actually available to service
        // the routing decision — matching the same pattern in
        // [`Self::rerank_with_embeddings`] above and
        // [`EvidenceStore::index_embedding`]. Without this guard
        // the admission-rate metric documented at
        // `vector_telemetry.rs` (`admitted / total = ONNX-call
        // admission rate`) would be inflated for retrievers
        // running in FTS+recency-only mode.
        let query_vec = if let Some(model) = self.embedding_model.as_ref() {
            let query_route = crate::embedding_routing::classify_for_embedding(query);
            crate::vector_telemetry::record_pre_embed_decision(query_route);
            if matches!(
                query_route,
                crate::embedding_routing::EmbeddingRoute::Skip(_),
            ) {
                None
            } else {
                match model.embed(query) {
                    Ok(v) => {
                        crate::vector_telemetry::record_embedding_computed(
                            crate::vector_telemetry::EmbedSite::Query,
                        );
                        Some(v)
                    }
                    Err(err) => {
                        crate::vector_telemetry::record_embedding_error_from(&err);
                        None
                    }
                }
            }
        } else {
            None
        };
        if let (Some(model), Some(query_vec)) = (self.embedding_model.as_ref(), query_vec) {
            for entry in by_id.values_mut() {
                let body_vec =
                    self.candidate_embedding(entry.evidence_id, query_vec.len(), model.as_ref())?;
                if let Some(body_vec) = body_vec {
                    entry.vector_score =
                        similarity_to_score(cosine_similarity(&query_vec, &body_vec));
                }
            }
        }

        // Final fan-in score uses whatever per-component scores the
        // lanes above produced (with `vector_score` defaulting to
        // `0.0` when no embedding model is wired in).
        for entry in by_id.values_mut() {
            entry.score = self.weights.fts * entry.fts_score
                + self.weights.recency * entry.recency_score
                + self.weights.vector * entry.vector_score;
        }

        let mut results: Vec<_> = by_id.into_values().collect();
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    /// Decrypt and decode the text body for `id`. Returns `None`
    /// when the row has no body, or when the body is binary (rather
    /// than failing the whole search). Surfaces hard SQL / crypto
    /// errors as [`EvidenceError`].
    fn lookup_body_text(&self, id: EvidenceId) -> Result<Option<String>> {
        match self.store.read_body(id) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) if !text.is_empty() => Ok(Some(text)),
                // Empty plaintext or a binary body just means we
                // cannot embed this row; skip it rather than failing
                // the whole search.
                Ok(_) | Err(_) => Ok(None),
            },
            Err(EvidenceError::NotFound(_) | EvidenceError::DanglingBodyRef) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Source the per-candidate embedding vector for the
    /// semantic-vector lane.
    ///
    /// Order of preference:
    ///   1. The cached row in `evidence_embeddings` *that was produced
    ///      by the active `model_tag`*, when its length matches the
    ///      query embedding's dimension. The `model_tag` filter is
    ///      load-bearing: without it, a row written by a previous
    ///      model that happens to share an output dimension with the
    ///      active model would silently be scored as if it had been
    ///      produced by the active model, yielding a meaningless
    ///      cosine score.
    ///   2. Re-embed the plaintext body via `model`.
    ///
    /// Returns `None` when neither path produces a usable vector
    /// (e.g. the row has no body, the body is binary, the model
    /// errored, the cache row is corrupted, the active `model_tag`
    /// has no cached row for this evidence id, or every available
    /// vector mismatches `query_dim`). The stored-cache hit
    /// short-circuits before reading the body, which is the embedding-cache
    /// perf win.
    ///
    /// Cache-load failures (`EvidenceError::Schema` from a corrupted
    /// blob, `EvidenceError::Sqlite` from a transient SQL hiccup) are
    /// **not** propagated — the embedding cache is a pure
    /// optimisation, so per-row read errors are demoted to "cache
    /// miss" and the live-embed path runs instead. Body-decryption
    /// errors (crypto failures, missing body rows, SQL hiccups when
    /// reading the body) are likewise demoted to a per-row miss with
    /// a `tracing::warn!`: a single corrupted body row must not abort
    /// the entire `search_hybrid` call. This mirrors the
    /// localised-failure contract documented in
    /// [`Self::search_hybrid`].
    fn candidate_embedding(
        &self,
        id: EvidenceId,
        query_dim: usize,
        model: &dyn EmbeddingModel,
    ) -> Result<Option<Vec<f32>>> {
        match self
            .store
            .get_embedding_for_model(id, &self.embedding_model_tag)
        {
            Ok(Some(stored)) if stored.len() == query_dim => {
                crate::vector_telemetry::record_cache_outcome(
                    crate::vector_telemetry::CacheOutcome::Hit,
                );
                return Ok(Some(stored));
            }
            // Three distinct cache-lookup dispositions, each demoting
            // to the live-embed path but routed to a distinct
            // telemetry counter so operators can distinguish
            // expected-miss (`MissNoRow`) from rotation-rule
            // violations (`MissDimension`) and from transient
            // storage errors (`MissReadError`):
            //
            // * `Ok(None)`: no cached row for the active
            //   `(evidence_id, model_tag)`.
            // * `Ok(Some(_))` past the dimension-match arm above:
            //   defensive — the cache row's dimension did not match.
            //   Should be impossible under the `model_tag` rotation
            //   rule (one tag ⇒ one dim); the counter makes any
            //   violation operator-visible.
            // * `Err(Schema | Sqlite)`: a corrupted cache row or
            //   transient SQL error must not abort the whole search.
            Ok(None) => {
                crate::vector_telemetry::record_cache_outcome(
                    crate::vector_telemetry::CacheOutcome::MissNoRow,
                );
            }
            Ok(Some(_)) => {
                crate::vector_telemetry::record_cache_outcome(
                    crate::vector_telemetry::CacheOutcome::MissDimension,
                );
            }
            Err(EvidenceError::Schema(_) | EvidenceError::Sqlite(_)) => {
                crate::vector_telemetry::record_cache_outcome(
                    crate::vector_telemetry::CacheOutcome::MissReadError,
                );
            }
            // `get_embedding_for_model` only constructs `Sqlite` (from
            // the `query_row` call) and `Schema` (from
            // `bytes_to_embedding`). The remaining `EvidenceError`
            // variants are listed explicitly so the compiler errors
            // out if someone adds a new variant: the demotion-to-miss
            // contract above is deliberately narrow, and any new
            // error path needs a conscious decision about whether to
            // demote or propagate.
            Err(
                err @ (EvidenceError::Crypto(_)
                | EvidenceError::Io(_)
                | EvidenceError::AppendOnlyViolation(_)
                | EvidenceError::NotFound(_)
                | EvidenceError::DanglingBodyRef
                | EvidenceError::InvalidConfig(_)
                | EvidenceError::InvalidUtf8
                | EvidenceError::Embedding(_)),
            ) => return Err(err),
        }
        // Body decryption errors are demoted to a per-row miss: a
        // single row with a corrupted body / crypto failure must not
        // take down the rest of the search. The warning is emitted at
        // `warn` level (not `debug`) because such an error indicates
        // real on-disk corruption operators will want to see, but the
        // result is the same as a cache miss — score 0.0 for this
        // row, continue with the remaining candidates.
        let body = match self.lookup_body_text(id) {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(None),
            Err(err) => {
                tracing::warn!(error = %err,
                    evidence_id = %id.as_uuid(),
                    "candidate_embedding: body lookup failed; scoring row at 0.0 and continuing"
                );
                return Ok(None);
            }
        };
        // pre-embedding routing gate for the
        // cache-miss fallback. A body classified as noise-only
        // would have its `vector_score` set from a near-zero
        // embed; skipping the embed and returning `None` here
        // matches the upstream caller's existing "no body"
        // semantics in `search_hybrid`.
        let body_route = crate::embedding_routing::classify_for_embedding(&body);
        crate::vector_telemetry::record_pre_embed_decision(body_route);
        if matches!(
            body_route,
            crate::embedding_routing::EmbeddingRoute::Skip(_),
        ) {
            return Ok(None);
        }
        match model.embed(&body) {
            Ok(v) => {
                crate::vector_telemetry::record_embedding_computed(
                    crate::vector_telemetry::EmbedSite::LiveBody,
                );
                Ok(Some(v))
            }
            Err(err) => {
                // Cache-miss fallback failed — score the row at 0.0
                // and continue. The per-variant error counter makes
                // adapter-health regressions visible even when no
                // cache row exists yet (e.g. first search after a
                // model rotation).
                crate::vector_telemetry::record_embedding_error_from(&err);
                Ok(None)
            }
        }
    }

    fn lookup_recency(&self, id: EvidenceId) -> Result<Option<f64>> {
        let created_at: Option<i64> = self
            .store
            .raw_conn()
            .query_row(
                "SELECT created_at FROM evidence WHERE id = ?1",
                params![id.as_uuid().as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let now = chrono::Utc::now().timestamp();
        Ok(created_at.map(|c| self.recency_decay(now, c)))
    }

    fn recency_decay(&self, now: i64, created_at: i64) -> f64 {
        let elapsed = (now - created_at).max(0) as f64;
        if self.recency_half_life_seconds <= 0.0 {
            return 0.0;
        }
        (-elapsed / self.recency_half_life_seconds * std::f64::consts::LN_2)
            .exp()
            .clamp(0.0, 1.0)
    }
}

fn slice_to_uuid(slice: &[u8]) -> Result<uuid::Uuid> {
    if slice.len() != 16 {
        return Err(EvidenceError::Schema("uuid column has wrong width"));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(slice);
    Ok(uuid::Uuid::from_bytes(bytes))
}
