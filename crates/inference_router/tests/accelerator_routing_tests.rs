//! End-to-end routing tests for the NPU/accelerator adapters.
//!
//! These build a real [`InferenceRouter`] from the concrete
//! [`CoreMlAdapter`] / [`OnnxRuntimeAdapter`] (wrapping an in-memory
//! [`MockAcceleratorBackend`]) plus a deterministic SLM stand-in and the
//! encoder-only [`FallbackAdapter`], then assert the *observable* result
//! of `dispatch` — i.e. that the accelerator wins when it should, and
//! that the router transparently falls through to the SLM / fallback
//! when the accelerator is absent or declines the task.
//!
//! The whole file is gated on both accelerator features (built under
//! `--all-features`), so it is empty — and harmless — when they are off.
#![cfg(all(feature = "coreml", feature = "onnx-runtime", feature = "test-support"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use inference_router::adapters::MockAcceleratorBackend;
use inference_router::{
    AdapterKind, CoreMlAdapter, DeviceTier, FallbackAdapter, InferenceAdapter, InferenceRouter,
    InferenceTask, OnnxRuntimeAdapter, ProbeResult, RouterConfig, RouterError,
};

/// Minimal deterministic SLM stand-in (plays the MLX / llama.cpp role)
/// that answers a fixed set of tasks. Lets us prove a task that the
/// accelerator declines falls through to the reproducible SLM path.
struct DeterministicSlm {
    available: AtomicBool,
    response: Mutex<Result<String, RouterError>>,
}

impl DeterministicSlm {
    fn ok(text: &str) -> Self {
        Self {
            available: AtomicBool::new(true),
            response: Mutex::new(Ok(text.to_string())),
        }
    }
}

impl InferenceAdapter for DeterministicSlm {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Mlx
    }
    fn probe(&self) -> ProbeResult {
        if self.available.load(Ordering::SeqCst) {
            ProbeResult::Available
        } else {
            ProbeResult::Unavailable
        }
    }
    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
    fn supports(&self, _task: InferenceTask) -> bool {
        true
    }
    fn generate(
        &self,
        _task_tag: &str,
        _prompt: &str,
        _grammar: &str,
    ) -> Result<String, RouterError> {
        self.response.lock().unwrap().clone()
    }
}

fn high() -> RouterConfig {
    RouterConfig::default().with_device_tier(DeviceTier::High)
}

fn boot(config: RouterConfig, adapters: Vec<Box<dyn InferenceAdapter>>) -> InferenceRouter {
    let router = InferenceRouter::new(config, adapters);
    router.bootstrap();
    router
}

#[test]
fn coreml_wins_synthesis_when_deterministic() {
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![
        Box::new(CoreMlAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("ane-brief")),
            true,
        )),
        Box::new(DeterministicSlm::ok("slm-brief")),
        Box::new(FallbackAdapter::new()),
    ];
    let router = boot(high(), adapters);
    assert_eq!(
        router.dispatch(InferenceTask::SynthSummary, "p").unwrap(),
        "ane-brief"
    );
}

#[test]
fn nondeterministic_coreml_yields_synthesis_to_slm_but_keeps_classification() {
    // Default config requires determinism. The ANE serves the
    // classification task (argmax is robust) but declines synthesis, so
    // synthesis falls through to the reproducible SLM.
    let config = high();
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![
        Box::new(CoreMlAdapter::with_platform_override(
            config.clone(),
            Box::new(MockAcceleratorBackend::nondeterministic("ane-out")),
            true,
        )),
        Box::new(DeterministicSlm::ok("slm-brief")),
        Box::new(FallbackAdapter::new()),
    ];
    let router = boot(config, adapters);

    assert_eq!(
        router.dispatch(InferenceTask::SynthSummary, "p").unwrap(),
        "slm-brief",
        "synthesis must use the deterministic SLM",
    );
    assert_eq!(
        router.dispatch(InferenceTask::TagImportance, "p").unwrap(),
        "ane-out",
        "classification stays on the accelerator",
    );
}

#[test]
fn absent_accelerator_falls_through_to_slm() {
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![
        Box::new(OnnxRuntimeAdapter::new(
            high(),
            Box::new(MockAcceleratorBackend::unavailable()),
        )),
        Box::new(DeterministicSlm::ok("slm-brief")),
        Box::new(FallbackAdapter::new()),
    ];
    let router = boot(high(), adapters);
    assert_eq!(
        router.dispatch(InferenceTask::SynthSummary, "p").unwrap(),
        "slm-brief"
    );
}

#[test]
fn off_platform_coreml_is_skipped() {
    // platform_supported = false (e.g. an Android build): the Core ML
    // slot is inert and the router uses the next adapter.
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![
        Box::new(CoreMlAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("ane")),
            false,
        )),
        Box::new(DeterministicSlm::ok("slm-brief")),
        Box::new(FallbackAdapter::new()),
    ];
    let router = boot(high(), adapters);
    assert_eq!(
        router.dispatch(InferenceTask::SynthSummary, "p").unwrap(),
        "slm-brief"
    );
}

#[test]
fn onnx_classification_dispatches_to_accelerator() {
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![
        Box::new(OnnxRuntimeAdapter::new(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("npu-label")),
        )),
        Box::new(DeterministicSlm::ok("slm")),
        Box::new(FallbackAdapter::new()),
    ];
    let router = boot(high(), adapters);
    assert_eq!(
        router
            .dispatch(InferenceTask::ExtractEntities, "p")
            .unwrap(),
        "npu-label"
    );
}

#[test]
fn low_tier_skips_accelerator_entirely() {
    let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![
        Box::new(CoreMlAdapter::with_platform_override(
            cfg.clone(),
            Box::new(MockAcceleratorBackend::full_deterministic("ane")),
            true,
        )),
        Box::new(FallbackAdapter::new()),
    ];
    let router = boot(cfg, adapters);
    // Classification falls to the fallback; synthesis has no home → error.
    assert!(router.dispatch(InferenceTask::TagImportance, "p").is_ok());
    assert!(matches!(
        router.dispatch(InferenceTask::SynthSummary, "p"),
        Err(RouterError::Unavailable { .. })
    ));
}
