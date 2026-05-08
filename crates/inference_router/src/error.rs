//! Router error type.

use thiserror::Error;

/// Errors emitted by the [`crate::InferenceRouter`] and its adapters.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum RouterError {
    /// No adapter is available for the requested task. The substrate
    /// is expected to handle this by falling back to a deterministic
    /// classifier (lexicon, encoder-only) per `ARCHITECTURE.md` §3.
    #[error("no inference adapter is available for task `{task}`")]
    Unavailable {
        /// The stable string tag for the task that could not be served.
        task: &'static str,
    },
    /// The adapter rejected the task because the device tier is too
    /// low to run the underlying model. Used by the [`crate::MlxAdapter`]
    /// and [`crate::LlamaCppAdapter`] gates.
    #[error("device tier `{tier}` does not support task `{task}`")]
    TierTooLow {
        /// String tag for the device tier.
        tier: &'static str,
        /// String tag for the task.
        task: &'static str,
    },
    /// The underlying SLM call failed (network, timeout, model error,
    /// JSON-grammar violation). The substrate should fall back to the
    /// classifier ladder per `PROPOSAL.md` §6.
    #[error("inference call failed: {0}")]
    InferenceFailure(String),
    /// The adapter has not been probed yet — the router refuses to
    /// dispatch tasks before [`crate::InferenceRouter::bootstrap`].
    #[error("adapter `{adapter}` has not been probed")]
    NotProbed {
        /// String tag for the adapter.
        adapter: &'static str,
    },
}

impl RouterError {
    /// `true` when this is the substrate's signal to fall back to the
    /// next classifier in the ladder.
    pub fn is_fallback(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::TierTooLow { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_signals_fallback() {
        let err = RouterError::Unavailable {
            task: "tag_importance",
        };
        assert!(err.is_fallback());
    }

    #[test]
    fn tier_too_low_signals_fallback() {
        let err = RouterError::TierTooLow {
            tier: "low",
            task: "synth_summary",
        };
        assert!(err.is_fallback());
    }

    #[test]
    fn inference_failure_does_not_signal_fallback() {
        let err = RouterError::InferenceFailure("network down".into());
        assert!(!err.is_fallback());
    }
}
