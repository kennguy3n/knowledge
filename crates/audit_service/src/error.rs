//! Error type for the audit service.

use thiserror::Error;

/// Errors raised by the audit service.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuditError {
    /// An attempt was made to mutate an existing entry. The audit log
    /// is append-only.
    #[error("audit entries are immutable; mutation rejected")]
    EntryImmutable,

    /// An audit-entry builder was missing required fields.
    #[error("audit entry builder missing field: {0}")]
    MissingField(&'static str),
}

/// Convenience result alias.
pub type Result<T, E = AuditError> = std::result::Result<T, E>;
