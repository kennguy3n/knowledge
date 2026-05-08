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
