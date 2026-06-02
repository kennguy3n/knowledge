//! llama.cpp loopback HTTP adapter.
//!
//! Production [`LlamaCppAdapter`] sends an HTTP POST to a sidecar
//! `llama-server` listening on localhost. To keep the substrate test
//! suite hermetic we factor the HTTP transport behind the
//! [`LlamaServerClient`] trait so unit tests can inject a fake.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::config::{DeviceTier, RouterConfig};
use crate::error::RouterError;
use crate::task::InferenceTask;

/// HTTP transport for the llama.cpp loopback server. Implementors
/// translate `(prompt, grammar)` into a real HTTP call against
/// `server_url`.
///
/// The substrate ships only the trait + a unit-testable in-memory
/// fake; the real HTTP client lives in the platform shells (iOS,
/// Electron, Go gateway).
pub trait LlamaServerClient: Send + Sync {
    /// `true` iff the server is reachable (e.g. responds to a `/health`
    /// probe).
    fn ping(&self) -> bool;

    /// Run a completion. `grammar` is fed verbatim as the
    /// `--grammar` parameter; pass an empty string for free-form
    /// completions.
    fn complete(&self, prompt: &str, grammar: &str) -> Result<String, String>;
}

/// llama.cpp adapter. Drives the SLM through a loopback HTTP server.
pub struct LlamaCppAdapter {
    config: RouterConfig,
    client: Box<dyn LlamaServerClient>,
    available: AtomicBool,
}

impl LlamaCppAdapter {
    /// Construct a new adapter wrapping the given HTTP client.
    pub fn new(config: RouterConfig, client: Box<dyn LlamaServerClient>) -> Self {
        Self {
            config,
            client,
            available: AtomicBool::new(false),
        }
    }
}

impl InferenceAdapter for LlamaCppAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::LlamaCpp
    }

    fn probe(&self) -> ProbeResult {
        let tier_ok = matches!(
            self.config.device_tier,
            DeviceTier::Medium | DeviceTier::High
        );
        let reachable = tier_ok && self.client.ping();
        self.available.store(reachable, Ordering::SeqCst);
        if reachable {
            ProbeResult::Available
        } else {
            ProbeResult::Unavailable
        }
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    fn supports(&self, task: InferenceTask) -> bool {
        match self.config.device_tier {
            DeviceTier::Low => false,
            DeviceTier::Medium => task.is_classification(),
            DeviceTier::High => true,
        }
    }

    fn generate(&self, task_tag: &str, prompt: &str, grammar: &str) -> Result<String, RouterError> {
        if !self.is_available() {
            return Err(RouterError::Unavailable {
                task: task_tag_static(task_tag),
            });
        }
        self.client
            .complete(prompt, grammar)
            .map_err(RouterError::InferenceFailure)
    }
}

fn task_tag_static(task_tag: &str) -> &'static str {
    match task_tag {
        "tag_importance" => "tag_importance",
        "extract_entities" => "extract_entities",
        "promote_observation" => "promote_observation",
        "synth_summary" => "synth_summary",
        "synth_concept" => "synth_concept",
        "adjudicate_contradiction" => "adjudicate_contradiction",
        _ => "unknown",
    }
}

/// Recording fake transport — captures every prompt for assertion
/// and replays a fixed response.
pub struct MockLlamaServerClient {
    /// Whether `ping()` should report reachable.
    pub reachable: bool,
    /// Canonical response returned by `complete()`.
    pub response: Mutex<Result<String, String>>,
    /// Captured prompts.
    pub captured: Mutex<Vec<(String, String)>>,
}

impl MockLlamaServerClient {
    /// Construct a reachable mock returning `response`.
    pub fn ok(response: impl Into<String>) -> Self {
        Self {
            reachable: true,
            response: Mutex::new(Ok(response.into())),
            captured: Mutex::new(Vec::new()),
        }
    }

    /// Construct a reachable mock returning `Err(message)`.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            reachable: true,
            response: Mutex::new(Err(message.into())),
            captured: Mutex::new(Vec::new()),
        }
    }

    /// Construct an unreachable mock — `ping()` returns `false`.
    pub fn unreachable() -> Self {
        Self {
            reachable: false,
            response: Mutex::new(Ok(String::new())),
            captured: Mutex::new(Vec::new()),
        }
    }
}

impl LlamaServerClient for MockLlamaServerClient {
    fn ping(&self) -> bool {
        self.reachable
    }

    fn complete(&self, prompt: &str, grammar: &str) -> Result<String, String> {
        self.captured
            .lock()
            .expect("captured")
            .push((prompt.to_string(), grammar.to_string()));
        self.response.lock().expect("response").clone()
    }
}

/// Strip every trailing `/` so `format!("{}/health", url)` stays
/// correct whether the caller passes `http://x:8080`,
/// `http://x:8080/`, or the pathological `http://x:8080//`. The
/// constructors of [`HttpLlamaServerClient`] (sync) and
/// [`AsyncHttpLlamaServerClient`] (async) both document "trailing
/// slashes are tolerated" — matching every trailing slash (not
/// just one) is the non-surprising implementation of that
/// contract, and sharing the helper across both clients keeps the
/// policy uniform regardless of which transport feature is enabled.
#[cfg(any(feature = "http-client", feature = "async-http-client"))]
fn normalise_url(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

#[cfg(feature = "http-client")]
pub use http_client::HttpLlamaServerClient;

#[cfg(feature = "http-client")]
mod http_client {
    //! Real HTTP transport for the llama.cpp loopback server.
    //!
    //! Wraps `reqwest::blocking::Client` so the synchronous router
    //! can dispatch SLM calls without dragging tokio into the
    //! inference path. The endpoint shape is the upstream
    //! `llama-server` HTTP API:
    //!
    //! * `GET /health` — liveness; `200 OK` = reachable.
    //! * `POST /completion` — body `{prompt, grammar, n_predict,
    //!   temperature}`; response `{"content": "<text>", …}`.
    //!
    //! Build-gated behind the `http-client` feature so substrate
    //! unit tests stay free of network deps.
    use std::time::Duration;

    use super::LlamaServerClient;

    /// Default request timeout for `/completion`. Synthesis prompts
    /// can take tens of seconds on a CPU-only `llama-server`, so the
    /// ceiling is intentionally generous; tune via
    /// [`HttpLlamaServerClient::with_timeout`].
    pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 120;

    /// Default request timeout for `/health` probes. The bootstrap
    /// path calls [`HttpLlamaServerClient::ping`] from
    /// [`crate::LlamaCppAdapter::probe`]; with the
    /// [`DEFAULT_HTTP_TIMEOUT_SECS`] timeout a hung `/health` could
    /// block startup for two minutes. `/health` is meant to return
    /// `200 OK` in milliseconds when the server is up, so a much
    /// shorter probe ceiling is the right default. Tune via
    /// [`HttpLlamaServerClient::with_timeouts`].
    pub const DEFAULT_HTTP_PROBE_TIMEOUT_SECS: u64 = 2;

    /// Default `n_predict` cap. Sized for one [`SummaryBundle`]
    /// payload — 512 tokens is comfortably above the GBNF-shaped
    /// JSON output.
    pub const DEFAULT_N_PREDICT: u32 = 512;

    /// Default sampling temperature. Synthesis is closer to
    /// extraction than to creative generation, so we keep it low.
    pub const DEFAULT_TEMPERATURE: f32 = 0.1;

    /// Real HTTP client for the llama.cpp loopback server.
    ///
    /// Constructed at startup by the platform shell once the
    /// `llama-server` sidecar is up; passed to
    /// [`crate::LlamaCppAdapter::new`] in place of the in-memory
    /// fake.
    ///
    /// Holds two `reqwest::blocking::Client`s with separate
    /// timeouts: a long one for `/completion` (synthesis can take
    /// tens of seconds) and a short one for `/health` probes (so
    /// bootstrap doesn't stall for two minutes against a hung
    /// server).
    pub struct HttpLlamaServerClient {
        server_url: String,
        client: reqwest::blocking::Client,
        probe_client: reqwest::blocking::Client,
    }

    impl HttpLlamaServerClient {
        /// Build a client targeting the loopback `llama-server` at
        /// `server_url` (e.g. `http://127.0.0.1:8080`). Trailing
        /// slashes on `server_url` are tolerated — `/health` and
        /// `/completion` are appended directly.
        ///
        /// Uses [`DEFAULT_HTTP_TIMEOUT_SECS`] for `/completion` and
        /// [`DEFAULT_HTTP_PROBE_TIMEOUT_SECS`] for `/health`.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::blocking::Client`
        /// builder rejects the timeout configuration.
        pub fn new(server_url: impl Into<String>) -> Result<Self, String> {
            Self::with_timeouts(
                server_url,
                Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS),
                Duration::from_secs(DEFAULT_HTTP_PROBE_TIMEOUT_SECS),
            )
        }

        /// Build a client with a custom `/completion` request
        /// timeout. The probe (`/health`) timeout defaults to
        /// [`DEFAULT_HTTP_PROBE_TIMEOUT_SECS`].
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::blocking::Client`
        /// builder rejects the timeout configuration.
        pub fn with_timeout(
            server_url: impl Into<String>,
            timeout: Duration,
        ) -> Result<Self, String> {
            Self::with_timeouts(
                server_url,
                timeout,
                Duration::from_secs(DEFAULT_HTTP_PROBE_TIMEOUT_SECS),
            )
        }

        /// Build a client with explicit `/completion` and `/health`
        /// timeouts. Use this when the integration test or platform
        /// shell wants finer-grained control — e.g. a slower probe
        /// for a remote server, or a faster `/completion` cap for
        /// known-small grammars.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::blocking::Client`
        /// builder rejects either timeout configuration.
        pub fn with_timeouts(
            server_url: impl Into<String>,
            completion_timeout: Duration,
            probe_timeout: Duration,
        ) -> Result<Self, String> {
            let server_url: String = server_url.into();
            let server_url = normalise_url(&server_url);
            let client = reqwest::blocking::Client::builder()
                .timeout(completion_timeout)
                .build()
                .map_err(|e| format!("reqwest completion client build failed: {e}"))?;
            let probe_client = reqwest::blocking::Client::builder()
                .timeout(probe_timeout)
                .build()
                .map_err(|e| format!("reqwest probe client build failed: {e}"))?;
            Ok(Self {
                server_url,
                client,
                probe_client,
            })
        }

        /// Borrow the resolved server URL (no trailing slash).
        pub fn server_url(&self) -> &str {
            &self.server_url
        }
    }

    impl LlamaServerClient for HttpLlamaServerClient {
        fn ping(&self) -> bool {
            // `/health` returns 200 once the model is loaded. We
            // treat anything other than a clean 2xx as unreachable
            // so a half-up server (loading, busy) doesn't slip into
            // the available pool.
            //
            // Uses the dedicated short-timeout probe client so a
            // hung server can't stall bootstrap on the
            // `/completion`-sized 120s timeout.
            let url = format!("{}/health", self.server_url);
            match self.probe_client.get(&url).send() {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            }
        }

        fn complete(&self, prompt: &str, grammar: &str) -> Result<String, String> {
            let url = format!("{}/completion", self.server_url);
            let mut body = serde_json::json!({
                "prompt": prompt,
                "n_predict": DEFAULT_N_PREDICT,
                "temperature": DEFAULT_TEMPERATURE,
                // Stream off — the blocking client consumes the
                // whole response in one shot.
                "stream": false,
            });
            if !grammar.is_empty() {
                body["grammar"] = serde_json::Value::String(grammar.to_string());
            }

            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .map_err(|e| format!("POST {url} failed: {e}"))?;
            let status = resp.status();
            if !status.is_success() {
                let detail = resp.text().unwrap_or_default();
                return Err(format!(
                    "llama-server returned {status} from {url}: {detail}"
                ));
            }
            let json: serde_json::Value = resp
                .json()
                .map_err(|e| format!("llama-server response was not JSON: {e}"))?;
            let content = json
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("llama-server response missing string `content`: {json}"))?;
            Ok(content.to_string())
        }
    }

    /// Re-export of the file-level [`super::normalise_url`] helper
    /// so existing in-module callers (`Self::with_timeouts`, the
    /// `tests` submodule) keep their original short path. The
    /// helper itself lives at the file root so the
    /// `async-http-client` sibling module can reuse it without
    /// forcing both transport features to be enabled together.
    pub(super) use super::normalise_url;

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn normalise_url_strips_trailing_slash() {
            assert_eq!(normalise_url("http://x:8080/"), "http://x:8080");
            assert_eq!(normalise_url("http://x:8080"), "http://x:8080");
            // Multiple trailing slashes must all be stripped so
            // appending `/health` never produces `//health`.
            assert_eq!(normalise_url("http://x:8080//"), "http://x:8080");
            assert_eq!(normalise_url("http://x:8080///"), "http://x:8080");
        }

        #[test]
        fn http_client_constructs_with_default_timeout() {
            let c =
                HttpLlamaServerClient::new("http://127.0.0.1:8080/").expect("client should build");
            assert_eq!(c.server_url(), "http://127.0.0.1:8080");
        }

        #[test]
        fn ping_against_unreachable_host_returns_false() {
            // Pick a port that nothing in CI should be listening on.
            // `reqwest` returns `Err` -> our `ping` returns `false`.
            let c = HttpLlamaServerClient::with_timeout(
                "http://127.0.0.1:1",
                Duration::from_millis(50),
            )
            .expect("client should build");
            assert!(!c.ping());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_unreachable_returns_unavailable() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = LlamaCppAdapter::new(cfg, Box::new(MockLlamaServerClient::unreachable()));
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
        assert!(!adapter.is_available());
    }

    #[test]
    fn probe_reachable_returns_available() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = LlamaCppAdapter::new(cfg, Box::new(MockLlamaServerClient::ok("ok")));
        assert_eq!(adapter.probe(), ProbeResult::Available);
        assert!(adapter.is_available());
    }

    #[test]
    fn low_tier_makes_adapter_unavailable_even_when_reachable() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
        let adapter = LlamaCppAdapter::new(cfg, Box::new(MockLlamaServerClient::ok("x")));
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
    }

    #[test]
    fn generate_round_trips_prompt_and_grammar() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let mock = MockLlamaServerClient::ok(r#"{"class":"useful","confidence":0.7}"#);
        let adapter = LlamaCppAdapter::new(cfg, Box::new(mock));
        adapter.probe();
        let out = adapter
            .generate("tag_importance", "msg body", "<grammar>")
            .expect("generate ok");
        assert!(out.contains("useful"));
    }

    #[test]
    fn generate_propagates_inference_failure() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let mock = MockLlamaServerClient::err("model crash");
        let adapter = LlamaCppAdapter::new(cfg, Box::new(mock));
        adapter.probe();
        let err = adapter.generate("tag_importance", "x", "").unwrap_err();
        assert!(matches!(err, RouterError::InferenceFailure(_)));
    }

    #[test]
    fn generate_when_unavailable_returns_unavailable() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = LlamaCppAdapter::new(cfg, Box::new(MockLlamaServerClient::unreachable()));
        adapter.probe();
        let err = adapter.generate("tag_importance", "x", "").unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    #[test]
    fn medium_tier_blocks_synthesis_tasks() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Medium);
        let adapter = LlamaCppAdapter::new(cfg, Box::new(MockLlamaServerClient::ok("x")));
        assert!(adapter.supports(InferenceTask::TagImportance));
        assert!(!adapter.supports(InferenceTask::SynthSummary));
    }
}

#[cfg(feature = "async-http-client")]
pub use http_client_async::{AsyncHttpLlamaServerClient, AsyncLlamaServerClient};

#[cfg(feature = "async-http-client")]
mod http_client_async {
    //! Async HTTP transport for the llama.cpp loopback server.
    //!
    //! Mirror of [`super::http_client::HttpLlamaServerClient`] that
    //! drives `reqwest::Client` (non-blocking) under a tokio
    //! runtime. The endpoint shape is identical to the sync path
    //! (`GET /health`, `POST /completion`) so the substrate can
    //! choose between the blocking and async client at startup
    //! without touching anything downstream.
    //!
    //! The async client implements its own
    //! [`AsyncLlamaServerClient`] trait — it's a sibling of
    //! [`super::LlamaServerClient`], not a `Box<dyn ...>` wrapper,
    //! because the underlying `complete` and `ping` futures cannot
    //! be hoisted into the sync trait's signature without going
    //! through `tokio::runtime::Handle::block_on` (which would
    //! defeat the point of the async surface).
    use std::time::Duration;

    use async_trait::async_trait;

    /// Async sibling of [`super::LlamaServerClient`]. Drives the
    /// llama.cpp loopback server via async HTTP under a tokio
    /// runtime. The substrate plumbs this into a future-driven
    /// inference path; the existing sync trait remains the
    /// supported entrypoint for substrate code that doesn't run a
    /// tokio runtime.
    #[async_trait]
    pub trait AsyncLlamaServerClient: Send + Sync {
        /// `true` iff the server is reachable.
        async fn ping(&self) -> bool;

        /// Run a completion. `grammar` is fed verbatim as the
        /// `--grammar` parameter; pass an empty string for free-form
        /// completions.
        async fn complete(&self, prompt: &str, grammar: &str) -> Result<String, String>;
    }

    /// Default request timeout for `/completion`. Matches the
    /// sync client's [`super::http_client::DEFAULT_HTTP_TIMEOUT_SECS`].
    pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 120;
    /// Default request timeout for `/health` probes. Matches the
    /// sync client.
    pub const DEFAULT_HTTP_PROBE_TIMEOUT_SECS: u64 = 2;
    /// Default `n_predict` cap, matching the sync client.
    pub const DEFAULT_N_PREDICT: u32 = 512;
    /// Default sampling temperature, matching the sync client.
    pub const DEFAULT_TEMPERATURE: f32 = 0.1;

    /// Real async HTTP client for the llama.cpp loopback server.
    ///
    /// Holds two `reqwest::Client`s with separate timeouts: a long
    /// one for `/completion` and a short one for `/health` probes
    /// — same shape as the sync client.
    pub struct AsyncHttpLlamaServerClient {
        server_url: String,
        client: reqwest::Client,
        probe_client: reqwest::Client,
    }

    impl AsyncHttpLlamaServerClient {
        /// Build a client with default timeouts.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::Client`
        /// builder rejects either timeout configuration.
        pub fn new(server_url: impl Into<String>) -> Result<Self, String> {
            Self::with_timeouts(
                server_url,
                Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS),
                Duration::from_secs(DEFAULT_HTTP_PROBE_TIMEOUT_SECS),
            )
        }

        /// Build a client with a custom completion timeout (probe
        /// defaults to [`DEFAULT_HTTP_PROBE_TIMEOUT_SECS`]).
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::Client`
        /// builder rejects the timeout configuration.
        pub fn with_timeout(
            server_url: impl Into<String>,
            timeout: Duration,
        ) -> Result<Self, String> {
            Self::with_timeouts(
                server_url,
                timeout,
                Duration::from_secs(DEFAULT_HTTP_PROBE_TIMEOUT_SECS),
            )
        }

        /// Build a client with explicit timeouts.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::Client`
        /// builder rejects either timeout configuration.
        pub fn with_timeouts(
            server_url: impl Into<String>,
            completion_timeout: Duration,
            probe_timeout: Duration,
        ) -> Result<Self, String> {
            let client = reqwest::Client::builder()
                .timeout(completion_timeout)
                .build()
                .map_err(|e| format!("reqwest async build failed: {e}"))?;
            let probe_client = reqwest::Client::builder()
                .timeout(probe_timeout)
                .build()
                .map_err(|e| format!("reqwest async build failed: {e}"))?;
            Ok(Self {
                server_url: super::normalise_url(&server_url.into()),
                client,
                probe_client,
            })
        }

        /// Borrow the resolved server URL (no trailing slash).
        #[must_use]
        pub fn server_url(&self) -> &str {
            &self.server_url
        }
    }

    #[async_trait]
    impl AsyncLlamaServerClient for AsyncHttpLlamaServerClient {
        async fn ping(&self) -> bool {
            let url = format!("{}/health", self.server_url);
            match self.probe_client.get(&url).send().await {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            }
        }

        async fn complete(&self, prompt: &str, grammar: &str) -> Result<String, String> {
            let url = format!("{}/completion", self.server_url);
            let mut body = serde_json::json!({
                "prompt": prompt,
                "n_predict": DEFAULT_N_PREDICT,
                "temperature": DEFAULT_TEMPERATURE,
                "stream": false,
            });
            if !grammar.is_empty() {
                body["grammar"] = serde_json::Value::String(grammar.to_string());
            }
            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("POST {url} failed: {e}"))?;
            let status = resp.status();
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "llama-server returned {status} from {url}: {detail}"
                ));
            }
            let json: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("llama-server response was not JSON: {e}"))?;
            let content = json
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("llama-server response missing string `content`: {json}"))?;
            Ok(content.to_string())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn constructs_with_default_timeout() {
            let c = AsyncHttpLlamaServerClient::new("http://127.0.0.1:8080/")
                .expect("client should build");
            assert_eq!(c.server_url(), "http://127.0.0.1:8080");
        }

        #[tokio::test]
        async fn ping_against_unreachable_host_returns_false() {
            let c = AsyncHttpLlamaServerClient::with_timeout(
                "http://127.0.0.1:1",
                Duration::from_millis(50),
            )
            .expect("client should build");
            assert!(!c.ping().await);
        }

        #[tokio::test]
        async fn complete_against_unreachable_host_returns_err() {
            let c = AsyncHttpLlamaServerClient::with_timeout(
                "http://127.0.0.1:1",
                Duration::from_millis(50),
            )
            .expect("client should build");
            let err = c.complete("hello", "").await.expect_err("must fail");
            assert!(err.contains("POST"), "got: {err}");
        }
    }
}
