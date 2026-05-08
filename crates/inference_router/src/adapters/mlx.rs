//! Apple Silicon MLX adapter.
//!
//! The production adapter binds to the MLX runtime via Swift / Obj-C++
//! and runs the SLM with INT4 quantisation on the Apple Neural Engine.
//! In Rust-only land this crate ships a skeleton: [`MlxAdapter`]
//! detects whether it is running on Apple Silicon, gates dispatch by
//! the configured device tier, and returns
//! [`crate::RouterError::Unavailable`] otherwise so the router falls
//! through to the next adapter.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::config::{DeviceTier, RouterConfig};
use crate::error::RouterError;
use crate::task::InferenceTask;

/// MLX adapter. Available on Apple Silicon (`aarch64-apple-darwin`)
/// when the device tier is `Medium` or `High`.
pub struct MlxAdapter {
    config: RouterConfig,
    available: AtomicBool,
    /// Test override — when `Some(true)` the adapter pretends to be
    /// running on Apple Silicon, when `Some(false)` it pretends not
    /// to. `None` consults the actual platform.
    platform_override: Option<bool>,
}

impl MlxAdapter {
    /// Construct a new MLX adapter from the given router config.
    pub fn new(config: RouterConfig) -> Self {
        Self {
            config,
            available: AtomicBool::new(false),
            platform_override: None,
        }
    }

    /// Construct an MLX adapter pinned to a fixed platform-detection
    /// answer. Used by tests so the unit suite can exercise both
    /// branches on any host.
    pub fn with_platform_override(config: RouterConfig, on_apple_silicon: bool) -> Self {
        Self {
            config,
            available: AtomicBool::new(false),
            platform_override: Some(on_apple_silicon),
        }
    }

    /// Whether this build is running on Apple Silicon. Honours the
    /// test override if set.
    fn on_apple_silicon(&self) -> bool {
        if let Some(forced) = self.platform_override {
            return forced;
        }
        cfg!(all(target_arch = "aarch64", target_os = "macos"))
    }
}

impl InferenceAdapter for MlxAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Mlx
    }

    fn probe(&self) -> ProbeResult {
        let is_apple = self.on_apple_silicon();
        let tier_ok = matches!(
            self.config.device_tier,
            DeviceTier::Medium | DeviceTier::High
        );
        let available = is_apple && tier_ok;
        self.available.store(available, Ordering::SeqCst);
        if available {
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

    fn generate(
        &self,
        task_tag: &str,
        _prompt: &str,
        _grammar: &str,
    ) -> Result<String, RouterError> {
        if !self.is_available() {
            return Err(RouterError::Unavailable {
                task: task_tag_static(task_tag),
            });
        }
        // The Rust crate cannot link the MLX runtime on its own; the
        // production binding lives in the iOS / macOS native shell.
        // For the substrate-side unit tests we surface this as a
        // structured error so the router falls through to llama.cpp.
        Err(RouterError::InferenceFailure(
            "mlx runtime is not linked into the rust crate".into(),
        ))
    }
}

/// Adapter-side helper: convert a runtime task tag string back into
/// the matching `&'static str` used by [`crate::RouterError`]. Falls
/// back to a stable `"unknown"` constant for untagged calls so the
/// error type can stay `'static`-ful.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_unavailable_off_apple_silicon() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, false);
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
        assert!(!adapter.is_available());
    }

    #[test]
    fn probe_available_on_apple_silicon_high_tier() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert_eq!(adapter.probe(), ProbeResult::Available);
        assert!(adapter.is_available());
    }

    #[test]
    fn low_tier_disables_adapter_even_on_apple_silicon() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
    }

    #[test]
    fn medium_tier_supports_only_classification() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Medium);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert!(adapter.supports(InferenceTask::TagImportance));
        assert!(!adapter.supports(InferenceTask::SynthSummary));
    }

    #[test]
    fn high_tier_supports_synthesis() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert!(adapter.supports(InferenceTask::SynthSummary));
    }

    #[test]
    fn generate_when_unavailable_returns_unavailable() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
        let adapter = MlxAdapter::with_platform_override(cfg, false);
        adapter.probe();
        let err = adapter.generate("tag_importance", "", "").unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }
}
