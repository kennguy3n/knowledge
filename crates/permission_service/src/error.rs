//! Error type for the permission service.

use thiserror::Error;

/// Errors raised by the permission service.
#[derive(Debug, Error, PartialEq, Eq)]
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
}

/// Convenience result alias.
pub type Result<T, E = PermissionError> = std::result::Result<T, E>;
