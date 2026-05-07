//! Error type for the tenant service.

use thiserror::Error;
use uuid::Uuid;

use crate::lifecycle::TenantStatus;

/// Errors raised by the tenant service.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TenantError {
    /// The supplied tenant id was not found in the registry.
    #[error("tenant not found: {0}")]
    NotFound(Uuid),

    /// A tenant lifecycle transition was rejected.
    #[error("invalid tenant lifecycle transition: {from:?} -> {to:?}")]
    InvalidLifecycleTransition {
        /// State the tenant was in.
        from: TenantStatus,
        /// State the caller asked to move to.
        to: TenantStatus,
    },

    /// A tenant config was rejected (e.g. zero retention budget).
    #[error("invalid tenant configuration: {0}")]
    InvalidConfig(String),

    /// A tenant id collision was detected on insert.
    #[error("tenant already exists: {0}")]
    AlreadyExists(Uuid),

    /// A user is already provisioned to this tenant.
    #[error("member already provisioned: {0}")]
    MemberAlreadyProvisioned(Uuid),

    /// A user is not provisioned to this tenant.
    #[error("member not provisioned: {0}")]
    MemberNotProvisioned(Uuid),
}

/// Convenience result alias.
pub type Result<T, E = TenantError> = std::result::Result<T, E>;
