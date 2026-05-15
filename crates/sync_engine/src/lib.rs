//! `sync_engine` — CRDT-based delta sync of synthesis objects.
//!
//! Per `docs/DESIGN.md` §3.2 and `docs/internal/PHASES.md` Phase 2: every replica
//! holds an [`AddWinsSet`] of synthesis-object ids per scope, plus an
//! append-only [`OpLog`] of [`SyncOp`] entries. Replicas exchange
//! their op logs out-of-band; [`merge_logs`] / [`OpLog::merge`]
//! produce a deterministic merged state regardless of arrival order.
//!
//! The high-level [`SyncEngine`] wires the two: `add` / `remove` /
//! `supersede` operations are recorded on the local op log and
//! replayed into the in-memory [`AddWinsSet`] by [`SyncEngine::state`].
//!
//! Cross-references:
//!
//! * `docs/DESIGN.md` §3.2 — CRDT delta protocol
//! * `ARCHITECTURE.md` §2.1 — sync engine module
//! * `docs/internal/PHASES.md` Phase 2 — concrete deliverables

#![deny(missing_docs)]

pub mod crdt;
pub mod error;
pub mod op_log;

use std::hash::Hash;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use crdt::AddWinsSet;
pub use error::{Result, SyncError};
pub use op_log::{merge_logs, OpLog, SyncOp, SyncOpKind};

/// Identifier for a sync scope (channel / domain / tenant memory
/// object).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncScopeId(
    /// Underlying UUID.
    pub Uuid,
);

/// Placeholder for a CRDT delta over synthesis objects. Phase 2
/// keeps the type opaque so the wire format can evolve without
/// breaking callers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrdtDelta {
    /// Opaque payload — e.g. a serialised `OpLog<Uuid>` slice.
    pub payload: Vec<u8>,
}

/// CRDT-based delta sync engine.
///
/// Phase 2: backed by a per-scope [`OpLog<T>`] and on-demand replay
/// into an [`AddWinsSet<T>`]. The element type `T` is parameterised
/// so callers can sync ids, evidence refs, or full synthesis-object
/// blobs.
#[derive(Debug)]
pub struct SyncEngine<T = Uuid>
where
    T: Eq + Hash + Clone,
{
    /// Replica id (UUID v4).
    replica_id: Uuid,
    log: OpLog<T>,
}

impl<T> SyncEngine<T>
where
    T: Eq + Hash + Clone,
{
    /// Construct a fresh sync engine instance with a new replica id.
    pub fn new() -> Self {
        let replica_id = Uuid::new_v4();
        Self {
            replica_id,
            log: OpLog::new(replica_id),
        }
    }

    /// Construct a sync engine bound to a specific replica id —
    /// useful in tests.
    pub fn with_replica_id(replica_id: Uuid) -> Self {
        Self {
            replica_id,
            log: OpLog::new(replica_id),
        }
    }

    /// Replica id.
    pub fn replica_id(&self) -> Uuid {
        self.replica_id
    }

    /// Borrow the underlying op log.
    pub fn op_log(&self) -> &OpLog<T> {
        &self.log
    }

    /// Mutably borrow the underlying op log.
    pub fn op_log_mut(&mut self) -> &mut OpLog<T> {
        &mut self.log
    }

    /// Record an `Add(value)` op.
    pub fn add(&mut self, value: T) -> Uuid {
        self.log.record_add(value)
    }

    /// Record a `Remove(value)` op observing all currently-visible
    /// tags for the value.
    pub fn remove(&mut self, value: T) {
        // Replay the log to find the observed tags so the remove is
        // an "observed" remove (necessary for add-wins).
        let Ok((set, _)) = self.log.replay() else {
            return;
        };
        let observed = set.tags_for(&value);
        self.log.record_remove(value, observed);
    }

    /// Record a `Supersede(value, successor)` op observing all
    /// currently-visible tags for the value.
    pub fn supersede(&mut self, value: T, successor: T) {
        let Ok((set, _)) = self.log.replay() else {
            return;
        };
        let observed = set.tags_for(&value);
        self.log.record_supersede(value, successor, observed);
    }

    /// Replay the op log and return the current materialised state.
    pub fn state(&self) -> Result<(AddWinsSet<T>, Vec<(T, T)>)> {
        self.log.replay()
    }

    /// Merge another engine's op log into this one. Idempotent.
    pub fn merge(&mut self, other: &Self) {
        self.log.merge(&other.log);
    }
}

impl<T> Default for SyncEngine<T>
where
    T: Eq + Hash + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}
