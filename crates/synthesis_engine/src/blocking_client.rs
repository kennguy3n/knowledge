//! Production blocking [`HttpClient`] adapter.
//!
//! Wraps a [`reqwest::blocking::Client`] and implements
//! [`HttpClient::send`] for the managed-endpoint synthesizer:
//!
//! * Resolves `cfg.api_key_ref` as an environment variable name and
//!   forwards the resolved cleartext as a `Bearer` token (so the
//!   secret never round-trips through the
//!   [`crate::managed_endpoint::EndpointConfig`] wire payload).
//! * Honours [`EndpointConfig::effective_timeout`] via the
//!   per-request `RequestBuilder::timeout` override (set at
//!   construction time as the client-wide default; the adapter is
//!   built once per `configure_synthesis_engine` call so the
//!   timeout matches the active config without rebuilding for every
//!   `send`).
//! * Maps `HTTP 429` / `503` to [`EndpointError::RateLimited`],
//!   preserving the `Retry-After` header value when present so
//!   callers can implement informed backoff.
//! * Maps any non-`2xx` to [`EndpointError::Endpoint`] with the
//!   server's response body for diagnostics.
//! * Maps TCP / TLS / connect-timeout failures to
//!   [`EndpointError::Transport`] and `reqwest::Error::is_timeout()`
//!   to [`EndpointError::Timeout`].
//!
//! Gated behind the crate's `http-client` feature so cross-checks
//! that do not link the network stack still build the engine. The
//! FFI substrate's `configure_synthesis_engine` returns
//! `FfiError::Unavailable { subsystem: "synthesis_engine" }` when
//! the feature is not enabled.

use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use reqwest::StatusCode;

use crate::managed_endpoint::{
    EndpointConfig, EndpointError, HttpClient, SynthesisRequest, SynthesisResponse,
};

/// Production [`HttpClient`] implementation built on
/// [`reqwest::blocking::Client`].
///
/// Created by [`BlockingHttpClientAdapter::new`]; the underlying
/// reqwest client is constructed lazily inside `new` and held by
/// value (reqwest's blocking client already wraps an internal thread
/// pool + connection pool, so a second `Arc` would be redundant).
#[derive(Debug)]
pub struct BlockingHttpClientAdapter {
    client: Client,
    /// Cached default timeout the client was built with. The reqwest
    /// builder applies it as the default for every request; we keep
    /// a copy so [`HttpClient::send`] can surface a meaningful
    /// [`EndpointError::Timeout`] payload when the deadline trips.
    default_timeout: Duration,
}

impl BlockingHttpClientAdapter {
    /// Build a fresh adapter sized for `cfg`'s effective timeout.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError::Transport`] if the underlying
    /// reqwest client fails to construct (e.g. the host process has
    /// no DNS resolver available, or rustls fails to load system
    /// roots).
    pub fn new(cfg: &EndpointConfig) -> Result<Self, EndpointError> {
        let timeout = cfg.effective_timeout();
        let client = Client::builder()
            .timeout(timeout)
            // Defence-in-depth: a synthesis call should never block
            // on a connect handshake longer than the overall budget,
            // and reqwest's default 30 s connect timeout would
            // dominate small per-request budgets. Cap at the
            // effective timeout (which already saturates at the
            // configured value).
            .connect_timeout(timeout)
            .build()
            .map_err(|e| {
                EndpointError::Transport(format!("failed to build reqwest blocking client: {e}"
                ))
            })?;
        Ok(Self {
            client,
            default_timeout: timeout,
        })
    }

    /// Borrow the underlying reqwest client. Exposed for adapters /
    /// integration tests that want to assert against the configured
    /// timeout, but production callers should treat
    /// [`BlockingHttpClientAdapter`] as opaque.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Effective per-request timeout the adapter was built with.
    pub fn timeout(&self) -> Duration {
        self.default_timeout
    }

    /// Resolve the API key referenced by `api_key_ref`.
    ///
    /// The substrate's contract is that
    /// [`EndpointConfig::api_key_ref`] is the **name of an
    /// environment variable** holding the cleartext token — the raw
    /// secret never travels through the wire payload. A missing or
    /// empty env var surfaces as [`EndpointError::InvalidRequest`]
    /// so the operator can fix the host configuration without
    /// hitting the remote endpoint at all.
    fn resolve_api_key(api_key_ref: &str) -> Result<String, EndpointError> {
        if api_key_ref.is_empty() {
            return Err(EndpointError::InvalidRequest("EndpointConfig.api_key_ref is empty".into(),
            ));
        }
        match std::env::var(api_key_ref) {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) => Err(EndpointError::InvalidRequest(format!("env var `{api_key_ref}` referenced by EndpointConfig.api_key_ref is set but empty"
            ))),
            Err(_) => Err(EndpointError::InvalidRequest(format!("env var `{api_key_ref}` referenced by EndpointConfig.api_key_ref is not set"
            ))),
        }
    }

    fn map_reqwest_error(&self, err: &reqwest::Error) -> EndpointError {
        if err.is_timeout() {
            EndpointError::Timeout(self.default_timeout)
        } else if err.is_connect() || err.is_request() {
            EndpointError::Transport(format!("reqwest transport failure: {err}"))
        } else {
            EndpointError::Transport(format!("reqwest error: {err}"))
        }
    }
}

impl HttpClient for BlockingHttpClientAdapter {
    fn send(&self,
        cfg: &EndpointConfig,
        req: &SynthesisRequest,
    ) -> Result<SynthesisResponse, EndpointError> {
        let api_key = Self::resolve_api_key(&cfg.api_key_ref)?;
        let bearer = format!("Bearer {api_key}");
        let auth_header = HeaderValue::from_str(&bearer).map_err(|e| {
            EndpointError::InvalidRequest(format!("resolved API key is not a valid HTTP header value: {e}"
            ))
        })?;

        let body = serde_json::to_vec(req).map_err(|e| {
            EndpointError::InvalidRequest(format!("failed to serialize SynthesisRequest: {e}"))
        })?;

        let response = self
            .client
            .post(&cfg.url)
            .header(AUTHORIZATION, auth_header)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|e| self.map_reqwest_error(&e))?;

        let status = response.status();

        if matches!(status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .map_or_else(|| "unspecified".to_string(),
                    std::string::ToString::to_string,
                );
            let body_excerpt = response.text().unwrap_or_default();
            return Err(EndpointError::RateLimited(format!("endpoint reported {} (retry-after: {retry_after}): {}",
                status,
                truncate_body(&body_excerpt, 256),
            )));
        }

        if !status.is_success() {
            let body_excerpt = response.text().unwrap_or_default();
            return Err(EndpointError::Endpoint(format!("endpoint reported status {}: {}",
                status,
                truncate_body(&body_excerpt, 512),
            )));
        }

        let parsed: SynthesisResponse = response.json().map_err(|e| {
            EndpointError::InvalidResponse(format!("failed to parse JSON synthesis response: {e}"))
        })?;
        Ok(parsed)
    }
}

/// Truncate `body` to `max_chars` UTF-8 chars for diagnostic
/// inclusion in error messages. Errors with multi-megabyte bodies
/// would otherwise spam logs and OOM ingest pipelines.
fn truncate_body(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_string();
    }
    let mut out = String::with_capacity(max_chars + 1);
    for (i, c) in body.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_key(api_key_ref: &str) -> EndpointConfig {
        EndpointConfig::new("https://example.test/synth", api_key_ref, "slm-recap-v1")
            .with_timeout(Duration::from_millis(500))
    }

    #[test]
    fn builder_honors_effective_timeout() {
        let cfg = cfg_with_key("KNOWLEDGE_TEST_KEY_NONEXISTENT");
        let adapter = BlockingHttpClientAdapter::new(&cfg).expect("build adapter");
        assert_eq!(adapter.timeout(), Duration::from_millis(500));
    }

    #[test]
    fn resolve_api_key_rejects_empty_ref() {
        let err = BlockingHttpClientAdapter::resolve_api_key("").unwrap_err();
        match err {
            EndpointError::InvalidRequest(msg) => {
                assert!(msg.contains("empty"), "unexpected message: {msg}");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn resolve_api_key_rejects_missing_env_var() {
        // SAFETY: per-test isolation — the variable name is unique
        // to this test so concurrent tests do not race the env.
        let var_name = "KNOWLEDGE_TEST_BLOCKING_CLIENT_MISSING_DO_NOT_SET";
        // We deliberately do NOT set the env var here.
        let err = BlockingHttpClientAdapter::resolve_api_key(var_name).unwrap_err();
        match err {
            EndpointError::InvalidRequest(msg) => {
                assert!(msg.contains(var_name), "unexpected message: {msg}");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn truncate_body_keeps_short_bodies() {
        let s = "abc";
        assert_eq!(truncate_body(s, 10), "abc");
    }

    #[test]
    fn truncate_body_truncates_long_bodies_with_ellipsis() {
        let s = "abcdefghij";
        let truncated = truncate_body(s, 3);
        // 3 chars + ellipsis sentinel.
        assert_eq!(truncated, "abc…");
    }
}
