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

    /// A delta byte-blob could not be decoded into a vector of ops.
    #[error("delta decode failure")]
    DeltaDecode,

    /// The supplied delta was authored at a compaction epoch that
    /// is incompatible with the local engine — the receiver is
    /// behind the sender's compaction point and must bootstrap
    /// from a snapshot (see [`crate::SyncEngine::snapshot`]) before
    /// further delta sync.
    #[error("delta compaction epoch mismatch: local={local}, delta={delta}")]
    CompactionEpochBehind {
        /// Local engine's compaction epoch.
        local: u64,
        /// Delta-authored compaction epoch.
        delta: u64,
    },

    /// SQLite / SQLCipher level error from the persistence layer.
    #[error("persistence sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Crypto-layer error (key derivation, AEAD seal/open) from the
    /// persistence layer.
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),

    /// Generic persistence-layer error with a static cause string.
    #[error("persistence error: {0}")]
    Persistence(&'static str),
}

/// Convenience result alias.
pub type Result<T, E = SyncError> = std::result::Result<T, E>;
