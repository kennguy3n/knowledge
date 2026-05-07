//! `knowledge_sync_engine` — CRDT-based delta sync of synthesis objects.
//!
//! This crate is a minimal stub for Phase 0. Real CRDT operation logs,
//! MLS group keying, and selective evidence-ref sync are scheduled for
//! Phase 2 (see `PHASES.md`). The public surface here exists so the
//! workspace compiles and downstream callers can already type their
//! integration against the real names.

#![deny(missing_docs)]

use thiserror::Error;
use uuid::Uuid;

/// Identifier for a sync scope (channel / domain / tenant memory object).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyncScopeId(pub Uuid);

/// Errors surfaced by the sync engine stub.
#[derive(Debug, Error)]
pub enum SyncError {
    /// The sync engine is not yet implemented for this code path.
    #[error("sync engine stub: {0}")]
    NotYetImplemented(&'static str),
}

/// Placeholder for a CRDT delta over synthesis objects.
#[derive(Debug, Clone, Default)]
pub struct CrdtDelta {
    /// Opaque payload — the real shape lands in Phase 2.
    pub payload: Vec<u8>,
}

/// CRDT-based delta sync engine.
///
/// Phase 0: this is a deliberate stub. Methods return
/// [`SyncError::NotYetImplemented`] so callers can wire the integration
/// surface without blocking on the full Phase 2 implementation.
#[derive(Debug, Default)]
pub struct SyncEngine;

impl SyncEngine {
    /// Construct a fresh sync engine instance.
    pub fn new() -> Self {
        Self
    }

    /// Apply an inbound CRDT delta to a sync scope. Stubbed.
    pub fn apply_delta(&self, _scope: SyncScopeId, _delta: &CrdtDelta) -> Result<(), SyncError> {
        Err(SyncError::NotYetImplemented("apply_delta"))
    }

    /// Produce the next outbound CRDT delta for a sync scope. Stubbed.
    pub fn next_delta(&self, _scope: SyncScopeId) -> Result<CrdtDelta, SyncError> {
        Err(SyncError::NotYetImplemented("next_delta"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_methods_return_not_yet_implemented() {
        let engine = SyncEngine::new();
        let scope = SyncScopeId(Uuid::nil());
        assert!(engine.apply_delta(scope, &CrdtDelta::default()).is_err());
        assert!(engine.next_delta(scope).is_err());
    }
}
