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
}

/// Convenience result alias.
pub type Result<T, E = EngineError> = std::result::Result<T, E>;
