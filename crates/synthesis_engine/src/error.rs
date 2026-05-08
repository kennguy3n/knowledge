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

    /// The synthesis engine itself refused to operate (e.g. the TEE
    /// worker's attestation has expired, the requested scope is not
    /// bound to the worker, the worker is not yet attested). The
    /// string payload preserves the engine-side error message so the
    /// audit log can attribute the failure.
    #[error("engine error: {0}")]
    Engine(String),
}

impl EngineError {
    /// Construct an [`EngineError::Engine`] from any displayable
    /// message — used by the TEE worker for attestation / scope /
    /// lifecycle refusals.
    pub fn engine(msg: impl Into<String>) -> Self {
        EngineError::Engine(msg.into())
    }
}

/// Convenience result alias.
pub type Result<T, E = EngineError> = std::result::Result<T, E>;
