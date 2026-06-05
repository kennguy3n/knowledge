//! Fixed-bucket latency histogram for dispatch / store-open timing.
//!
//! The substrate exposes SLM dispatch latency as the Prometheus
//! histogram `knowledge_slm_dispatch_duration_seconds` (labelled by
//! `task` and `adapter`) and store-open latency as
//! `knowledge_open_store_duration_seconds`. Both are backed by this
//! type: a small, lock-free-on-read, fixed-bucket histogram that
//! tracks per-bucket counts, the running sample count, and the running
//! sum of observed seconds.
//!
//! The bucket boundaries are the canonical Prometheus `le` (less-than-
//! or-equal) upper bounds; the implicit `+Inf` bucket catches every
//! sample larger than the last finite bound. Quantile estimates
//! ([`LatencyHistogram::quantile`]) use the standard Prometheus
//! `histogram_quantile` linear-interpolation-within-bucket scheme, so
//! the reported p50 / p95 carry the same approximation characteristics
//! as a Prometheus server computing them from the exposed buckets —
//! resolution is bounded by the bucket width straddling the quantile.

use std::time::Duration;

/// Upper bucket bounds in seconds (Prometheus `le` boundaries),
/// excluding the implicit `+Inf` bucket.
///
/// Tuned to span the substrate's two timed paths:
///
/// * **SLM dispatch** — sub-millisecond encoder-only classification
///   (fallback adapter) through multi-second cold llama.cpp synthesis.
/// * **store open** — single-digit-millisecond opens on a small
///   database through hundreds-of-milliseconds opens that replay a
///   large tombstone / rehydration backlog.
pub const LATENCY_BUCKETS_SECONDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// A fixed-bucket latency histogram.
///
/// Stores one count per finite bucket plus a trailing `+Inf` bucket
/// (so `counts.len() == LATENCY_BUCKETS_SECONDS.len() + 1`), the total
/// sample `count`, and the running `sum_seconds`. Cumulative bucket
/// counts and quantiles are derived on read so the hot `record` path
/// is a single bucket search + three scalar updates.
#[derive(Debug, Clone)]
pub struct LatencyHistogram {
    /// Per-bucket sample counts; the final element is the `+Inf`
    /// overflow bucket for samples larger than the last finite bound.
    counts: Vec<u64>,
    /// Running sum of every observed sample in seconds.
    sum_seconds: f64,
    /// Total number of observed samples.
    count: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    /// Construct an empty histogram over [`LATENCY_BUCKETS_SECONDS`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            // +1 for the implicit `+Inf` bucket.
            counts: vec![0; LATENCY_BUCKETS_SECONDS.len() + 1],
            sum_seconds: 0.0,
            count: 0,
        }
    }

    /// Record one observed [`Duration`].
    pub fn record(&mut self, elapsed: Duration) {
        self.record_seconds(elapsed.as_secs_f64());
    }

    /// Record one observed latency in seconds.
    ///
    /// Negative or non-finite inputs are ignored — they can only come
    /// from a misbehaving clock and would corrupt the sum.
    pub fn record_seconds(&mut self, seconds: f64) {
        if !seconds.is_finite() || seconds < 0.0 {
            return;
        }
        let idx = LATENCY_BUCKETS_SECONDS
            .iter()
            .position(|&le| seconds <= le)
            .unwrap_or(LATENCY_BUCKETS_SECONDS.len());
        self.counts[idx] += 1;
        self.sum_seconds += seconds;
        self.count += 1;
    }

    /// Total number of recorded samples.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Running sum of every recorded sample in seconds.
    #[must_use]
    pub fn sum_seconds(&self) -> f64 {
        self.sum_seconds
    }

    /// Fold another histogram's samples into this one.
    ///
    /// Both histograms share the same fixed [`LATENCY_BUCKETS_SECONDS`]
    /// boundaries, so the merge is an element-wise add of per-bucket
    /// counts plus the scalar `count` / `sum_seconds`. Used to compute
    /// an overall dispatch-latency distribution across every
    /// `(task, adapter)` pair (e.g. for the health envelope's aggregate
    /// p50/p95) without re-observing the raw samples.
    pub fn merge(&mut self, other: &LatencyHistogram) {
        for (slot, &c) in self.counts.iter_mut().zip(other.counts.iter()) {
            *slot += c;
        }
        self.sum_seconds += other.sum_seconds;
        self.count += other.count;
    }

    /// Cumulative bucket counts as `(le, cumulative_count)` pairs,
    /// including the trailing `+Inf` bucket (rendered as
    /// [`f64::INFINITY`]).
    ///
    /// This is the shape a Prometheus `_bucket` series exposes: each
    /// `le` carries the count of every sample whose value is `≤ le`.
    #[must_use]
    pub fn cumulative_buckets(&self) -> Vec<(f64, u64)> {
        let mut out = Vec::with_capacity(self.counts.len());
        let mut cumulative = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            cumulative += c;
            let le = LATENCY_BUCKETS_SECONDS
                .get(i)
                .copied()
                .unwrap_or(f64::INFINITY);
            out.push((le, cumulative));
        }
        out
    }

    /// Estimate the `q`-quantile (0.0..=1.0) in seconds using the
    /// Prometheus `histogram_quantile` interpolation scheme.
    ///
    /// Returns `None` when no samples have been recorded. When the
    /// quantile falls in the `+Inf` bucket the largest finite bound is
    /// returned (there is no upper bound to interpolate toward), which
    /// is the same clamping Prometheus applies.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let q = q.clamp(0.0, 1.0);
        let rank = q * self.count as f64;
        let mut cumulative_before = 0u64;
        for (i, &bucket_count) in self.counts.iter().enumerate() {
            let cumulative_after = cumulative_before + bucket_count;
            if (cumulative_after as f64) >= rank && bucket_count > 0 {
                let upper = LATENCY_BUCKETS_SECONDS
                    .get(i)
                    .copied()
                    .unwrap_or(f64::INFINITY);
                if !upper.is_finite() {
                    // Quantile lands in the `+Inf` bucket — clamp to
                    // the largest finite bound (cannot interpolate
                    // toward an unbounded upper edge).
                    return LATENCY_BUCKETS_SECONDS.last().copied();
                }
                let lower = if i == 0 {
                    0.0
                } else {
                    LATENCY_BUCKETS_SECONDS[i - 1]
                };
                // The loop guard above requires `bucket_count > 0`, so
                // the interpolation divisor is always non-zero here.
                let within = rank - cumulative_before as f64;
                let fraction = within / bucket_count as f64;
                return Some(lower + (upper - lower) * fraction);
            }
            cumulative_before = cumulative_after;
        }
        // Rank beyond every populated bucket (e.g. q == 1.0): the last
        // finite bound is the best estimate.
        LATENCY_BUCKETS_SECONDS.last().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_quantile() {
        let h = LatencyHistogram::new();
        assert_eq!(h.count(), 0);
        // Exact-zero by construction; compare bit patterns to stay
        // clear of the workspace `float_cmp` denial.
        assert_eq!(h.sum_seconds().to_bits(), 0.0_f64.to_bits());
        assert_eq!(h.quantile(0.5), None);
    }

    #[test]
    fn records_into_correct_bucket() {
        let mut h = LatencyHistogram::new();
        // 0.003s → first bucket whose le >= 0.003 is 0.005.
        h.record_seconds(0.003);
        let buckets = h.cumulative_buckets();
        // le=0.001 has 0, le=0.005 has the cumulative 1.
        assert_eq!(buckets[0], (0.001, 0));
        assert_eq!(buckets[1], (0.005, 1));
        // Cumulative carries through to +Inf.
        assert_eq!(buckets.last().unwrap().1, 1);
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn ignores_non_finite_and_negative() {
        let mut h = LatencyHistogram::new();
        h.record_seconds(f64::NAN);
        h.record_seconds(f64::INFINITY);
        h.record_seconds(-1.0);
        assert_eq!(h.count(), 0);
    }

    #[test]
    fn overflow_bucket_catches_large_samples() {
        let mut h = LatencyHistogram::new();
        h.record_seconds(42.0);
        let buckets = h.cumulative_buckets();
        let (le, cumulative) = *buckets.last().unwrap();
        assert!(le.is_infinite());
        assert_eq!(cumulative, 1);
        // p95 clamps to the largest finite bound (10.0).
        assert_eq!(h.quantile(0.95), Some(10.0));
    }

    #[test]
    fn quantile_interpolates_within_bucket() {
        let mut h = LatencyHistogram::new();
        // 100 samples uniformly at 0.05s → all land in the le=0.05
        // bucket (lower edge 0.025). The median interpolates halfway
        // through that bucket.
        for _ in 0..100 {
            h.record_seconds(0.05);
        }
        let p50 = h.quantile(0.5).expect("p50");
        assert!(p50 > 0.025 && p50 <= 0.05, "p50 within bucket: {p50}");
    }

    #[test]
    fn quantile_is_monotone_in_q() {
        let mut h = LatencyHistogram::new();
        for i in 0..1000 {
            h.record_seconds(f64::from(i) / 1000.0);
        }
        let p50 = h.quantile(0.5).expect("p50");
        let p95 = h.quantile(0.95).expect("p95");
        assert!(p95 >= p50, "p95 {p95} >= p50 {p50}");
    }
}
