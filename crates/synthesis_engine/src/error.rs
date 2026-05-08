//! Error type for the synthesis engine.

use thiserror::Error;

use synthesis_pipeline::PipelineError;

/// Errors raised by the synthesis engine.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The supplied input bundle violated the hierarchy contract.
    #[error("hierarchy violation: {0}")]
    Hierarchy(String),

    /// The underlying [`synthesis_pipeline`] surfaced an error.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),

    /// The remote managed-endpoint adapter surfaced an error
    /// (timeout, rate limit, malformed response, ...). The string
    /// payload preserves the original adapter-side error message so
    /// the audit log can attribute the failure.
    #[error("managed endpoint error: {0}")]
    Endpoint(String),
}

/// Convenience result alias.
pub type Result<T, E = EngineError> = std::result::Result<T, E>;
