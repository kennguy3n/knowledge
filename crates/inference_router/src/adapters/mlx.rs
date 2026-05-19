//! Apple Silicon MLX adapter.
//!
//! The production adapter binds to the MLX runtime via Swift / Obj-C++
//! and runs the SLM with INT4 quantisation on the Apple Neural Engine.
//! In Rust-only land this crate ships a skeleton: [`MlxAdapter`]
//! detects whether it is running on Apple Silicon, gates dispatch by
//! the configured device tier, and **verifies the MLX runtime is
//! actually linked** before reporting itself as `Available`. If the
//! runtime is absent (as in the pure-Rust crate), both `probe()` and
//! `generate()` return [`crate::RouterError::Unavailable`] so the
//! router falls through to the next adapter (e.g. llama.cpp).

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
        let runtime_linked = mlx_runtime_linked();
        let available = is_apple && tier_ok && runtime_linked;
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
        // Return Unavailable (not InferenceFailure) so the router's
        // `is_fallback()` check allows fallthrough to llama.cpp.
        Err(RouterError::Unavailable {
            task: task_tag_static(task_tag),
        })
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

/// Returns `true` only when the MLX Swift runtime is linked into this
/// binary. The pure-Rust crate never has it; the iOS / macOS native
/// shell sets the runtime-linked flag at init time via
/// [`set_mlx_runtime_linked`]. When the flag is not set (default),
/// this returns `false` so `probe()` reports `Unavailable` and the
/// router falls through to the next adapter.
fn mlx_runtime_linked() -> bool {
    MLX_RUNTIME_LINKED.load(Ordering::Acquire)
}

/// Global flag: set to `true` by the native shell (iOS / macOS) once
/// the MLX Swift runtime is initialised and ready for inference.
static MLX_RUNTIME_LINKED: AtomicBool = AtomicBool::new(false);

/// Called by the native shell to signal that the MLX runtime is
/// available and ready. This must be called before [`MlxAdapter::probe`]
/// for the adapter to report `Available`.
pub fn set_mlx_runtime_linked(linked: bool) {
    MLX_RUNTIME_LINKED.store(linked, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Tests that touch the global `MLX_RUNTIME_LINKED` flag must be
    /// serialized — Cargo runs tests in parallel by default and the
    /// flag is process-global mutable state.
    fn mlx_lock() -> MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn probe_unavailable_off_apple_silicon() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, false);
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
        assert!(!adapter.is_available());
    }

    #[test]
    fn probe_unavailable_on_apple_silicon_without_runtime() {
        let _g = mlx_lock();
        set_mlx_runtime_linked(false);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
        assert!(!adapter.is_available());
    }

    #[test]
    fn probe_available_on_apple_silicon_high_tier_with_runtime() {
        let _g = mlx_lock();
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert_eq!(adapter.probe(), ProbeResult::Available);
        assert!(adapter.is_available());
        set_mlx_runtime_linked(false);
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

    #[test]
    fn generate_returns_fallback_error_when_runtime_not_linked() {
        let _g = mlx_lock();
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        adapter.probe();
        assert!(adapter.is_available());
        // Even when probe says available, generate returns Unavailable
        // (a fallback error) because the MLX runtime isn't truly linked
        // — the Rust crate always hits this path.
        set_mlx_runtime_linked(false);
        // Force re-probe so is_available correctly reflects the state
        adapter.probe();
        let err = adapter.generate("synth_summary", "", "").unwrap_err();
        assert!(
            err.is_fallback(),
            "MLX generate() must return a fallback error"
        );
    }
}
