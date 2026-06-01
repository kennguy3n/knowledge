//! Process-singleton observability counters for the multilingual
//! FTS5 path (Phase 1.10).
//!
//! Phases 1.2 – 1.9 split the lexical retrieval pipeline into
//! three FTS5 lanes (`evidence_fts` unicode61, `evidence_fts_cjk`
//! trigram, `evidence_fts_bigram` bigram) with Phase 1.9
//! per-script stopword stripping wrapped around the two recall
//! lanes — but no metrics were ever exposed for which lane
//! produced rows, how often the trigram / bigram lane short-
//! circuited on a pure-stopword query, or how many stopword
//! codepoints actually got stripped at index time vs. query
//! time. This module closes that observability gap.
//!
//! # Counter taxonomy
//!
//! * **Lane invocation totals** — every entry into
//!   [`crate::store::merged_fts_search`]'s unicode61 / trigram /
//!   bigram branch bumps `<lane>_lane_queries_total`. The
//!   trigram branch counts as "invoked" even when the stripped
//!   query was non-empty but produced zero rows; the bigram
//!   branch counts as "invoked" only after
//!   [`crate::bigram::compute_cjk_bigram_query`] returned a
//!   non-empty bigram match string (i.e. the query had at least
//!   two adjacent CJK codepoints to bigram-window).
//!
//! * **Lane row totals** — cumulative row count across all
//!   invocations of each lane (`<lane>_lane_rows_total`). Useful
//!   when divided by the matching `_queries_total` to derive a
//!   "rows per query" precision signal per lane.
//!
//! * **Recall-lane skips** — when the trigram / bigram lane is
//!   *not* invoked because the input was structurally
//!   incompatible (pure-stopword stripped query for trigram,
//!   no CJK content for bigram), a sibling `<lane>_skips_*_total`
//!   counter is bumped instead. The skips counters are NOT
//!   subsumed by the query totals — combining them yields the
//!   true call rate (`queries + skips = total_attempts`).
//!
//! * **Stopword strip totals** — every invocation of
//!   [`crate::fts_stopwords::strip_recall_lane_stopwords_counted`]
//!   bumps the matching `<site>_stopwords_stripped_total` counter
//!   by the number of stopword instances removed. The three
//!   sites are mutually exclusive (index-write,
//!   query-time, v15-to-v16 migration), so summing all three
//!   gives the total strip volume since process boot.
//!
//! # Wire-format stability
//!
//! [`FtsTelemetrySnapshot`] is the wire-flat read-out structure
//! platform hosts deserialize via the FFI. New counters must be
//! added as additional fields with `#[serde(default)]` on the
//! FFI-mirror struct in `crates/ffi/src/metrics.rs` — see the
//! sibling `observation_engine::lexicon_telemetry` module doc
//! for the full wire-evolution rationale.
//!
//! # Performance
//!
//! Same model as `observation_engine::lexicon_telemetry`: one
//! [`AtomicU64::fetch_add`] with [`Ordering::Relaxed`] per
//! decision point. No allocations, no locks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Process-singleton bag of atomic counters.  Internal — callers
/// touch via [`record_lane_query`] / [`record_lane_skip`] /
/// [`record_stopwords_stripped`] and read via [`snapshot`].
///
/// The `_total` field suffix is the Prometheus-canonical naming
/// convention for monotonic counter metrics — preserved through
/// the FFI [`crate::ffi::metrics::FtsTelemetry`] mirror and into
/// any downstream metric exporter.  The clippy
/// `struct_field_names` lint flags the shared postfix, but
/// dropping `_total` would diverge from the convention and
/// confuse exporters expecting `<metric>_total` for counters.
#[derive(Default, Debug)]
#[allow(clippy::struct_field_names)]
pub(crate) struct Counters {
    // ─── Lane invocation totals ─────────────────────────────────
    pub(crate) unicode61_lane_queries_total: AtomicU64,
    pub(crate) cjk_trigram_lane_queries_total: AtomicU64,
    pub(crate) bigram_lane_queries_total: AtomicU64,

    // ─── Lane row totals ────────────────────────────────────────
    pub(crate) unicode61_lane_rows_total: AtomicU64,
    pub(crate) cjk_trigram_lane_rows_total: AtomicU64,
    pub(crate) bigram_lane_rows_total: AtomicU64,

    // ─── Recall-lane skips ──────────────────────────────────────
    pub(crate) cjk_trigram_lane_skips_pure_stopword_query_total: AtomicU64,
    pub(crate) bigram_lane_skips_no_cjk_query_total: AtomicU64,

    // ─── Stopword strip totals (mutually exclusive sites) ──────
    pub(crate) index_write_stopwords_stripped_total: AtomicU64,
    pub(crate) query_time_stopwords_stripped_total: AtomicU64,
    pub(crate) v16_migration_stopwords_stripped_total: AtomicU64,
}

static COUNTERS: OnceLock<Counters> = OnceLock::new();

/// Borrow the process-singleton counter block.  Internal.
#[inline]
fn counters() -> &'static Counters {
    COUNTERS.get_or_init(Counters::default)
}

/// Which FTS5 lane the increment applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// `evidence_fts` (unicode61, with `remove_diacritics 2`).
    Unicode61,
    /// `evidence_fts_cjk` (unicode61 over the trigrammed body).
    CjkTrigram,
    /// `evidence_fts_bigram` (unicode61 over the bigrammed body).
    Bigram,
}

/// Record a lane query invocation that produced (or attempted
/// to produce) rows.  `rows` is the number of rows returned —
/// pass `0` when the query was syntactically valid but returned
/// nothing.  The matching `<lane>_lane_queries_total` AND
/// `<lane>_lane_rows_total` counters are both bumped (the latter
/// by `rows`).
#[inline]
pub fn record_lane_query(lane: Lane, rows: u64) {
    let c = counters();
    let (q, r) = match lane {
        Lane::Unicode61 => (
            &c.unicode61_lane_queries_total,
            &c.unicode61_lane_rows_total,
        ),
        Lane::CjkTrigram => (
            &c.cjk_trigram_lane_queries_total,
            &c.cjk_trigram_lane_rows_total,
        ),
        Lane::Bigram => (&c.bigram_lane_queries_total, &c.bigram_lane_rows_total),
    };
    q.fetch_add(1, Ordering::Relaxed);
    if rows > 0 {
        r.fetch_add(rows, Ordering::Relaxed);
    }
}

/// Reason a recall lane was skipped (not invoked).  Each variant
/// maps to a distinct counter in [`Counters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Trigram lane: stopword stripping collapsed the query to
    /// empty (pure-stopword input), so no MATCH was attempted.
    CjkTrigramPureStopwordQuery,
    /// Bigram lane: the query contained no adjacent-CJK
    /// codepoint pair, so [`crate::bigram::compute_cjk_bigram_query`]
    /// returned `None` and no MATCH was attempted.
    BigramNoCjkQuery,
}

/// Record a recall lane skip — the lane was structurally
/// declined for this query rather than invoked-and-empty.
#[inline]
pub fn record_lane_skip(reason: SkipReason) {
    let c = counters();
    let counter = match reason {
        SkipReason::CjkTrigramPureStopwordQuery => {
            &c.cjk_trigram_lane_skips_pure_stopword_query_total
        }
        SkipReason::BigramNoCjkQuery => &c.bigram_lane_skips_no_cjk_query_total,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Which call site reported the stopword strip.  Each variant
/// maps to a distinct counter (and the three are mutually
/// exclusive — a single body / query never appears in more
/// than one site's count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripSite {
    /// Index-time stripping invoked from
    /// [`crate::store::EvidenceStore::index_fts`] when a body
    /// is written to the trigram / bigram recall lanes.
    IndexWrite,
    /// Query-time stripping invoked from
    /// [`crate::store::merged_fts_search`] before the trigram
    /// / bigram lanes window.
    QueryTime,
    /// Migration-time stripping invoked from the v15 → v16
    /// chunked re-tokenisation backfill (one-shot per database
    /// upgrade).
    V16Migration,
}

/// Record `count` stopword instances stripped at `site`.  No-op
/// when `count == 0` (call-site convenience — caller can pass
/// the raw strip count without branching).
#[inline]
pub fn record_stopwords_stripped(site: StripSite, count: u64) {
    if count == 0 {
        return;
    }
    let c = counters();
    let counter = match site {
        StripSite::IndexWrite => &c.index_write_stopwords_stripped_total,
        StripSite::QueryTime => &c.query_time_stopwords_stripped_total,
        StripSite::V16Migration => &c.v16_migration_stopwords_stripped_total,
    };
    counter.fetch_add(count, Ordering::Relaxed);
}

/// Wire-flat read-out of every counter at the moment of the
/// [`snapshot`] call.
///
/// `_total` suffix is preserved on every field for the same
/// Prometheus-convention reason documented on [`Counters`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct FtsTelemetrySnapshot {
    /// Times the unicode61 lane (`evidence_fts`) was invoked
    /// with a non-empty query in
    /// [`crate::store::merged_fts_search`].  Every call counts
    /// even when zero rows were returned.
    pub unicode61_lane_queries_total: u64,
    /// Cumulative row count across all
    /// [`unicode61_lane_queries_total`](Self::unicode61_lane_queries_total)
    /// invocations.
    pub unicode61_lane_rows_total: u64,
    /// Times the CJK trigram lane (`evidence_fts_cjk`) was
    /// invoked with a non-empty stripped query.
    pub cjk_trigram_lane_queries_total: u64,
    /// Cumulative row count across all
    /// [`cjk_trigram_lane_queries_total`](Self::cjk_trigram_lane_queries_total)
    /// invocations.
    pub cjk_trigram_lane_rows_total: u64,
    /// Times the CJK trigram lane was skipped because the
    /// stopword stripping collapsed the query to empty.
    pub cjk_trigram_lane_skips_pure_stopword_query_total: u64,
    /// Times the CJK bigram lane (`evidence_fts_bigram`) was
    /// invoked with a non-empty bigram match string.
    pub bigram_lane_queries_total: u64,
    /// Cumulative row count across all
    /// [`bigram_lane_queries_total`](Self::bigram_lane_queries_total)
    /// invocations.
    pub bigram_lane_rows_total: u64,
    /// Times the CJK bigram lane was skipped because the query
    /// contained no adjacent-CJK codepoint pair.
    pub bigram_lane_skips_no_cjk_query_total: u64,
    /// Cumulative count of stopword instances stripped at
    /// index-write time (i.e. when a body is written into the
    /// trigram / bigram recall lanes via
    /// [`crate::store::EvidenceStore::index_fts`]).
    pub index_write_stopwords_stripped_total: u64,
    /// Cumulative count of stopword instances stripped at
    /// query time (i.e. before the trigram / bigram recall
    /// lanes window in
    /// [`crate::store::merged_fts_search`]).
    pub query_time_stopwords_stripped_total: u64,
    /// Cumulative count of stopword instances stripped during
    /// the v15 → v16 chunked re-tokenisation migration.
    /// One-shot per database upgrade — should remain 0 after
    /// the first reopen.
    pub v16_migration_stopwords_stripped_total: u64,
}

/// Return a wire-flat snapshot of every FTS-telemetry counter.
#[must_use]
pub fn snapshot() -> FtsTelemetrySnapshot {
    let c = counters();
    FtsTelemetrySnapshot {
        unicode61_lane_queries_total: c.unicode61_lane_queries_total.load(Ordering::Relaxed),
        unicode61_lane_rows_total: c.unicode61_lane_rows_total.load(Ordering::Relaxed),
        cjk_trigram_lane_queries_total: c.cjk_trigram_lane_queries_total.load(Ordering::Relaxed),
        cjk_trigram_lane_rows_total: c.cjk_trigram_lane_rows_total.load(Ordering::Relaxed),
        cjk_trigram_lane_skips_pure_stopword_query_total: c
            .cjk_trigram_lane_skips_pure_stopword_query_total
            .load(Ordering::Relaxed),
        bigram_lane_queries_total: c.bigram_lane_queries_total.load(Ordering::Relaxed),
        bigram_lane_rows_total: c.bigram_lane_rows_total.load(Ordering::Relaxed),
        bigram_lane_skips_no_cjk_query_total: c
            .bigram_lane_skips_no_cjk_query_total
            .load(Ordering::Relaxed),
        index_write_stopwords_stripped_total: c
            .index_write_stopwords_stripped_total
            .load(Ordering::Relaxed),
        query_time_stopwords_stripped_total: c
            .query_time_stopwords_stripped_total
            .load(Ordering::Relaxed),
        v16_migration_stopwords_stripped_total: c
            .v16_migration_stopwords_stripped_total
            .load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin lane query + row aggregation: a single
    /// `record_lane_query(Unicode61, 5)` bumps both the
    /// `_queries_total` (by 1) and the `_rows_total` (by 5).
    #[test]
    fn record_lane_query_bumps_queries_and_rows() {
        let before = snapshot();
        record_lane_query(Lane::Unicode61, 5);
        let after = snapshot();
        assert_eq!(
            after.unicode61_lane_queries_total,
            before.unicode61_lane_queries_total + 1
        );
        assert_eq!(
            after.unicode61_lane_rows_total,
            before.unicode61_lane_rows_total + 5
        );
    }

    /// Pin the zero-rows case: a `record_lane_query(Trigram, 0)`
    /// still bumps `_queries_total` (the lane was invoked) but
    /// leaves `_rows_total` unchanged.
    #[test]
    fn record_lane_query_zero_rows_bumps_queries_only() {
        let before = snapshot();
        record_lane_query(Lane::CjkTrigram, 0);
        let after = snapshot();
        assert_eq!(
            after.cjk_trigram_lane_queries_total,
            before.cjk_trigram_lane_queries_total + 1
        );
        assert_eq!(
            after.cjk_trigram_lane_rows_total, before.cjk_trigram_lane_rows_total,
            "zero-rows query must NOT bump _rows_total"
        );
    }

    /// Pin lane independence: a query on the bigram lane does
    /// NOT bump the trigram lane's counters (and vice versa).
    #[test]
    fn lanes_are_independent() {
        let before = snapshot();
        record_lane_query(Lane::Bigram, 3);
        let after = snapshot();
        assert_eq!(
            after.bigram_lane_queries_total,
            before.bigram_lane_queries_total + 1
        );
        assert_eq!(
            after.bigram_lane_rows_total,
            before.bigram_lane_rows_total + 3
        );
        assert_eq!(
            after.unicode61_lane_queries_total, before.unicode61_lane_queries_total,
            "Bigram query must NOT bump Unicode61 counters"
        );
        assert_eq!(
            after.cjk_trigram_lane_queries_total, before.cjk_trigram_lane_queries_total,
            "Bigram query must NOT bump CjkTrigram counters"
        );
    }

    /// Pin skip → counter mapping.
    #[test]
    fn lane_skips_increment_distinct_counters() {
        let before = snapshot();
        record_lane_skip(SkipReason::CjkTrigramPureStopwordQuery);
        record_lane_skip(SkipReason::BigramNoCjkQuery);
        let after = snapshot();
        assert_eq!(
            after.cjk_trigram_lane_skips_pure_stopword_query_total,
            before.cjk_trigram_lane_skips_pure_stopword_query_total + 1
        );
        assert_eq!(
            after.bigram_lane_skips_no_cjk_query_total,
            before.bigram_lane_skips_no_cjk_query_total + 1
        );
    }

    /// Pin stopword strip counter aggregation: count is added,
    /// not just bumped by 1.
    #[test]
    fn record_stopwords_stripped_adds_count() {
        let before = snapshot();
        record_stopwords_stripped(StripSite::IndexWrite, 7);
        record_stopwords_stripped(StripSite::QueryTime, 2);
        record_stopwords_stripped(StripSite::V16Migration, 100);
        let after = snapshot();
        assert_eq!(
            after.index_write_stopwords_stripped_total,
            before.index_write_stopwords_stripped_total + 7
        );
        assert_eq!(
            after.query_time_stopwords_stripped_total,
            before.query_time_stopwords_stripped_total + 2
        );
        assert_eq!(
            after.v16_migration_stopwords_stripped_total,
            before.v16_migration_stopwords_stripped_total + 100
        );
    }

    /// Pin the zero-count short-circuit: a strip with `count == 0`
    /// is a no-op on the counter (no atomic add at all).
    #[test]
    fn zero_count_strip_is_noop() {
        let before = snapshot();
        record_stopwords_stripped(StripSite::IndexWrite, 0);
        record_stopwords_stripped(StripSite::QueryTime, 0);
        record_stopwords_stripped(StripSite::V16Migration, 0);
        let after = snapshot();
        assert_eq!(after, before, "zero-count strips must not bump any counter");
    }
}
