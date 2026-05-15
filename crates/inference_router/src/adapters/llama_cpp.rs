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
    //! * `GET  /health`     — liveness; `200 OK` = reachable.
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
    pub struct HttpLlamaServerClient {
        server_url: String,
        client: reqwest::blocking::Client,
    }

    impl HttpLlamaServerClient {
        /// Build a client targeting the loopback `llama-server` at
        /// `server_url` (e.g. `http://127.0.0.1:8080`). Trailing
        /// slash on `server_url` is tolerated — `/health` and
        /// `/completion` are appended directly.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::blocking::Client`
        /// builder rejects the timeout configuration.
        pub fn new(server_url: impl Into<String>) -> Result<Self, String> {
            Self::with_timeout(server_url, Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
        }

        /// Build a client with a custom request timeout.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::blocking::Client`
        /// builder rejects the timeout configuration.
        pub fn with_timeout(
            server_url: impl Into<String>,
            timeout: Duration,
        ) -> Result<Self, String> {
            let server_url = normalise_url(server_url.into());
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| format!("reqwest client build failed: {e}"))?;
            Ok(Self { server_url, client })
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
            let url = format!("{}/health", self.server_url);
            match self.client.get(&url).send() {
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

    /// Strip a single trailing `/` so `format!("{}/health", url)`
    /// stays correct whether the caller passes `http://x:8080` or
    /// `http://x:8080/`.
    fn normalise_url(mut s: String) -> String {
        if s.ends_with('/') {
            s.pop();
        }
        s
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn normalise_url_strips_trailing_slash() {
            assert_eq!(normalise_url("http://x:8080/".into()), "http://x:8080");
            assert_eq!(normalise_url("http://x:8080".into()), "http://x:8080");
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
