//! Error type for the memory manager.

use thiserror::Error;
use uuid::Uuid;

use crate::state::MemoryState;

/// Errors surfaced by the memory manager.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MemoryError {
    /// A memory state transition was attempted that is not permitted
    /// by the decay state machine (`ARCHITECTURE.md` §7).
    #[error("invalid transition: {from:?} -> {to:?}")]
    InvalidTransition {
        /// State the object was in when the transition was attempted.
        from: MemoryState,
        /// State the caller asked to move to.
        to: MemoryState,
    },

    /// The requested memory object id is not present.
    #[error("memory object not found: {0}")]
    NotFound(Uuid),

    /// A retention-score computation produced a non-finite value.
    /// This indicates a bug in the inputs (NaN / infinity) — callers
    /// must never persist a non-finite score.
    #[error("retention score is non-finite (likely NaN or infinity)")]
    NonFiniteRetentionScore,

    /// Caller-side validation failure (e.g. summarising an empty
    /// session, ingesting an out-of-order observation stream).
    #[error("validation error: {0}")]
    Validation(String),
}

/// Convenience result alias.
pub type Result<T, E = MemoryError> = std::result::Result<T, E>;
