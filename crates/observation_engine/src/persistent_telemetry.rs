//! Phase 2.2 — Persistent telemetry snapshot.
//!
//! The three sibling telemetry modules
//! ([`evidence_store::fts_telemetry`], [`crate::lexicon_telemetry`],
//! [`evidence_store::vector_telemetry`]) and the unified
//! [`crate::retrieval_telemetry::snapshot`] read surface (Phase 2.0)
//! all expose **process-singleton** counters: they live in
//! [`std::sync::atomic`] statics for the lifetime of the running
//! process and reset to zero on restart.
//!
//! Operator dashboards and post-incident review tooling want to see
//! counter values **across** process boundaries — both for "the
//! deployment restarted at 14:32, what were the last counter values
//! before the restart" and for the lower-resolution "what's the
//! 5-minute moving average of vector errors" question.  This module
//! closes that gap by adding a file-backed persistence layer:
//!
//! * [`capture`] reads the current
//!   [`crate::retrieval_telemetry::snapshot`] and wraps it in a
//!   [`PersistentRetrievalSnapshot`] envelope with a schema version
//!   and a Unix-millisecond capture timestamp.
//! * [`write_snapshot`] captures + atomically writes the envelope to
//!   a path (`pretty-printed JSON` for human cat-ability), so a
//!   periodic host-side scheduler (e.g. a `tokio::time::interval`
//!   loop in the platform layer) can roll the file over without
//!   risk of half-written reads.
//! * [`read_snapshot`] reads an envelope back from disk with schema-
//!   version validation, so a future version-bump that changes the
//!   envelope shape is surfaced as a typed error rather than a
//!   silent field-drop.
//! * [`delta`] computes the per-counter delta between two envelopes
//!   using **saturating subtraction**, so the delta is well-defined
//!   even when `latest < prior` — which happens whenever the prior
//!   snapshot was captured before a process restart and the latest
//!   was captured after (counters reset to zero across restarts,
//!   so the "delta" in that case is simply `latest` itself, but
//!   reported as `0` rather than a wrap-around).
//!
//! ## Why no spawned scheduling task in this module
//!
//! Scheduling the periodic write belongs to the platform host
//! (iOS / Android / desktop) which already owns the long-lived
//! async runtime.  Spawning a `tokio::task` here would couple
//! `observation_engine` to a specific runtime flavour and force
//! every consumer (including the FFI smoke tests) to provide one.
//! The platform-side scheduling is a 5-line `tokio::time::interval`
//! loop that calls [`write_snapshot`] each tick — see the FFI
//! integration test for an exact-pattern example.
//!
//! ## Why no counter restoration on read
//!
//! [`read_snapshot`] returns the envelope verbatim — it does NOT
//! re-seed the process-singleton counters.  Restoring counters
//! across processes would conflate two distinct process lifetimes
//! into one number, which would break the "monotonically increasing
//! since process start" invariant every Prometheus-shape counter
//! relies on.  Operators reasoning about "all-time totals across
//! restarts" do that arithmetic in the dashboard layer (sum the
//! per-process deltas), not inside the substrate.
//!
//! ## Schema versioning
//!
//! [`PersistentRetrievalSnapshot::SCHEMA_VERSION`] is a hard-coded
//! constant.  Any *incompatible* change to the envelope shape (e.g.
//! splitting a field, changing a type, renaming the outer key) bumps
//! this constant and adds an explicit migration branch in
//! [`read_snapshot`].  **Additive** changes — new optional fields,
//! new counters in the upstream sub-snapshots — do NOT require a
//! version bump because [`crate::retrieval_telemetry::
//! RetrievalMetricsSnapshot`] is derived with `#[serde(default)]`
//! on every field, so older emitter JSON missing a new field
//! deserialises to `Default::default()` for that field (the
//! "additive forward-compat" pattern from the FFI layer).
//!
//! ## Atomic write discipline
//!
//! [`write_snapshot`] writes to a sibling tempfile in the *same
//! directory* as `path` and `rename`s on success.  Same-directory
//! placement is required because POSIX `rename(2)` is only
//! guaranteed atomic on the same filesystem — a `/tmp`-staged
//! tempfile would silently fall back to copy-and-delete and lose
//! atomicity if `/tmp` is a separate mount.  On error, the tempfile
//! is cleaned up by [`tempfile::NamedTempFile`]'s `Drop` impl, so
//! a write failure leaves no orphaned `.tmp` files behind.

use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::retrieval_telemetry::{self, RetrievalMetricsSnapshot};

/// Versioned wrapper around [`RetrievalMetricsSnapshot`] for on-disk
/// persistence.  The envelope tags every persisted snapshot with a
/// schema version and the Unix-millisecond capture timestamp so the
/// reader can (a) refuse to deserialise an incompatible schema
/// without silently dropping fields and (b) compute time-windowed
/// rates across multiple snapshots.
///
/// ### Wire-format stability
///
/// `schema_version` is the **only** field whose semantics are
/// fixed at this layer; the inner [`RetrievalMetricsSnapshot`]
/// gains fields whenever new counters land in any of the three
/// upstream telemetry modules, and the additive-forward-compat
/// rule there (`#[serde(default)]` on every field) lets older
/// emitter JSON keep deserialising cleanly without a version
/// bump — see the module doc for the discipline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentRetrievalSnapshot {
    /// On-disk schema version for the envelope itself.  Bumped
    /// only when the envelope shape changes in an incompatible
    /// way (e.g. splitting a field, renaming the outer key);
    /// additive field additions to
    /// [`RetrievalMetricsSnapshot`] do NOT bump this counter
    /// because [`RetrievalMetricsSnapshot`] is derived with
    /// `#[serde(default)]` on every field for the additive-
    /// forward-compat rule.
    pub schema_version: u32,
    /// Unix-millisecond timestamp of when this envelope was
    /// captured.  Set from [`SystemTime::now`] in [`capture`];
    /// callers reading prior envelopes for delta-rate
    /// computation should use the *delta* between two
    /// `captured_at_unix_ms` values as the denominator (rather
    /// than wall-clock now, which would include scheduling
    /// latency).
    pub captured_at_unix_ms: u64,
    /// The wire-flat read-out of every retrieval-telemetry
    /// counter at capture time.  See
    /// [`crate::retrieval_telemetry::snapshot`] for the per-lane
    /// rationale.
    pub retrieval_metrics: RetrievalMetricsSnapshot,
}

impl PersistentRetrievalSnapshot {
    /// Current on-disk schema version for this envelope.
    ///
    /// **Bump only on incompatible shape changes** — adding new
    /// counters to any of the three upstream sub-snapshots does
    /// NOT require a bump (the `#[serde(default)]` derive on
    /// [`RetrievalMetricsSnapshot`] handles forward compat
    /// additively).  Bumping examples that DO require a
    /// migration branch: splitting `retrieval_metrics` into
    /// per-lane top-level keys, changing `captured_at_unix_ms`
    /// to a string, dropping a field that older readers
    /// require.
    pub const SCHEMA_VERSION: u32 = 1;
}

/// Errors surfaced by [`read_snapshot`] / [`write_snapshot`].
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// Filesystem-level error (open, read, write, rename, etc.).
    #[error("filesystem error reading or writing telemetry snapshot: {0}")]
    Io(#[from] io::Error),
    /// JSON serialisation / deserialisation error.
    #[error("JSON parse error in telemetry snapshot: {0}")]
    Json(#[from] serde_json::Error),
    /// On-disk envelope's `schema_version` is incompatible with
    /// the current [`PersistentRetrievalSnapshot::SCHEMA_VERSION`].
    /// The on-disk envelope is parsed (so the reader can inspect
    /// the captured timestamp + extract whatever fields are still
    /// compatible) but treated as a hard error to force an
    /// explicit migration branch rather than a silent field-drop.
    #[error("telemetry snapshot schema version mismatch: expected {expected}, found {found}")]
    SchemaVersionMismatch {
        /// The schema version expected by this build.
        expected: u32,
        /// The schema version found in the on-disk envelope.
        found: u32,
    },
}

/// Capture the current retrieval-telemetry snapshot and wrap it in
/// a [`PersistentRetrievalSnapshot`] envelope tagged with the
/// current Unix-millisecond timestamp.
///
/// Does NOT write to disk — use [`write_snapshot`] for the
/// disk-backed persistence path.  This split lets in-memory
/// consumers (e.g. an FFI getter that returns the latest snapshot
/// without touching disk) reuse the envelope construction without
/// paying the file-IO cost.
#[must_use]
pub fn capture() -> PersistentRetrievalSnapshot {
    let captured_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // Clock skew can in principle make `now < UNIX_EPOCH`
        // (e.g. on first-boot devices before NTP sync).  Saturate
        // to zero rather than panic so the envelope is always
        // well-formed; an obvious `0` timestamp is easier for
        // operators to spot than a panic in the snapshot loop.
        .map_or(0, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        });
    PersistentRetrievalSnapshot {
        schema_version: PersistentRetrievalSnapshot::SCHEMA_VERSION,
        captured_at_unix_ms,
        retrieval_metrics: retrieval_telemetry::snapshot(),
    }
}

/// Capture + atomically write a [`PersistentRetrievalSnapshot`] to
/// `path` as pretty-printed JSON.
///
/// **Atomicity** — writes to a sibling tempfile in the same
/// directory as `path` and `rename`s on success, so a reader
/// concurrent with the write will see either the prior file
/// contents or the new contents, never a half-written file.
/// Same-directory placement is required because POSIX `rename(2)`
/// is only atomic on the same filesystem.
///
/// **Cleanup on failure** — [`tempfile::NamedTempFile`]'s `Drop`
/// impl deletes the staging file if the rename fails (or if any
/// earlier step fails), so a partial write leaves no orphaned
/// `.tmp` siblings behind.
///
/// Returns the envelope that was just persisted, so callers can
/// log the timestamp / forward the in-memory copy to a dashboard
/// emitter without re-reading the file they just wrote.
///
/// # Errors
///
/// * [`PersistError::Io`] — failed to create the tempfile, write
///   bytes, persist (rename), or sync the directory.  Returned
///   verbatim from the underlying `std::io` operation so callers
///   can match on `ErrorKind`.
/// * [`PersistError::Json`] — failed to serialise the envelope
///   (should be impossible given the snapshot is plain `u64`
///   counters, but propagated for completeness).
pub fn write_snapshot(path: &Path) -> Result<PersistentRetrievalSnapshot, PersistError> {
    let envelope = capture();
    write_envelope(path, &envelope)?;
    Ok(envelope)
}

/// Variant of [`write_snapshot`] that writes a *pre-captured*
/// envelope rather than capturing fresh.  Useful for tests that
/// want to pin a deterministic envelope on disk, and for callers
/// that want to capture once and write to multiple paths.
///
/// # Errors
///
/// Same as [`write_snapshot`].
pub fn write_envelope(
    path: &Path,
    envelope: &PersistentRetrievalSnapshot,
) -> Result<(), PersistError> {
    // Serialise first so a JSON failure doesn't leave behind a
    // partially-written file.  Pretty-printed for human
    // cat-ability — the disk-cost delta over compact JSON is a
    // few KB per snapshot, well worth the operator ergonomics.
    let bytes = serde_json::to_vec_pretty(envelope)?;

    // Tempfile in the same directory as `path` so `persist`
    // (rename) is atomic on the same filesystem.  If `path` has
    // no parent (e.g. a bare filename in the cwd), default to
    // the current directory.
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let mut staging = match parent {
        Some(dir) => tempfile::NamedTempFile::new_in(dir)?,
        None => tempfile::NamedTempFile::new_in(".")?,
    };
    staging.write_all(&bytes)?;
    // `flush` before `persist` so the rename observes a committed
    // file rather than relying on the OS to flush during rename.
    staging.flush()?;
    // `persist` performs the atomic rename + drops the tempfile
    // handle; on `Err(PersistError)` (file-already-exists on
    // Windows etc.) we destructure the error and return its inner
    // `io::Error`, which is what the caller actually needs to
    // see.  On success the tempfile's `Drop` is short-circuited
    // and the target file is the persisted one.
    staging
        .persist(path)
        .map_err(|e| PersistError::Io(e.error))?;
    Ok(())
}

/// Read a [`PersistentRetrievalSnapshot`] from `path`, validating
/// the schema version.
///
/// # Errors
///
/// * [`PersistError::Io`] — failed to open or read the file.
/// * [`PersistError::Json`] — file contents are not valid JSON,
///   or the JSON doesn't match the envelope shape.
/// * [`PersistError::SchemaVersionMismatch`] — the on-disk
///   envelope's `schema_version` does not equal
///   [`PersistentRetrievalSnapshot::SCHEMA_VERSION`].
///
/// On schema version mismatch the returned error includes both the
/// expected and the found version, so the caller can decide whether
/// to migrate the on-disk file or fall back to a fresh capture.
pub fn read_snapshot(path: &Path) -> Result<PersistentRetrievalSnapshot, PersistError> {
    let bytes = std::fs::read(path)?;
    let envelope: PersistentRetrievalSnapshot = serde_json::from_slice(&bytes)?;
    if envelope.schema_version != PersistentRetrievalSnapshot::SCHEMA_VERSION {
        return Err(PersistError::SchemaVersionMismatch {
            expected: PersistentRetrievalSnapshot::SCHEMA_VERSION,
            found: envelope.schema_version,
        });
    }
    Ok(envelope)
}

/// Per-counter delta between two [`PersistentRetrievalSnapshot`]
/// envelopes, with saturating subtraction.
///
/// Same shape as [`RetrievalMetricsSnapshot`] but represents the
/// *change* in each counter between `prior` and `latest`.  Useful
/// for computing rate-of-change ("how many vector errors per
/// second over the last 5 minutes") in the dashboard layer.
///
/// ### Saturation discipline
///
/// Every per-field subtraction uses [`u64::saturating_sub`], so
/// `latest < prior` (counters lower in the newer snapshot than
/// the older one — happens on a process restart, since the
/// process-singleton atomics reset to zero) produces a delta of
/// `0` rather than a `u64` wrap-around.  This is the only safe
/// shape for cross-process delta computation: a wrap-around would
/// surface a `~u64::MAX` spike on the rate dashboard at every
/// restart, which is exactly the false alert we're trying to
/// avoid.
///
/// Operators reasoning about "true cumulative across restarts"
/// sum the per-process deltas — i.e. each snapshot pair's delta
/// captures the work done by one process lifetime, and the sum
/// is the work done across all observed lifetimes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalSnapshotDelta {
    /// Unix-millisecond duration between
    /// `prior.captured_at_unix_ms` and `latest.captured_at_unix_ms`
    /// (saturating-sub, so equal or out-of-order timestamps
    /// produce `0`).  Useful as the denominator for rate
    /// calculations done in the dashboard layer.
    pub elapsed_ms: u64,
    /// Per-counter saturating delta of the retrieval metrics
    /// across the two envelopes.  Same wire-flat shape as
    /// [`RetrievalMetricsSnapshot`] so dashboards reading deltas
    /// can use the same field-access path as ones reading
    /// absolute snapshots.
    pub retrieval_metrics: RetrievalMetricsSnapshot,
}

/// Compute the [`RetrievalSnapshotDelta`] between two envelopes.
///
/// See [`RetrievalSnapshotDelta`] for the saturation discipline
/// and the cross-process restart semantics.  This function is
/// pure and never panics — every arithmetic step uses saturating
/// variants — so it is safe to call from any context.
#[must_use]
pub fn delta(
    prior: &PersistentRetrievalSnapshot,
    latest: &PersistentRetrievalSnapshot,
) -> RetrievalSnapshotDelta {
    let elapsed_ms = latest
        .captured_at_unix_ms
        .saturating_sub(prior.captured_at_unix_ms);
    let retrieval_metrics =
        retrieval_metrics_saturating_sub(&latest.retrieval_metrics, &prior.retrieval_metrics);
    RetrievalSnapshotDelta {
        elapsed_ms,
        retrieval_metrics,
    }
}

/// Per-counter saturating subtraction across the unified
/// [`RetrievalMetricsSnapshot`] shape.  Pulled into a helper so
/// the per-lane sub-functions stay in one place — adding a new
/// counter to any of the three upstream snapshots requires
/// extending the corresponding sub-function below, no need to
/// touch [`delta`] directly.
fn retrieval_metrics_saturating_sub(
    latest: &RetrievalMetricsSnapshot,
    prior: &RetrievalMetricsSnapshot,
) -> RetrievalMetricsSnapshot {
    RetrievalMetricsSnapshot {
        fts: fts_saturating_sub(&latest.fts, &prior.fts),
        lexicon: lexicon_saturating_sub(&latest.lexicon, &prior.lexicon),
        vector: vector_saturating_sub(&latest.vector, &prior.vector),
    }
}

fn fts_saturating_sub(
    latest: &evidence_store::fts_telemetry::FtsTelemetrySnapshot,
    prior: &evidence_store::fts_telemetry::FtsTelemetrySnapshot,
) -> evidence_store::fts_telemetry::FtsTelemetrySnapshot {
    use evidence_store::fts_telemetry::FtsTelemetrySnapshot;
    FtsTelemetrySnapshot {
        unicode61_lane_queries_total: latest
            .unicode61_lane_queries_total
            .saturating_sub(prior.unicode61_lane_queries_total),
        unicode61_lane_rows_total: latest
            .unicode61_lane_rows_total
            .saturating_sub(prior.unicode61_lane_rows_total),
        cjk_trigram_lane_queries_total: latest
            .cjk_trigram_lane_queries_total
            .saturating_sub(prior.cjk_trigram_lane_queries_total),
        cjk_trigram_lane_rows_total: latest
            .cjk_trigram_lane_rows_total
            .saturating_sub(prior.cjk_trigram_lane_rows_total),
        cjk_trigram_lane_skips_pure_stopword_query_total: latest
            .cjk_trigram_lane_skips_pure_stopword_query_total
            .saturating_sub(prior.cjk_trigram_lane_skips_pure_stopword_query_total),
        bigram_lane_queries_total: latest
            .bigram_lane_queries_total
            .saturating_sub(prior.bigram_lane_queries_total),
        bigram_lane_rows_total: latest
            .bigram_lane_rows_total
            .saturating_sub(prior.bigram_lane_rows_total),
        bigram_lane_skips_pure_stopword_query_total: latest
            .bigram_lane_skips_pure_stopword_query_total
            .saturating_sub(prior.bigram_lane_skips_pure_stopword_query_total),
        bigram_lane_skips_no_cjk_query_total: latest
            .bigram_lane_skips_no_cjk_query_total
            .saturating_sub(prior.bigram_lane_skips_no_cjk_query_total),
        index_write_stopwords_stripped_total: latest
            .index_write_stopwords_stripped_total
            .saturating_sub(prior.index_write_stopwords_stripped_total),
        query_time_stopwords_stripped_total: latest
            .query_time_stopwords_stripped_total
            .saturating_sub(prior.query_time_stopwords_stripped_total),
        v16_migration_stopwords_stripped_total: latest
            .v16_migration_stopwords_stripped_total
            .saturating_sub(prior.v16_migration_stopwords_stripped_total),
    }
}

fn lexicon_saturating_sub(
    latest: &crate::lexicon_telemetry::LexiconTelemetrySnapshot,
    prior: &crate::lexicon_telemetry::LexiconTelemetrySnapshot,
) -> crate::lexicon_telemetry::LexiconTelemetrySnapshot {
    use crate::lexicon_telemetry::LexiconTelemetrySnapshot;
    LexiconTelemetrySnapshot {
        hits_ar: latest.hits_ar.saturating_sub(prior.hits_ar),
        hits_bo: latest.hits_bo.saturating_sub(prior.hits_bo),
        hits_de: latest.hits_de.saturating_sub(prior.hits_de),
        hits_en: latest.hits_en.saturating_sub(prior.hits_en),
        hits_es: latest.hits_es.saturating_sub(prior.hits_es),
        hits_fr: latest.hits_fr.saturating_sub(prior.hits_fr),
        hits_he: latest.hits_he.saturating_sub(prior.hits_he),
        hits_hi: latest.hits_hi.saturating_sub(prior.hits_hi),
        hits_id: latest.hits_id.saturating_sub(prior.hits_id),
        hits_it: latest.hits_it.saturating_sub(prior.hits_it),
        hits_ja: latest.hits_ja.saturating_sub(prior.hits_ja),
        hits_km: latest.hits_km.saturating_sub(prior.hits_km),
        hits_ko: latest.hits_ko.saturating_sub(prior.hits_ko),
        hits_lo: latest.hits_lo.saturating_sub(prior.hits_lo),
        hits_ms: latest.hits_ms.saturating_sub(prior.hits_ms),
        hits_my: latest.hits_my.saturating_sub(prior.hits_my),
        hits_pt: latest.hits_pt.saturating_sub(prior.hits_pt),
        hits_ru: latest.hits_ru.saturating_sub(prior.hits_ru),
        hits_th: latest.hits_th.saturating_sub(prior.hits_th),
        hits_vi: latest.hits_vi.saturating_sub(prior.hits_vi),
        hits_zh: latest.hits_zh.saturating_sub(prior.hits_zh),
        unknown_tag_fallbacks_total: latest
            .unknown_tag_fallbacks_total
            .saturating_sub(prior.unknown_tag_fallbacks_total),
        strategy_first_token: latest
            .strategy_first_token
            .saturating_sub(prior.strategy_first_token),
        strategy_first_bigram: latest
            .strategy_first_bigram
            .saturating_sub(prior.strategy_first_bigram),
        strategy_substring: latest
            .strategy_substring
            .saturating_sub(prior.strategy_substring),
        strategy_first_token_with_arabic_clitics: latest
            .strategy_first_token_with_arabic_clitics
            .saturating_sub(prior.strategy_first_token_with_arabic_clitics),
        strategy_first_token_with_hebrew_clitics: latest
            .strategy_first_token_with_hebrew_clitics
            .saturating_sub(prior.strategy_first_token_with_hebrew_clitics),
        arabic_peel_depth_0_matches: latest
            .arabic_peel_depth_0_matches
            .saturating_sub(prior.arabic_peel_depth_0_matches),
        arabic_peel_depth_1_matches: latest
            .arabic_peel_depth_1_matches
            .saturating_sub(prior.arabic_peel_depth_1_matches),
        arabic_peel_depth_2_matches: latest
            .arabic_peel_depth_2_matches
            .saturating_sub(prior.arabic_peel_depth_2_matches),
        arabic_peel_depth_3_matches: latest
            .arabic_peel_depth_3_matches
            .saturating_sub(prior.arabic_peel_depth_3_matches),
        arabic_peel_depth_exhausted: latest
            .arabic_peel_depth_exhausted
            .saturating_sub(prior.arabic_peel_depth_exhausted),
        hebrew_peel_depth_0_matches: latest
            .hebrew_peel_depth_0_matches
            .saturating_sub(prior.hebrew_peel_depth_0_matches),
        hebrew_peel_depth_1_matches: latest
            .hebrew_peel_depth_1_matches
            .saturating_sub(prior.hebrew_peel_depth_1_matches),
        hebrew_peel_depth_2_matches: latest
            .hebrew_peel_depth_2_matches
            .saturating_sub(prior.hebrew_peel_depth_2_matches),
        hebrew_peel_depth_3_matches: latest
            .hebrew_peel_depth_3_matches
            .saturating_sub(prior.hebrew_peel_depth_3_matches),
        hebrew_peel_depth_exhausted: latest
            .hebrew_peel_depth_exhausted
            .saturating_sub(prior.hebrew_peel_depth_exhausted),
    }
}

fn vector_saturating_sub(
    latest: &evidence_store::vector_telemetry::VectorTelemetrySnapshot,
    prior: &evidence_store::vector_telemetry::VectorTelemetrySnapshot,
) -> evidence_store::vector_telemetry::VectorTelemetrySnapshot {
    use evidence_store::vector_telemetry::VectorTelemetrySnapshot;
    VectorTelemetrySnapshot {
        query_embeddings_total: latest
            .query_embeddings_total
            .saturating_sub(prior.query_embeddings_total),
        index_write_embeddings_total: latest
            .index_write_embeddings_total
            .saturating_sub(prior.index_write_embeddings_total),
        live_body_embeddings_total: latest
            .live_body_embeddings_total
            .saturating_sub(prior.live_body_embeddings_total),
        cache_hits_total: latest
            .cache_hits_total
            .saturating_sub(prior.cache_hits_total),
        cache_misses_no_row_total: latest
            .cache_misses_no_row_total
            .saturating_sub(prior.cache_misses_no_row_total),
        cache_misses_dimension_total: latest
            .cache_misses_dimension_total
            .saturating_sub(prior.cache_misses_dimension_total),
        cache_misses_read_error_total: latest
            .cache_misses_read_error_total
            .saturating_sub(prior.cache_misses_read_error_total),
        dedup_copy_hits_total: latest
            .dedup_copy_hits_total
            .saturating_sub(prior.dedup_copy_hits_total),
        runtime_unavailable_total: latest
            .runtime_unavailable_total
            .saturating_sub(prior.runtime_unavailable_total),
        model_load_errors_total: latest
            .model_load_errors_total
            .saturating_sub(prior.model_load_errors_total),
        inference_failures_total: latest
            .inference_failures_total
            .saturating_sub(prior.inference_failures_total),
        model_tag_dimension_violations_total: latest
            .model_tag_dimension_violations_total
            .saturating_sub(prior.model_tag_dimension_violations_total),
        pre_embed_admitted_total: latest
            .pre_embed_admitted_total
            .saturating_sub(prior.pre_embed_admitted_total),
        pre_embed_skipped_empty_after_trim_total: latest
            .pre_embed_skipped_empty_after_trim_total
            .saturating_sub(prior.pre_embed_skipped_empty_after_trim_total),
        pre_embed_skipped_no_linguistic_content_total: latest
            .pre_embed_skipped_no_linguistic_content_total
            .saturating_sub(prior.pre_embed_skipped_no_linguistic_content_total),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `capture` always tags the envelope with the current
    /// schema version constant.
    #[test]
    fn capture_uses_current_schema_version() {
        let envelope = capture();
        assert_eq!(
            envelope.schema_version,
            PersistentRetrievalSnapshot::SCHEMA_VERSION
        );
    }

    /// `delta` saturates to zero when `latest < prior` (cross-
    /// process restart shape).  Without saturation a `u64`
    /// wrap-around would surface a `~u64::MAX` spike on every
    /// restart, which is exactly the false alert we're trying to
    /// avoid.
    #[test]
    fn delta_saturates_on_counter_reset_across_restart() {
        let mut prior = capture();
        // Pretend the prior process had bumped a counter.
        prior.retrieval_metrics.fts.unicode61_lane_queries_total = 1_000;
        prior.retrieval_metrics.vector.query_embeddings_total = 42;
        prior.retrieval_metrics.lexicon.hits_en = 5;
        prior.captured_at_unix_ms = 1_000;

        // Latest is a fresh capture in a new process (all
        // counters reset to zero) at a later timestamp.
        let mut latest = capture();
        latest.retrieval_metrics = RetrievalMetricsSnapshot::default();
        latest.captured_at_unix_ms = 2_000;

        let d = delta(&prior, &latest);
        assert_eq!(d.elapsed_ms, 1_000);
        // Each saturating-sub must produce 0, not a wrap-around.
        assert_eq!(d.retrieval_metrics.fts.unicode61_lane_queries_total, 0);
        assert_eq!(d.retrieval_metrics.vector.query_embeddings_total, 0);
        assert_eq!(d.retrieval_metrics.lexicon.hits_en, 0);
    }

    /// `delta` computes the obvious diff when `latest >= prior`.
    #[test]
    fn delta_computes_per_counter_diff_in_typical_case() {
        let mut prior = capture();
        prior.retrieval_metrics.fts.unicode61_lane_queries_total = 100;
        prior.retrieval_metrics.vector.query_embeddings_total = 7;
        prior.retrieval_metrics.lexicon.hits_ja = 3;
        prior.captured_at_unix_ms = 10_000;

        let mut latest = capture();
        latest.retrieval_metrics = prior.retrieval_metrics.clone();
        latest.retrieval_metrics.fts.unicode61_lane_queries_total = 150;
        latest.retrieval_metrics.vector.query_embeddings_total = 13;
        latest.retrieval_metrics.lexicon.hits_ja = 11;
        latest.captured_at_unix_ms = 14_500;

        let d = delta(&prior, &latest);
        assert_eq!(d.elapsed_ms, 4_500);
        assert_eq!(d.retrieval_metrics.fts.unicode61_lane_queries_total, 50);
        assert_eq!(d.retrieval_metrics.vector.query_embeddings_total, 6);
        assert_eq!(d.retrieval_metrics.lexicon.hits_ja, 8);
    }

    /// `elapsed_ms` saturates when `prior.captured_at_unix_ms >
    /// latest.captured_at_unix_ms` (clock skew or out-of-order
    /// envelopes).
    #[test]
    fn delta_elapsed_ms_saturates_on_clock_skew() {
        let mut prior = capture();
        prior.captured_at_unix_ms = 5_000;
        let mut latest = capture();
        latest.captured_at_unix_ms = 4_000;
        let d = delta(&prior, &latest);
        assert_eq!(d.elapsed_ms, 0);
    }
}
