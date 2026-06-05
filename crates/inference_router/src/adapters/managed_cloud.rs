//! Managed-cloud OpenAI-compatible HTTP adapter.
//!
//! [`ManagedCloudAdapter`] lets an operator point synthesis at an
//! external OpenAI-compatible `/v1/chat/completions` endpoint
//! (OpenAI, Anthropic via a compatible proxy, Groq, Together, a local
//! Ollama, …) instead of running a self-hosted `llama-server`
//! sidecar. It sits between [`crate::LlamaCppAdapter`] and
//! [`crate::FallbackAdapter`] in the router's priority chain, so it is
//! only reached when no on-device SLM (MLX / llama.cpp) is available:
//!
//! ```text
//! MLX → llama.cpp → ManagedCloud → Fallback
//! ```
//!
//! Because the compute is remote, the adapter is **independent of the
//! device tier** — it serves synthesis even on a `Low`-tier device
//! that could never run an SLM locally. Classification still falls
//! through to the free encoder-only [`crate::FallbackAdapter`], so an
//! SME is never billed per-message for tasks the local classifier can
//! already serve.
//!
//! To keep the substrate test suite hermetic the HTTP transport is
//! factored behind the [`ManagedInferenceClient`] trait, so unit
//! tests inject an in-memory fake. The real reqwest-backed client
//! ([`HttpManagedInferenceClient`]) is gated behind the `http-client`
//! feature.

use std::sync::atomic::{AtomicBool, Ordering};
// Only the test double below uses `Mutex`; keep the import gated with it
// so the default build carries no unused-import warning.
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::error::RouterError;
use crate::task::InferenceTask;

/// HTTP transport for an OpenAI-compatible managed-cloud endpoint.
/// Implementors translate `(prompt, grammar)` into a real
/// `POST /chat/completions` call against the configured base URL.
///
/// The substrate ships only the trait + a unit-testable in-memory
/// fake; the real HTTP client ([`HttpManagedInferenceClient`]) is
/// compiled in under the `http-client` feature.
pub trait ManagedInferenceClient: Send + Sync {
    /// `true` iff the endpoint is reachable (e.g. responds to a
    /// `GET /models` probe).
    fn ping(&self) -> bool;

    /// Run a chat completion. `grammar` is the GBNF grammar for the
    /// task (empty string for free-form output); implementors apply
    /// it via whatever structured-output mechanism the endpoint
    /// supports (e.g. a `grammar` field for llama.cpp / Ollama and
    /// `response_format` for the OpenAI family).
    fn complete(&self, prompt: &str, grammar: &str) -> Result<String, String>;
}

/// Managed-cloud adapter. Drives synthesis through an external
/// OpenAI-compatible endpoint.
pub struct ManagedCloudAdapter {
    client: Box<dyn ManagedInferenceClient>,
    available: AtomicBool,
}

impl ManagedCloudAdapter {
    /// Construct a new adapter wrapping the given HTTP client.
    pub fn new(client: Box<dyn ManagedInferenceClient>) -> Self {
        Self {
            client,
            available: AtomicBool::new(false),
        }
    }
}

impl InferenceAdapter for ManagedCloudAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::ManagedCloud
    }

    fn probe(&self) -> ProbeResult {
        // No device-tier gate: the compute is remote, so a `Low`-tier
        // device that could never run an SLM locally can still use a
        // managed endpoint for synthesis. Availability is purely a
        // function of endpoint reachability.
        let reachable = self.client.ping();
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
        // Synthesis only. Classification tasks deliberately fall
        // through to the free encoder-only `FallbackAdapter` so an
        // operator is not billed per-message for work the local
        // classifier already handles.
        task.is_synthesis()
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
///
/// Gated behind `cfg(any(test, feature = "test-support"))` per
/// CONTRIBUTING.md: this double is only consumed by this crate's own
/// unit tests, so it stays out of the default public surface. (The
/// sibling `MockLlamaServerClient` is left ungated because it is needed
/// cross-crate by `tests/` and `synthesis_pipeline`; this one is not.)
#[cfg(any(test, feature = "test-support"))]
pub struct MockManagedInferenceClient {
    /// Whether `ping()` should report reachable.
    pub reachable: bool,
    /// Canonical response returned by `complete()`.
    pub response: Mutex<Result<String, String>>,
    /// Captured `(prompt, grammar)` pairs.
    pub captured: Mutex<Vec<(String, String)>>,
}

#[cfg(any(test, feature = "test-support"))]
impl MockManagedInferenceClient {
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

#[cfg(any(test, feature = "test-support"))]
impl ManagedInferenceClient for MockManagedInferenceClient {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::InferenceTask;

    fn probed(client: Box<dyn ManagedInferenceClient>) -> ManagedCloudAdapter {
        let adapter = ManagedCloudAdapter::new(client);
        adapter.probe();
        adapter
    }

    #[test]
    fn kind_is_managed_cloud() {
        let adapter = ManagedCloudAdapter::new(Box::new(MockManagedInferenceClient::ok("{}")));
        assert_eq!(adapter.kind(), AdapterKind::ManagedCloud);
    }

    #[test]
    fn probe_available_when_reachable() {
        let adapter = probed(Box::new(MockManagedInferenceClient::ok("{}")));
        assert_eq!(adapter.probe(), ProbeResult::Available);
        assert!(adapter.is_available());
    }

    #[test]
    fn probe_unavailable_when_unreachable() {
        let adapter = probed(Box::new(MockManagedInferenceClient::unreachable()));
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
        assert!(!adapter.is_available());
    }

    #[test]
    fn supports_synthesis_only() {
        let adapter = ManagedCloudAdapter::new(Box::new(MockManagedInferenceClient::ok("{}")));
        // Synthesis tasks are served.
        assert!(adapter.supports(InferenceTask::SynthSummary));
        assert!(adapter.supports(InferenceTask::SynthConcept));
        assert!(adapter.supports(InferenceTask::AdjudicateContradiction));
        // Classification falls through to the free fallback.
        assert!(!adapter.supports(InferenceTask::TagImportance));
        assert!(!adapter.supports(InferenceTask::ExtractEntities));
        assert!(!adapter.supports(InferenceTask::PromoteObservation));
    }

    #[test]
    fn generate_forwards_prompt_and_grammar() {
        let client = Box::new(MockManagedInferenceClient::ok("{\"recap\":\"ok\"}"));
        let adapter = probed(client);
        let out = adapter
            .generate("synth_summary", "the prompt", "the-grammar")
            .expect("generate should succeed");
        assert_eq!(out, "{\"recap\":\"ok\"}");
    }

    #[test]
    fn generate_unavailable_when_not_probed() {
        // No probe() call → adapter starts unavailable.
        let adapter = ManagedCloudAdapter::new(Box::new(MockManagedInferenceClient::ok("{}")));
        let err = adapter
            .generate("synth_summary", "p", "")
            .expect_err("must be unavailable before probe");
        assert!(matches!(
            err,
            RouterError::Unavailable {
                task: "synth_summary"
            }
        ));
    }

    #[test]
    fn generate_maps_transport_error_to_inference_failure() {
        let adapter = probed(Box::new(MockManagedInferenceClient::err("502 bad gateway")));
        let err = adapter
            .generate("synth_concept", "p", "g")
            .expect_err("transport error must surface");
        assert!(matches!(err, RouterError::InferenceFailure(msg) if msg.contains("502")));
    }
}

/// Real HTTP transport for an OpenAI-compatible managed-cloud
/// endpoint. Gated behind the `http-client` feature so the substrate
/// unit-test suite stays free of network deps.
#[cfg(feature = "http-client")]
pub use http_client::HttpManagedInferenceClient;

#[cfg(feature = "http-client")]
mod http_client {
    //! Real HTTP transport for an OpenAI-compatible managed-cloud
    //! endpoint.
    //!
    //! Wraps `reqwest::blocking::Client` so the synchronous router can
    //! dispatch synthesis calls without dragging tokio into the
    //! inference path. The endpoint shape is the OpenAI HTTP API:
    //!
    //! * `GET  {base}/models` — liveness probe.
    //! * `POST {base}/chat/completions` — body
    //!   `{model, messages, temperature, max_tokens}`; response
    //!   `{"choices": [{"message": {"content": "<text>"}}], …}`.
    //!
    //! When the task carries a GBNF grammar the request also sets a
    //! top-level `grammar` field (honoured by llama.cpp / Ollama
    //! OpenAI-compatible servers) and `response_format:
    //! {"type":"json_object"}` (honoured by the OpenAI family) so the
    //! emitted JSON is constrained regardless of backend.
    use std::time::Duration;

    use super::ManagedInferenceClient;

    /// Default request timeout for `/chat/completions`. Synthesis
    /// prompts can take tens of seconds on a busy endpoint, so the
    /// ceiling is generous; tune via
    /// [`HttpManagedInferenceClient::with_timeouts`].
    pub const DEFAULT_MANAGED_TIMEOUT_SECS: u64 = 60;

    /// Default request timeout for `/models` liveness probes. The
    /// bootstrap path calls [`HttpManagedInferenceClient::ping`] from
    /// [`super::ManagedCloudAdapter::probe`]; a short ceiling keeps a
    /// hung endpoint from stalling startup.
    pub const DEFAULT_MANAGED_PROBE_TIMEOUT_SECS: u64 = 5;

    /// Default `max_tokens` cap. Sized for one synthesis payload —
    /// 512 tokens comfortably exceeds the GBNF-shaped JSON output.
    pub const DEFAULT_MANAGED_MAX_TOKENS: u32 = 512;

    /// Default sampling temperature. Synthesis is closer to
    /// extraction than creative generation, so we keep it low.
    pub const DEFAULT_MANAGED_TEMPERATURE: f64 = 0.1;

    /// Default model name used when `KNOWLEDGE_MANAGED_INFERENCE_MODEL`
    /// is unset. A small, cheap, widely-available OpenAI model is a
    /// sensible SME default; override per-provider (e.g.
    /// `llama-3.1-8b-instant` on Groq, `qwen2.5:3b` on Ollama).
    pub const DEFAULT_MANAGED_MODEL: &str = "gpt-4o-mini";

    /// Environment variable holding the OpenAI-compatible base URL
    /// (e.g. `https://api.openai.com/v1`). When unset or empty the
    /// managed-cloud adapter is not wired (see
    /// [`HttpManagedInferenceClient::from_env`]).
    pub const ENV_MANAGED_URL: &str = "KNOWLEDGE_MANAGED_INFERENCE_URL";

    /// Environment variable holding the bearer API key. Optional — a
    /// local Ollama endpoint needs none, so an empty value simply
    /// omits the `Authorization` header.
    pub const ENV_MANAGED_KEY: &str = "KNOWLEDGE_MANAGED_INFERENCE_KEY";

    /// Environment variable holding the model name. Falls back to
    /// [`DEFAULT_MANAGED_MODEL`] when unset or empty.
    pub const ENV_MANAGED_MODEL: &str = "KNOWLEDGE_MANAGED_INFERENCE_MODEL";

    /// Real HTTP client for an OpenAI-compatible managed-cloud
    /// endpoint.
    ///
    /// Holds two `reqwest::blocking::Client`s with separate timeouts:
    /// a long one for `/chat/completions` (synthesis can take tens of
    /// seconds) and a short one for `/models` probes (so bootstrap
    /// doesn't stall against a hung endpoint).
    pub struct HttpManagedInferenceClient {
        base_url: String,
        api_key: String,
        model: String,
        client: reqwest::blocking::Client,
        probe_client: reqwest::blocking::Client,
    }

    impl HttpManagedInferenceClient {
        /// Build a client targeting the OpenAI-compatible endpoint at
        /// `base_url` (e.g. `https://api.openai.com/v1`). Trailing
        /// slashes are tolerated — `/models` and `/chat/completions`
        /// are appended directly. `api_key` may be empty (no
        /// `Authorization` header is sent). `model` names the model
        /// to request.
        ///
        /// Uses [`DEFAULT_MANAGED_TIMEOUT_SECS`] for completions and
        /// [`DEFAULT_MANAGED_PROBE_TIMEOUT_SECS`] for probes.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::blocking::Client`
        /// builder rejects the timeout configuration.
        pub fn new(
            base_url: impl Into<String>,
            api_key: impl Into<String>,
            model: impl Into<String>,
        ) -> Result<Self, String> {
            Self::with_timeouts(
                base_url,
                api_key,
                model,
                Duration::from_secs(DEFAULT_MANAGED_TIMEOUT_SECS),
                Duration::from_secs(DEFAULT_MANAGED_PROBE_TIMEOUT_SECS),
            )
        }

        /// Auto-discover a managed-cloud endpoint from the
        /// `KNOWLEDGE_MANAGED_INFERENCE_*` environment variables.
        ///
        /// Returns:
        /// * `Ok(None)` when [`ENV_MANAGED_URL`] is unset or empty —
        ///   no managed endpoint is configured, so the caller should
        ///   skip the adapter.
        /// * `Ok(Some(client))` when the URL is non-empty and the
        ///   client builds successfully.
        /// * `Err` when the URL is set but the underlying
        ///   [`reqwest::blocking::Client`] builder rejects the
        ///   configuration.
        ///
        /// # Errors
        ///
        /// Propagates the [`Self::new`] client-builder error when the
        /// URL is set but construction fails.
        pub fn from_env() -> Result<Option<Self>, String> {
            Self::from_env_values(
                std::env::var(ENV_MANAGED_URL).ok().as_deref(),
                std::env::var(ENV_MANAGED_KEY).ok().as_deref(),
                std::env::var(ENV_MANAGED_MODEL).ok().as_deref(),
            )
        }

        /// Core of [`Self::from_env`], split out so the discovery
        /// logic can be unit-tested without touching the
        /// process-global environment (`std::env::set_var` is not
        /// thread-safe and is `unsafe` from the 2024 edition).
        ///
        /// `url`/`key`/`model` are the raw env values (`None` when
        /// unset). Values are trimmed; an empty / whitespace-only URL
        /// yields `Ok(None)`. An empty key omits the `Authorization`
        /// header. An empty model falls back to
        /// [`DEFAULT_MANAGED_MODEL`].
        fn from_env_values(
            url: Option<&str>,
            key: Option<&str>,
            model: Option<&str>,
        ) -> Result<Option<Self>, String> {
            let url = match url.map(str::trim) {
                Some(u) if !u.is_empty() => u,
                _ => return Ok(None),
            };
            let key = key.map_or("", str::trim);
            let model = match model.map(str::trim) {
                Some(m) if !m.is_empty() => m,
                _ => DEFAULT_MANAGED_MODEL,
            };
            Self::new(url, key, model).map(Some)
        }

        /// Build a client with explicit completion and probe timeouts.
        ///
        /// # Errors
        ///
        /// Returns `Err` if the underlying `reqwest::blocking::Client`
        /// builder rejects either timeout configuration.
        pub fn with_timeouts(
            base_url: impl Into<String>,
            api_key: impl Into<String>,
            model: impl Into<String>,
            completion_timeout: Duration,
            probe_timeout: Duration,
        ) -> Result<Self, String> {
            let base_url = normalise_base(&base_url.into());
            let client = reqwest::blocking::Client::builder()
                .timeout(completion_timeout)
                .build()
                .map_err(|e| format!("reqwest completion client build failed: {e}"))?;
            let probe_client = reqwest::blocking::Client::builder()
                .timeout(probe_timeout)
                .build()
                .map_err(|e| format!("reqwest probe client build failed: {e}"))?;
            Ok(Self {
                base_url,
                api_key: api_key.into(),
                model: model.into(),
                client,
                probe_client,
            })
        }

        /// Borrow the resolved base URL (no trailing slash).
        pub fn base_url(&self) -> &str {
            &self.base_url
        }

        /// Borrow the configured model name.
        pub fn model(&self) -> &str {
            &self.model
        }
    }

    /// Strip every trailing `/` so `format!("{}/models", base)` and
    /// `format!("{}/chat/completions", base)` never produce a doubled
    /// slash, mirroring the llama.cpp client's URL handling.
    fn normalise_base(s: &str) -> String {
        s.trim_end_matches('/').to_string()
    }

    impl ManagedInferenceClient for HttpManagedInferenceClient {
        fn ping(&self) -> bool {
            // `GET {base}/models` is the canonical OpenAI liveness
            // surface. Treat ANY received HTTP response as reachable
            // (even 401/404): the endpoint is up, and a real
            // misconfiguration surfaces as an `InferenceFailure` on
            // the first dispatch rather than silently dropping the
            // adapter. Only a transport-level error (DNS, connect,
            // timeout) marks it unreachable.
            let url = format!("{}/models", self.base_url);
            let mut req = self.probe_client.get(&url);
            if !self.api_key.is_empty() {
                req = req.bearer_auth(&self.api_key);
            }
            req.send().is_ok()
        }

        fn complete(&self, prompt: &str, grammar: &str) -> Result<String, String> {
            let url = format!("{}/chat/completions", self.base_url);
            let mut body = serde_json::json!({
                "model": self.model,
                "messages": [{ "role": "user", "content": prompt }],
                "temperature": DEFAULT_MANAGED_TEMPERATURE,
                "max_tokens": DEFAULT_MANAGED_MAX_TOKENS,
                "stream": false,
            });
            if !grammar.is_empty() {
                // llama.cpp / Ollama OpenAI-compatible servers accept a
                // top-level `grammar` field for true GBNF enforcement.
                body["grammar"] = serde_json::Value::String(grammar.to_string());
                // OpenAI / Groq / Together honour `response_format` to
                // force a valid JSON object even when they ignore the
                // `grammar` extension.
                body["response_format"] = serde_json::json!({ "type": "json_object" });
            }

            let mut req = self.client.post(&url).json(&body);
            if !self.api_key.is_empty() {
                req = req.bearer_auth(&self.api_key);
            }
            let resp = req.send().map_err(|e| format!("POST {url} failed: {e}"))?;
            let status = resp.status();
            if !status.is_success() {
                let detail = resp.text().unwrap_or_default();
                return Err(format!(
                    "managed endpoint returned {status} from {url}: {detail}"
                ));
            }
            let json: serde_json::Value = resp
                .json()
                .map_err(|e| format!("managed endpoint response was not JSON: {e}"))?;
            let content = json
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    format!("managed endpoint response missing choices[0].message.content: {json}")
                })?;
            Ok(content.to_string())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn normalise_base_strips_trailing_slashes() {
            assert_eq!(normalise_base("https://x/v1"), "https://x/v1");
            assert_eq!(normalise_base("https://x/v1/"), "https://x/v1");
            assert_eq!(normalise_base("https://x/v1///"), "https://x/v1");
        }

        #[test]
        fn new_normalises_url_and_keeps_model() {
            let c = HttpManagedInferenceClient::new(
                "https://api.openai.com/v1/",
                "sk-test",
                "gpt-4o-mini",
            )
            .expect("client should build");
            assert_eq!(c.base_url(), "https://api.openai.com/v1");
            assert_eq!(c.model(), "gpt-4o-mini");
        }

        #[test]
        fn from_env_values_requires_url() {
            // Unset URL → no adapter.
            assert!(
                HttpManagedInferenceClient::from_env_values(None, None, None)
                    .expect("unset must not error")
                    .is_none()
            );
            // Blank / whitespace URL → no adapter.
            for blank in ["", "   ", "\n\t "] {
                assert!(
                    HttpManagedInferenceClient::from_env_values(Some(blank), None, None)
                        .expect("blank must not error")
                        .is_none()
                );
            }
        }

        #[test]
        fn from_env_values_defaults_model_and_trims() {
            let c = HttpManagedInferenceClient::from_env_values(
                Some("  https://api.groq.com/openai/v1/  "),
                Some(""),
                Some("   "),
            )
            .expect("valid URL must not error")
            .expect("non-empty URL must yield a client");
            assert_eq!(c.base_url(), "https://api.groq.com/openai/v1");
            // Empty model → default.
            assert_eq!(c.model(), DEFAULT_MANAGED_MODEL);
        }

        #[test]
        fn from_env_values_honours_explicit_model() {
            let c = HttpManagedInferenceClient::from_env_values(
                Some("https://api.groq.com/openai/v1"),
                Some("gsk_key"),
                Some("llama-3.1-8b-instant"),
            )
            .expect("valid URL must not error")
            .expect("non-empty URL must yield a client");
            assert_eq!(c.model(), "llama-3.1-8b-instant");
        }

        #[test]
        fn ping_against_unreachable_host_returns_false() {
            let c = HttpManagedInferenceClient::with_timeouts(
                "http://127.0.0.1:1/v1",
                "",
                "m",
                Duration::from_millis(50),
                Duration::from_millis(50),
            )
            .expect("client should build");
            assert!(!c.ping());
        }
    }
}
