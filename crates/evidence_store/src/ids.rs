//! Newtype wrappers for the IDs used across the evidence plane.
//!
//! Using newtypes (rather than raw `Uuid`) enforces at the type level
//! that an `EvidenceId` cannot accidentally be passed where a
//! `ScopeId` is expected.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifier for a single evidence row (UUID v4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceId(pub Uuid);

impl EvidenceId {
    /// Generate a fresh random `EvidenceId`.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Return the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for EvidenceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifier for a scope (channel / domain / tenant memory object).
///
/// Per `docs/technical/design.md` §3.1 every storage path is keyed by a
/// `(scope_id, epoch)` pair; the scope id is the unit of
/// cryptographic forgetting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScopeId(pub Uuid);

impl ScopeId {
    /// Generate a fresh random `ScopeId`.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct a scope id from a raw [`Uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Return the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for ScopeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
