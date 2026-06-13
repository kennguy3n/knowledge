//! Shared core for the on-device NPU/accelerator adapters.
//!
//! Both the Apple **Core ML / ANE** adapter ([`crate::adapters::coreml`])
//! and the **ONNX Runtime Mobile** adapter
//! ([`crate::adapters::onnx_runtime`]) are structurally identical: they
//! wrap a host-provided [`AcceleratorBackend`], gate dispatch on
//! capability detection + device tier + the determinism contract, and
//! otherwise behave exactly like every other [`InferenceAdapter`]. This
//! module factors that shared behaviour into one generic
//! [`AcceleratorAdapter<C>`] so the two concrete adapters differ only in
//! their [`AdapterKind`] tag and platform predicate (supplied by the
//! [`AcceleratorClass`] marker), with no duplicated dispatch logic.
//!
//! ## Why a host-provided backend (not a linked native crate)
//!
//! The heavy accelerator runtime — Core ML on the Apple Neural Engine,
//! or ONNX Runtime Mobile with an NNAPI / QNN / Core ML execution
//! provider — lives in the platform shell (Swift on iOS/macOS, the
//! Kotlin/C++ JNI layer on Android), exactly like the MLX runtime and
//! the `llama-server` HTTP transport. The Rust substrate holds the
//! routing seam: a [`Box<dyn AcceleratorBackend>`] the shell constructs
//! and injects. This keeps the core crate free of platform-specific
//! native build dependencies, so the workspace builds (and the
//! selection/fallback logic is unit-tested with an in-memory mock) on
//! every host — including Linux CI with no NPU — while the real kernels
//! run on-device. The adapters themselves are compiled behind the
//! `coreml` / `onnx-runtime` features so a build that does not target
//! those accelerators carries none of the code.
//!
//! ## Determinism contract
//!
//! Synthesis must stay byte-reproducible for a fixed `(model, prompt)`
//! (see `docs/technical/inference-routing.md`). Accelerator kernels are
//! frequently fused / fixed-point and need not produce bit-identical
//! logits across OS versions, so an accelerator backend reports whether
//! it guarantees a reproducible greedy-decode path via
//! [`AcceleratorCapabilities::deterministic`]. When the host requires
//! determinism ([`RouterConfig::require_deterministic_synthesis`], the
//! default) a non-deterministic accelerator simply declines synthesis in
//! [`InferenceAdapter::supports`], and the router falls through to the
//! byte-reproducible llama.cpp / MLX / CPU adapters. Classification and
//! extraction are always admitted: argmax over a small label set is
//! robust to the small numerical differences an accelerator introduces.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::config::{DeviceTier, RouterConfig, SamplingConfig};
use crate::error::RouterError;
use crate::task::InferenceTask;

/// Result of probing an accelerator backend for what it can do *right
/// now* on this device. Cheap to compute (a few flag reads) so the
/// adapter can re-query it on every `probe()` / `supports()` call
/// without caching staleness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceleratorCapabilities {
    /// The accelerator runtime is linked, the model graph is loaded,
    /// and the NPU/ANE is ready to serve inference. `false` means the
    /// adapter probes `Unavailable` and the router skips it.
    pub present: bool,
    /// The backend can run generative *synthesis* (free-form prose),
    /// not just classification/extraction. A graph compiled only for
    /// the fixed-shape classification head sets this `false`, so
    /// synthesis falls through to an SLM adapter while classification
    /// still benefits from the accelerator.
    pub supports_synthesis: bool,
    /// The backend guarantees a reproducible greedy-decode path (same
    /// `(model, prompt)` → same tokens). Gates synthesis admission when
    /// [`RouterConfig::require_deterministic_synthesis`] is set.
    pub deterministic: bool,
}

impl AcceleratorCapabilities {
    /// Capabilities for a backend that is not usable at all — the
    /// common case on a device without the accelerator, and the value a
    /// backend returns before its runtime has finished initialising.
    pub const fn unavailable() -> Self {
        Self {
            present: false,
            supports_synthesis: false,
            deterministic: false,
        }
    }

    /// A fully-capable, deterministic accelerator (present + synthesis +
    /// reproducible). Convenience constructor for hosts and tests.
    pub const fn full_deterministic() -> Self {
        Self {
            present: true,
            supports_synthesis: true,
            deterministic: true,
        }
    }
}

/// One accelerator runtime, injected by the platform shell.
///
/// Implementors translate `(prompt, grammar, sampling)` into a call
/// against the on-device NPU/ANE via their native runtime. The
/// substrate ships only the trait + an in-memory test double; the real
/// Core ML / ONNX-Runtime binding lives in the platform shell, mirroring
/// how [`crate::LlamaServerClient`] / [`crate::adapters::mlx`] are wired.
pub trait AcceleratorBackend: Send + Sync {
    /// Probe what the backend can currently do. Called on every
    /// `probe()` and `supports()`, so it must be cheap and must not
    /// block on model load (return `present: false` until ready).
    fn capabilities(&self) -> AcceleratorCapabilities;

    /// Run one inference on the accelerator. `grammar` is the GBNF
    /// constraint (empty string when none); `sampling` is the
    /// authoritative per-call sampling the adapter threads through (the
    /// router-config preset, or a synthesis adaptive-budget override).
    ///
    /// On success returns the model's textual output. On failure returns
    /// `Err(message)`; the adapter wraps it in
    /// [`RouterError::InferenceFailure`] — a *hard* error (the
    /// accelerator ran but produced unusable output), so the router does
    /// not silently retry the same task on a slower backend.
    fn generate(
        &self,
        task_tag: &str,
        prompt: &str,
        grammar: &str,
        sampling: &SamplingConfig,
    ) -> Result<String, String>;
}

/// Compile-time identity of one accelerator adapter: its stable
/// [`AdapterKind`] tag and the platform predicate that decides whether
/// the accelerator *could* exist on this build target. Implemented by a
/// zero-sized marker type per accelerator (`CoreMl`, `OnnxRuntime`).
pub trait AcceleratorClass: 'static {
    /// Stable adapter-kind tag used for metrics and diagnostics.
    const KIND: AdapterKind;

    /// Whether this accelerator can exist on the current build target
    /// (e.g. Core ML only on Apple silicon). The *runtime* presence of
    /// the hardware/model is a separate, dynamic check
    /// ([`AcceleratorBackend::capabilities`]); this is the static
    /// compile-target gate. Tests override it via
    /// [`AcceleratorAdapter::with_platform_override`].
    fn platform_supported() -> bool;
}

/// Generic NPU/accelerator adapter. Parameterised by an
/// [`AcceleratorClass`] marker so the two concrete adapters
/// (`CoreMlAdapter`, `OnnxRuntimeAdapter`) share all dispatch logic.
pub struct AcceleratorAdapter<C: AcceleratorClass> {
    config: RouterConfig,
    backend: Box<dyn AcceleratorBackend>,
    platform_supported: bool,
    available: AtomicBool,
    // `fn() -> C` is unconditionally `Send + Sync` and covariant, so the
    // adapter stays `Send + Sync` (required by `InferenceAdapter`)
    // without forcing a bound on the zero-sized marker `C`.
    _marker: PhantomData<fn() -> C>,
}

impl<C: AcceleratorClass> AcceleratorAdapter<C> {
    /// Construct an adapter wrapping `backend`, using the compile-target
    /// platform predicate from `C`.
    pub fn new(config: RouterConfig, backend: Box<dyn AcceleratorBackend>) -> Self {
        Self {
            config,
            backend,
            platform_supported: C::platform_supported(),
            available: AtomicBool::new(false),
            _marker: PhantomData,
        }
    }

    /// Construct an adapter pinned to a fixed platform-detection answer,
    /// so the unit suite can exercise both the supported and
    /// unsupported branches on any host (mirrors
    /// [`crate::adapters::mlx::MlxAdapter::with_platform_override`]).
    pub fn with_platform_override(
        config: RouterConfig,
        backend: Box<dyn AcceleratorBackend>,
        platform_supported: bool,
    ) -> Self {
        Self {
            config,
            backend,
            platform_supported,
            available: AtomicBool::new(false),
            _marker: PhantomData,
        }
    }

    /// `true` iff a synthesis task may run on this accelerator given the
    /// backend's capabilities and the config's determinism requirement.
    fn synthesis_admitted(&self, caps: AcceleratorCapabilities) -> bool {
        caps.supports_synthesis
            && (caps.deterministic || !self.config.require_deterministic_synthesis)
    }
}

impl<C: AcceleratorClass> InferenceAdapter for AcceleratorAdapter<C> {
    fn kind(&self) -> AdapterKind {
        C::KIND
    }

    fn probe(&self) -> ProbeResult {
        let tier_ok = matches!(
            self.config.device_tier,
            DeviceTier::Medium | DeviceTier::High
        );
        let present = self.backend.capabilities().present;
        let available = self.platform_supported && tier_ok && present;
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
        if !self.platform_supported {
            return false;
        }
        let caps = self.backend.capabilities();
        if !caps.present {
            return false;
        }
        match self.config.device_tier {
            // Low tier never admits an on-device accelerator: even when
            // the NPU exists, the rest of the working set (model +
            // context) does not fit the memory budget.
            DeviceTier::Low => false,
            // Mid devices use the accelerator for cheap classification
            // only, matching the MLX / llama.cpp tier profile.
            DeviceTier::Medium => task.is_classification(),
            // High devices admit synthesis too, subject to the
            // determinism contract; classification is always admitted.
            DeviceTier::High => {
                if task.is_synthesis() {
                    self.synthesis_admitted(caps)
                } else {
                    true
                }
            }
        }
    }

    fn generate(&self, task_tag: &str, prompt: &str, grammar: &str) -> Result<String, RouterError> {
        self.generate_with_sampling(task_tag, prompt, grammar, &self.config.sampling)
    }

    fn generate_with_sampling(
        &self,
        task_tag: &str,
        prompt: &str,
        grammar: &str,
        sampling: &SamplingConfig,
    ) -> Result<String, RouterError> {
        if !self.is_available() {
            return Err(RouterError::Unavailable {
                task: task_tag_static(task_tag),
            });
        }
        self.backend
            .generate(task_tag, prompt, grammar, sampling)
            .map_err(RouterError::InferenceFailure)
    }
}

/// Convert a runtime task tag back into the matching `&'static str` the
/// [`RouterError`] type stores. Falls back to a stable `"unknown"`
/// constant (mirrors the helper in the MLX / managed-cloud adapters).
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

/// In-memory accelerator backend for unit and integration tests.
///
/// Gated behind `cfg(any(test, feature = "test-support"))` so the
/// crate's own unit tests and the sibling integration tests (built with
/// `--all-features`) can inject a fully-controlled backend without an
/// actual NPU, while production builds never compile it.
#[cfg(any(test, feature = "test-support"))]
pub struct MockAcceleratorBackend {
    caps: AcceleratorCapabilities,
    response: std::sync::Mutex<Result<String, String>>,
}

#[cfg(any(test, feature = "test-support"))]
impl MockAcceleratorBackend {
    /// A backend with the given capabilities that echoes a fixed `Ok`
    /// response from [`AcceleratorBackend::generate`].
    pub fn new(caps: AcceleratorCapabilities, response: impl Into<String>) -> Self {
        Self {
            caps,
            response: std::sync::Mutex::new(Ok(response.into())),
        }
    }

    /// A backend that is not present (the router skips it on probe).
    pub fn unavailable() -> Self {
        Self::new(AcceleratorCapabilities::unavailable(), String::new())
    }

    /// A present, synthesis-capable, deterministic backend.
    pub fn full_deterministic(response: impl Into<String>) -> Self {
        Self::new(AcceleratorCapabilities::full_deterministic(), response)
    }

    /// A present backend whose synthesis path is *not* reproducible
    /// (the realistic ANE / NPU-EP case). Synthesis is declined when the
    /// config requires determinism; classification is still served.
    pub fn nondeterministic(response: impl Into<String>) -> Self {
        Self::new(
            AcceleratorCapabilities {
                present: true,
                supports_synthesis: true,
                deterministic: false,
            },
            response,
        )
    }

    /// A present backend that returns a hard inference error.
    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            caps: AcceleratorCapabilities::full_deterministic(),
            response: std::sync::Mutex::new(Err(message.into())),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl AcceleratorBackend for MockAcceleratorBackend {
    fn capabilities(&self) -> AcceleratorCapabilities {
        self.caps
    }

    fn generate(
        &self,
        _task_tag: &str,
        _prompt: &str,
        _grammar: &str,
        _sampling: &SamplingConfig,
    ) -> Result<String, String> {
        self.response
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Marker used only by the shared-core tests so this module is
    /// exercisable without depending on the feature-gated concrete
    /// adapters.
    struct TestClass;
    impl AcceleratorClass for TestClass {
        const KIND: AdapterKind = AdapterKind::CoreMl;
        fn platform_supported() -> bool {
            true
        }
    }
    type TestAdapter = AcceleratorAdapter<TestClass>;

    fn high() -> RouterConfig {
        RouterConfig::default().with_device_tier(DeviceTier::High)
    }

    #[test]
    fn probe_unavailable_off_platform() {
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
            false,
        );
        assert_eq!(a.probe(), ProbeResult::Unavailable);
        assert!(!a.is_available());
    }

    #[test]
    fn probe_unavailable_when_backend_absent() {
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::unavailable()),
            true,
        );
        assert_eq!(a.probe(), ProbeResult::Unavailable);
    }

    #[test]
    fn probe_available_on_platform_high_tier_with_backend() {
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
            true,
        );
        assert_eq!(a.probe(), ProbeResult::Available);
        assert!(a.is_available());
    }

    #[test]
    fn low_tier_disables_accelerator() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
        let a = TestAdapter::with_platform_override(
            cfg,
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
            true,
        );
        assert_eq!(a.probe(), ProbeResult::Unavailable);
        assert!(!a.supports(InferenceTask::TagImportance));
    }

    #[test]
    fn medium_tier_supports_only_classification() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Medium);
        let a = TestAdapter::with_platform_override(
            cfg,
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
            true,
        );
        assert!(a.supports(InferenceTask::TagImportance));
        assert!(!a.supports(InferenceTask::SynthSummary));
    }

    #[test]
    fn high_tier_supports_synthesis_when_deterministic() {
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
            true,
        );
        assert!(a.supports(InferenceTask::SynthSummary));
    }

    #[test]
    fn nondeterministic_backend_declines_synthesis_when_determinism_required() {
        // Default config requires determinism → synthesis declined, but
        // classification still served (argmax is robust).
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::nondeterministic("x")),
            true,
        );
        assert!(!a.supports(InferenceTask::SynthSummary));
        assert!(a.supports(InferenceTask::TagImportance));
    }

    #[test]
    fn nondeterministic_backend_admits_synthesis_when_determinism_waived() {
        let cfg = high().with_require_deterministic_synthesis(false);
        let a = TestAdapter::with_platform_override(
            cfg,
            Box::new(MockAcceleratorBackend::nondeterministic("x")),
            true,
        );
        assert!(a.supports(InferenceTask::SynthSummary));
    }

    #[test]
    fn generate_returns_unavailable_before_probe() {
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("ok")),
            true,
        );
        // Not probed yet → not available → fallback error.
        let err = a.generate("synth_summary", "p", "").unwrap_err();
        assert!(err.is_fallback());
    }

    #[test]
    fn generate_dispatches_through_backend_when_available() {
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("the-answer")),
            true,
        );
        a.probe();
        assert_eq!(a.generate("synth_summary", "p", "").unwrap(), "the-answer");
    }

    #[test]
    fn backend_error_is_hard_inference_failure() {
        let a = TestAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::failing("npu fault")),
            true,
        );
        a.probe();
        let err = a.generate("synth_summary", "p", "").unwrap_err();
        assert!(
            !err.is_fallback(),
            "a backend error must not trigger fallback"
        );
        assert!(matches!(err, RouterError::InferenceFailure(_)));
    }
}
