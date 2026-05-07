//! Error type for the observation engine.

use thiserror::Error;

/// Errors surfaced by the observation engine.
#[derive(Debug, Error)]
pub enum ObservationError {
    /// The pipeline was asked to extract from empty input.
    #[error("observation pipeline received empty input")]
    EmptyInput,

    /// An underlying memory-manager call failed.
    #[error(transparent)]
    Memory(#[from] memory_manager::MemoryError),
}

/// Convenience result alias.
pub type Result<T, E = ObservationError> = std::result::Result<T, E>;
