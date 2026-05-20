//! Error type for the audit service.

use thiserror::Error;

/// Errors raised by the audit service.
#[derive(Debug, Error)]
pub enum AuditError {
    /// An attempt was made to mutate an existing entry. The audit log
    /// is append-only.
    #[error("audit entries are immutable; mutation rejected")]
    EntryImmutable,

    /// An audit-entry builder was missing required fields.
    #[error("audit entry builder missing field: {0}")]
    MissingField(&'static str),

    /// A persistence-layer invariant was violated.
    #[error("audit-service persistence error: {0}")]
    Persistence(&'static str),

    /// The underlying SQLCipher driver surfaced an error.
    #[error("audit-service sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The underlying [`crypto`] crate surfaced an error.
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),
}

impl PartialEq for AuditError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::EntryImmutable, Self::EntryImmutable) => true,
            // Both variants carry `&'static str`, identical semantic.
            (Self::MissingField(a), Self::MissingField(b))
            | (Self::Persistence(a), Self::Persistence(b)) => a == b,
            // SQLite + crypto errors carry opaque inner state — we
            // intentionally treat them as never structurally equal
            // so existing tests comparing hot-path variants keep
            // working without relying on driver-specific equality.
            _ => false,
        }
    }
}

impl Eq for AuditError {}

/// Convenience result alias.
pub type Result<T, E = AuditError> = std::result::Result<T, E>;
