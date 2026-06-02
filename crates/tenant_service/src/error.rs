//! Error type for the tenant service.

use thiserror::Error;
use uuid::Uuid;

use crate::lifecycle::TenantStatus;

/// Errors raised by the tenant service.
#[derive(Debug, Error)]
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

    /// The membership row exists but has already been removed; it
    /// is kept around as an audit artefact and must not be mutated.
    #[error("member already removed: {0}")]
    MemberAlreadyRemoved(Uuid),

    /// A persistence-layer invariant was violated.
    #[error("tenant-service persistence error: {0}")]
    Persistence(&'static str),

    /// The underlying SQLCipher driver surfaced an error.
    #[error("tenant-service sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The underlying [`crypto`] crate surfaced an error.
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),
}

impl PartialEq for TenantError {
    fn eq(&self, other: &Self) -> bool {
        // Each arm's body is `a == b` but `a`/`b` bind to different
        // types per variant (Uuid, String, lifecycle states, ...), so
        // the arms cannot be collapsed into a single `|`-pattern in
        // safe Rust. `match_same_arms` is silenced locally on that
        // basis.
        #[allow(clippy::match_same_arms)]
        match (self, other) {
            (Self::NotFound(a), Self::NotFound(b)) => a == b,
            (Self::InvalidLifecycleTransition { from: a1, to: a2 },
                Self::InvalidLifecycleTransition { from: b1, to: b2 },
            ) => a1 == b1 && a2 == b2,
            (Self::InvalidConfig(a), Self::InvalidConfig(b)) => a == b,
            (Self::AlreadyExists(a), Self::AlreadyExists(b)) => a == b,
            (Self::MemberAlreadyProvisioned(a), Self::MemberAlreadyProvisioned(b)) => a == b,
            (Self::MemberNotProvisioned(a), Self::MemberNotProvisioned(b)) => a == b,
            (Self::MemberAlreadyRemoved(a), Self::MemberAlreadyRemoved(b)) => a == b,
            (Self::Persistence(a), Self::Persistence(b)) => a == b,
            // SQLite + crypto errors carry opaque inner state — we
            // intentionally treat them as never structurally equal
            // so existing tests comparing hot-path variants keep
            // working without relying on driver-specific equality.
            _ => false,
        }
    }
}

impl Eq for TenantError {}

/// Convenience result alias.
pub type Result<T, E = TenantError> = std::result::Result<T, E>;
