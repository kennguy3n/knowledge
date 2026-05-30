//! Per-provider rate limiter for outbound connector traffic.
//!
//! At scale, aggregate API-call volume against Google /
//! Microsoft / Slack / Notion needs explicit management — every
//! one of those providers publishes a documented per-tenant
//! quota and silently 429s once it is crossed. The connector
//! runtime fans out across many tenants and many instances per
//! tenant, so a single connector cannot self-throttle without
//! coordination across the runtime.
//!
//! [`ProviderRateLimiter`] is that coordination point. It
//! maintains one token bucket per **provider host** (the host
//! component of the request URL, e.g. `api.notion.com`,
//! `graph.microsoft.com`, `slack.com`). Each
//! [`acquire`](ProviderRateLimiter::acquire) call:
//!
//! 1. Looks up the bucket for the host, lazily creating one
//!    using the configured default `refill_rate` / `max_tokens`.
//! 2. Refills the bucket by `(now - last_refill) * refill_rate`
//!    tokens, saturating at `max_tokens` (no permanent banking).
//! 3. If at least one token is present, deducts one and returns.
//! 4. If the bucket is empty, computes the wall-clock delay
//!    until one token will be available
//!    (`(1.0 - tokens) / refill_rate`) and `sleep`s for that
//!    long, then deducts the freshly-refilled token and
//!    returns.
//!
//! The default policy is **block-and-deduct** rather than
//! reject-fast — every connector poll is a real user-facing
//! synchronisation and dropping a request would cause the
//! corresponding evidence ingest to skip. Operators who want
//! reject-fast behaviour wrap the limiter in their own
//! `try_acquire` (exposed for completeness alongside `acquire`).
//!
//! ## Why a token bucket and not a fixed window
//!
//! The synthesis-engine rate limiter (`crate::rate_limiter` in
//! `synthesis_engine`) uses a fixed window because operator
//! billing is per-minute. Connector providers charge per
//! sustained QPS, so a token bucket matches their published
//! per-tenant quota more faithfully — and absorbs the bursty
//! initial-sync workload (a 50k-document Slack workspace
//! pulled in one go) without rejecting the burst outright.
//!
//! ## Thread safety
//!
//! The bucket map sits behind a single `Mutex` — fine for the
//! N=few-providers cardinality and avoids the per-bucket
//! locking complexity that we don't need at the substrate's
//! scale. Each `acquire` holds the lock for the duration of
//! the refill + deduct check, then releases it *before*
//! sleeping (so concurrent acquires on a different host do
//! not block on the sleeping caller).
//!
//! ## Provider key derivation
//!
//! [`provider_key_for_url`] is the canonical helper for turning
//! a request URL into the lookup key. We use the lowercase
//! host, stripping any port suffix — different paths on the
//! same host share one bucket, which matches the provider's
//! quota model (Slack's `chat.postMessage` and `users.list`
//! both count against the same workspace quota).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default per-provider sustained rate (50 requests/second).
///
/// Roughly matches the published per-app quotas for Notion
/// (3 req/s, but most active connectors fan out across many
/// integration tokens), Google Drive (1 000 req / 100 s ≈
/// 10 req/s per project), and Slack (Tier 4 ≈ 100 req/min ≈
/// 1.7 req/s) — operators who need stricter caps pin them via
/// [`ProviderRateLimiter::set_policy_for_host`].
pub const DEFAULT_REFILL_RATE_PER_SEC: f64 = 50.0;

/// Default per-provider burst capacity (50 tokens).
///
/// Equivalent to one full second's worth of sustained rate so
/// the bucket can absorb a short burst without inflating the
/// effective sustained rate.
pub const DEFAULT_MAX_TOKENS: f64 = 50.0;

/// Per-provider token-bucket policy. Cloned into the per-host
/// bucket on first use.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPolicy {
    /// Steady-state tokens-per-second refill rate.
    pub refill_rate_per_sec: f64,
    /// Maximum bucket capacity (i.e. peak burst size).
    pub max_tokens: f64,
}

impl ProviderPolicy {
    /// Construct a policy from `(refill_per_sec, max_tokens)`.
    pub fn new(refill_rate_per_sec: f64, max_tokens: f64) -> Self {
        assert!(
            refill_rate_per_sec > 0.0,
            "refill_rate_per_sec must be strictly positive"
        );
        assert!(max_tokens > 0.0, "max_tokens must be strictly positive");
        Self {
            refill_rate_per_sec,
            max_tokens,
        }
    }
}

impl Default for ProviderPolicy {
    fn default() -> Self {
        Self {
            refill_rate_per_sec: DEFAULT_REFILL_RATE_PER_SEC,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn from_policy(policy: ProviderPolicy, now: Instant) -> Self {
        Self {
            // Start each bucket *full* so the first acquire on a
            // fresh provider is unblocked — without this, every
            // brand-new tenant would pay one tick of warm-up
            // latency on the very first sync.
            tokens: policy.max_tokens,
            max_tokens: policy.max_tokens,
            refill_rate: policy.refill_rate_per_sec,
            last_refill: now,
        }
    }

    /// Refill `tokens` by the elapsed-since-last-refill amount,
    /// saturating at `max_tokens`.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        let elapsed_secs = elapsed.as_secs_f64();
        if elapsed_secs > 0.0 {
            let topup = elapsed_secs * self.refill_rate;
            self.tokens = (self.tokens + topup).min(self.max_tokens);
            self.last_refill = now;
        }
    }

    /// Try to deduct one token. On success returns
    /// `Ok(())`; on failure returns the wall-clock duration
    /// before one token will be available.
    fn try_consume(&mut self) -> Result<(), Duration> {
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        // tokens < 1.0 — the wait is (1.0 - tokens) / refill_rate.
        let deficit = 1.0 - self.tokens;
        let wait_secs = deficit / self.refill_rate;
        Err(Duration::from_secs_f64(wait_secs))
    }
}

/// Per-provider rate limiter shared across the connector
/// runtime. Wrap in `Arc` and share across every
/// [`crate::http::BlockingHttpTransport`] instance whose calls
/// should count against the same per-provider quota.
#[derive(Debug)]
pub struct ProviderRateLimiter {
    default_policy: ProviderPolicy,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    overrides: HashMap<String, ProviderPolicy>,
    buckets: HashMap<String, TokenBucket>,
}

impl ProviderRateLimiter {
    /// Construct a limiter that uses [`ProviderPolicy::default`]
    /// for every host until [`Self::set_policy_for_host`] sets
    /// a per-host override.
    pub fn new() -> Self {
        Self::with_default_policy(ProviderPolicy::default())
    }

    /// Construct a limiter with a custom default per-host policy.
    pub fn with_default_policy(default_policy: ProviderPolicy) -> Self {
        Self {
            default_policy,
            state: Mutex::new(State::default()),
        }
    }

    /// Pin a per-host policy that overrides the default. Setting
    /// a host after a bucket has already been created for it
    /// resets the bucket (so the new policy applies on the next
    /// acquire) — operators are expected to call this at startup,
    /// not under traffic.
    pub fn set_policy_for_host(&self, host: impl Into<String>, policy: ProviderPolicy) {
        let host = host.into();
        let mut state = self
            .state
            .lock()
            .expect("provider rate limiter state mutex poisoned");
        state.buckets.remove(&host);
        state.overrides.insert(host, policy);
    }

    /// Block until one token can be deducted from the bucket
    /// for `provider_key`. Sleeps if necessary.
    ///
    /// The total wait is bounded by `(1.0 - tokens) /
    /// refill_rate` for the bucket — at the default 50 tokens
    /// per second that is at most 20 ms per acquire under
    /// sustained pressure.
    pub fn acquire(&self, provider_key: &str) {
        loop {
            let wait = {
                let mut state = self
                    .state
                    .lock()
                    .expect("provider rate limiter state mutex poisoned");
                let policy = state
                    .overrides
                    .get(provider_key)
                    .copied()
                    .unwrap_or(self.default_policy);
                let bucket = state
                    .buckets
                    .entry(provider_key.to_string())
                    .or_insert_with(|| TokenBucket::from_policy(policy, Instant::now()));
                bucket.refill(Instant::now());
                match bucket.try_consume() {
                    Ok(()) => return,
                    Err(wait) => wait,
                }
            };
            // Sleep *outside* the lock so concurrent acquires
            // on a different host do not block on us.
            std::thread::sleep(wait);
        }
    }

    /// Non-blocking variant. Returns `Ok(())` if the bucket had
    /// a token to deduct, or `Err(wait)` with the wall-clock
    /// delay before one will be available.
    pub fn try_acquire(&self, provider_key: &str) -> Result<(), Duration> {
        let mut state = self
            .state
            .lock()
            .expect("provider rate limiter state mutex poisoned");
        let policy = state
            .overrides
            .get(provider_key)
            .copied()
            .unwrap_or(self.default_policy);
        let bucket = state
            .buckets
            .entry(provider_key.to_string())
            .or_insert_with(|| TokenBucket::from_policy(policy, Instant::now()));
        bucket.refill(Instant::now());
        bucket.try_consume()
    }

    /// Observability helper: how many tokens are currently in
    /// the bucket for `provider_key` (after refilling to now).
    /// Returns `None` if no bucket has ever been created for
    /// `provider_key`.
    pub fn tokens_for_host(&self, provider_key: &str) -> Option<f64> {
        let mut state = self
            .state
            .lock()
            .expect("provider rate limiter state mutex poisoned");
        let bucket = state.buckets.get_mut(provider_key)?;
        bucket.refill(Instant::now());
        Some(bucket.tokens)
    }
}

impl Default for ProviderRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRateLimiter {
    /// Convenience constructor that returns an `Arc` so callers
    /// can directly hand it to `BlockingHttpTransport::with_provider_rate_limiter`.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

/// Extract the lookup key from a request URL.
///
/// Returns the lowercase host, with any port suffix stripped.
/// On malformed URLs returns `"<unparsed-url>"` so the bucket
/// still receives traffic (rather than silently bypassing the
/// limiter); operators see the synthetic key in dashboards and
/// can repair the offending URL.
pub fn provider_key_for_url(url: &str) -> String {
    // Trim the scheme. We only handle absolute URLs — connectors
    // build absolute URLs everywhere — but stay defensive against
    // the odd debug call site that passes a path.
    let after_scheme = if let Some(idx) = url.find("://") {
        &url[idx + 3..]
    } else {
        return "<unparsed-url>".to_string();
    };
    // The authority section ends at the first '/', '?', or '#'.
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    // Strip the userinfo prefix `user:pass@`.
    let host_with_port = match authority.find('@') {
        Some(idx) => &authority[idx + 1..],
        None => authority,
    };
    // Strip the port suffix `:port`. Be careful with IPv6
    // literal authorities `[::1]:8080`; we only strip the
    // trailing `:port` if the host does not start with `[`.
    let host = if host_with_port.starts_with('[') {
        // IPv6 literal — strip only if there's a `]:port` suffix.
        match host_with_port.rfind("]:") {
            Some(idx) => &host_with_port[..=idx],
            None => host_with_port,
        }
    } else {
        match host_with_port.rfind(':') {
            Some(idx) => &host_with_port[..idx],
            None => host_with_port,
        }
    };
    host.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn provider_key_lowercases_and_strips_port() {
        assert_eq!(
            provider_key_for_url("https://API.notion.com/v1/databases/xyz"),
            "api.notion.com"
        );
        assert_eq!(
            provider_key_for_url("https://graph.microsoft.com:443/v1.0/me"),
            "graph.microsoft.com"
        );
        assert_eq!(
            provider_key_for_url("http://user:pass@example.com:8080/path"),
            "example.com"
        );
        assert_eq!(provider_key_for_url("/relative/path"), "<unparsed-url>");
    }

    #[test]
    fn provider_key_handles_ipv6_authority() {
        assert_eq!(provider_key_for_url("https://[::1]:8443/healthz"), "[::1]");
        assert_eq!(
            provider_key_for_url("https://[2001:db8::1]/"),
            "[2001:db8::1]"
        );
    }

    #[test]
    fn buckets_start_full_so_first_acquire_does_not_block() {
        let limiter = ProviderRateLimiter::new();
        let start = Instant::now();
        limiter.acquire("api.notion.com");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "first acquire should be near-instant; took {elapsed:?}"
        );
        // We deducted one token from a default-50-token bucket.
        let tokens = limiter
            .tokens_for_host("api.notion.com")
            .expect("bucket exists after acquire");
        assert!(
            tokens > 48.0 && tokens < 50.0,
            "expected ~49 tokens left, got {tokens}"
        );
    }

    #[test]
    fn try_acquire_returns_wait_when_bucket_empty() {
        let limiter = ProviderRateLimiter::with_default_policy(
            // 1 token/sec, max 2 tokens — small numbers so the
            // test exhausts the bucket in two acquires.
            ProviderPolicy::new(1.0, 2.0),
        );
        // Drain the bucket.
        limiter.try_acquire("api.example.com").expect("token 1");
        limiter.try_acquire("api.example.com").expect("token 2");
        let wait = limiter
            .try_acquire("api.example.com")
            .expect_err("third token must wait");
        // At 1 token/sec, the wait should be ~1 second.
        assert!(
            wait >= Duration::from_millis(500),
            "expected ≥ 500 ms wait, got {wait:?}"
        );
        assert!(
            wait <= Duration::from_secs(2),
            "expected ≤ 2 s wait, got {wait:?}"
        );
    }

    #[test]
    fn acquire_blocks_and_then_succeeds_after_refill() {
        let limiter = ProviderRateLimiter::with_default_policy(ProviderPolicy::new(100.0, 1.0));
        // Drain the single-token bucket.
        limiter.acquire("api.example.com");
        // The second acquire must block for ~10 ms (1 token /
        // 100 tokens-per-sec) and then succeed.
        let start = Instant::now();
        limiter.acquire("api.example.com");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(5),
            "second acquire must wait at least a few ms; took {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(200),
            "second acquire shouldn't take that long; took {elapsed:?}"
        );
    }

    #[test]
    fn separate_hosts_have_independent_buckets() {
        let limiter = ProviderRateLimiter::with_default_policy(ProviderPolicy::new(1.0, 1.0));
        limiter.acquire("api.notion.com");
        // The Slack bucket is fresh so this must not block.
        let start = Instant::now();
        limiter.acquire("slack.com");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "fresh host must not be throttled by sibling host"
        );
    }

    #[test]
    fn per_host_policy_overrides_the_default() {
        let limiter = ProviderRateLimiter::with_default_policy(ProviderPolicy::new(50.0, 50.0));
        limiter.set_policy_for_host("slack.com", ProviderPolicy::new(2.0, 2.0));
        limiter.acquire("slack.com");
        limiter.acquire("slack.com");
        let wait = limiter
            .try_acquire("slack.com")
            .expect_err("override caps Slack at 2 tokens");
        assert!(
            wait >= Duration::from_millis(250),
            "expected ≥ 250 ms wait under the 2 req/s override; got {wait:?}"
        );
    }

    #[test]
    fn concurrent_acquires_on_same_host_serialise() {
        let limiter = Arc::new(ProviderRateLimiter::with_default_policy(
            ProviderPolicy::new(20.0, 4.0),
        ));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let l = Arc::clone(&limiter);
            handles.push(std::thread::spawn(move || {
                let start = Instant::now();
                l.acquire("api.example.com");
                start.elapsed()
            }));
        }
        let mut elapsed_total = Duration::ZERO;
        for h in handles {
            elapsed_total += h.join().expect("worker panicked");
        }
        // 8 acquires through a 4-token bucket at 20 tokens/sec.
        // First 4 are immediate; remaining 4 each wait ~50 ms ≈
        // 200 ms aggregate. The total of `elapsed_total` is the
        // *sum* of per-thread durations; with serialised sleeps
        // it should be at least ~200 ms aggregate.
        assert!(
            elapsed_total >= Duration::from_millis(100),
            "expected aggregate wait ≥ 100 ms; got {elapsed_total:?}"
        );
    }
}
