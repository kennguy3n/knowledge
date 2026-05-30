//! Token-bucket rate limiter for `trigger_server_synthesis`
//! (Phase 10 Item 5).
//!
//! # Why
//!
//! The Phase-7 [`crate::synthesis::PER_SCOPE_COOLDOWN_SECS`]
//! cap throttles repeated dispatch on a single `(scope, tier)`
//! pair, but a host that fans out across many scopes
//! concurrently can still exhaust the engine. This bucket adds
//! a **global** ceiling at the FFI boundary:
//!
//! * `capacity` tokens accumulate up to a cap (burst capacity).
//! * Each [`TokenBucket::try_acquire`] consumes one token.
//! * Tokens refill continuously at `refill_per_sec`.
//! * When no token is available, the caller learns how many
//!   milliseconds to wait before the next attempt will succeed.
//!
//! The bucket lives on
//! [`crate::runtime::FfiRuntime::synthesis_rate_limiter`] and is
//! created at `open_store` time with the values from
//! [`crate::synthesis::DEFAULT_TRIGGER_RATE_CAPACITY`] and
//! [`crate::synthesis::DEFAULT_TRIGGER_RATE_REFILL_PER_SEC`].
//! [`crate::synthesis::configure_synthesis_engine`] reconfigures
//! it via [`TokenBucket::reconfigure`] when the host supplies
//! non-zero `rate_capacity` / `rate_refill_per_sec` fields on
//! [`crate::types::SynthesisEngineConfig`].
//!
//! # Why not a per-scope limiter?
//!
//! Per-scope rate limiting is already handled by the existing
//! cooldown map — adding a per-scope token bucket on top would
//! be redundant and would mask legitimate fan-out across
//! distinct tenants. The global bucket complements the
//! per-scope cooldown by protecting the shared engine resource.

use chrono::{DateTime, Utc};

/// Continuous-refill token bucket. Not thread-safe on its own
/// — the FFI layer accesses it under
/// [`crate::runtime::FfiRuntime`]'s mutex.
///
/// `tokens` is stored as `f64` so partial-token refills survive
/// across sub-second calls without integer truncation. `capacity`
/// is reported as `u32` because that's the units hosts configure
/// in; the bucket clamps fractional tokens to that ceiling.
#[derive(Debug, Clone)]
pub(crate) struct TokenBucket {
    capacity: u32,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: DateTime<Utc>,
}

impl TokenBucket {
    /// Build a fresh bucket. The bucket starts full (`tokens ==
    /// capacity`) so the first `capacity` calls succeed without
    /// waiting on the refill clock.
    pub(crate) fn new(capacity: u32, refill_per_sec: f64, now: DateTime<Utc>) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: f64::from(capacity),
            last_refill: now,
        }
    }

    /// Adjust the bucket's configured rate without resetting the
    /// current token count. If `capacity` is reduced below the
    /// current token count, the excess is dropped (clamping
    /// keeps the invariant `tokens <= capacity`). A `capacity`
    /// of `0` would deadlock the bucket; callers are expected to
    /// validate non-zero before reaching this path.
    ///
    /// `#[cfg_attr(not(feature = "http-client"), allow(dead_code))]`
    /// because the only production caller is the
    /// `configure_engine_impl` variant gated on `http-client`
    /// (no engine, no rate-shaping needed). The unit tests inside
    /// this module still exercise this method directly, so the
    /// implementation stays in-tree under all feature
    /// configurations.
    #[cfg_attr(not(feature = "http-client"), allow(dead_code))]
    pub(crate) fn reconfigure(&mut self, capacity: u32, refill_per_sec: f64) {
        debug_assert!(capacity > 0, "rate limiter capacity must be > 0");
        debug_assert!(refill_per_sec > 0.0, "rate limiter refill rate must be > 0");
        self.capacity = capacity;
        self.refill_per_sec = refill_per_sec;
        if self.tokens > f64::from(capacity) {
            self.tokens = f64::from(capacity);
        }
    }

    /// Apply continuous refill since the last update. Internal —
    /// `try_acquire` calls this implicitly.
    fn refill(&mut self, now: DateTime<Utc>) {
        let elapsed = now.signed_duration_since(self.last_refill);
        // Clock-skew defence: if `now` is in the past relative
        // to `last_refill`, treat it as a no-op rather than
        // crediting negative tokens. Real-world cause: NTP
        // step-back or a host that does `Utc::now()` on two
        // threads where the second observed a slightly earlier
        // time. The bucket still drains correctly on the next
        // forward-moving call.
        if elapsed <= chrono::Duration::zero() {
            return;
        }
        // `num_milliseconds` is i64 but `elapsed` is bounded by
        // wall-clock progress between mutex-serialised calls;
        // overflow is implausible (would require months of
        // suspended VM time on a sub-second-frequency caller).
        let elapsed_ms = elapsed.num_milliseconds() as f64;
        let added = (elapsed_ms / 1000.0) * self.refill_per_sec;
        self.tokens = (self.tokens + added).min(f64::from(self.capacity));
        self.last_refill = now;
    }

    /// Try to consume one token. On success returns `Ok(())`. On
    /// failure returns `Err(retry_after_ms)` — the wall-clock
    /// time the caller should wait before retrying. The wait is
    /// computed from the **current** deficit, so concurrent
    /// callers draining the bucket between hint and retry may
    /// extend the actual wait — hosts should treat the value as
    /// a lower bound.
    pub(crate) fn try_acquire(&mut self, now: DateTime<Utc>) -> Result<(), u64> {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            // Token deficit / refill rate = seconds until the
            // next whole token. Round up so a 250.3 ms wait is
            // reported as 251 ms — under-reporting would let
            // the caller retry while the deficit is still > 1.
            let deficit = 1.0 - self.tokens;
            let wait_secs = deficit / self.refill_per_sec;
            // `wait_secs * 1000` is non-negative (deficit > 0,
            // refill_per_sec > 0 by `reconfigure`'s
            // `debug_assert!`), so the cast to u64 cannot lose
            // a sign. Truncation is possible only if the
            // `ceil()` result exceeds 2^64-1 milliseconds (~584
            // million years) — implausible for any host that
            // hasn't suspended its clock since the Cambrian.
            // We saturate via `as u64` rather than `try_into` so
            // a debug-build panic on negative inputs cannot
            // escape the cooldown gate.
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
            )]
            let retry_after_ms = (wait_secs * 1000.0).ceil() as u64;
            Err(retry_after_ms.max(1))
        }
    }

    /// Inspect the current configured capacity. Used by the
    /// synthesis subsystem's health probe to surface the
    /// active rate-shaping posture alongside `single_tenant=`
    /// and the other diagnostic fields, and by tests that
    /// verify [`Self::reconfigure`] landed.
    pub(crate) fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Inspect the current refill rate. Used in the same
    /// places as [`Self::capacity`].
    pub(crate) fn refill_per_sec(&self) -> f64 {
        self.refill_per_sec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t(offset_secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000 + offset_secs, 0)
            .expect("static timestamp must be valid")
    }

    #[test]
    fn fresh_bucket_serves_capacity_calls_then_throttles() {
        let mut bucket = TokenBucket::new(3, 1.0, t(0));
        for i in 0..3 {
            bucket
                .try_acquire(t(0))
                .unwrap_or_else(|_| panic!("call {i} must succeed (burst)"));
        }
        let err = bucket
            .try_acquire(t(0))
            .expect_err("4th call must throttle");
        assert!(err >= 1, "retry_after_ms must be at least 1 (got {err})");
    }

    #[test]
    fn refill_credits_tokens_after_elapsed_time() {
        let mut bucket = TokenBucket::new(2, 2.0, t(0));
        // Drain.
        bucket.try_acquire(t(0)).unwrap();
        bucket.try_acquire(t(0)).unwrap();
        // Throttled at t=0.
        assert!(bucket.try_acquire(t(0)).is_err());
        // After 0.5s at 2 tokens/s we should have 1 token.
        let now = t(0) + Duration::milliseconds(500);
        bucket.try_acquire(now).expect("refilled token");
        // And immediately drained again.
        assert!(bucket.try_acquire(now).is_err());
    }

    #[test]
    fn refill_caps_at_capacity_after_long_idle() {
        let mut bucket = TokenBucket::new(5, 1.0, t(0));
        bucket.try_acquire(t(0)).unwrap();
        bucket.try_acquire(t(0)).unwrap();
        // Wait 1 hour: refill credits would be 3600 tokens but
        // must clamp to capacity (5) minus the consumed (2) = 3
        // remaining headroom on top of the 3 still in the
        // bucket — total 5, not 3603.
        let now = t(3600);
        for i in 0..5 {
            bucket
                .try_acquire(now)
                .unwrap_or_else(|_| panic!("burst call {i} after long idle must succeed"));
        }
        assert!(
            bucket.try_acquire(now).is_err(),
            "6th call after long idle must throttle (cap held at capacity=5)",
        );
    }

    #[test]
    fn negative_elapsed_does_not_credit_tokens() {
        // Simulate a clock that goes backwards (NTP step-back).
        // The bucket must NOT credit negative tokens — if it
        // did, `tokens` could exceed `capacity` or even go
        // below zero on the next forward step.
        let mut bucket = TokenBucket::new(1, 1.0, t(10));
        bucket.try_acquire(t(10)).unwrap();
        // Now `tokens == 0`. Step the clock backwards.
        let err = bucket
            .try_acquire(t(5))
            .expect_err("clock step-back must still throttle");
        assert!(err >= 1);
    }

    #[test]
    fn reconfigure_clamps_excess_tokens_to_new_capacity() {
        let mut bucket = TokenBucket::new(10, 1.0, t(0));
        // 10 tokens are in the bucket. Reconfigure to capacity 3.
        bucket.reconfigure(3, 1.0);
        for i in 0..3 {
            bucket
                .try_acquire(t(0))
                .unwrap_or_else(|_| panic!("call {i} must succeed under new cap"));
        }
        assert!(
            bucket.try_acquire(t(0)).is_err(),
            "4th call must throttle (capacity clamped from 10 to 3)",
        );
    }

    #[test]
    fn retry_after_ms_at_least_one_ms() {
        // A throttle with a *fractional* sub-millisecond wait
        // (e.g. 0.4 ms deficit at 1000 tokens/sec) must still
        // report at least 1 ms — hosts that poll on 0-ms hints
        // would spin without yielding.
        let mut bucket = TokenBucket::new(1, 1000.0, t(0));
        bucket.try_acquire(t(0)).unwrap();
        let err = bucket
            .try_acquire(t(0))
            .expect_err("immediate second call must throttle");
        assert!(err >= 1, "retry_after_ms must be at least 1 ms (got {err})");
    }
}
