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

    /// Run a completion with an explicit [`SamplingConfig`] supplied by
    /// the caller (the [`crate::LlamaCppAdapter`] passes its
    /// [`RouterConfig::sampling`](crate::RouterConfig) so that a
    /// host-installed override actually reaches the wire).
    ///
    /// The default implementation ignores the config and delegates to
    /// [`Self::complete`], so existing platform-shell implementors that
    /// build their own request body keep working unchanged; the
    /// substrate's own HTTP client overrides it to serialise the
    /// supplied knobs into the body. This is the seam the synthesis
    /// pipeline later uses (via the adapter) to vary `n_predict`
    /// per-call for adaptive budgeting and verify-and-retry.
    fn complete_with_sampling(
        &self,
        prompt: &str,
        grammar: &str,
        sampling: &crate::config::SamplingConfig,
    ) -> Result<String, String> {
        let _ = sampling;
        self.complete(prompt, grammar)
    }
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
        // Thread *this adapter's* configured sampling onto the call so
        // a host-installed `RouterConfig::with_sampling` override (or
        // the `KNOWLEDGE_SLM_*`-seeded default the config carries)
        // actually reaches the request body — the client's own
        // `from_env` sampling is only the fallback for direct,
        // non-routed calls.
        self.client
            .complete_with_sampling(prompt, grammar, &self.config.sampling)
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
    /// Captured `(prompt, grammar)` pairs, one per call.
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

/// Build the `llama-server` `/completion` request body from `prompt`,
/// an optional `grammar`, and the deterministic [`SamplingConfig`].
///
/// This is the single source of truth for the request shape, shared
/// verbatim by the sync ([`HttpLlamaServerClient`]) and async
/// ([`AsyncHttpLlamaServerClient`]) transports so the two can never
/// drift apart. Crucially it threads **`seed`** plus every sampling
/// knob (`top_k` / `top_p` / `min_p` / `repeat_penalty`) into the
/// body — the historical bodies sent only `{prompt, n_predict,
/// temperature, stream}`, so with `llama-server`'s default seed
/// (`-1`) identical `(model, prompt)` pairs drew a fresh sample every
/// call and produced a different briefing run-to-run. Pinning the
/// seed (default `0`) + greedy decoding makes synthesis
/// byte-reproducible.
///
/// Gated on the HTTP-client features (plus `test`, so the hermetic
/// reproducibility regression test can assert the serialized fields
/// without standing up a real server).
#[cfg(any(feature = "http-client", feature = "async-http-client", test))]
pub(crate) fn build_completion_body(
    prompt: &str,
    grammar: &str,
    sampling: &crate::config::SamplingConfig,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "prompt": prompt,
        // The reproducibility fix: an explicit, fixed seed. Without
        // this `llama-server` defaults to `-1` (random per call).
        "seed": sampling.seed,
        "temperature": sampling.temperature,
        "top_k": sampling.top_k,
        "top_p": sampling.top_p,
        "min_p": sampling.min_p,
        "repeat_penalty": sampling.repeat_penalty,
        "n_predict": sampling.n_predict,
        // Stream off — both clients consume the whole response in one
        // shot.
        "stream": false,
    });
    if !grammar.is_empty() {
        body["grammar"] = serde_json::Value::String(grammar.to_string());
    }
    body
}

/// Hermetic reproducibility regression test for
/// [`build_completion_body`]. Runs in the default (feature-less)
/// `cargo test -p inference_router` build — it needs no network and
/// no `llama-server`. Fails on the pre-fix bodies (which carried no
/// `seed` and none of the extra sampling knobs).
#[cfg(test)]
mod completion_body_tests {
    use super::build_completion_body;
    use crate::config::SamplingConfig;

    #[test]
    fn body_carries_seed_and_every_sampling_param() {
        let sampling = SamplingConfig::synthesis_default();
        let body = build_completion_body("hello", "", &sampling);

        // The crux of the fix: a fixed seed is present and pinned.
        assert_eq!(
            body.get("seed").and_then(serde_json::Value::as_i64),
            Some(sampling.seed),
            "request body must carry an explicit seed for reproducibility"
        );
        assert_eq!(body["prompt"], "hello");
        assert_eq!(body["temperature"], sampling.temperature);
        assert_eq!(
            body.get("top_k").and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(body["top_p"], sampling.top_p);
        assert_eq!(body["min_p"], sampling.min_p);
        assert_eq!(body["repeat_penalty"], sampling.repeat_penalty);
        assert_eq!(
            body.get("n_predict").and_then(serde_json::Value::as_u64),
            Some(u64::from(sampling.n_predict))
        );
        assert_eq!(body["stream"], false);
        // No grammar passed → key omitted (free-form completion).
        assert!(body.get("grammar").is_none());
    }

    #[test]
    fn body_is_byte_identical_for_identical_inputs() {
        // Determinism at the request layer: the same (prompt, grammar,
        // sampling) must serialise to the same bytes every time, which
        // is the precondition for a reproducible (model, prompt) →
        // bundle mapping.
        let sampling = SamplingConfig::synthesis_default();
        let a = build_completion_body("p", "root ::= \"x\"", &sampling);
        let b = build_completion_body("p", "root ::= \"x\"", &sampling);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        assert_eq!(a["grammar"], "root ::= \"x\"");
    }

    #[test]
    fn body_reflects_overridden_sampling() {
        // A tuned config (e.g. via KNOWLEDGE_SLM_* env) must flow into
        // the body unchanged.
        let sampling = SamplingConfig::from_env_values(
            Some("7"),
            Some("0.2"),
            Some("20"),
            Some("0.8"),
            Some("0.02"),
            Some("1.2"),
            Some("700"),
        );
        let body = build_completion_body("p", "", &sampling);
        assert_eq!(
            body.get("seed").and_then(serde_json::Value::as_i64),
            Some(7)
        );
        // Compare with an `f32` literal: the body stores an `f32`
        // widened to `f64`, so `0.2_f64` would not match bit-for-bit.
        assert_eq!(body["temperature"], 0.2_f32);
        assert_eq!(
            body.get("top_k").and_then(serde_json::Value::as_u64),
            Some(20)
        );
        assert_eq!(
            body.get("n_predict").and_then(serde_json::Value::as_u64),
            Some(700)
        );
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

    use super::build_completion_body;
    use crate::config::SamplingConfig;

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
    ///
    /// The [`SamplingConfig`] threaded into every `/completion` body
    /// carries the deterministic seed + sampling knobs; it defaults
    /// to [`SamplingConfig::from_env`] at construction so a
    /// `KNOWLEDGE_SLM_*` deployment override takes effect even though
    /// the FFI runtime builds this client through
    /// [`Self::from_env`] / [`Self::new`] without an explicit config.
    pub struct HttpLlamaServerClient {
        server_url: String,
        client: reqwest::blocking::Client,
        probe_client: reqwest::blocking::Client,
        sampling: SamplingConfig,
    }

    /// Environment variable used to auto-discover the llama.cpp
    /// loopback sidecar. When set to a non-empty URL,
    /// [`HttpLlamaServerClient::from_env`] builds a client pointing at
    /// it, letting a `docker compose` / desktop deployment wire the
    /// `llama-server` sidecar into the substrate's synthesis path
    /// without any host-side glue code. The deploy compose file sets
    /// this to `http://llama-server:8081` on the substrate service.
    ///
    /// Kept module-private (consumed only by
    /// [`HttpLlamaServerClient::from_env`]); hosts wire the sidecar by
    /// setting the variable, not by reading this symbol.
    const ENV_LLAMA_SERVER_URL: &str = "KNOWLEDGE_LLAMA_SERVER_URL";

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

        /// Auto-discover a llama.cpp sidecar from the
        /// [`ENV_LLAMA_SERVER_URL`] environment variable.
        ///
        /// Returns:
        /// * `Ok(None)` when the variable is unset or empty — no
        ///   sidecar is configured, so the caller should fall back to
        ///   its configured `server_url` (or skip the adapter).
        /// * `Ok(Some(client))` when the variable holds a non-empty
        ///   URL and the client builds successfully.
        /// * `Err` when the variable is set but the underlying
        ///   [`reqwest::blocking::Client`] builder rejects the
        ///   configuration.
        ///
        /// Leading / trailing whitespace is trimmed before use so a
        /// stray newline in a compose `environment:` entry does not
        /// produce an unreachable URL.
        ///
        /// # Errors
        ///
        /// Propagates the [`Self::new`] client-builder error when the
        /// variable is set but construction fails.
        pub fn from_env() -> Result<Option<Self>, String> {
            // `std::env::var` errors on both unset and non-UTF-8; `.ok()`
            // collapses both to `None` ("no sidecar"). The parsing /
            // construction logic lives in `from_env_value` so it can be
            // unit-tested without mutating the process-global environment.
            Self::from_env_value(std::env::var(ENV_LLAMA_SERVER_URL).ok().as_deref())
        }

        /// Core of [`Self::from_env`], split out so the discovery logic
        /// can be unit-tested without touching the process-global
        /// environment. `std::env::set_var` / `remove_var` are not
        /// thread-safe (and are `unsafe` from the 2024 edition), so the
        /// test drives this pure function with explicit inputs instead.
        ///
        /// `raw` is the raw value of [`ENV_LLAMA_SERVER_URL`] — `None`
        /// when the variable is unset (or non-UTF-8). A `Some` value is
        /// trimmed; an empty / whitespace-only string yields `Ok(None)`
        /// so a stray newline in a compose `environment:` entry does not
        /// produce an unreachable URL.
        fn from_env_value(raw: Option<&str>) -> Result<Option<Self>, String> {
            match raw {
                Some(s) => {
                    let url = s.trim();
                    if url.is_empty() {
                        Ok(None)
                    } else {
                        Self::new(url).map(Some)
                    }
                }
                None => Ok(None),
            }
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
                // Deterministic synthesis preset, with `KNOWLEDGE_SLM_*`
                // overrides applied. Override per-client via
                // `with_sampling`.
                sampling: SamplingConfig::from_env(),
            })
        }

        /// Borrow the resolved server URL (no trailing slash).
        pub fn server_url(&self) -> &str {
            &self.server_url
        }

        /// Override the [`SamplingConfig`] threaded into every
        /// `/completion` body. Defaults to [`SamplingConfig::from_env`].
        #[must_use]
        pub fn with_sampling(mut self, sampling: SamplingConfig) -> Self {
            self.sampling = sampling;
            self
        }

        /// Borrow the active [`SamplingConfig`] (useful for tests).
        #[must_use]
        pub fn sampling(&self) -> &SamplingConfig {
            &self.sampling
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
            // Direct (non-routed) calls use the client's own configured
            // sampling. The adapter path instead supplies the
            // authoritative `RouterConfig::sampling` via
            // `complete_with_sampling`.
            self.complete_with_sampling(prompt, grammar, &self.sampling)
        }

        fn complete_with_sampling(
            &self,
            prompt: &str,
            grammar: &str,
            sampling: &SamplingConfig,
        ) -> Result<String, String> {
            let url = format!("{}/completion", self.server_url);
            // Deterministic body: carries `seed` + every sampling knob
            // (see `build_completion_body`) so the same (model, prompt)
            // is byte-reproducible.
            let body = build_completion_body(prompt, grammar, sampling);

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

        #[test]
        fn from_env_value_discovers_url_and_ignores_blank() {
            // Exercises the discovery logic directly with explicit
            // inputs — no `std::env::set_var` / `remove_var`, so this is
            // sound under parallel test execution (those calls are not
            // thread-safe and are `unsafe` from the 2024 edition).

            // Unset (or non-UTF-8) → no sidecar.
            assert!(
                HttpLlamaServerClient::from_env_value(None)
                    .expect("unset must not error")
                    .is_none(),
                "absent value must yield no client"
            );

            // Empty / whitespace-only → no sidecar (trimmed).
            for blank in ["", "   ", "\n\t "] {
                assert!(
                    HttpLlamaServerClient::from_env_value(Some(blank))
                        .expect("blank must not error")
                        .is_none(),
                    "blank value {blank:?} must yield no client"
                );
            }

            // Set + surrounding whitespace → trimmed, normalised URL.
            let client =
                HttpLlamaServerClient::from_env_value(Some("  http://llama-server:8081/  "))
                    .expect("a valid URL must not error")
                    .expect("a non-empty URL must yield a client");
            assert_eq!(client.server_url(), "http://llama-server:8081");
        }

        #[test]
        fn from_env_reads_process_environment() {
            // Thin wrapper coverage: `from_env` must agree with
            // `from_env_value` for whatever the process env currently
            // holds. Read-only (`std::env::var`), so no mutation / no
            // thread-safety hazard.
            let expected = HttpLlamaServerClient::from_env_value(
                std::env::var(ENV_LLAMA_SERVER_URL).ok().as_deref(),
            )
            .map(|opt| opt.map(|c| c.server_url().to_string()));
            let actual = HttpLlamaServerClient::from_env()
                .map(|opt| opt.map(|c| c.server_url().to_string()));
            assert_eq!(actual, expected);
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

    #[test]
    fn generate_threads_config_sampling_to_client() {
        // Regression: `RouterConfig::with_sampling` must actually reach
        // the wire. Before the fix the adapter called the budget-free
        // `complete`, so a host override was silently dropped. Now the
        // adapter threads `config.sampling` via `complete_with_sampling`
        // and the spy must capture the override bit-for-bit.
        use crate::config::SamplingConfig;
        use std::sync::Arc;

        /// Spy recording the sampling each call received into an
        /// externally-held `Arc`, so the test can inspect it after the
        /// spy is boxed into the adapter.
        struct SamplingSpy {
            seen: Arc<Mutex<Vec<Option<SamplingConfig>>>>,
        }
        impl LlamaServerClient for SamplingSpy {
            fn ping(&self) -> bool {
                true
            }
            fn complete(&self, _prompt: &str, _grammar: &str) -> Result<String, String> {
                self.seen.lock().expect("seen").push(None);
                Ok("{}".to_string())
            }
            fn complete_with_sampling(
                &self,
                _prompt: &str,
                _grammar: &str,
                sampling: &SamplingConfig,
            ) -> Result<String, String> {
                self.seen.lock().expect("seen").push(Some(*sampling));
                Ok("{}".to_string())
            }
        }

        let custom = SamplingConfig::synthesis_default().with_n_predict(777);
        let cfg = RouterConfig::default()
            .with_device_tier(DeviceTier::High)
            .with_sampling(custom);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let adapter = LlamaCppAdapter::new(
            cfg,
            Box::new(SamplingSpy {
                seen: Arc::clone(&seen),
            }),
        );
        adapter.probe();
        adapter
            .generate("synth_summary", "evidence", "root ::= \"x\"")
            .expect("generate ok");

        let seen = seen.lock().expect("seen").clone();
        assert_eq!(seen.len(), 1);
        // The adapter must have used the sampling-aware path with the
        // host override, not the bare `complete` (which would record
        // `None`).
        let got = seen[0].expect("sampling-aware path used");
        assert_eq!(got.n_predict, 777);
        assert_eq!(got.seed, custom.seed);
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

        /// Async sibling of
        /// [`super::LlamaServerClient::complete_with_sampling`] — run a
        /// completion with a caller-supplied [`SamplingConfig`]. The
        /// default delegates to [`Self::complete`] so existing
        /// implementors keep working.
        async fn complete_with_sampling(
            &self,
            prompt: &str,
            grammar: &str,
            sampling: &SamplingConfig,
        ) -> Result<String, String> {
            let _ = sampling;
            self.complete(prompt, grammar).await
        }
    }

    /// Default request timeout for `/completion`. Matches the
    /// sync client's [`super::http_client::DEFAULT_HTTP_TIMEOUT_SECS`].
    pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 120;
    /// Default request timeout for `/health` probes. Matches the
    /// sync client.
    pub const DEFAULT_HTTP_PROBE_TIMEOUT_SECS: u64 = 2;

    use super::build_completion_body;
    use crate::config::SamplingConfig;

    /// Real async HTTP client for the llama.cpp loopback server.
    ///
    /// Holds two `reqwest::Client`s with separate timeouts: a long
    /// one for `/completion` and a short one for `/health` probes
    /// — same shape as the sync client.
    ///
    /// Carries the same deterministic [`SamplingConfig`] as the sync
    /// client (defaulting to [`SamplingConfig::from_env`]) and threads
    /// it through the shared [`build_completion_body`], so the async
    /// transport produces byte-identical request bodies.
    pub struct AsyncHttpLlamaServerClient {
        server_url: String,
        client: reqwest::Client,
        probe_client: reqwest::Client,
        sampling: SamplingConfig,
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
                sampling: SamplingConfig::from_env(),
            })
        }

        /// Borrow the resolved server URL (no trailing slash).
        #[must_use]
        pub fn server_url(&self) -> &str {
            &self.server_url
        }

        /// Override the [`SamplingConfig`] threaded into every
        /// `/completion` body. Defaults to [`SamplingConfig::from_env`].
        #[must_use]
        pub fn with_sampling(mut self, sampling: SamplingConfig) -> Self {
            self.sampling = sampling;
            self
        }

        /// Borrow the active [`SamplingConfig`] (useful for tests).
        #[must_use]
        pub fn sampling(&self) -> &SamplingConfig {
            &self.sampling
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
            self.complete_with_sampling(prompt, grammar, &self.sampling)
                .await
        }

        async fn complete_with_sampling(
            &self,
            prompt: &str,
            grammar: &str,
            sampling: &SamplingConfig,
        ) -> Result<String, String> {
            let url = format!("{}/completion", self.server_url);
            // Shared body builder → byte-identical to the sync client,
            // carrying `seed` + every sampling knob.
            let body = build_completion_body(prompt, grammar, sampling);
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
