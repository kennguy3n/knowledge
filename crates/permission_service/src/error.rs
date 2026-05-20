//! Error type for the permission service.

use thiserror::Error;

/// Errors raised by the permission service.
#[derive(Debug, Error)]
pub enum PermissionError {
    /// The supplied tuple was already present in the store.
    #[error("relation tuple already exists")]
    DuplicateTuple,

    /// The supplied tuple did not match any tuple in the store.
    #[error("relation tuple not found")]
    NotFound,

    /// A namespace configuration was registered twice for the same
    /// object type.
    #[error("namespace already registered: {0:?}")]
    NamespaceAlreadyRegistered(crate::tuple::ObjectType),

    /// A persistence-layer invariant was violated.
    #[error("permission-service persistence error: {0}")]
    Persistence(&'static str),

    /// The underlying SQLCipher driver surfaced an error.
    #[error("permission-service sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The underlying [`crypto`] crate surfaced an error.
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),
}

impl PartialEq for PermissionError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::DuplicateTuple, Self::DuplicateTuple) => true,
            (Self::NotFound, Self::NotFound) => true,
            (Self::NamespaceAlreadyRegistered(a), Self::NamespaceAlreadyRegistered(b)) => a == b,
            (Self::Persistence(a), Self::Persistence(b)) => a == b,
            // SQLite + crypto errors carry opaque inner state — we
            // intentionally treat them as never structurally equal so
            // existing tests that compare hot-path variants keep
            // working without relying on driver-specific equality.
            _ => false,
        }
    }
}

impl Eq for PermissionError {}

/// Convenience result alias.
pub type Result<T, E = PermissionError> = std::result::Result<T, E>;
