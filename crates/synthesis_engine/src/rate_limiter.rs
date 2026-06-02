//! Fixed-window per-minute rate limiter for the managed-endpoint
//! synthesizer.
//!
//! This is the **cost-control** counterpart to the per-host
//! `ProviderRateLimiter` in `connector_framework/src/http.rs`. Both
//! exist so a burst from the substrate cannot run up an arbitrary
//! bill against the upstream provider — the connector limiter caps
//! *outbound polling* against vendor APIs, this limiter caps the
//! synthesis engine's calls against the operator's managed
//! inference endpoint.
//!
//! ## Algorithm
//!
//! A **fixed window** counter, refilled every 60 s. We do not use a
//! token bucket here for two reasons:
//!
//! 1. The synthesizer is server-side and burst-bounded — a token
//!    bucket's leaky behaviour buys us nothing over a hard cap.
//! 2. Operators charge by requests-per-minute, not by sustained
//!    rate, so matching the billing window directly is the natural
//!    shape and the one whose violations are easiest for the audit
//!    log to correlate against an upstream invoice line item.
//!
//! Each [`check`](RateLimiter::check) call:
//!
//! 1. Loads the current window-start under the lock and rotates it
//!    forward if `Instant::now() - window_start >= 60 s` (resetting
//!    `request_count` to 0 atomically with the rotate).
//! 2. Loads the current `request_count`; if it is `< max`, fetch-
//!    increments and returns `Ok(())`; otherwise returns
//!    `Err(remaining)` where `remaining` is the wall-clock gap
//!    between `now` and the next window start.
//!
//! The contended path holds the lock for two atomic loads, an
//! `Instant::checked_duration_since`, and at most one
//! `fetch_add` — single-digit microseconds even under contention.
//!
//! ## Thread safety
//!
//! [`RateLimiter`] is `Send + Sync` and intended to be wrapped in
//! `Arc` and shared across the synthesizer's call sites. The
//! window-start `Mutex` plus the relaxed-ordering `AtomicU64` for
//! the count is sufficient because we never need the count to
//! synchronise other operations — the `check` itself is the only
//! consumer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Width of the rate-limiter's rolling window.
///
/// Set to 60 s so the cap matches operator billing units
/// (`requests / minute`). This is wired into [`RateLimiter::new`]
/// at construction time so individual call sites do not need to
/// hard-code the number.
pub const WINDOW: Duration = Duration::from_secs(60);

/// Fixed-window request counter capped at `max_requests_per_window`
/// requests per [`WINDOW`].
#[derive(Debug)]
pub struct RateLimiter {
    max_requests_per_window: u64,
    request_count: AtomicU64,
    /// Wall-clock start of the *current* window. Rotated forward
    /// inside [`check`] whenever `now - window_start >= WINDOW`.
    window_start: Mutex<Instant>,
}

impl RateLimiter {
    /// Construct a rate limiter with the supplied cap.
    ///
    /// A cap of `0` produces a limiter that rejects every request;
    /// callers that want to *disable* rate limiting should keep
    /// the `Option<RateLimiter>` field as `None` rather than
    /// constructing a zero-cap limiter.
    pub fn new(max_per_minute: u64) -> Self {
        Self {
            max_requests_per_window: max_per_minute,
            request_count: AtomicU64::new(0),
            window_start: Mutex::new(Instant::now()),
        }
    }

    /// Test-only constructor that pins `window_start` to a
    /// caller-supplied `Instant`. Used by the unit tests so the
    /// "request rolled over into a fresh window" branch can be
    /// exercised deterministically (without sleeping for 60 s).
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_window_start(max_per_minute: u64, window_start: Instant) -> Self {
        Self {
            max_requests_per_window: max_per_minute,
            request_count: AtomicU64::new(0),
            window_start: Mutex::new(window_start),
        }
    }

    /// Inspect the configured per-window cap.
    pub fn max_per_window(&self) -> u64 {
        self.max_requests_per_window
    }

    /// Either record a request against the current window (returns
    /// `Ok(())`) or reject it with the wall-clock duration the
    /// caller would need to wait before the window rolls over
    /// (returns `Err(remaining)`).
    ///
    /// The wait duration is the *exact* gap between the current
    /// instant and the next window start — callers may choose to
    /// sleep that long and retry, surface the wait to the caller,
    /// or convert it into a `Retry-After` header.
    pub fn check(&self) -> Result<(), Duration> {
        let now = Instant::now();

        let mut window_start = self
            .window_start
            .lock()
            .expect("rate limiter window_start mutex poisoned");

        // Rotate the window forward if we have crossed the boundary
        // since the last check. Reset the count *under the same
        // lock* so the rotate is observed atomically with the
        // counter reset by any concurrent `check` calls.
        if now.duration_since(*window_start) >= WINDOW {
            *window_start = now;
            self.request_count.store(0, Ordering::Relaxed);
        }

        // Claim the next slot in the current window. The mutex
        // above already serialises every write to
        // `request_count`, so a plain `u64` behind the same lock
        // would also be sound. We keep the atomic so that
        // `current_window_count()` (the observability hook used by
        // tests and operator dashboards) can `load(Relaxed)` the
        // counter **without** contending the write path's mutex.
        // The asymmetry — writes-under-lock, reads-lock-free — is
        // intentional and the only reason this field is atomic.
        let prev = self.request_count.fetch_add(1, Ordering::Relaxed);
        if prev < self.max_requests_per_window {
            return Ok(());
        }

        // Cap exceeded. Roll the spurious increment back so a long
        // stretch of rejected calls cannot eventually overflow the
        // counter or skew a debugging readout of "how many requests
        // did this window see".
        self.request_count.fetch_sub(1, Ordering::Relaxed);

        // The next window starts at `window_start + WINDOW`. Compute
        // the gap; saturate at zero so a borderline call (now is
        // *exactly* at the boundary) returns a zero wait, not an
        // arithmetic underflow.
        let next_window = *window_start + WINDOW;
        let remaining = next_window.saturating_duration_since(now);
        Err(remaining)
    }

    /// Observability helper: how many requests have been admitted
    /// against the *current* window. Useful in tests and in
    /// operator dashboards that want to render a "75 / 100" gauge.
    pub fn current_window_count(&self) -> u64 {
        self.request_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn admits_up_to_the_cap_then_rejects() {
        let limiter = RateLimiter::new(3);
        assert_eq!(limiter.current_window_count(), 0);
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert_eq!(limiter.current_window_count(), 3);

        let rejected = limiter.check().expect_err("4th call must be rejected");
        assert!(rejected > Duration::ZERO,
            "rejected calls must surface a non-zero wait so callers can sleep"
        );
        assert!(rejected <= WINDOW, "wait must never exceed one full window");

        // The roll-back means a *second* rejected call still sees
        // count == 3, not count == 5.
        assert_eq!(limiter.current_window_count(), 3);
        assert!(limiter.check().is_err());
        assert_eq!(limiter.current_window_count(), 3);
    }

    #[test]
    fn zero_cap_rejects_every_call() {
        let limiter = RateLimiter::new(0);
        let err = limiter.check().expect_err("cap=0 must reject");
        assert!(err <= WINDOW);
    }

    #[test]
    fn rotates_window_when_the_clock_advances_past_60s() {
        // Pin the window_start one full WINDOW in the past so the
        // first `check` is guaranteed to rotate forward — without
        // this we'd need a real `std::thread::sleep(WINDOW)` to
        // observe rotation, which would make the test ~60 s long.
        let backdated = Instant::now()
            .checked_sub(WINDOW + Duration::from_millis(10))
            .expect("test clock supports back-dating one minute");
        let limiter = RateLimiter::new_with_window_start(2, backdated);
        // Pre-populate the count so we can verify it gets reset on
        // rotation.
        limiter.request_count.store(2, Ordering::Relaxed);

        // Next check rotates the window forward and admits the
        // request because count is now 0 < 2.
        assert!(limiter.check().is_ok());
        assert_eq!(limiter.current_window_count(), 1);
        assert!(limiter.check().is_ok());
        assert_eq!(limiter.current_window_count(), 2);
        assert!(limiter.check().is_err());
    }

    #[test]
    fn concurrent_checks_respect_the_cap() {
        // 1 000 concurrent admit attempts against a 50-slot
        // window must see exactly 50 ok results and 950 errs.
        let limiter = Arc::new(RateLimiter::new(50));
        let mut handles = Vec::new();
        for _ in 0..1_000 {
            let l = Arc::clone(&limiter);
            handles.push(thread::spawn(move || l.check()));
        }
        let mut admitted = 0;
        let mut rejected = 0;
        for h in handles {
            match h.join().expect("worker thread panicked") {
                Ok(()) => admitted += 1,
                Err(_) => rejected += 1,
            }
        }
        assert_eq!(admitted, 50);
        assert_eq!(rejected, 950);
        assert_eq!(limiter.current_window_count(), 50);
    }
}
