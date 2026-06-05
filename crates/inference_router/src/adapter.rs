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

    /// Run the inference. `task_tag` is the stable string tag for
    /// metrics; `prompt` is the fully-rendered prompt; `grammar` is
    /// the GBNF grammar (empty string when no grammar is required).
    ///
    /// Returns the model's textual output. On failure callers should
    /// fall back to the next adapter in priority order or to the
    /// classifier ladder.
    fn generate(&self, task_tag: &str, prompt: &str, grammar: &str) -> Result<String, RouterError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_kind_strings_are_stable() {
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
