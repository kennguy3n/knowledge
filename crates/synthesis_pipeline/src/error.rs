//! Error type for the synthesis pipeline.

use thiserror::Error;
use uuid::Uuid;

/// Errors surfaced by the synthesis pipeline.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// The pipeline was given a window with a non-positive duration
    /// (`window_end <= window_start`).
    #[error("invalid synthesis window: window_end must be after window_start")]
    InvalidWindow,

    /// A window-status transition that the manager does not allow
    /// (e.g. `Complete -> InProgress`).
    #[error("invalid window transition")]
    InvalidWindowTransition,

    /// The requested window id is not present.
    #[error("synthesis window not found: {0}")]
    WindowNotFound(Uuid),

    /// The requested synthesis object id is not present.
    #[error("synthesis object not found: {0}")]
    ObjectNotFound(Uuid),

    /// The election protocol could not pick a candidate (no eligible
    /// candidates in the pool).
    #[error("synthesizer election: no eligible candidates")]
    NoEligibleSynthesizer,

    /// The election was queried for a device id that has never been
    /// registered with [`crate::election::SynthesizerElection::register`].
    #[error("election candidate not found: {0}")]
    CandidateNotFound(Uuid),

    /// Underlying crypto operation failed (publish / consume).
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),

    /// Underlying serialisation failed (publish / consume).
    #[error("synthesis object serialisation failed: {0}")]
    Serialisation(&'static str),

    /// A synthesis-hierarchy rule was violated (`PROPOSAL.md` §6.3 —
    /// e.g. a domain window was offered raw evidence, or a tenant
    /// window was offered a channel object).
    #[error("synthesis hierarchy violation: {0}")]
    HierarchyViolation(String),
}

/// Convenience result alias.
pub type Result<T, E = PipelineError> = std::result::Result<T, E>;
