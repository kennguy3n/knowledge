//! Synthesis-quality counters for the SLM-backed
//! [`crate::LlamaCppSynthesizer`].
//!
//! These complement the router's dispatch-latency histogram
//! (`inference_router::InferenceRouter::dispatch_latencies`,
//! exposed as `knowledge_slm_dispatch_duration_seconds`) with
//! synthesis-specific quality signals the substrate exposes alongside
//! the existing telemetry:
//!
//! * `synthesis_retry_total` — verify-and-retry second attempts made.
//! * `synthesis_retry_failed_total` — retries that were dispatched but
//!   errored, so the first (mediocre) bundle was kept rather than
//!   failing the synthesis. Makes the otherwise-silent graceful
//!   degradation observable (a flaky retry-only adapter shows up here).
//! * `synthesis_lowquality_total` — bundles whose first attempt tripped
//!   a [`crate::quality::QualityReport`] flag.
//! * `synthesis_truncated_total` — outputs the token cap truncated
//!   (recovered by the salvage parser).
//! * `synthesis_exemplar_leaks_stripped_total` — structured-list entries
//!   the quality gate scrubbed because they copied a prompt exemplar
//!   placeholder before persistence. The on-device path emits a
//!   `tracing::warn!` for the same event; this counter gives the
//!   logging-free server path equivalent observability (a rising value
//!   means the prompt exemplar is leaking and warrants attention).
//! * recap length — running sum + count, so the host can expose a mean
//!   (or a gauge of the last value) recap-length signal.
//!
//! The counters are process-global atomics behind an [`Arc`] so a
//! long-lived synthesizer — or several clones sharing one
//! [`SynthesisMetrics`] — accumulate into the same totals. The host
//! (FFI health envelope / server Prometheus surface) reads a
//! [`SynthesisMetricsSnapshot`] via
//! [`crate::LlamaCppSynthesizer::metrics_snapshot`] and folds it into
//! the metrics exposition next to the dispatch histogram.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Process-global synthesis-quality counters. Cheap to clone (it is an
/// [`Arc`] internally via the synthesizer); every clone increments the
/// same totals.
#[derive(Debug, Default)]
pub struct SynthesisMetrics {
    retry_total: AtomicU64,
    retry_failed_total: AtomicU64,
    lowquality_total: AtomicU64,
    truncated_total: AtomicU64,
    exemplar_leaks_stripped_total: AtomicU64,
    recap_length_sum: AtomicU64,
    recap_length_count: AtomicU64,
}

impl SynthesisMetrics {
    /// Construct a fresh, zeroed metrics handle wrapped in an [`Arc`] so
    /// it can be shared across synthesizer clones.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record a completed verify-and-retry second attempt.
    pub fn incr_retry(&self) {
        self.retry_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a dispatched retry *errored*, so the first bundle was
    /// kept (graceful degradation). Counted in addition to
    /// [`Self::incr_retry`], never instead of it.
    pub fn incr_retry_failed(&self) {
        self.retry_failed_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a synthesis run's first attempt was flagged
    /// low-quality (regardless of whether the retry improved it).
    pub fn incr_lowquality(&self) {
        self.lowquality_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an attempt's output was truncated by the token cap
    /// and recovered by the salvage parser.
    pub fn incr_truncated(&self) {
        self.truncated_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Add `count` leaked-exemplar list entries the quality gate scrubbed
    /// before persistence (see
    /// [`crate::quality::strip_exemplar_leak`]). Counts entries, not runs,
    /// so it is comparable to the FFI path's `tracing::warn!` `stripped`
    /// field; a no-op when `count` is `0`, so callers can pass the
    /// per-run total unconditionally.
    pub fn add_exemplar_leaks_stripped(&self, count: usize) {
        if count == 0 {
            return;
        }
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        self.exemplar_leaks_stripped_total
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Record the recap length (in Unicode scalar values) of the bundle a
    /// synthesis run ultimately returned.
    pub fn observe_recap_length(&self, chars: usize) {
        // Defensive `try_from` rather than an `as` cast — matches the FFI
        // metrics sink (`ffi::metrics::observe_synthesis_recap_chars`) so
        // both paths convert `usize -> u64` identically and stay correct
        // on a hypothetical target where `usize > u64`. Recap lengths are
        // tiny, so the saturating fallback never triggers in practice.
        let chars = u64::try_from(chars).unwrap_or(u64::MAX);
        self.recap_length_sum.fetch_add(chars, Ordering::Relaxed);
        self.recap_length_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a consistent-enough point-in-time snapshot of every counter.
    #[must_use]
    pub fn snapshot(&self) -> SynthesisMetricsSnapshot {
        SynthesisMetricsSnapshot {
            retry_total: self.retry_total.load(Ordering::Relaxed),
            retry_failed_total: self.retry_failed_total.load(Ordering::Relaxed),
            lowquality_total: self.lowquality_total.load(Ordering::Relaxed),
            truncated_total: self.truncated_total.load(Ordering::Relaxed),
            exemplar_leaks_stripped_total: self
                .exemplar_leaks_stripped_total
                .load(Ordering::Relaxed),
            recap_length_sum: self.recap_length_sum.load(Ordering::Relaxed),
            recap_length_count: self.recap_length_count.load(Ordering::Relaxed),
        }
    }
}

/// Wire-flat copy of the synthesis counters for the metrics exposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SynthesisMetricsSnapshot {
    /// `synthesis_retry_total`.
    pub retry_total: u64,
    /// `synthesis_retry_failed_total`.
    pub retry_failed_total: u64,
    /// `synthesis_lowquality_total`.
    pub lowquality_total: u64,
    /// `synthesis_truncated_total`.
    pub truncated_total: u64,
    /// `synthesis_exemplar_leaks_stripped_total` — leaked-exemplar list
    /// entries scrubbed before persistence across every synthesis run.
    pub exemplar_leaks_stripped_total: u64,
    /// Running sum of returned-bundle recap lengths (scalar values).
    pub recap_length_sum: u64,
    /// Number of recorded recap-length observations.
    pub recap_length_count: u64,
}

impl SynthesisMetricsSnapshot {
    /// Mean recap length over all observations, or `None` before the
    /// first synthesis run.
    #[must_use]
    pub fn mean_recap_length(&self) -> Option<f64> {
        if self.recap_length_count == 0 {
            None
        } else {
            Some(self.recap_length_sum as f64 / self.recap_length_count as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_across_shared_clones() {
        let metrics = SynthesisMetrics::new();
        let clone = Arc::clone(&metrics);
        metrics.incr_retry();
        clone.incr_retry();
        clone.incr_retry_failed();
        clone.incr_lowquality();
        clone.incr_truncated();
        metrics.add_exemplar_leaks_stripped(2);
        clone.add_exemplar_leaks_stripped(1);
        clone.add_exemplar_leaks_stripped(0); // no-op
        metrics.observe_recap_length(40);
        clone.observe_recap_length(20);

        let snap = metrics.snapshot();
        assert_eq!(snap.retry_total, 2);
        assert_eq!(snap.retry_failed_total, 1);
        assert_eq!(snap.lowquality_total, 1);
        assert_eq!(snap.truncated_total, 1);
        assert_eq!(snap.exemplar_leaks_stripped_total, 3);
        assert_eq!(snap.recap_length_sum, 60);
        assert_eq!(snap.recap_length_count, 2);
        assert_eq!(snap.mean_recap_length(), Some(30.0));
    }

    #[test]
    fn mean_recap_length_is_none_before_first_observation() {
        let metrics = SynthesisMetrics::new();
        assert_eq!(metrics.snapshot().mean_recap_length(), None);
    }
}
