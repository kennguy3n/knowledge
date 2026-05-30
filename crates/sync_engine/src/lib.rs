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

/// Default auto-compaction threshold for [`SyncEngine`]. Devices
/// running steady-state CRDT workloads accumulate tombstones at the
/// rate of one per `remove`/`supersede`; 10K covers typical day-long
/// activity for a power user before compaction kicks in.
pub const DEFAULT_COMPACT_THRESHOLD: usize = 10_000;

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
#[serde(bound(
    serialize = "T: Serialize + Eq + Hash + Clone",
    deserialize = "T: serde::de::DeserializeOwned + Eq + Hash + Clone"
))]
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
    /// Auto-compaction threshold. When the op-log has grown by
    /// more than this many entries *since the last successful
    /// compaction*, the engine runs [`Self::compact`] in-place to
    /// keep tombstones bounded. `None` disables auto-compaction
    /// (callers must invoke [`Self::compact`] explicitly); the
    /// default is `Some(10_000)`.
    ///
    /// The check is against `log.ops.len() - compact_baseline`,
    /// not against `log.ops.len()` directly — that matters because
    /// `compact()` only removes historical `Remove`/superseded
    /// `Add` ops. A log composed entirely of live `Add`s shrinks
    /// by zero on compaction, so a naive `log.ops.len() >
    /// threshold` check would re-fire on every subsequent mutation
    /// once the set's *live* size exceeded the threshold, turning
    /// each mutation into an O(n) replay. Tracking the post-
    /// compaction baseline gives amortised O(1) per mutation
    /// regardless of live-element count: compaction only re-fires
    /// after `threshold` more *compactable* ops have accumulated
    /// since the previous pass.
    compact_threshold: Option<usize>,
    /// Op-log length immediately after the most recent successful
    /// compaction (or `0` on a fresh / never-compacted engine).
    /// Used as the watermark for the auto-compaction trigger; see
    /// [`Self::compact_threshold`] for the rationale.
    compact_baseline: usize,
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
            compact_threshold: Some(DEFAULT_COMPACT_THRESHOLD),
            compact_baseline: 0,
        }
    }

    /// Configure the auto-compaction threshold. `Some(n)` triggers
    /// [`Self::compact`] automatically whenever the op-log has
    /// grown by more than `n` entries since the previous successful
    /// compaction (so the trigger amortises to O(1) per mutation
    /// regardless of live-element count); `None` disables
    /// auto-compaction. Default: `Some(10_000)`.
    pub fn with_compact_threshold(mut self, threshold: Option<usize>) -> Self {
        self.compact_threshold = threshold;
        self
    }

    /// Currently-configured auto-compaction threshold (see
    /// [`Self::with_compact_threshold`]).
    pub fn compact_threshold(&self) -> Option<usize> {
        self.compact_threshold
    }

    /// Internal hook: run a compaction pass if the configured
    /// threshold has been exceeded *since the last successful
    /// compaction* (i.e. `log.ops.len() - compact_baseline >
    /// threshold`). Errors from `compact` are swallowed —
    /// compaction is best-effort housekeeping, never a correctness
    /// requirement, and callers of the public mutators have
    /// already received their `Ok(())` by the time the threshold
    /// check fires. (The next explicit [`Self::compact`] call
    /// still surfaces the underlying `SyncError`.)
    ///
    /// # Amortised cost
    ///
    /// The watermark comparison (`log.ops.len() -
    /// compact_baseline > threshold`) is what keeps auto-compaction
    /// from degenerating into per-mutation O(n) work. `compact()`
    /// only removes historical `Remove` / superseded `Add` ops, so
    /// a workload that is mostly live `Add`s shrinks the log by
    /// near-zero on each pass — a naive `log.ops.len() > threshold`
    /// trigger would therefore re-fire on every subsequent mutation
    /// once the live-element count crossed the threshold, replaying
    /// the whole log each time. By only re-firing after `threshold`
    /// *additional* ops have accumulated since the previous
    /// successful compaction, the amortised cost stays O(1) per
    /// mutation regardless of live-element count.
    ///
    /// # Observability
    ///
    /// Auto-compaction failures emit a [`tracing::warn`] event
    /// under the `sync_engine::auto_compact` target with the
    /// `error` field set to the underlying `SyncError`. Operators
    /// monitoring the substrate via `tracing-subscriber` /
    /// Datadog / Honeycomb / etc. see one warn per failed pass
    /// (so persistent failures show up as a rising rate, not as
    /// silence). The op-log and `compact_baseline` remain
    /// untouched on failure (the `OpLog::compact` contract
    /// preserves the original log when `materialise` errors), so
    /// the *next* mutation still trips the threshold and emits
    /// another warn — repeated failures are inherently visible,
    /// not silent.
    fn maybe_auto_compact(&mut self) {
        if let Some(threshold) = self.compact_threshold {
            // Growth-since-baseline comparison, expressed via
            // `saturating_sub` so a transient inversion (e.g. a
            // restored snapshot whose log is shorter than the
            // pre-restore baseline) cannot underflow.
            let growth = self.log.ops.len().saturating_sub(self.compact_baseline);
            if growth > threshold {
                if let Err(err) = self.compact() {
                    // Auto-compaction is best-effort housekeeping;
                    // the mutator's `Ok(())` has already been
                    // returned by the time we get here, so we
                    // cannot propagate the failure. We surface it
                    // via tracing so operator dashboards (logs ->
                    // metric pipelines) can alert on a rising
                    // failure rate rather than discover the
                    // unbounded op-log growth at the next OOM.
                    tracing::warn!(
                        target: "sync_engine::auto_compact",
                        op_log_len = self.log.ops.len(),
                        threshold = threshold,
                        error = %err,
                        "auto-compaction pass failed; op-log will keep growing until the next successful compact() call"
                    );
                }
            }
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
        self.maybe_auto_compact();
        tag
    }

    /// Record a `Remove(value)` op observing all currently-visible
    /// tags for the value, and reflect the tombstones in the cache.
    ///
    /// **Short-circuit:** if there are no currently-visible tags for
    /// `value` (i.e. the local replica has never seen `value`, or all
    /// of its known tags are already tombstoned), this is a no-op:
    /// no op is appended to the log and no on-disk row is written.
    /// A `Remove { observed_tags: [] }` op is a no-op on every
    /// receiver — `AddWinsSet::remove_tags(value, &[])` does nothing
    /// — so recording one would only grow the log + persisted table
    /// without carrying any information. Defensive `remove()` of an
    /// unknown value is the canonical case this guards against.
    // `value: T` is by-value rather than `&T` to keep `remove(v)` symmetric
    // with the sibling `add(v)` / `supersede(v, s)` methods (both of which
    // genuinely consume `value` — `add` hands it to `add_with_tag` and
    // `supersede` pushes it into the cached `supers` vec). Removing it is
    // the only one that just clones, but flipping this single method to
    // `&T` would force every test/caller of `sync_engine` to switch
    // between `.remove(v)` and `.add(v)` styles for no observable gain.
    #[allow(clippy::needless_pass_by_value)]
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
        if observed.is_empty() {
            return;
        }
        self.log.record_remove(value.clone(), observed.clone());
        if let Some((set, _supers)) = self.cached_state.get_mut().as_mut() {
            set.remove_tags(&value, &observed);
            self.cache_watermark.set(self.log.ops.len());
        }
        self.maybe_auto_compact();
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
        self.maybe_auto_compact();
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
    /// * If the cache is `Some` but stale (watermark `<` len), ops
    ///   with index `>= watermark` are applied incrementally on top.
    /// * If the cache is `None`, the full log is replayed.
    ///
    /// The invariant `cache_watermark <= log.ops.len()` is enforced
    /// on every mutation path the engine controls and on the only
    /// raw-mutation entry point ([`Self::op_log_mut`], which calls
    /// [`Self::invalidate_cache`] before returning). A debug-build
    /// assertion fires if a caller manages to violate this anyway;
    /// release builds gracefully fall back to a full replay so a
    /// post-truncation cache rebuild is still correct, just
    /// slower.
    fn materialise(&self) -> Result<()> {
        let len = self.log.ops.len();
        let mut slot = self.cached_state.borrow_mut();
        let watermark = self.cache_watermark.get();
        debug_assert!(
            slot.is_none() || watermark <= len,
            "sync_engine cache invariant violated: watermark={watermark} > log.ops.len()={len}; \
             a caller mutated the op log without going through op_log_mut() / invalidate_cache()"
        );
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
                // Either the cache was None, or the watermark
                // exceeded `len` (invariant violation, only
                // reachable in release builds). Either way the safe
                // recovery is a full replay.
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
        self.maybe_auto_compact();
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
        let absorbed = after - before;
        self.maybe_auto_compact();
        absorbed
    }

    /// Compact the underlying op log, dropping every historical
    /// `Remove` op while preserving the materialised set **and**
    /// the supersession history. Bumps the
    /// [`OpLog::compaction_epoch`] so peers know they must
    /// bootstrap via [`Self::snapshot`] if they are behind.
    ///
    /// Returns the number of ops removed.
    pub fn compact(&mut self) -> Result<usize> {
        // [`OpLog::compact`] is responsible for preserving both the
        // live `Add` tags and the supersession history in the
        // rewritten log — so a fresh replay after compaction
        // reproduces the same `(set, supersessions)` pair. The
        // engine only needs to invalidate and rehydrate its own
        // materialised-state cache afterwards.
        let removed = self.log.compact()?;
        self.invalidate_cache();
        self.materialise()?;
        // Re-arm the auto-compaction trigger relative to the
        // post-compaction log length — see
        // [`Self::maybe_auto_compact`] for why this matters.
        self.compact_baseline = self.log.ops.len();
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
    /// [`Self::snapshot`] **for the same replica that authored
    /// it** — i.e. the "resume after restart" case, not the
    /// "new peer joining the cluster" case.
    ///
    /// The restored engine adopts the snapshot's op log and
    /// `replica_id` verbatim, so any subsequent local op it
    /// authors continues the same replica's `(replica_id, seq)`
    /// stream. The materialised-state cache is hydrated directly
    /// from the snapshot's `set` + `supersessions` (so the first
    /// [`Self::state`] call is O(1)).
    ///
    /// **Do not** use this to bootstrap a brand-new peer from
    /// another replica's snapshot — you would silently inherit
    /// the author's `replica_id` and start attributing your local
    /// writes to them, corrupting the dedup table on every other
    /// peer in the cluster. Use [`Self::bootstrap_from_snapshot`]
    /// for that case.
    ///
    /// **Note on `compact_threshold`**: the auto-compaction trigger
    /// is **runtime configuration**, not persistent engine state —
    /// it is *not* serialised into [`EngineSnapshot`]. The restored
    /// engine therefore starts with the default threshold
    /// ([`DEFAULT_COMPACT_THRESHOLD`]); callers that rely on a
    /// non-default value (e.g. set via
    /// [`Self::with_compact_threshold`]) must re-apply it after
    /// `restore_snapshot` returns. This is by design: the threshold
    /// is a tuning knob, not part of the engine's CRDT semantics,
    /// and operators may legitimately want to change it between
    /// runs.
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

    /// Bootstrap a fresh peer from another replica's snapshot.
    ///
    /// Unlike [`Self::restore_snapshot`], the receiver keeps its
    /// **own** freshly-generated `replica_id` (or one supplied via
    /// [`Self::bootstrap_from_snapshot_with_replica_id`]); the
    /// snapshot's log is merged in op-by-op with every entry's
    /// original authoring `replica_id` preserved, so subsequent
    /// delta sync with the original author still dedupes correctly
    /// via `(replica_id, seq)`.
    ///
    /// The receiver's local `clock` starts at 0 — future local
    /// ops are authored under the new `replica_id` with a fresh
    /// seq stream, so they cannot collide with the snapshot author's
    /// stream. The compaction epoch is inherited from the snapshot
    /// so the receiver does not accept stale deltas authored
    /// before the author's last compaction.
    ///
    /// Same `compact_threshold` caveat as
    /// [`Self::restore_snapshot`]: the bootstrapped engine starts
    /// with [`DEFAULT_COMPACT_THRESHOLD`] regardless of what the
    /// snapshot author had configured. Callers that need a
    /// non-default threshold must re-apply it after this returns.
    pub fn bootstrap_from_snapshot(bytes: &[u8]) -> Result<Self>
    where
        T: for<'de> Deserialize<'de> + Serialize,
    {
        Self::bootstrap_from_snapshot_with_replica_id(bytes, Uuid::new_v4())
    }

    /// Like [`Self::bootstrap_from_snapshot`] but lets the caller
    /// pin the receiver's `replica_id` — used by the persistence
    /// layer to keep the on-disk replica identity stable across
    /// process restarts even when bootstrapping from a snapshot.
    pub fn bootstrap_from_snapshot_with_replica_id(
        bytes: &[u8],
        new_replica_id: Uuid,
    ) -> Result<Self>
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

        // Build a fresh op log under the receiver's own replica_id.
        // Then absorb every snapshot op via `merge_single`, which
        // preserves each op's original authoring `replica_id` and
        // `seq` so the receiver's `(replica_id, seq)` dedup table
        // covers exactly the snapshot's ops — subsequent deltas
        // from the original author dedupe correctly.
        let mut local_log: OpLog<T> = OpLog::new(new_replica_id);
        for op in snap.log.ops {
            local_log.merge_single(op);
        }
        local_log.compaction_epoch = snap.log.compaction_epoch;

        let engine = Self::from_log(new_replica_id, local_log);
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

#[cfg(test)]
mod auto_compact_tests {
    //! Auto-compaction is engaged through `add`/`remove`/`supersede`/
    //! `merge`/`merge_delta_envelope`. These tests exercise the real
    //! mutators and verify that:
    //!   1. The op-log is compacted (length drops below the
    //!      threshold) once the configured threshold is exceeded.
    //!   2. The materialised set is preserved across the compaction
    //!      (which is the invariant `compact` already guarantees).
    //!   3. `with_compact_threshold(None)` disables the trigger.

    use super::*;

    #[test]
    fn auto_compact_fires_once_threshold_exceeded_for_remove_heavy_workload() {
        let mut engine: SyncEngine<u64> = SyncEngine::new().with_compact_threshold(Some(8));

        // Add + remove repeatedly. Each remove appends a Remove op
        // (with observed tags) and a fresh Add op had already been
        // appended for the value. After ~10 iterations the op-log
        // length is well above 8 and the threshold trigger must
        // fire on the next mutation, dropping the historical
        // Removes.
        for v in 0..16u64 {
            engine.add(v);
            engine.remove(v);
        }

        // Compaction shrinks the log. The set should be empty
        // (everything was removed) and the log should be shorter
        // than the unmodified path's length (32 ops without
        // compaction).
        let (set, _supers) = engine.state().unwrap();
        assert_eq!(
            set.elements_count(),
            0,
            "all values were removed; set must be empty"
        );
        assert!(
            engine.op_log().ops.len() < 32,
            "auto-compaction must drop superseded Add/Remove pairs; got log_len={}",
            engine.op_log().ops.len()
        );
        assert!(
            engine.compaction_epoch() >= 1,
            "compaction must have run, bumping the epoch"
        );
    }

    #[test]
    fn auto_compact_threshold_none_disables_trigger() {
        let mut engine: SyncEngine<u64> = SyncEngine::new().with_compact_threshold(None);
        let epoch_before = engine.compaction_epoch();

        for v in 0..100u64 {
            engine.add(v);
            engine.remove(v);
        }

        assert_eq!(
            engine.compaction_epoch(),
            epoch_before,
            "compaction_epoch must not bump when auto-compaction is disabled"
        );
        // 100 adds + 100 removes; the log keeps every op verbatim.
        assert_eq!(engine.op_log().ops.len(), 200);
    }

    #[test]
    fn auto_compact_fires_on_merge_when_combined_log_exceeds_threshold() {
        let mut a: SyncEngine<u64> = SyncEngine::new().with_compact_threshold(Some(5));
        let mut b: SyncEngine<u64> = SyncEngine::new().with_compact_threshold(None);
        for v in 0..4u64 {
            b.add(v);
            b.remove(v);
        }
        // a is empty, threshold 5; merging in b's 8 ops should
        // overshoot and trigger.
        a.merge(&b);
        assert!(
            a.op_log().ops.len() <= 5,
            "post-merge auto-compaction must drop tombstones; got log_len={}",
            a.op_log().ops.len()
        );
    }

    #[test]
    fn with_compact_threshold_round_trips_the_value() {
        let engine: SyncEngine<u64> = SyncEngine::new();
        assert_eq!(engine.compact_threshold(), Some(DEFAULT_COMPACT_THRESHOLD));
        let engine = engine.with_compact_threshold(Some(42));
        assert_eq!(engine.compact_threshold(), Some(42));
        let engine = engine.with_compact_threshold(None);
        assert_eq!(engine.compact_threshold(), None);
    }

    /// Regression test for the all-live-Adds case. Compaction
    /// only removes historical `Remove` / superseded `Add` ops, so
    /// a workload composed entirely of live `Add`s shrinks the log
    /// by zero on each pass. A naive `log.ops.len() > threshold`
    /// trigger would re-fire on **every** subsequent mutation once
    /// the live-element count crossed the threshold, replaying the
    /// entire log each time and turning each mutation into an O(n)
    /// operation.
    ///
    /// The watermark-based trigger
    /// (`log.ops.len() - compact_baseline > threshold`) keeps the
    /// amortised cost O(1) per mutation: compaction re-fires only
    /// once per `threshold` *additional* ops, regardless of how
    /// many live elements the set holds. For a workload of N
    /// all-live Adds against threshold K, the naive trigger fires
    /// ~`N - K` times (once per add past the threshold); the
    /// watermark trigger fires at most ~`N / K` times.
    ///
    /// This test pins both bounds: 100 live Adds against threshold
    /// 8 must produce far fewer compactions than the naive trigger
    /// would — concretely, at most `(N / K) × 2` passes (with a
    /// small slack factor for boundary effects), not the ~92 a
    /// per-mutation trigger would cause.
    #[test]
    fn auto_compact_does_not_refire_on_each_add_when_live_set_exceeds_threshold() {
        let threshold = 8usize;
        let total_mutations = 100usize;
        let mut engine: SyncEngine<u64> = SyncEngine::new().with_compact_threshold(Some(threshold));

        // All-live-Adds workload — exercises the exact case where
        // the naive trigger degrades to O(n) per mutation.
        for v in 0..total_mutations {
            engine.add(v as u64);
        }

        let epoch = engine.compaction_epoch();

        // Amortised-cost upper bound: the watermark trigger fires
        // at most once per `threshold` additional ops. Allow a
        // small slack factor (×2) to absorb boundary effects (the
        // first pass fires at log_len = threshold + 1, leaving
        // baseline = threshold + 1 instead of a clean threshold
        // multiple).
        let amortised_upper_bound = (total_mutations / threshold) * 2;
        let epoch_usize = usize::try_from(epoch).expect("epoch fits in usize on supported targets");
        assert!(
            epoch_usize <= amortised_upper_bound,
            "auto-compaction must amortise to O(N/threshold) passes; \
             got epoch={epoch} for {total_mutations} all-live-Adds against threshold={threshold} \
             (naive log.ops.len()>threshold would inflate the epoch to ~{} here)",
            total_mutations.saturating_sub(threshold),
        );

        // Naive-trigger lower bound: the broken implementation
        // would have fired on essentially every mutation past the
        // threshold (~92 passes for 100 Adds / threshold 8).
        // Pinning the assertion below half that catches any
        // regression to per-mutation triggering even with generous
        // slack for unrelated implementation churn.
        let naive_lower_bound = total_mutations.saturating_sub(threshold) / 2;
        assert!(
            epoch_usize < naive_lower_bound,
            "auto-compaction must NOT re-fire on every mutation; \
             got epoch={epoch}, naive-trigger lower bound is {naive_lower_bound}"
        );

        // Sanity: the set still contains everything we added.
        let (set, _supers) = engine.state().unwrap();
        assert_eq!(
            set.elements_count(),
            total_mutations,
            "all live values survived"
        );
    }
}
