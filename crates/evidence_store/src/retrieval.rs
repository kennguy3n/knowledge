//! Hybrid retrieval over the evidence plane.
//!
//! Per `PHASES.md` Phase 1: "Hybrid retrieval — FTS5 + semantic
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
use crate::store::EvidenceStore;

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
/// supplied the retriever degrades to the pre-Phase-1 behaviour
/// (vector_score = 0.0).
pub struct HybridRetriever<'a> {
    store: &'a EvidenceStore,
    weights: HybridWeights,
    /// Decay constant (seconds) for the recency score. Defaults to 7
    /// days; smaller values bias toward "what just happened", larger
    /// values produce a flatter recency curve.
    recency_half_life_seconds: f64,
    embedding_model: Option<Box<dyn EmbeddingModel>>,
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
    pub fn with_embedding_model<M: EmbeddingModel + 'static>(mut self, model: M) -> Self {
        self.embedding_model = Some(Box::new(model));
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
    pub fn search_fts(
        &self,
        scope_id: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RetrievalResult>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.store.raw_conn().prepare(
            "SELECT evidence_id, rank
             FROM evidence_fts
             WHERE evidence_fts MATCH ?1 AND scope_id = ?2
             ORDER BY rank
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                query,
                scope_id.as_uuid().as_bytes().as_slice(),
                limit as i64,
            ],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, f64>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (id_bytes, rank) = row?;
            let id = EvidenceId(slice_to_uuid(&id_bytes)?);
            let fts_score = 1.0 / (1.0 + (-rank).max(0.0));
            out.push(RetrievalResult {
                evidence_id: id,
                score: fts_score,
                fts_score,
                recency_score: 0.0,
                vector_score: 0.0,
            });
        }
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
            params![scope_id.as_uuid().as_bytes().as_slice(), limit as i64],
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
        let query_vec = model
            .embed(query)
            .map_err(|e| EvidenceError::Embedding(format!("embedding query failed: {e}")))?;
        let body_lookup: std::collections::HashMap<EvidenceId, &str> = bodies
            .iter()
            .map(|(id, body)| (*id, body.as_str()))
            .collect();
        let mut out = Vec::with_capacity(candidates.len());
        for mut hit in candidates {
            let vector_score = match body_lookup.get(&hit.evidence_id) {
                Some(body) => match model.embed(body) {
                    Ok(v) => similarity_to_score(cosine_similarity(&query_vec, &v)),
                    Err(_) => 0.0,
                },
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
        // store's `evidence_embeddings` cache when present (Phase B);
        // otherwise we fall back to decrypting + re-embedding the body
        // on the fly. Failures are localised: if the query embed
        // fails we skip the lane wholesale (vector_score = 0.0);
        // per-row failures (missing body, runtime hiccup, dimension
        // mismatch with a stale cached row) just leave that single
        // row at 0.0.
        let query_vec = self
            .embedding_model
            .as_ref()
            .and_then(|model| model.embed(query).ok());
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
    ///   1. The cached row in `evidence_embeddings`, when its length
    ///      matches the query embedding's dimension.
    ///   2. Re-embed the plaintext body via `model`.
    ///
    /// Returns `None` when neither path produces a usable vector
    /// (e.g. the row has no body, the body is binary, the model
    /// errored, or every available vector mismatches `query_dim`).
    /// The stored-cache hit short-circuits before reading the body,
    /// which is the Phase-B perf win.
    fn candidate_embedding(
        &self,
        id: EvidenceId,
        query_dim: usize,
        model: &dyn EmbeddingModel,
    ) -> Result<Option<Vec<f32>>> {
        if let Some(stored) = self.store.get_embedding(id)? {
            if stored.len() == query_dim {
                return Ok(Some(stored));
            }
            // Dimension mismatch: stored vector is from a different
            // model. Fall through to the live-embed path so the score
            // is still useful for this query.
        }
        let Some(body) = self.lookup_body_text(id)? else {
            return Ok(None);
        };
        Ok(model.embed(&body).ok())
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
