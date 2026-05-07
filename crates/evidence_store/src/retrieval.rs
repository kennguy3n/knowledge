//! Hybrid retrieval over the evidence plane.
//!
//! Per `PHASES.md` Phase 1: "Hybrid retrieval — FTS5 + semantic
//! vector + recency". This module implements the FTS5 (lexical) and
//! recency components and stubs the semantic vector component
//! (returns `0.0`) until the XLM-R ONNX path lands.
//!
//! The fan-in scoring is intentionally simple: each component
//! produces a `0.0 ..= 1.0` score, and the final score is a weighted
//! sum. Callers can override the weights via [`HybridWeights`].

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

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
    /// Semantic-vector score — stubbed at `0.0` for Phase 1.
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
        // 0.6 + 0.3 + 0.1 = 1.0; vector weight is reserved for the
        // XLM-R rollout but contributes nothing today (the score is
        // hard-stubbed to 0.0).
        Self {
            fts: 0.6,
            recency: 0.3,
            vector: 0.1,
        }
    }
}

/// Hybrid retriever — combines FTS5 + recency + (stub) vector
/// similarity over the encrypted evidence plane.
pub struct HybridRetriever<'a> {
    store: &'a EvidenceStore,
    weights: HybridWeights,
    /// Decay constant (seconds) for the recency score. Defaults to 7
    /// days; smaller values bias toward "what just happened", larger
    /// values produce a flatter recency curve.
    recency_half_life_seconds: f64,
}

impl<'a> HybridRetriever<'a> {
    /// Build a retriever with the default weights and a 7-day
    /// recency half-life.
    pub fn new(store: &'a EvidenceStore) -> Self {
        Self {
            store,
            weights: HybridWeights::default(),
            recency_half_life_seconds: 7.0 * 86_400.0,
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

    /// Hybrid search — fan-in over FTS5 + recency + (stub) semantic
    /// vector. Returns the top `limit` rows by combined score.
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
            // Vector similarity is stubbed at 0.0 until XLM-R ships.
            entry.vector_score = 0.0;
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
