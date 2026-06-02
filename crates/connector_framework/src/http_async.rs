//!  — async reqwest-backed HTTP transport.
//!
//! Mirror of [`BlockingHttpTransport`](crate::http::BlockingHttpTransport)
//! that drives the request loop with `tokio::time::sleep` instead of
//! `std::thread::sleep`, lets the substrate share a single connection
//! pool across many concurrent connectors, and reuses the existing
//! [`RetryPolicy`](crate::http::RetryPolicy) / [`HttpResponse`](crate::http::HttpResponse)
//! types so connectors don't need to learn a second wire shape.
//!
//! The async transport is gated behind the `async-http-client` feature
//! (which itself enables `async-runtime`). A substrate host that doesn't
//! need network I/O at all (e.g. offline cross-checks) builds the crate
//! without either feature; a host that wants to drive sync connectors
//! from an async runtime can enable `async-runtime` alone (the
//! [`BlockingConnectorAdapter`](crate::async_runtime::BlockingConnectorAdapter)
//! works without reqwest).

use std::time::Duration;

use async_trait::async_trait;

use crate::{
    async_runtime::AsyncHttpTransport,
    error::{ConnectorError, Result},
    http::{HttpMethod, HttpRequest, HttpResponse, RetryPolicy},
};

/// Default request timeout for the async transport. Mirrors
/// [`crate::http::DEFAULT_HTTP_TIMEOUT_SECS`] so the async and sync
/// surfaces have identical behaviour out of the box.
pub const DEFAULT_ASYNC_HTTP_TIMEOUT_SECS: u64 = 30;

/// Real async HTTP transport backed by `reqwest::Client`.
///
/// Configured with a single timeout that applies to the entire
/// request (connect + send + receive). Retries on transient failures
/// (transport-level errors and `is_transient()` responses) up to
/// [`RetryPolicy::max_retries`] times, respecting `Retry-After` on
/// 429 / 503. Uses `tokio::time::sleep` between attempts so the
/// runtime can drive other tasks while the transport backs off.
#[derive(Debug, Clone)]
pub struct ReqwestAsyncHttpTransport {
    client: reqwest::Client,
    retry: RetryPolicy,
}

impl ReqwestAsyncHttpTransport {
    /// Construct with the default 30s timeout and the default retry
    /// policy.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if the underlying
    /// reqwest builder rejects the timeout configuration.
    pub fn new() -> Result<Self> {
        Self::with_timeout(Duration::from_secs(DEFAULT_ASYNC_HTTP_TIMEOUT_SECS))
    }

    /// Construct with a custom timeout.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if the underlying
    /// reqwest builder rejects the timeout configuration.
    pub fn with_timeout(timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ConnectorError::Transport(format!("reqwest build failed: {e}")))?;
        Ok(Self {
            client,
            retry: RetryPolicy::default(),
        })
    }

    /// Construct from an externally-built reqwest client (e.g. one
    /// pre-configured with proxy / TLS / connection-pool settings
    /// the substrate manages globally).
    #[must_use]
    pub fn from_client(client: reqwest::Client) -> Self {
        Self {
            client,
            retry: RetryPolicy::default(),
        }
    }

    /// Override the retry policy. Chainable.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Borrow the underlying reqwest client. Exposed so the
    /// substrate can share connection pooling across multiple
    /// transports (e.g. one tuned for OAuth, one for general
    /// traffic).
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Borrow the active retry policy.
    #[must_use]
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry
    }

    async fn execute_once(&self, request: &HttpRequest) -> Result<HttpResponse> {
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
        let resp = builder.send().await.map_err(|e| {
            // Strip the URL from the error string — reqwest's
            // `Display` impl includes the request URL which can
            // contain sensitive query parameters.
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
            .await
            .map_err(|e| ConnectorError::Transport(format!("read body failed: {e}")))?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[async_trait]
impl AsyncHttpTransport for ReqwestAsyncHttpTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        for attempt in 0..=self.retry.max_retries {
            match self.execute_once(&request).await {
                Ok(resp) if resp.is_transient() && attempt < self.retry.max_retries => {
                    let hint = resp.retry_after_seconds().map(Duration::from_secs);
                    let wait = self.retry.backoff(attempt + 1, hint);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Ok(resp) => return Ok(resp),
                Err(_) if attempt < self.retry.max_retries => {
                    // Transient transport failure (reqwest-level —
                    // DNS, connect, read timeout). The error is
                    // dropped here so the next attempt can run; if
                    // every retry fails the final iteration returns
                    // the *last* error (via the arm below), which
                    // is the most useful for diagnosing outages.
                    let wait = self.retry.backoff(attempt + 1, None);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        // Unreachable: every iteration of `0..=max_retries` either
        // returns or continues; the only `continue` paths require
        // `attempt < max_retries`, which is false on the final
        // iteration. We panic on a future refactor that breaks the
        // invariant rather than fabricate an opaque `Err`.
        unreachable!(
            "ReqwestAsyncHttpTransport retry loop must always return inside the loop body",
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
        ReqwestAsyncHttpTransport::new().expect("default builder should succeed");
    }

    #[test]
    fn with_timeout_custom() {
        ReqwestAsyncHttpTransport::with_timeout(Duration::from_secs(5))
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

    #[test]
    fn from_client_preserves_default_retry() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(7))
            .build()
            .expect("builder ok");
        let t = ReqwestAsyncHttpTransport::from_client(client).with_retry(RetryPolicy::no_retry());
        assert_eq!(t.retry_policy().max_retries, 0);
    }

    /// Validates the retry loop terminates with a transport error
    /// when every attempt fails (here: an unreachable host on a
    /// bogus port). Uses a tiny timeout + no-retry policy so the
    /// test is fast on offline CI.
    #[tokio::test]
    async fn execute_returns_transport_error_for_unreachable_host() {
        let transport = ReqwestAsyncHttpTransport::with_timeout(Duration::from_millis(200))
            .expect("builder")
            .with_retry(RetryPolicy::no_retry());
        let req = HttpRequest::get("http://127.0.0.1:1/");
        let err = transport.execute(req).await.expect_err("must fail");
        assert!(matches!(err, ConnectorError::Transport(_)));
    }
}
