//! HTTP transport for connectors.
//!
//! Per `docs/technical/design.md` §10.2 every connector talks to its source
//! system over HTTP(S). To keep the [`crate::Connector`] trait pure
//! and unit-testable, connectors don't depend on a concrete HTTP
//! client directly — they hold a [`HttpTransport`] trait object and
//! the production runtime wires in [`BlockingHttpTransport`] (a
//! reqwest-backed blocking client behind the `http-client` feature
//! flag).
//!
//! The trait deliberately exposes a tiny surface — `get`, `post`,
//! `request` — because that's the entire set of verbs the connector
//! framework needs:
//!
//! * `get` → list / page / delta endpoints.
//! * `post` → OAuth2 token endpoints, webhook subscriptions,
//!   per-provider search APIs that use POST bodies (Notion).
//! * `request` → escape hatch for `PATCH`/`PUT`/`DELETE` if a
//!   provider needs it (HubSpot batch updates, Slack file ops).
//!
//! The transport handles retries, exponential backoff, `Retry-After`
//! parsing for 429 / 503 responses, and request-level timeouts. The
//! connector code stays declarative.
//!
//! A [`MockHttpTransport`] is shipped behind the `test-support`
//! feature flag so neighbouring crates can compose canned responses
//! per `(method, url)` tuple for unit tests.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// HTTP method — enumerated rather than `&str` so the transport
/// implementation can do exhaustive `match` and so connector code
/// can't accidentally typo `"GET"` vs `"Get"`.
///
/// Serialises as the wire-format string (`"GET"`, `"POST"`, …) so it
/// reads cleanly inside on-disk [`cassette`](crate::cassette)
/// fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HttpMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl HttpMethod {
    /// Render as the standard wire-format string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// One HTTP request — built by the connector, executed by the
/// transport.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// Wire method.
    pub method: HttpMethod,
    /// Absolute request URL (the transport does not concatenate
    /// path segments; connectors are responsible for building the
    /// full URL).
    pub url: String,
    /// Request headers in declaration order (some providers — Slack,
    /// Atlassian — require `Authorization` to appear before custom
    /// headers).
    pub headers: Vec<(String, String)>,
    /// Optional request body. Empty for GET / DELETE; populated for
    /// POST / PUT / PATCH. Body content-type is conveyed via
    /// [`Self::headers`] — the transport does not infer it.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Build a GET request with no body.
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Build a POST request with a body.
    #[must_use]
    pub fn post(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// Add a header. Returns `self` so calls can be chained.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Add a bearer-token `Authorization` header. The transport
    /// **must not** log the resulting header value — `Display`
    /// implementations on this type intentionally omit the body and
    /// headers to avoid leaking secrets in error traces.
    #[must_use]
    pub fn with_bearer(self, token: &str) -> Self {
        self.with_header("Authorization", format!("Bearer {token}"))
    }
}

/// One HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code (e.g. 200, 401, 429).
    pub status: u16,
    /// Response headers as `(name, value)` pairs, lower-cased on the
    /// `name` side so connectors can do `==` comparisons without
    /// caring about provider header casing.
    pub headers: Vec<(String, String)>,
    /// Response body bytes — typically JSON, but the transport
    /// doesn't assume.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// True iff `status` is in `[200, 300)`.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// True iff `status` represents a transient error worth retrying.
    ///
    /// The set we treat as transient is the conventional SaaS-API retry
    /// list — 408 (request timeout), 429 (too many requests), 500
    /// (internal server error), 502 (bad gateway), 503 (service
    /// unavailable), 504 (gateway timeout) — i.e. everything reqwest's
    /// `retry-policies`, Python `urllib3.Retry(status_forcelist=[500,
    /// 502, 503, 504])`, the Atlassian / Notion / HubSpot SDKs, and the
    /// AWS / Azure SDKs all retry by default.
    ///
    /// 500 is intentionally included even though the strict reading of
    /// the spec ("the server encountered an unexpected condition")
    /// could be a deterministic bug that retry won't fix. In practice
    /// every cloud SaaS this substrate targets — Slack, Notion,
    /// Atlassian (Jira / Confluence), Google Drive, Microsoft Graph,
    /// HubSpot, Figma — issues 500s for transient capacity / load /
    /// dependency-blip events that succeed on retry. Treating 500 as
    /// permanent here would surface those as connector-sync failures
    /// to the host, which would then have to add its own retry layer
    /// on top — defeating the purpose of having a transport-level
    /// retry policy. The exponential backoff in [`RetryPolicy::backoff`]
    /// keeps us from hammering a truly-broken upstream.
    ///
    /// Safety for non-idempotent POSTs: every POST the connector
    /// framework issues is either (a) an OAuth2 token endpoint, where a
    /// 500 has not consumed the `code` / `refresh_token` (a successful
    /// consume would have returned 2xx); a subsequent retry that lands
    /// on a healthy server either succeeds or gets a 4xx that
    /// `is_transient` returns false for; (b) a webhook subscription
    /// create, which providers dedup by callback URL; or (c) a
    /// provider-specific search endpoint (Notion's `/v1/search`) that
    /// is server-side idempotent. Connectors that need non-idempotent
    /// POST semantics must opt out of transport-level retry by setting
    /// [`RetryPolicy::max_retries`] to 0 on a per-call basis.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self.status, 408 | 429 | 500 | 502 | 503 | 504)
    }

    /// Look up a response header by lower-cased name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let needle = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &needle)
            .map(|(_, v)| v.as_str())
    }

    /// Parse a `Retry-After` header — supports both the integer
    /// "seconds" form (`Retry-After: 30`) and the HTTP-date form
    /// (`Retry-After: Wed, 21 Oct 2015 07:28:00 GMT`). The date form
    /// is rare in practice (Slack / Notion / Atlassian all return
    /// seconds), so the transport only handles the integer case and
    /// returns `None` for date-form values.
    #[must_use]
    pub fn retry_after_seconds(&self) -> Option<u64> {
        self.header("retry-after")?.trim().parse::<u64>().ok()
    }
}

/// Transport abstraction. Connectors hold a `Box<dyn HttpTransport>`
/// and the runtime wires in either [`BlockingHttpTransport`]
/// (production, `http-client` feature) or [`MockHttpTransport`]
/// (tests, `test-support` feature).
///
/// The trait is intentionally **blocking** — connectors are called
/// from `spawn_blocking` on the async runtime side, and a sync
/// transport keeps the connector framework free of `tokio`. A future
/// async variant could be layered on top without rewriting the
/// connectors.
pub trait HttpTransport: Send + Sync {
    /// Execute one HTTP request, applying retries / backoff per the
    /// transport's policy. Implementations should map low-level
    /// transport errors to [`ConnectorError::Transport`]; HTTP
    /// status codes (including 4xx) are surfaced via
    /// [`HttpResponse::status`] rather than as `Err`.
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse>;

    /// Convenience: `GET url`. Default impl builds an [`HttpRequest`]
    /// and dispatches via [`Self::execute`].
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
        let mut req = HttpRequest::get(url);
        for (k, v) in headers {
            req = req.with_header(*k, *v);
        }
        self.execute(req)
    }

    /// Convenience: `POST url` with a body.
    fn post(&self, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<HttpResponse> {
        let mut req = HttpRequest::post(url, body.to_vec());
        for (k, v) in headers {
            req = req.with_header(*k, *v);
        }
        self.execute(req)
    }
}

// ───────────── retry policy ─────────────

/// Hard ceiling on how long the transport will sleep in response to
/// a server-provided `Retry-After` hint, irrespective of the
/// configured exponential `max_backoff`. A misbehaving or adversarial
/// upstream returning `Retry-After: 86400` (24h) would otherwise stall
/// the connector runtime's blocking thread pool for a whole day.
/// Five minutes is a generous-but-bounded compromise: long enough to
/// respect a real rate-limit window from Slack / Notion / Atlassian
/// (their typical `Retry-After` is single-digit seconds, occasionally
/// up to a minute), short enough that a runaway hint won't pin a
/// substrate thread indefinitely. Callers that need a higher ceiling
/// can construct a policy via [`RetryPolicy::with_max_retry_after`].
pub const DEFAULT_MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

/// Retry policy for [`BlockingHttpTransport`] and any other
/// transport that wants to reuse it. Defaults to three retries with
/// exponential backoff starting at 250ms and doubling, capped at 5s.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts after the first try (so a
    /// value of `3` means up to 4 total HTTP requests).
    pub max_retries: u32,
    /// Initial backoff duration. Doubled on each attempt.
    pub initial_backoff: Duration,
    /// Cap on the per-attempt backoff (after exponential growth).
    pub max_backoff: Duration,
    /// Cap on the per-attempt sleep when the server emits a
    /// `Retry-After` hint. Server hints are honoured (the transport
    /// sleeps at least this long) but bounded so a single
    /// misbehaving upstream can't pin a substrate thread for hours.
    /// Defaults to [`DEFAULT_MAX_RETRY_AFTER`].
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
        }
    }
}

impl RetryPolicy {
    /// A no-retry policy — useful for tests and probes where we
    /// want the first failure to surface immediately.
    #[must_use]
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            max_retry_after: Duration::ZERO,
        }
    }

    /// Override the [`Self::max_retry_after`] ceiling, returning the
    /// updated policy. Useful for tests and for the rare connector
    /// whose provider documentation specifies multi-minute rate-limit
    /// windows (e.g. Atlassian's per-app limit).
    #[must_use]
    pub fn with_max_retry_after(mut self, cap: Duration) -> Self {
        self.max_retry_after = cap;
        self
    }

    /// Compute the backoff for `attempt` (1-indexed: `attempt == 1`
    /// is the first retry after the initial request failed).
    /// Honours an optional server-provided `retry_after` hint —
    /// the larger of the exponential value and the hint wins so
    /// the substrate respects rate-limit windows without going
    /// below its own minimum, but the hint is bounded above by
    /// [`Self::max_retry_after`] so a misbehaving server returning
    /// `Retry-After: 86400` cannot stall a substrate thread for
    /// hours.
    #[must_use]
    pub fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exp = self
            .initial_backoff
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)));
        let exp = exp.min(self.max_backoff);
        match retry_after {
            Some(server) => {
                // Honour the hint but clamp to `max_retry_after` so a
                // pathological upstream value can't blow past our
                // ceiling. The `.max(exp)` floor still applies — we
                // never sleep less than the local exponential value.
                let bounded = server.min(self.max_retry_after);
                bounded.max(exp)
            }
            None => exp,
        }
    }
}

// ───────────── reqwest-backed transport (http-client feature) ─────────────

#[cfg(feature = "http-client")]
mod blocking_impl {
    use super::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, RetryPolicy};
    use crate::error::{ConnectorError, Result};
    use crate::provider_rate_limiter::{provider_key_for_url, ProviderRateLimiter};
    use std::sync::Arc;
    use std::time::Duration;

    /// Default request timeout. Connector list / delta endpoints
    /// typically return in seconds; the substrate cap is 30s so a
    /// hung provider can't stall the connector runtime indefinitely.
    pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30;

    /// Real HTTP transport backed by `reqwest::blocking::Client`.
    ///
    /// Configured with a single timeout that applies to the entire
    /// request (connect + send + receive). Retries on transient
    /// failures (transport-level errors and `is_transient()`
    /// responses) up to [`RetryPolicy::max_retries`] times,
    /// respecting `Retry-After` on 429 / 503.
    ///
    /// # Rate-limit accounting contract
    ///
    /// The optional [`ProviderRateLimiter`] is consumed **once per
    /// logical [`HttpTransport::execute`] call** — i.e. one token
    /// is deducted from the provider's bucket per *logical*
    /// operation requested by the caller, regardless of how many
    /// *physical* HTTP attempts that operation expands into via
    /// the retry loop. The deduction happens *before* the first
    /// physical attempt; subsequent retries on the same logical
    /// call do not consume additional tokens.
    ///
    /// This intentional asymmetry exists because:
    ///
    /// * Provider-published quotas (e.g. Slack's `Tier 3` 50 rpm,
    ///   Notion's 3 rps) are documented per *successful logical
    ///   request*; the provider's *own* 429 / Retry-After
    ///   mechanism already throttles per *physical* attempt, so
    ///   double-counting attempts here would conflict with the
    ///   provider's intent.
    /// * Charging retries against the bucket would let a single
    ///   misbehaving endpoint exhaust the entire provider quota
    ///   via repeated 429s, starving other endpoints on the same
    ///   host (e.g. a flaky `users.list` would starve `chat.post`).
    /// * Operators reason about cost in terms of *logical
    ///   operations performed by the substrate*, not the
    ///   `reqwest`-level packet count, so per-logical accounting
    ///   matches what they see in dashboards and bill lines.
    ///
    /// Implications for operators:
    ///
    /// * If your bucket is calibrated to a provider's published
    ///   per-second quota, actual outbound QPS *can* briefly
    ///   exceed it during a retry burst — the spike is bounded
    ///   by `1 + RetryPolicy::max_retries` extra packets per
    ///   logical call, and is *intentionally* gated by the
    ///   provider's own back-pressure rather than the local
    ///   bucket.
    /// * If you need strict per-packet rate-limiting (e.g. for a
    ///   provider that itself bills per attempt), use
    ///   `RetryPolicy::default()` with `max_retries = 0`, which
    ///   collapses logical-and-physical 1:1.
    #[derive(Debug, Clone)]
    pub struct BlockingHttpTransport {
        client: reqwest::blocking::Client,
        retry: RetryPolicy,
        /// Optional per-provider rate limiter (Item 21).
        ///
        /// When `Some`, every `execute` call calls
        /// `limiter.acquire(provider_key_for_url(&request.url))`
        /// before dispatching the underlying HTTP request — so the
        /// connector runtime can pin one token bucket per provider
        /// host (e.g. `api.notion.com`, `graph.microsoft.com`) and
        /// keep aggregate outbound QPS inside the provider's
        /// published per-tenant quota. Wrapped in `Arc` so the same
        /// limiter can be shared across many transports.
        rate_limiter: Option<Arc<ProviderRateLimiter>>,
    }

    impl BlockingHttpTransport {
        /// Construct with the default 30s timeout and the default
        /// retry policy.
        ///
        /// # Errors
        ///
        /// Returns [`ConnectorError::Transport`] if the underlying
        /// reqwest client builder rejects the timeout configuration.
        pub fn new() -> Result<Self> {
            Self::with_timeout(Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
        }

        /// Construct with a custom timeout.
        ///
        /// # Errors
        ///
        /// Returns [`ConnectorError::Transport`] if the underlying
        /// reqwest client builder rejects the timeout configuration.
        pub fn with_timeout(timeout: Duration) -> Result<Self> {
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| ConnectorError::Transport(format!("reqwest build failed: {e}")))?;
            Ok(Self {
                client,
                retry: RetryPolicy::default(),
                rate_limiter: None,
            })
        }

        /// Override the retry policy. Chainable.
        #[must_use]
        pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
            self.retry = retry;
            self
        }

        /// Attach a shared per-provider rate limiter. Chainable.
        ///
        /// Every `execute` call will call
        /// `limiter.acquire(provider_key_for_url(&request.url))`
        /// before dispatching, so aggregate outbound traffic
        /// against each provider host stays inside the operator's
        /// per-tenant quota.
        #[must_use]
        pub fn with_provider_rate_limiter(mut self, limiter: Arc<ProviderRateLimiter>) -> Self {
            self.rate_limiter = Some(limiter);
            self
        }

        /// Borrow the attached rate limiter, if any.
        #[must_use]
        pub fn rate_limiter(&self) -> Option<&Arc<ProviderRateLimiter>> {
            self.rate_limiter.as_ref()
        }

        /// Borrow the underlying reqwest client. Exposed for
        /// integration with tooling that wants to share connection
        /// pooling (e.g. metrics middleware in a future revision).
        #[must_use]
        pub fn client(&self) -> &reqwest::blocking::Client {
            &self.client
        }

        fn execute_once(&self, request: &HttpRequest) -> Result<HttpResponse> {
            let method = match request.method {
                HttpMethod::Get => reqwest::Method::GET,
                HttpMethod::Post => reqwest::Method::POST,
                HttpMethod::Put => reqwest::Method::PUT,
                HttpMethod::Patch => reqwest::Method::PATCH,
                HttpMethod::Delete => reqwest::Method::DELETE,
            };
            let mut builder = self.client.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if !request.body.is_empty() {
                builder = builder.body(request.body.clone());
            }
            let resp = builder.send().map_err(|e| {
                // Strip the URL from the error string — reqwest's
                // `Display` impl includes the request URL which can
                // contain sensitive query parameters (page tokens,
                // search terms).
                ConnectorError::Transport(format!("send failed: {}", scrub_url(&e.to_string())))
            })?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_ascii_lowercase(),
                        v.to_str().unwrap_or("").to_string(),
                    )
                })
                .collect();
            let body = resp
                .bytes()
                .map_err(|e| ConnectorError::Transport(format!("read body failed: {e}")))?
                .to_vec();
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        }
    }

    impl HttpTransport for BlockingHttpTransport {
        fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            // Cost-control gate. The limiter blocks (sleeps) until
            // a token is available for `provider_key_for_url(&url)`;
            // we hold no inner lock across the sleep so other
            // transports targeting a *different* provider host
            // continue to make progress.
            if let Some(limiter) = &self.rate_limiter {
                let key = provider_key_for_url(&request.url);
                limiter.acquire(&key);
            }
            for attempt in 0..=self.retry.max_retries {
                match self.execute_once(&request) {
                    Ok(resp) if resp.is_transient() && attempt < self.retry.max_retries => {
                        let hint = resp.retry_after_seconds().map(Duration::from_secs);
                        let wait = self.retry.backoff(attempt + 1, hint);
                        std::thread::sleep(wait);
                        continue;
                    }
                    Ok(resp) => return Ok(resp),
                    Err(_) if attempt < self.retry.max_retries => {
                        // Transient transport failure (reqwest-level —
                        // DNS, connect, read timeout). We drop the
                        // error here; if every retry fails the final
                        // iteration returns the *last* error (via the
                        // arm below), which is the most useful for
                        // diagnosing systemic outages.
                        let wait = self.retry.backoff(attempt + 1, None);
                        std::thread::sleep(wait);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            // Unreachable: every iteration of `0..=max_retries` either
            // returns (`Ok(resp) | Err(e)` on the final attempt) or
            // continues (the only `continue` paths trip on
            // `attempt < max_retries`, which is false once
            // `attempt == max_retries`). The loop therefore cannot
            // complete normally. We use `unreachable!()` (which
            // panics if the invariant is ever broken by a future
            // refactor) instead of a synthetic `Err` that would lie
            // about which retry failed.
            unreachable!(
                "BlockingHttpTransport retry loop must always return inside the loop body",
            );
        }
    }

    /// Scrub a URL out of a reqwest error string. Reqwest formats
    /// errors as `"... for url (https://...)"`; we keep the prefix
    /// and drop the parenthesised URL so credentials in the path /
    /// query don't leak into log lines.
    fn scrub_url(s: &str) -> String {
        if let Some(idx) = s.find(" for url") {
            s[..idx].to_string()
        } else {
            s.to_string()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn constructs_with_default_timeout() {
            BlockingHttpTransport::new().expect("default builder should succeed");
        }

        #[test]
        fn with_timeout_custom() {
            BlockingHttpTransport::with_timeout(Duration::from_secs(5))
                .expect("custom timeout should succeed");
        }

        #[test]
        fn scrub_url_strips_url_segment() {
            let s = "error sending request for url (https://api.example.com/secret?token=x)";
            assert_eq!(scrub_url(s), "error sending request");
        }

        #[test]
        fn scrub_url_passthrough() {
            let s = "operation timed out";
            assert_eq!(scrub_url(s), s);
        }
    }
}

#[cfg(feature = "http-client")]
pub use blocking_impl::{BlockingHttpTransport, DEFAULT_HTTP_TIMEOUT_SECS};

// ───────────── MockHttpTransport (test-support feature) ─────────────

#[cfg(any(test, feature = "test-support"))]
mod mock_impl {
    use super::{HttpMethod, HttpRequest, HttpResponse, HttpTransport};
    use crate::error::Result;
    use std::sync::Mutex;

    /// Canned response keyed by `(method, url)`. Multiple responses
    /// can be registered for the same key; they're returned in
    /// FIFO order so tests can model multi-call sequences (page 1 →
    /// page 2 → empty).
    #[derive(Debug, Clone)]
    pub struct MockResponse {
        /// HTTP status to return.
        pub status: u16,
        /// Response headers.
        pub headers: Vec<(String, String)>,
        /// Response body bytes.
        pub body: Vec<u8>,
    }

    impl MockResponse {
        /// Build a 200 OK with a JSON body.
        #[must_use]
        pub fn ok_json(body: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.into(),
            }
        }

        /// Build a transient failure (429 / Retry-After: 1).
        #[must_use]
        pub fn too_many_requests() -> Self {
            Self {
                status: 429,
                headers: vec![("retry-after".into(), "1".into())],
                body: br#"{"error":"rate_limited"}"#.to_vec(),
            }
        }

        /// Build a response with an arbitrary status and body. The
        /// `content-type` header is left unset so callers can pin
        /// the exact shape they need (e.g. text/plain for error
        /// bodies).
        #[must_use]
        pub fn status(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }
    }

    /// Recorded request — useful for asserting that the connector
    /// hit the right endpoint with the right headers / body.
    #[derive(Debug, Clone)]
    pub struct RecordedRequest {
        /// Method.
        pub method: HttpMethod,
        /// URL.
        pub url: String,
        /// Headers as `(name, value)` pairs.
        pub headers: Vec<(String, String)>,
        /// Body bytes.
        pub body: Vec<u8>,
    }

    /// In-memory HTTP transport for unit tests. Holds a list of
    /// canned responses keyed by `(method, url)` and records every
    /// request it sees so tests can assert on the wire-level shape.
    #[derive(Debug, Default)]
    pub struct MockHttpTransport {
        responses: Mutex<Vec<(HttpMethod, String, Vec<MockResponse>)>>,
        recorded: Mutex<Vec<RecordedRequest>>,
        default_response: Mutex<Option<MockResponse>>,
    }

    impl MockHttpTransport {
        /// Construct an empty mock.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Register a canned response for `(method, url)`. Calls
        /// stack in FIFO order — the first call to the matching
        /// `(method, url)` pair returns the first response added,
        /// the next call returns the second, and so on. Once
        /// exhausted, subsequent calls fall back to
        /// [`Self::with_default_response`] if set, otherwise emit a
        /// synthetic 404.
        pub fn expect(&self, method: HttpMethod, url: impl Into<String>, response: MockResponse) {
            let url = url.into();
            let mut responses = self.responses.lock().expect("mock responses lock");
            if let Some((_, _, existing)) = responses
                .iter_mut()
                .find(|(m, u, _)| *m == method && u == &url)
            {
                existing.push(response);
            } else {
                responses.push((method, url, vec![response]));
            }
        }

        /// Set a fallback response used when no canned `(method,
        /// url)` match exists. Useful for tests that don't care
        /// about the precise endpoint and just want to assert the
        /// connector dispatched something.
        pub fn with_default_response(&self, response: MockResponse) {
            *self.default_response.lock().expect("default lock") = Some(response);
        }

        /// Borrow the list of recorded requests.
        #[must_use]
        pub fn recorded(&self) -> Vec<RecordedRequest> {
            self.recorded.lock().expect("recorded lock").clone()
        }
    }

    /// Normalise header names to lowercase so the mock honours the
    /// `HttpResponse::headers` invariant ("`name` side is lowercased").
    /// `BlockingHttpTransport` enforces this for real network
    /// responses; the mock must do the same so tests that construct
    /// `MockResponse` with mixed-case names (`"Retry-After"`) still
    /// behave correctly when callers look the value up via
    /// `HttpResponse::header()` (which lowercases the needle).
    fn lowercase_header_names(headers: Vec<(String, String)>) -> Vec<(String, String)> {
        headers
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect()
    }

    impl HttpTransport for MockHttpTransport {
        fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.recorded
                .lock()
                .expect("recorded lock")
                .push(RecordedRequest {
                    method: request.method,
                    url: request.url.clone(),
                    headers: request.headers.clone(),
                    body: request.body.clone(),
                });
            let mut responses = self.responses.lock().expect("mock responses lock");
            if let Some((_, _, queue)) = responses
                .iter_mut()
                .find(|(m, u, _)| *m == request.method && u == &request.url)
            {
                if !queue.is_empty() {
                    let r = queue.remove(0);
                    return Ok(HttpResponse {
                        status: r.status,
                        headers: lowercase_header_names(r.headers),
                        body: r.body,
                    });
                }
            }
            if let Some(r) = self.default_response.lock().expect("default lock").clone() {
                return Ok(HttpResponse {
                    status: r.status,
                    headers: lowercase_header_names(r.headers),
                    body: r.body,
                });
            }
            // No canned response, no default — fail loud rather than
            // hiding the test bug behind a synthetic 200.
            Ok(HttpResponse {
                status: 404,
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"error":"mock_not_configured"}"#.to_vec(),
            })
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use mock_impl::{MockHttpTransport, MockResponse, RecordedRequest};

// ───────────── tests ─────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_default_is_three_retries() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries, 3);
        assert!(p.initial_backoff <= p.max_backoff);
    }

    #[test]
    fn retry_policy_backoff_doubles() {
        let p = RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
        };
        assert_eq!(p.backoff(1, None), Duration::from_millis(100));
        assert_eq!(p.backoff(2, None), Duration::from_millis(200));
        assert_eq!(p.backoff(3, None), Duration::from_millis(400));
    }

    #[test]
    fn retry_policy_backoff_caps_at_max() {
        let p = RetryPolicy {
            max_retries: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(3),
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
        };
        // 1s, 2s, 4s -> capped at 3s, then sticks.
        assert_eq!(p.backoff(1, None), Duration::from_secs(1));
        assert_eq!(p.backoff(2, None), Duration::from_secs(2));
        assert_eq!(p.backoff(3, None), Duration::from_secs(3));
        assert_eq!(p.backoff(10, None), Duration::from_secs(3));
    }

    #[test]
    fn retry_policy_honours_server_hint() {
        let p = RetryPolicy::default();
        // Server says "wait 60s" — that beats our exponential.
        assert_eq!(
            p.backoff(1, Some(Duration::from_secs(60))),
            Duration::from_secs(60)
        );
        // Server says "wait 1ms" — our exponential floor still wins.
        let exp = p.backoff(2, None);
        assert!(p.backoff(2, Some(Duration::from_millis(1))) >= exp);
    }

    /// Regression: a pathological `Retry-After` value (24 hours) must
    /// be clamped at [`DEFAULT_MAX_RETRY_AFTER`] (5 minutes) so a
    /// misbehaving upstream cannot pin a substrate blocking thread
    /// for arbitrarily long.
    #[test]
    fn retry_policy_caps_pathological_retry_after_hint() {
        let p = RetryPolicy::default();
        let pathological = Duration::from_secs(86_400); // 24 hours
        let observed = p.backoff(1, Some(pathological));
        assert_eq!(
            observed, DEFAULT_MAX_RETRY_AFTER,
            "Retry-After hint must be clamped to DEFAULT_MAX_RETRY_AFTER"
        );
    }

    /// A caller that has a documented multi-minute rate-limit window
    /// can raise the cap via [`RetryPolicy::with_max_retry_after`].
    #[test]
    fn retry_policy_with_max_retry_after_raises_cap() {
        let p = RetryPolicy::default().with_max_retry_after(Duration::from_secs(900));
        let server_hint = Duration::from_secs(600);
        // Server hint < cap → server hint wins (still respected).
        assert_eq!(p.backoff(1, Some(server_hint)), server_hint);
        // Server hint > cap → clamped to cap.
        let too_long = Duration::from_secs(3_600);
        assert_eq!(p.backoff(1, Some(too_long)), Duration::from_secs(900));
    }

    #[test]
    fn no_retry_policy_has_zero_retries() {
        let p = RetryPolicy::no_retry();
        assert_eq!(p.max_retries, 0);
    }

    #[test]
    fn http_response_is_success_in_2xx() {
        let r = HttpResponse {
            status: 204,
            headers: vec![],
            body: vec![],
        };
        assert!(r.is_success());
    }

    #[test]
    fn http_response_is_transient_for_standard_retry_set() {
        // Pin the exact retry set so a future change to `is_transient`
        // (e.g. adding 425 Too Early, or stripping 500 again) shows up
        // here loudly instead of silently shifting connector retry
        // behaviour across the whole substrate. 500 is part of the
        // retry set — see the `is_transient` doc for the rationale
        // (every cloud SaaS we target issues 500 for transient
        // capacity / dependency-blip events that succeed on retry).
        for s in [408u16, 429, 500, 502, 503, 504] {
            assert!(
                HttpResponse {
                    status: s,
                    headers: vec![],
                    body: vec![],
                }
                .is_transient(),
                "status {s} should be transient"
            );
        }
        for s in [200u16, 301, 400, 401, 403, 404, 409, 422] {
            assert!(
                !HttpResponse {
                    status: s,
                    headers: vec![],
                    body: vec![],
                }
                .is_transient(),
                "status {s} should NOT be transient"
            );
        }
    }

    #[test]
    fn http_response_retry_after_parses_integer_seconds() {
        let r = HttpResponse {
            status: 429,
            headers: vec![("retry-after".into(), "30".into())],
            body: vec![],
        };
        assert_eq!(r.retry_after_seconds(), Some(30));
    }

    #[test]
    fn http_response_retry_after_rejects_date_form() {
        let r = HttpResponse {
            status: 429,
            headers: vec![("retry-after".into(), "Wed, 21 Oct 2015 07:28:00 GMT".into())],
            body: vec![],
        };
        assert_eq!(r.retry_after_seconds(), None);
    }

    #[test]
    fn http_request_builder_appends_headers() {
        let req = HttpRequest::get("https://api.example.com/v1/items")
            .with_header("X-Trace", "abc")
            .with_bearer("xyzzy");
        assert_eq!(req.method, HttpMethod::Get);
        assert_eq!(req.headers.len(), 2);
        assert_eq!(req.headers[0], ("X-Trace".into(), "abc".into()));
        assert_eq!(
            req.headers[1],
            ("Authorization".into(), "Bearer xyzzy".into())
        );
    }

    #[test]
    fn mock_transport_returns_canned_response() {
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            "https://api.example.com/users",
            MockResponse::ok_json(br#"{"items":[]}"#.to_vec()),
        );
        let resp = mock
            .get("https://api.example.com/users", &[])
            .expect("mock get");
        assert_eq!(resp.status, 200);
        assert!(resp.is_success());
        assert_eq!(&resp.body, br#"{"items":[]}"#);
    }

    #[test]
    fn mock_transport_returns_fifo_sequence() {
        let mock = MockHttpTransport::new();
        for i in 0..3 {
            mock.expect(
                HttpMethod::Get,
                "https://api.example.com/page",
                MockResponse::ok_json(format!(r#"{{"page":{i}}}"#).into_bytes()),
            );
        }
        for i in 0..3 {
            let resp = mock.get("https://api.example.com/page", &[]).expect("get");
            assert_eq!(resp.body, format!(r#"{{"page":{i}}}"#).into_bytes());
        }
    }

    #[test]
    fn mock_transport_lowercases_response_header_names() {
        // Regression: tests that pass `MockResponse` with mixed-case
        // header names (`"Retry-After"`, `"Content-Type"`) used to
        // pass the keys through verbatim, breaking
        // `HttpResponse::header()` (which lowercases the needle)
        // for any caller that constructed the mock with the
        // wire-style casing.
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            "https://api.example.com/x",
            MockResponse {
                status: 429,
                headers: vec![
                    ("Retry-After".into(), "12".into()),
                    ("Content-Type".into(), "application/json".into()),
                ],
                body: br#"{"error":"slow_down"}"#.to_vec(),
            },
        );
        let resp = mock
            .get("https://api.example.com/x", &[])
            .expect("mock get");
        assert_eq!(resp.status, 429);
        assert_eq!(resp.header("retry-after"), Some("12"));
        assert_eq!(resp.header("Retry-After"), Some("12"));
        assert_eq!(resp.header("content-type"), Some("application/json"));
    }

    #[test]
    fn mock_transport_records_requests() {
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Post,
            "https://api.example.com/token",
            MockResponse::ok_json(br#"{"access_token":"a"}"#.to_vec()),
        );
        let _ = mock.post(
            "https://api.example.com/token",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            b"grant_type=refresh_token&refresh_token=r",
        );
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].method, HttpMethod::Post);
        assert_eq!(recorded[0].url, "https://api.example.com/token");
        assert_eq!(
            recorded[0].body,
            b"grant_type=refresh_token&refresh_token=r"
        );
    }

    #[test]
    fn mock_transport_falls_through_to_default() {
        let mock = MockHttpTransport::new();
        mock.with_default_response(MockResponse::ok_json(br#"{"default":true}"#.to_vec()));
        let resp = mock.get("https://nowhere/", &[]).expect("get");
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body, br#"{"default":true}"#);
    }

    #[test]
    fn mock_transport_emits_404_when_unconfigured() {
        let mock = MockHttpTransport::new();
        let resp = mock.get("https://nowhere/", &[]).expect("get");
        assert_eq!(resp.status, 404);
    }
}
