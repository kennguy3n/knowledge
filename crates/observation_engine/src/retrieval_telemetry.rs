//! Unified retrieval-telemetry read surface (Phase 2.0).
//!
//! Three sibling telemetry modules ship with the multilingual stack
//! today, each tracking a distinct decision point on the retrieval
//! pipeline:
//!
//! * [`evidence_store::fts_telemetry`] — per-lane FTS5 query / row
//!   totals, recall-lane structural skips, stopword-strip volumes
//!   per call site (Phase 1.10).
//! * [`crate::lexicon_telemetry`] — per-BCP-47 lexicon hits,
//!   [`MatchStrategy`](crate::MatchStrategy) fires, Arabic / Hebrew
//!   clitic-peel depth distribution (Phase 1.10).
//! * [`evidence_store::vector_telemetry`] — embedding-call-site
//!   volumes, `evidence_embeddings` cache outcomes, adapter error
//!   variants, `model_tag` rotation-rule violations (Phase 1.11).
//!
//! Operator dashboards need to read all three to assess retrieval
//! health.  Today that means three separate
//! [`snapshot`](crate::lexicon_telemetry::snapshot)-style calls
//! into three different modules — workable, but the dashboard code
//! has to know which module owns which counter, which inverts the
//! "operator first" surface contract.
//!
//! This module aggregates the three per-lane snapshots into one
//! [`RetrievalMetricsSnapshot`] via a single [`snapshot`] call.
//! It also adds cross-lane rollup helpers
//! ([`RetrievalMetricsSnapshot::total_fts_lane_queries`],
//! [`RetrievalMetricsSnapshot::total_vector_embeddings`],
//! [`RetrievalMetricsSnapshot::total_vector_errors`]) that surface
//! the "single line on a Grafana panel" view of each retrieval
//! lane's volume / error rate without operators needing to know
//! the per-counter field names.
//!
//! ## Atomicity
//!
//! [`snapshot`] reads three independent process-singletons
//! sequentially; the three reads are NOT a single linearisation
//! point.  Under heavy concurrent writes a snapshot may catch the
//! FTS counter post-bump and the vector counter pre-bump for the
//! same logical operation.  This is the same trade-off documented
//! on [`crate::lexicon_telemetry::snapshot`] and
//! [`evidence_store::fts_telemetry::snapshot`] — best-effort
//! observability, not a transactional read.  Dashboards plotting
//! rate-of-change handle the sub-second skew via aggregation
//! windowing; no caller needs the three counters tied to a single
//! timeline edge.
//!
//! ## Wire mirror
//!
//! The FFI mirror lives in `crates/ffi/src/metrics.rs` as
//! `RetrievalMetrics` (the `uniffi::Record` / serde derives there
//! cannot apply here because `observation_engine` does not depend
//! on either FFI runtime).  The mirror's three sub-fields are
//! populated identically to this struct's three fields, and the
//! FFI snapshot already returns both the flat
//! `fts_telemetry` / `lexicon_telemetry` / `vector_telemetry`
//! fields AND the new grouped `retrieval_metrics` view for
//! backwards-compatible dashboard consumption.

use evidence_store::fts_telemetry::{self, FtsTelemetrySnapshot};
use evidence_store::vector_telemetry::{self, VectorTelemetrySnapshot};

use crate::lexicon_telemetry::{self, LexiconTelemetrySnapshot};

/// Wire-flat read-out of all three retrieval-telemetry lanes at
/// the moment of the [`snapshot`] call.
///
/// Each field mirrors the per-lane snapshot type verbatim — extending
/// any of the three upstream snapshot structs automatically extends
/// the corresponding sub-snapshot here, no symmetric edit required.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetrievalMetricsSnapshot {
    /// FTS5-path telemetry (Phase 1.10) — per-lane query / row totals,
    /// recall-lane structural skips, stopword-strip volumes per call
    /// site.  See [`evidence_store::fts_telemetry`] for the per-field
    /// rationale.
    pub fts: FtsTelemetrySnapshot,
    /// Lexicon-path telemetry (Phase 1.10) — per-BCP-47 lexicon hits,
    /// match-strategy fires, Arabic / Hebrew clitic-peel depth
    /// distribution.  See [`crate::lexicon_telemetry`] for the
    /// per-field rationale.
    pub lexicon: LexiconTelemetrySnapshot,
    /// Vector-path telemetry (Phase 1.11) — embedding-call-site
    /// volumes, `evidence_embeddings` cache outcomes, adapter error
    /// variants, `model_tag` rotation-rule violations.  See
    /// [`evidence_store::vector_telemetry`] for the per-field
    /// rationale.
    pub vector: VectorTelemetrySnapshot,
}

impl RetrievalMetricsSnapshot {
    /// Total FTS5 lane query volume across all three lanes
    /// (`unicode61 + cjk_trigram + bigram`).
    ///
    /// Useful as the "lexical query volume" line on dashboards
    /// without needing to know the three per-lane field names.
    /// Note: this counts *lane invocations*, not *user queries* —
    /// a single user query that routes to all three lanes bumps
    /// each lane's counter, so this rollup can exceed the user-query
    /// volume by up to 3×.  See [`evidence_store::fts_telemetry`]
    /// for the per-lane scoping discipline.
    #[must_use]
    pub fn total_fts_lane_queries(&self) -> u64 {
        self.fts
            .unicode61_lane_queries_total
            .saturating_add(self.fts.cjk_trigram_lane_queries_total)
            .saturating_add(self.fts.bigram_lane_queries_total)
    }

    /// Total FTS5 row volume across all three lanes
    /// (`unicode61 + cjk_trigram + bigram`).
    ///
    /// Useful for sizing index pressure across the whole FTS surface.
    /// Same lane-counting caveat as
    /// [`Self::total_fts_lane_queries`].
    #[must_use]
    pub fn total_fts_lane_rows(&self) -> u64 {
        self.fts
            .unicode61_lane_rows_total
            .saturating_add(self.fts.cjk_trigram_lane_rows_total)
            .saturating_add(self.fts.bigram_lane_rows_total)
    }

    /// Total vector-side embedding-compute calls across all three
    /// `EmbedSite` variants (`query + index-write + live-body`).
    ///
    /// Useful for plotting "ONNX runtime work" as a single line.
    /// Excludes cache hits (which avoid the runtime entirely) and
    /// dedup-copy hits (which also avoid the runtime).
    #[must_use]
    pub fn total_vector_embeddings(&self) -> u64 {
        self.vector
            .query_embeddings_total
            .saturating_add(self.vector.index_write_embeddings_total)
            .saturating_add(self.vector.live_body_embeddings_total)
    }

    /// Total vector-side adapter error volume across all three
    /// `EmbeddingError` variants
    /// (`runtime_unavailable + model_load + inference_failure`).
    ///
    /// Useful as the alerting line on dashboards — any persistent
    /// rate signals an adapter / model / runtime regression.
    /// Excludes `EmptyInput` errors (caller-contract violations,
    /// not adapter health — see
    /// [`evidence_store::vector_telemetry::record_embedding_error_from`]).
    #[must_use]
    pub fn total_vector_errors(&self) -> u64 {
        self.vector
            .runtime_unavailable_total
            .saturating_add(self.vector.model_load_errors_total)
            .saturating_add(self.vector.inference_failures_total)
    }

    /// Total embedding-cache lookups across all four outcome
    /// variants (`hit + miss-no-row + miss-dimension + miss-read-error`).
    ///
    /// Useful as the denominator for cache-hit-rate calculations:
    /// `cache_hit_rate = vector.cache_hits_total / total_cache_lookups()`.
    /// Documented invariant: the four counters partition every
    /// reachable cache-lookup outcome (see
    /// [`evidence_store::vector_telemetry`] module doc).
    #[must_use]
    pub fn total_cache_lookups(&self) -> u64 {
        self.vector
            .cache_hits_total
            .saturating_add(self.vector.cache_misses_no_row_total)
            .saturating_add(self.vector.cache_misses_dimension_total)
            .saturating_add(self.vector.cache_misses_read_error_total)
    }
}

/// Return a wire-flat snapshot of every retrieval-telemetry
/// counter across all three lanes (FTS, lexicon, vector).
///
/// Reads the three process-singleton counter blocks sequentially
/// (FTS → lexicon → vector); under heavy concurrent writes the
/// reads may not all observe the same logical timeline edge —
/// see the module-level doc for the rationale.
#[must_use]
pub fn snapshot() -> RetrievalMetricsSnapshot {
    RetrievalMetricsSnapshot {
        fts: fts_telemetry::snapshot(),
        lexicon: lexicon_telemetry::snapshot(),
        vector: vector_telemetry::snapshot(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin per-lane sub-snapshot equality: the unified [`snapshot`]'s
    /// `fts` / `lexicon` / `vector` fields equal the per-lane
    /// snapshots taken at adjacent points in time (modulo the
    /// concurrent-writer skew documented on the module).
    #[test]
    fn snapshot_subfields_match_per_lane_snapshots() {
        let s1_fts = fts_telemetry::snapshot();
        let s1_lex = lexicon_telemetry::snapshot();
        let s1_vec = vector_telemetry::snapshot();
        let unified = snapshot();
        let s2_fts = fts_telemetry::snapshot();
        let s2_lex = lexicon_telemetry::snapshot();
        let s2_vec = vector_telemetry::snapshot();

        // Each unified sub-field must lie between the bracketing
        // per-lane reads (monotonic counters; equal under no
        // concurrent activity).  Lower-bound + upper-bound pattern
        // matches the FFI metrics tests for the same race-free
        // reason documented at `ffi/src/metrics.rs:1403-1419`.
        assert!(
            unified.fts.unicode61_lane_queries_total >= s1_fts.unicode61_lane_queries_total,
            "unified fts.unicode61 must be >= pre-snapshot"
        );
        assert!(
            unified.fts.unicode61_lane_queries_total <= s2_fts.unicode61_lane_queries_total,
            "unified fts.unicode61 must be <= post-snapshot"
        );
        assert!(
            unified.lexicon.hits_en >= s1_lex.hits_en,
            "unified lexicon.hits_en must be >= pre-snapshot"
        );
        assert!(
            unified.lexicon.hits_en <= s2_lex.hits_en,
            "unified lexicon.hits_en must be <= post-snapshot"
        );
        assert!(
            unified.vector.query_embeddings_total >= s1_vec.query_embeddings_total,
            "unified vector.query must be >= pre-snapshot"
        );
        assert!(
            unified.vector.query_embeddings_total <= s2_vec.query_embeddings_total,
            "unified vector.query must be <= post-snapshot"
        );
    }

    /// Pin [`RetrievalMetricsSnapshot::total_fts_lane_queries`]
    /// = sum of the three per-lane query counters, both from a
    /// fresh-default value (all zero) and from a snythetic
    /// non-zero snapshot.
    #[test]
    fn total_fts_lane_queries_matches_sum() {
        let zero = RetrievalMetricsSnapshot::default();
        assert_eq!(zero.total_fts_lane_queries(), 0);

        let synthetic = RetrievalMetricsSnapshot {
            fts: FtsTelemetrySnapshot {
                unicode61_lane_queries_total: 10,
                cjk_trigram_lane_queries_total: 5,
                bigram_lane_queries_total: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(synthetic.total_fts_lane_queries(), 18);
    }

    /// Pin [`RetrievalMetricsSnapshot::total_fts_lane_rows`] = sum
    /// of the three per-lane row counters.
    #[test]
    fn total_fts_lane_rows_matches_sum() {
        let synthetic = RetrievalMetricsSnapshot {
            fts: FtsTelemetrySnapshot {
                unicode61_lane_rows_total: 100,
                cjk_trigram_lane_rows_total: 50,
                bigram_lane_rows_total: 30,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(synthetic.total_fts_lane_rows(), 180);
    }

    /// Pin [`RetrievalMetricsSnapshot::total_vector_embeddings`]
    /// = sum of the three [`EmbedSite`] counters.
    #[test]
    fn total_vector_embeddings_matches_sum() {
        let synthetic = RetrievalMetricsSnapshot {
            vector: VectorTelemetrySnapshot {
                query_embeddings_total: 100,
                index_write_embeddings_total: 50,
                live_body_embeddings_total: 25,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(synthetic.total_vector_embeddings(), 175);
    }

    /// Pin [`RetrievalMetricsSnapshot::total_vector_errors`] = sum
    /// of the three adapter-error counters; `EmptyInput` (caller-
    /// contract violation, no counter) is correctly excluded.
    #[test]
    fn total_vector_errors_matches_sum() {
        let synthetic = RetrievalMetricsSnapshot {
            vector: VectorTelemetrySnapshot {
                runtime_unavailable_total: 7,
                model_load_errors_total: 2,
                inference_failures_total: 4,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(synthetic.total_vector_errors(), 13);
    }

    /// Pin [`RetrievalMetricsSnapshot::total_cache_lookups`] = sum
    /// of all four cache-outcome counters.  Locks the documented
    /// "four counters partition every reachable cache-lookup
    /// outcome" invariant from
    /// [`evidence_store::vector_telemetry`].
    #[test]
    fn total_cache_lookups_matches_sum() {
        let synthetic = RetrievalMetricsSnapshot {
            vector: VectorTelemetrySnapshot {
                cache_hits_total: 80,
                cache_misses_no_row_total: 15,
                cache_misses_dimension_total: 0,
                cache_misses_read_error_total: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(synthetic.total_cache_lookups(), 97);
    }

    /// Pin saturating-addition for the rollup helpers: a snapshot
    /// with `u64::MAX` on one lane must not panic when summed
    /// (saturates at `u64::MAX`).  Defensive — in practice no
    /// real deployment would reach `u64::MAX` on any single
    /// counter, but the rollup helpers should never panic.
    #[test]
    fn rollup_helpers_saturate_rather_than_wrap() {
        let saturated = RetrievalMetricsSnapshot {
            fts: FtsTelemetrySnapshot {
                unicode61_lane_queries_total: u64::MAX,
                cjk_trigram_lane_queries_total: 1,
                bigram_lane_queries_total: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(saturated.total_fts_lane_queries(), u64::MAX);
    }

    /// Default value is all-zero across all three sub-fields.
    /// Pins the wire-default contract (fresh snapshot before any
    /// telemetry activity has zero rollup volumes).
    #[test]
    fn default_snapshot_is_all_zero() {
        let zero = RetrievalMetricsSnapshot::default();
        assert_eq!(zero.total_fts_lane_queries(), 0);
        assert_eq!(zero.total_fts_lane_rows(), 0);
        assert_eq!(zero.total_vector_embeddings(), 0);
        assert_eq!(zero.total_vector_errors(), 0);
        assert_eq!(zero.total_cache_lookups(), 0);
    }
}
