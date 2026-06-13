//! [`InferenceAdapter`] trait and probe result.
//!
//! An adapter is one concrete inference backend (MLX, llama.cpp, …)
//! plugged into the [`crate::InferenceRouter`]. The trait surface is
//! deliberately tiny so adapters can be unit-tested against in-memory
//! fakes.

use crate::error::RouterError;
use crate::task::InferenceTask;

/// What kind of adapter this is — used for diagnostics and as the
/// stable string tag in metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterKind {
    /// Apple Core ML, executed on the Apple Neural Engine (ANE) when
    /// the model graph is ANE-resident. Highest-priority on-device
    /// accelerator on Apple silicon.
    CoreMl,
    /// ONNX Runtime Mobile with an NPU execution provider
    /// (NNAPI / QNN on Android, Core ML EP on iOS). Cross-platform
    /// on-device accelerator path.
    OnnxRuntime,
    /// Apple Silicon MLX runtime.
    Mlx,
    /// llama.cpp loopback HTTP server.
    LlamaCpp,
    /// External OpenAI-compatible managed-cloud endpoint (synthesis
    /// without a self-hosted SLM).
    ManagedCloud,
    /// Encoder-only fallback. No SLM; classification only.
    Fallback,
    /// Mock adapter for tests.
    Mock,
}

impl AdapterKind {
    /// Stable string tag for the adapter kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CoreMl => "coreml",
            Self::OnnxRuntime => "onnx_runtime",
            Self::Mlx => "mlx",
            Self::LlamaCpp => "llama_cpp",
            Self::ManagedCloud => "managed_cloud",
            Self::Fallback => "fallback",
            Self::Mock => "mock",
        }
    }
}

/// Result of probing one adapter at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResult {
    /// Adapter is available and ready to receive tasks.
    Available,
    /// Adapter is not available on this device (wrong platform, no
    /// server, missing model, …). The router must skip it and try
    /// the next one in priority order.
    Unavailable,
}

/// One inference backend.
///
/// Implementors are expected to be cheap to construct (the heavy
/// model load happens on the first call to [`Self::generate`]) and
/// to be safe to call concurrently.
pub trait InferenceAdapter: Send + Sync {
    /// Stable string tag.
    fn kind(&self) -> AdapterKind;

    /// Probe the adapter. Called once at boot; the router caches the
    /// result.
    fn probe(&self) -> ProbeResult;

    /// `true` iff the most recent [`Self::probe`] returned
    /// [`ProbeResult::Available`].
    fn is_available(&self) -> bool;

    /// `true` iff this adapter can serve `task`. Synthesis tasks are
    /// rejected by the [`crate::FallbackAdapter`].
    fn supports(&self, task: InferenceTask) -> bool;

    /// `true` iff a [`crate::InferenceRouter::warm_up`] no-op request
    /// pages something useful in for this adapter. Local backends
    /// (MLX, llama.cpp) keep the default `true`; remote adapters that
    /// bill per request (e.g. managed cloud) return `false` so warm-up
    /// never spends money priming weights that live off-device.
    fn benefits_from_warm_up(&self) -> bool {
        true
    }

    /// Run the inference. `task_tag` is the stable string tag for
    /// metrics; `prompt` is the fully-rendered prompt; `grammar` is
    /// the GBNF grammar (empty string when no grammar is required).
    ///
    /// Returns the model's textual output. On failure callers should
    /// fall back to the next adapter in priority order or to the
    /// classifier ladder.
    fn generate(&self, task_tag: &str, prompt: &str, grammar: &str) -> Result<String, RouterError>;

    /// Run inference with a caller-supplied [`SamplingConfig`] that
    /// overrides the adapter's configured sampling for this one call.
    ///
    /// The default implementation ignores `sampling` and delegates to
    /// [`Self::generate`], so adapters that cannot vary sampling
    /// per-call (the classifier [`crate::FallbackAdapter`], MLX) keep
    /// working unchanged. SLM adapters override it to thread the
    /// supplied knobs onto the wire. This is the seam the synthesis
    /// pipeline uses to raise `n_predict` for an adaptive token budget
    /// and a verify-and-retry second attempt while holding every other
    /// knob fixed, so the call stays as reproducible as the base
    /// [`crate::RouterConfig::sampling`] preset.
    fn generate_with_sampling(
        &self,
        task_tag: &str,
        prompt: &str,
        grammar: &str,
        sampling: &crate::config::SamplingConfig,
    ) -> Result<String, RouterError> {
        let _ = sampling;
        self.generate(task_tag, prompt, grammar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_kind_strings_are_stable() {
        assert_eq!(AdapterKind::CoreMl.as_str(), "coreml");
        assert_eq!(AdapterKind::OnnxRuntime.as_str(), "onnx_runtime");
        assert_eq!(AdapterKind::Mlx.as_str(), "mlx");
        assert_eq!(AdapterKind::LlamaCpp.as_str(), "llama_cpp");
        assert_eq!(AdapterKind::ManagedCloud.as_str(), "managed_cloud");
        assert_eq!(AdapterKind::Fallback.as_str(), "fallback");
        assert_eq!(AdapterKind::Mock.as_str(), "mock");
    }

    #[test]
    fn probe_result_equality() {
        assert_eq!(ProbeResult::Available, ProbeResult::Available);
        assert_ne!(ProbeResult::Available, ProbeResult::Unavailable);
    }
}
