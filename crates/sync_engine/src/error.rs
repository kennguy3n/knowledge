//! Error type for the sync engine.

use thiserror::Error;
use uuid::Uuid;

/// Errors surfaced by the sync engine.
#[derive(Debug, Error)]
pub enum SyncError {
    /// A sync code path that is not yet implemented.
    #[error("sync engine: {0}")]
    NotYetImplemented(&'static str),

    /// An op-log entry referenced an element that has not been
    /// observed via a prior `Add`.
    #[error("op log references unknown element: {0}")]
    UnknownElement(Uuid),

    /// A serialisation failure when persisting / replaying ops.
    #[error("op-log serialisation failure: {0}")]
    Serialisation(&'static str),
}

/// Convenience result alias.
pub type Result<T, E = SyncError> = std::result::Result<T, E>;
