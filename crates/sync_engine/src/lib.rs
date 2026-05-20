//! `sync_engine` — CRDT-based delta sync of synthesis objects.
//!
//! Per `docs/DESIGN.md` §3.2: every replica
//! holds an [`AddWinsSet`] of synthesis-object ids per scope, plus an
//! append-only [`OpLog`] of [`SyncOp`] entries. Replicas exchange
//! their op logs out-of-band; [`merge_logs`] / [`OpLog::merge`]
//! produce a deterministic merged state regardless of arrival order.
//!
//! The high-level [`SyncEngine`] wires the two: `add` / `remove` /
//! `supersede` operations are recorded on the local op log and
//! reflected into a **cached** in-memory [`AddWinsSet`]. The cache
//! is updated incrementally on every mutation so [`SyncEngine::state`]
//! is `O(live elements)` rather than `O(total ops in history)`. When
//! the op log is mutated through the raw [`SyncEngine::op_log_mut`]
//! borrow, the cache is invalidated and rebuilt lazily on the next
//! [`SyncEngine::state`] call.
//!
//! Additional facilities layered on top:
//!
//! * Compaction — [`SyncEngine::compact`] rewrites the local op
//!   log into a minimal `Add`-only form, bumping a
//!   [`OpLog::compaction_epoch`] counter that peers exchange so
//!   they can detect they need a snapshot bootstrap.
//! * Delta serialisation — [`crate::delta`] encodes / decodes /
//!   applies the post-`since_seq` ops as a wire-format byte blob
//!   (with the compaction epoch as a guard).
//! * Snapshot checkpointing — [`SyncEngine::snapshot`] /
//!   [`SyncEngine::restore_snapshot`] serialise the materialised
//!   set directly, bypassing log replay, so a new replica can
//!   bootstrap without the full op history.
//! * Persistence — [`crate::persist::PersistentSyncEngine`] mirrors
//!   the op log to a SQLCipher database (per-scope AEAD on the
//!   payload column, following the `concept_graph` pattern).
//!
//! Cross-references:
//!
//! * `docs/DESIGN.md` §3.2 — CRDT delta protocol
//! * `ARCHITECTURE.md` §2.1 — sync engine module
//! * `docs/DESIGN.md` §3.2 — CRDT delta protocol (deliverables)

#![deny(missing_docs)]

pub mod crdt;
pub mod delta;
pub mod error;
pub mod op_log;
pub mod persist;

use std::cell::RefCell;
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

impl SyncScopeId {
    /// Generate a fresh random scope id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct a scope id from a raw UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

/// Placeholder for a CRDT delta over synthesis objects. The type
/// is kept opaque so the wire format can evolve without
/// breaking callers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrdtDelta {
    /// Opaque payload — a serialised [`crate::delta::DeltaEnvelope`].
    pub payload: Vec<u8>,
}

/// Snapshot of a materialised [`SyncEngine`] state, suitable for
/// bootstrapping a fresh replica without transferring the *historic*
/// op stream.
///
/// A snapshot carries three things:
///
/// * `log` — the **authoring engine's current `OpLog`**, including
///   `replica_id`, `clock`, and `compaction_epoch`. The receiver
///   adopts this log verbatim, so the existing `(replica_id, seq)`
///   dedup invariant is preserved: any subsequent delta the
///   authoring peer sends will dedupe against the receiver's log
///   instead of duplicating ops with new seqs. Callers that want
///   the snapshot to be *small* should [`SyncEngine::compact`]
///   first; the on-wire size is then `~ live elements + supersession
///   history`.
/// * `set` / `supersessions` — the materialised state of the
///   authoring engine. The receiver hydrates its
///   [`SyncEngine`]-level cache directly from these without needing
///   to re-replay `log` on restore, so the first
///   [`SyncEngine::state`] call after restore is O(1).
///
/// The doubled storage (live state ≈ live Adds in the log) is
/// intentional: it lets the receiver simultaneously satisfy the
/// dedup invariant *and* the bootstrap-without-replay invariant
/// promised in `docs/DESIGN.md` §3.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineSnapshot<T>
where
    T: Eq + Hash + Clone,
{
    /// Replica id of the authoring engine. Mirrors `log.replica_id`
    /// (kept for fast inspection without deserialising the log).
    pub replica_id: Uuid,
    /// The authoring engine's op log at the time of snapshot.
    pub log: OpLog<T>,
    /// Materialised set. Hydrates the receiver's cache directly.
    pub set: AddWinsSet<T>,
    /// History of supersession events (predecessor → successor).
    pub supersessions: Vec<(T, T)>,
}

/// CRDT-based delta sync engine.
///
/// Backed by a per-scope [`OpLog<T>`] and an incrementally-maintained
/// [`AddWinsSet<T>`] cache. The element type `T` is parameterised so
/// callers can sync ids, evidence refs, or full synthesis-object
/// blobs.
pub struct SyncEngine<T = Uuid>
where
    T: Eq + Hash + Clone,
{
    /// Replica id (UUID v4).
    replica_id: Uuid,
    /// Append-only op log of all local + merged ops.
    log: OpLog<T>,
    /// Materialised state cache. `RefCell` so [`Self::state`] can
    /// lazily rebuild it without taking `&mut self` — invalidated by
    /// mutations that the engine cannot incrementally track (e.g.
    /// raw [`Self::op_log_mut`] access or [`Self::compact`]).
    cached_state: RefCell<Option<(AddWinsSet<T>, Vec<(T, T)>)>>,
    /// Number of ops covered by the current cache. When the cache
    /// is `Some` and `cache_watermark == log.ops.len()` the cache
    /// is up to date; otherwise the cache must be rebuilt or
    /// incrementally extended before serving [`Self::state`].
    cache_watermark: std::cell::Cell<usize>,
}

impl<T> std::fmt::Debug for SyncEngine<T>
where
    T: Eq + Hash + Clone + std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncEngine")
            .field("replica_id", &self.replica_id)
            .field("log_len", &self.log.ops.len())
            .field("compaction_epoch", &self.log.compaction_epoch)
            .field("cache_watermark", &self.cache_watermark.get())
            .finish_non_exhaustive()
    }
}

impl<T> SyncEngine<T>
where
    T: Eq + Hash + Clone,
{
    /// Construct a fresh sync engine instance with a new replica id.
    pub fn new() -> Self {
        let replica_id = Uuid::new_v4();
        Self::from_log(replica_id, OpLog::new(replica_id))
    }

    /// Construct a sync engine bound to a specific replica id —
    /// useful in tests.
    pub fn with_replica_id(replica_id: Uuid) -> Self {
        Self::from_log(replica_id, OpLog::new(replica_id))
    }

    /// Construct an engine from an existing op log. Used by the
    /// persistence layer to rehydrate after a restart.
    pub fn from_log(replica_id: Uuid, log: OpLog<T>) -> Self {
        Self {
            replica_id,
            log,
            cached_state: RefCell::new(None),
            cache_watermark: std::cell::Cell::new(0),
        }
    }

    /// Replica id.
    pub fn replica_id(&self) -> Uuid {
        self.replica_id
    }

    /// Current compaction-generation counter — bumps every time
    /// [`Self::compact`] is called, or whenever a merged peer log
    /// pulls in a higher epoch via [`OpLog::merge`].
    pub fn compaction_epoch(&self) -> u64 {
        self.log.compaction_epoch
    }

    /// Borrow the underlying op log.
    pub fn op_log(&self) -> &OpLog<T> {
        &self.log
    }

    /// Mutably borrow the underlying op log.
    ///
    /// **Invalidates the materialised-state cache** because the
    /// caller may append, mutate, or reorder ops the engine cannot
    /// observe. The next call to [`Self::state`] rebuilds the cache
    /// from scratch.
    pub fn op_log_mut(&mut self) -> &mut OpLog<T> {
        self.invalidate_cache();
        &mut self.log
    }

    fn invalidate_cache(&mut self) {
        *self.cached_state.get_mut() = None;
        self.cache_watermark.set(0);
    }

    /// Record an `Add(value)` op and reflect it in the cache.
    pub fn add(&mut self, value: T) -> Uuid {
        let tag = self.log.record_add(value.clone());
        if let Some((set, _supers)) = self.cached_state.get_mut().as_mut() {
            set.add_with_tag(value, tag);
            self.cache_watermark.set(self.log.ops.len());
        }
        tag
    }

    /// Record a `Remove(value)` op observing all currently-visible
    /// tags for the value, and reflect the tombstones in the cache.
    pub fn remove(&mut self, value: T) {
        // Read observed tags from the cached state (rebuilding the
        // cache if necessary) so we do not pay the O(log_len) cost
        // of a fresh replay on every remove.
        let observed: Vec<Uuid> = match self.materialise() {
            Ok(()) => {
                let cache = self.cached_state.borrow();
                cache
                    .as_ref()
                    .map(|(set, _)| {
                        set.tags_for(&value)
                            .into_iter()
                            .filter(|tag| !set.tombstones().contains(tag))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            Err(_) => return,
        };
        self.log.record_remove(value.clone(), observed.clone());
        if let Some((set, _supers)) = self.cached_state.get_mut().as_mut() {
            set.remove_tags(&value, &observed);
            self.cache_watermark.set(self.log.ops.len());
        }
    }

    /// Record a `Supersede(value, successor)` op observing all
    /// currently-visible tags for the value, and reflect the
    /// tombstones / supersession pair in the cache.
    pub fn supersede(&mut self, value: T, successor: T) {
        let observed: Vec<Uuid> = match self.materialise() {
            Ok(()) => {
                let cache = self.cached_state.borrow();
                cache
                    .as_ref()
                    .map(|(set, _)| {
                        set.tags_for(&value)
                            .into_iter()
                            .filter(|tag| !set.tombstones().contains(tag))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            Err(_) => return,
        };
        self.log
            .record_supersede(value.clone(), successor.clone(), observed.clone());
        if let Some((set, supers)) = self.cached_state.get_mut().as_mut() {
            set.remove_tags(&value, &observed);
            supers.push((value, successor));
            self.cache_watermark.set(self.log.ops.len());
        }
    }

    /// Replay the op log and return the current materialised state.
    ///
    /// O(1) when the cache is up to date; O(new ops since the last
    /// `state()` call) otherwise. After this call returns
    /// successfully the cache is guaranteed to be up to date.
    pub fn state(&self) -> Result<(AddWinsSet<T>, Vec<(T, T)>)> {
        self.materialise()?;
        let cache = self.cached_state.borrow();
        let (set, supers) = cache
            .as_ref()
            .expect("materialise() populates the cache on success");
        Ok((set.clone(), supers.clone()))
    }

    /// Ensure `self.cached_state` reflects the entire current op log.
    ///
    /// * If the cache is `Some` and the watermark is current, it's
    ///   a no-op.
    /// * If the cache is `Some` but stale, ops with index
    ///   `>= watermark` are applied incrementally on top.
    /// * If the cache is `None`, the full log is replayed.
    fn materialise(&self) -> Result<()> {
        let len = self.log.ops.len();
        let mut slot = self.cached_state.borrow_mut();
        let watermark = self.cache_watermark.get();
        match slot.as_mut() {
            Some((set, supers)) if watermark <= len => {
                if watermark == len {
                    return Ok(());
                }
                for entry in &self.log.ops[watermark..] {
                    apply_op_to(set, supers, entry);
                }
                self.cache_watermark.set(len);
                Ok(())
            }
            _ => {
                let (set, supers) = self.log.replay()?;
                *slot = Some((set, supers));
                self.cache_watermark.set(len);
                Ok(())
            }
        }
    }

    /// Merge another engine's op log into this one. Idempotent.
    /// The cache is incrementally extended with the new ops so
    /// [`Self::state`] remains O(new-ops) after the merge.
    pub fn merge(&mut self, other: &Self) {
        let before = self.log.ops.len();
        self.log.merge(&other.log);
        let after = self.log.ops.len();
        if let Some((set, supers)) = self.cached_state.get_mut().as_mut() {
            for entry in &self.log.ops[before..after] {
                apply_op_to(set, supers, entry);
            }
            self.cache_watermark.set(after);
        }
    }

    /// Absorb every op in a [`crate::delta::DeltaEnvelope`] into the
    /// local op log, deduping by `(replica_id, seq)`, and
    /// incrementally extend the cache with the newly-absorbed ops.
    /// Returns the number of ops newly absorbed.
    ///
    /// Bumps `compaction_epoch` to the max of (`self`, envelope).
    ///
    /// This is the cache-aware counterpart to
    /// [`OpLog::merge_single`]: callers driving sync through the
    /// delta wire protocol should reach for [`crate::delta::apply_delta`]
    /// (which validates the epoch first), and `apply_delta` reaches
    /// for this method to avoid invalidating the engine's
    /// materialised cache.
    pub fn merge_delta_envelope(&mut self, envelope: crate::delta::DeltaEnvelope<T>) -> usize {
        let before = self.log.ops.len();
        for op in envelope.ops {
            self.log.merge_single(op);
        }
        let after = self.log.ops.len();
        self.log.compaction_epoch = self.log.compaction_epoch.max(envelope.compaction_epoch);
        if let Some((set, supers)) = self.cached_state.get_mut().as_mut() {
            for entry in &self.log.ops[before..after] {
                apply_op_to(set, supers, entry);
            }
            self.cache_watermark.set(after);
        }
        after - before
    }

    /// Compact the underlying op log, dropping every historical
    /// `Remove` / `Supersede` op while preserving the materialised
    /// set. Bumps the [`OpLog::compaction_epoch`] so peers know they
    /// must bootstrap via [`Self::snapshot`] if they are behind.
    ///
    /// Returns the number of ops removed.
    pub fn compact(&mut self) -> Result<usize> {
        // Pre-compaction: capture the current materialised supersessions so
        // we can preserve them across compaction. `compact()` itself only
        // re-emits `Add` ops (the live tag carriers) and intentionally
        // drops every `Remove` / `Supersede` entry — but the supersession
        // *history* is part of the visible state, and callers of
        // `state()` rely on it surfacing after compaction too.
        let (_set, supers) = self.log.replay()?;
        let removed = self.log.compact()?;

        // After `compact()` the log holds only `Add` ops, so a fresh
        // replay reproduces the same materialised set with `supers`
        // empty. Re-emit the historical supersessions as `Supersede`
        // ops with **empty** `observed_tags` so they end up in the
        // `supersessions` Vec on replay without tombstoning anything
        // (the live tags are already in `Add` ops we just emitted).
        for (pred, succ) in supers {
            self.log.record_supersede(pred, succ, Vec::new());
        }

        // Rebuild the cache from the rewritten log.
        self.invalidate_cache();
        self.materialise()?;
        Ok(removed)
    }

    /// Serialise the current op log and materialised state into a
    /// portable JSON snapshot blob, suitable for bootstrapping a
    /// fresh replica.
    ///
    /// Callers that want the snapshot to be *small* should
    /// [`Self::compact`] before calling `snapshot` — the
    /// compacted log is `O(live elements)` rather than `O(history)`.
    ///
    /// Preserving the full op log (rather than just the
    /// materialised set) is what keeps subsequent delta sync from
    /// the authoring peer correct: the receiver's
    /// `(replica_id, seq)` dedup table covers the same ids the
    /// sender's deltas will reference, so deltas dedupe rather than
    /// re-introduce equivalent ops with fresh seqs.
    pub fn snapshot(&self) -> Result<Vec<u8>>
    where
        T: Serialize,
    {
        self.materialise()?;
        let cache = self.cached_state.borrow();
        let (set, supersessions) = cache
            .as_ref()
            .expect("materialise() populates the cache on success");
        let snap = EngineSnapshot {
            replica_id: self.replica_id,
            log: self.log.clone(),
            set: set.clone(),
            supersessions: supersessions.clone(),
        };
        serde_json::to_vec(&snap)
            .map_err(|_| SyncError::Serialisation("could not serialise engine snapshot"))
    }

    /// Reconstruct a [`SyncEngine`] from a snapshot produced by
    /// [`Self::snapshot`].
    ///
    /// The restored engine adopts the snapshot's op log verbatim
    /// (so `(replica_id, seq)` dedup against subsequent peer
    /// deltas works) and hydrates its materialised-state cache
    /// directly from the snapshot's `set` + `supersessions` (so the
    /// first [`Self::state`] call is O(1) and does not need to
    /// re-replay the log).
    pub fn restore_snapshot(bytes: &[u8]) -> Result<Self>
    where
        T: for<'de> Deserialize<'de> + Serialize,
    {
        let snap: EngineSnapshot<T> = serde_json::from_slice(bytes)
            .map_err(|_| SyncError::Serialisation("could not deserialise engine snapshot"))?;

        if snap.log.replica_id != snap.replica_id {
            return Err(SyncError::Serialisation(
                "snapshot replica_id does not match log.replica_id",
            ));
        }

        let engine = Self::from_log(snap.replica_id, snap.log);
        *engine.cached_state.borrow_mut() = Some((snap.set, snap.supersessions));
        engine.cache_watermark.set(engine.log.ops.len());
        Ok(engine)
    }
}

/// Apply a single op into a working `(set, supersessions)` pair —
/// the same logic as [`OpLog::replay`]'s match arm, factored out so
/// [`SyncEngine`] can incrementally extend the cache without
/// re-traversing the entire log.
fn apply_op_to<T>(set: &mut AddWinsSet<T>, supers: &mut Vec<(T, T)>, entry: &SyncOp<T>)
where
    T: Eq + Hash + Clone,
{
    match &entry.op {
        SyncOpKind::Add { value, tag } => set.add_with_tag(value.clone(), *tag),
        SyncOpKind::Remove {
            value,
            observed_tags,
        } => set.remove_tags(value, observed_tags),
        SyncOpKind::Supersede {
            value,
            successor,
            observed_tags,
        } => {
            set.remove_tags(value, observed_tags);
            supers.push((value.clone(), successor.clone()));
        }
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
