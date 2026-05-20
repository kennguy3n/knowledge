//! Handle-based FFI runtime registry.
//!
//! The UniFFI / N-API surface is shaped like a flat C-style API: each
//! function takes plain data and returns plain data. The Rust core,
//! however, owns stateful objects (an open SQLCipher database, a
//! `DekRegistry` of destroyed keys, a master key). This module mediates
//! between those two worlds by holding a process-wide map of open
//! runtimes keyed by an opaque [`RuntimeHandle`].
//!
//! # Lifecycle
//!
//! * [`open_store`] allocates a fresh runtime, inserts it into the map
//!   and returns the caller's [`RuntimeHandle`].
//! * Every other FFI function takes a [`RuntimeHandle`] as its first
//!   argument and reaches into the map to find the corresponding
//!   runtime. Calls with an unknown handle return
//!   [`FfiError::Unavailable`] with `subsystem = "evidence_store"`.
//! * [`close_store`] removes the entry from the map and then blocks
//!   until any concurrent in-flight call on the same handle has
//!   released its clone of the `Arc`. The underlying `FfiRuntime` is
//!   dropped (zeroizing the master key and closing the SQLite handle)
//!   before `close_store` returns, restoring the implicit synchronous
//!   teardown that the pre-handle singleton design provided.
//!   Hosts can therefore `move`/`unlink` the database file immediately
//!   after `close_store` returns without risking a Windows
//!   mandatory-file-lock conflict.
//!
//! Multiple handles can be open simultaneously — each holds an
//! independent SQLCipher database, master key, and `DekRegistry`.
//! Hosts that want a single global runtime simply call `open_store`
//! once and reuse the handle; hosts that want per-account isolation
//! (multi-profile desktop app, integration tests running in parallel,
//! …) call `open_store` once per profile.
//!
//! # Concurrency
//!
//! The handle map is wrapped in an `RwLock`. Lookups acquire the read
//! lock just long enough to clone an `Arc<Mutex<FfiRuntime>>` out of
//! the entry. The actual FFI call then locks the per-handle `Mutex`,
//! so calls against the same handle serialize while calls against
//! distinct handles run in parallel.
//!
//! `close_store` first takes the write lock and removes the entry
//! (so no new clones can be minted) and then drains any
//! already-in-flight calls by spinning on `Arc::try_unwrap` until it
//! owns the only remaining strong reference. Concurrent calls on
//! *other* handles are unaffected — they touch different entries in
//! the map.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard};

use chrono::{DateTime, TimeZone, Utc};
use crypto::forgetting::{self, DekRegistry, TombstoneStore};
use crypto::{CryptoError, MasterKey};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};
use memory_manager::{ChannelMemoryObject, UserMemoryObject};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::error::{FfiError, FfiResult};

/// Opaque handle to an open evidence-store runtime, returned by
/// [`open_store`] and required as the first argument of every other
/// FFI function.
///
/// Handles are monotonically-allocated `u64`s; the host treats them
/// as opaque. The `0` value is reserved as an invalid sentinel —
/// passing it to any FFI function returns
/// [`FfiError::Unavailable`] with `subsystem = "evidence_store"`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct RuntimeHandle(pub u64);

impl RuntimeHandle {
    /// The reserved sentinel value. Never returned by [`open_store`].
    /// Hosts can use it as a "no handle yet" placeholder before
    /// calling `open_store`.
    pub const NONE: RuntimeHandle = RuntimeHandle(0);

    /// The raw `u64` carried by the handle.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl From<u64> for RuntimeHandle {
    fn from(v: u64) -> Self {
        RuntimeHandle(v)
    }
}

impl From<RuntimeHandle> for u64 {
    fn from(h: RuntimeHandle) -> Self {
        h.0
    }
}

/// `crypto::forgetting::TombstoneStore` implementation backed by the
/// `EvidenceStore`'s SQLCipher tables.
///
/// `crypto` cannot take a SQLite dependency (it would create a
/// circular dep with `evidence_store`), so this adapter lives in the
/// FFI runtime crate. It threads tombstone persistence through the
/// existing `forgotten_scopes` (v4) and `epoch_tombstones` (v8)
/// tables so [`crypto::forgetting::destroy_scope_dek`] and
/// [`crypto::forgetting::destroy_epoch_dek`] can record the
/// destruction in a single atomic step — the in-memory DEK is
/// zeroized first, then the on-disk tombstone is written before the
/// destroy call returns.
///
/// Both persist methods are idempotent at the SQL layer via the
/// underlying `INSERT OR IGNORE` semantics, matching the
/// `TombstoneStore` contract.
pub(crate) struct EvidenceStoreTombstoneStore<'a> {
    store: &'a mut EvidenceStore,
}

impl<'a> EvidenceStoreTombstoneStore<'a> {
    pub(crate) fn new(store: &'a mut EvidenceStore) -> Self {
        Self { store }
    }
}

/// Convert a `chrono::DateTime<Utc>` into the Unix-epoch-seconds
/// representation used by both the `forgotten_scopes` and
/// `epoch_tombstones` tables.
fn dt_to_unix(at: DateTime<Utc>) -> i64 {
    at.timestamp()
}

/// Inverse of [`dt_to_unix`]. Returns `Utc::now()` instead of
/// erroring on a malformed timestamp because the alternative is to
/// fail the whole `open_store` replay over what would necessarily be
/// a corrupted on-disk row — the surrounding code already treats
/// timestamps as advisory metadata for the audit trail (the
/// destruction itself is what re-establishes the registry invariant).
fn unix_to_dt(unix: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(unix, 0).single().unwrap_or_else(Utc::now)
}

impl TombstoneStore for EvidenceStoreTombstoneStore<'_> {
    fn persist_tombstone(
        &mut self,
        scope: forgetting::ScopeId,
        epoch: forgetting::EpochId,
        destroyed_at: DateTime<Utc>,
    ) -> Result<(), CryptoError> {
        let scope_id = ScopeId::from_uuid(scope.0);
        self.store
            .record_epoch_tombstone(scope_id, epoch.0, dt_to_unix(destroyed_at))
            .map_err(|e| CryptoError::TombstonePersistence(e.to_string()))
    }

    fn persist_forgotten_scope(
        &mut self,
        scope: forgetting::ScopeId,
        destroyed_at: DateTime<Utc>,
    ) -> Result<(), CryptoError> {
        let scope_id = ScopeId::from_uuid(scope.0);
        self.store
            .record_forgotten_scope_at(scope_id, dt_to_unix(destroyed_at))
            .map_err(|e| CryptoError::TombstonePersistence(e.to_string()))
    }

    fn load_tombstones(
        &self,
    ) -> Result<Vec<(forgetting::ScopeId, forgetting::EpochId, DateTime<Utc>)>, CryptoError> {
        let rows = self
            .store
            .load_epoch_tombstones()
            .map_err(|e| CryptoError::TombstonePersistence(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(scope_id, epoch_id, ts)| {
                (
                    forgetting::ScopeId(scope_id.as_uuid()),
                    forgetting::EpochId(epoch_id),
                    unix_to_dt(ts),
                )
            })
            .collect())
    }

    fn load_forgotten_scopes(
        &self,
    ) -> Result<Vec<(forgetting::ScopeId, DateTime<Utc>)>, CryptoError> {
        let rows = self
            .store
            .load_forgotten_scopes_with_timestamps()
            .map_err(|e| CryptoError::TombstonePersistence(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(scope_id, ts)| (forgetting::ScopeId(scope_id.as_uuid()), unix_to_dt(ts)))
            .collect())
    }
}

/// In-memory runtime state carried across FFI calls.
///
/// Holds the open [`EvidenceStore`], the per-user [`MasterKey`] (used
/// to derive [`encrypt`](crate::encrypt) / [`decrypt`](crate::decrypt)
/// keys and to seed [`DekRegistry`] entries lazily on ingest), and the
/// destroyed-key registry that backs [`forget`](crate::forget).
///
/// One `FfiRuntime` exists per open [`RuntimeHandle`]. The runtime is
/// dropped once the handle is removed from the registry *and* every
/// in-flight call that had cloned its `Arc` has released it.
pub struct FfiRuntime {
    pub(crate) master_key: MasterKey,
    pub(crate) store: EvidenceStore,
    pub(crate) registry: DekRegistry,
    /// Per-scope user-memory CRUD layer. Rehydrated on `open_store`
    /// from the encrypted `memory_objects` table and flushed back on
    /// every mutation so the state survives process restarts.
    pub(crate) user_memories: HashMap<ScopeId, UserMemoryObject>,
    /// Per-scope channel-memory recap home. Persisted to the same
    /// `memory_objects` table under the `channel_memory` kind.
    pub(crate) channel_memories: HashMap<ScopeId, ChannelMemoryObject>,
}

impl Drop for FfiRuntime {
    fn drop(&mut self) {
        self.master_key.zeroize();
    }
}

impl FfiRuntime {
    /// Borrow the open evidence store.
    pub(crate) fn store(&self) -> &EvidenceStore {
        &self.store
    }

    /// Borrow the open evidence store mutably.
    pub(crate) fn store_mut(&mut self) -> &mut EvidenceStore {
        &mut self.store
    }

    /// Borrow the in-memory destroyed-key registry.
    pub(crate) fn registry(&self) -> &DekRegistry {
        &self.registry
    }

    /// Borrow the in-memory destroyed-key registry mutably.
    pub(crate) fn registry_mut(&mut self) -> &mut DekRegistry {
        &mut self.registry
    }

    /// Borrow the per-user master key. Kept `pub(crate)` so callers
    /// cannot leak it across the FFI boundary.
    #[allow(dead_code)]
    pub(crate) fn master_key(&self) -> &MasterKey {
        &self.master_key
    }

    /// Borrow the per-scope user memory if one has already been
    /// allocated. Returns `None` for scopes that have never had a
    /// memory object ingested or sweep run against them.
    ///
    /// Read-only FFI paths (`get_user_memory`, `list_memories`)
    /// use this accessor instead of [`Self::user_memory_mut`] so a
    /// query for an unknown scope does not permanently allocate an
    /// empty `UserMemoryObject` in the per-handle map.
    pub(crate) fn user_memory(&self, scope: ScopeId) -> Option<&UserMemoryObject> {
        self.user_memories.get(&scope)
    }

    /// Borrow the per-scope user memory, creating an empty one if it
    /// does not yet exist. The runtime treats `scope_id` as the
    /// owning user id for now (no separate user-id surface yet);
    /// a future release will make this a separate handle.
    ///
    /// Prefer [`Self::user_memory`] for read-only paths to avoid
    /// growing the per-scope map on benign lookups.
    pub(crate) fn user_memory_mut(&mut self, scope: ScopeId) -> &mut UserMemoryObject {
        self.user_memories
            .entry(scope)
            .or_insert_with(|| UserMemoryObject::new(scope.as_uuid(), scope))
    }

    /// Borrow the per-scope channel memory, if one exists.
    pub(crate) fn channel_memory(&self, scope: ScopeId) -> Option<&ChannelMemoryObject> {
        self.channel_memories.get(&scope)
    }

    /// Borrow the per-scope channel memory, creating an empty one if
    /// it does not yet exist.
    ///
    /// Currently unused on the FFI surface — `trigger_synthesis`
    /// returns `Unavailable` without allocating, and a real
    /// allocation only happens once the SLM-driven synthesizer
    /// lands. Kept here so the call-site doesn't need to
    /// re-derive map manipulation logic when that wiring arrives.
    #[allow(dead_code)]
    pub(crate) fn channel_memory_mut(&mut self, scope: ScopeId) -> &mut ChannelMemoryObject {
        self.channel_memories
            .entry(scope)
            .or_insert_with(|| ChannelMemoryObject::new(scope))
    }

    /// Flush the in-memory `UserMemoryObject` for `scope` to the
    /// encrypted evidence store. Called after every mutation (pin,
    /// unpin, decay_sweep) so the state survives process restarts.
    pub(crate) fn flush_user_memory(&self, scope: ScopeId) -> crate::error::FfiResult<()> {
        if let Some(umo) = self.user_memories.get(&scope) {
            let json = serde_json::to_vec(umo).map_err(|e| crate::error::FfiError::Memory {
                message: format!("failed to serialize user memory: {e}"),
            })?;
            self.store
                .save_memory_blob(scope, "user_memory", &json)
                .map_err(|e| crate::error::FfiError::Evidence {
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }

    /// Flush the in-memory `ChannelMemoryObject` for `scope` to the
    /// encrypted evidence store.
    #[allow(dead_code)]
    pub(crate) fn flush_channel_memory(&self, scope: ScopeId) -> crate::error::FfiResult<()> {
        if let Some(cmo) = self.channel_memories.get(&scope) {
            let json = serde_json::to_vec(cmo).map_err(|e| crate::error::FfiError::Memory {
                message: format!("failed to serialize channel memory: {e}"),
            })?;
            self.store
                .save_memory_blob(scope, "channel_memory", &json)
                .map_err(|e| crate::error::FfiError::Evidence {
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }
}

// ───────────────────────── Handle registry ─────────────────────────

type HandleEntry = Arc<Mutex<FfiRuntime>>;

fn registry() -> &'static RwLock<HashMap<u64, HandleEntry>> {
    static REGISTRY: OnceLock<RwLock<HashMap<u64, HandleEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_handle() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    // `Relaxed` is sufficient: `fetch_add` is an atomic RMW so each
    // caller is guaranteed to receive a distinct value regardless of
    // memory ordering. There is no other memory location whose
    // visibility must be sequenced against this counter — the
    // registry write lock (taken in `open_store` right after the
    // increment) carries the actual happens-before edge for inserting
    // the new entry.
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn read_registry() -> RwLockReadGuard<'static, HashMap<u64, HandleEntry>> {
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_registry() -> std::sync::RwLockWriteGuard<'static, HashMap<u64, HandleEntry>> {
    registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` against the runtime bound to `handle`, returning
/// [`FfiError::Unavailable`] when the handle is unknown (either never
/// opened or already closed).
///
/// The function clones the per-handle `Arc<Mutex<FfiRuntime>>` out of
/// the registry under the read lock and releases the read lock before
/// taking the per-runtime `Mutex` — so a long-running call on handle
/// A never blocks a short-running call on handle B, and `close_store`
/// can land its `remove` even while a different handle is busy.
/// `close_store` then blocks (only on its own handle) until the
/// in-flight call on that handle finishes; see its docs for details.
pub(crate) fn with_runtime<F, T>(handle: RuntimeHandle, f: F) -> FfiResult<T>
where
    F: FnOnce(&mut FfiRuntime) -> FfiResult<T>,
{
    let entry = {
        let guard = read_registry();
        guard
            .get(&handle.0)
            .cloned()
            .ok_or_else(|| FfiError::Unavailable {
                subsystem: "evidence_store".into(),
            })?
    };
    let mut rt = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut rt)
}

/// Open the SQLCipher-backed evidence store at `path` using the
/// 32-byte master key encoded as `master_key_hex` (64 lower-case hex
/// chars). Returns the [`RuntimeHandle`] that every other FFI function
/// needs as its first argument.
///
/// Multiple stores can be opened concurrently — each call returns a
/// fresh handle bound to an independent `EvidenceStore`, master key
/// and `DekRegistry`. Re-opening the same on-disk database under a
/// second handle is supported but the two handles do **not** share an
/// in-memory `DekRegistry` cache, so they will each replay the
/// `forgotten_scopes` table on open.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `master_key_hex` is not exactly 64
///   hex characters.
/// * [`FfiError::Evidence`] if SQLCipher fails to open the underlying
///   database or the tombstone-replay path errors out.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
pub fn open_store(path: String, master_key_hex: String) -> FfiResult<RuntimeHandle> {
    let master_key = parse_master_key_hex(&master_key_hex)?;
    let mut store = EvidenceStore::open(&path, &master_key, EvidenceStoreConfig::default())
        .map_err(|e| FfiError::Evidence {
            message: e.to_string(),
        })?;

    // Durable cryptographic-forgetting tombstones.
    // The on-disk `forgotten_scopes` table is the authoritative
    // record of every scope whose DEK has been destroyed. Replay
    // it into a fresh in-memory `DekRegistry` so post-restart
    // calls for those scopes continue to short-circuit with
    // `NotFound { kind: "scope" }` — the in-memory short-circuit
    // is what every public `is_scope_forgotten` check reads.
    let mut registry = DekRegistry::new();
    let tombstones: HashSet<ScopeId> = store
        .load_forgotten_scopes()
        .map_err(|e| FfiError::Evidence {
            message: e.to_string(),
        })?
        .into_iter()
        .collect();
    for scope in &tombstones {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        // The return value is the list of `KeyDestructionEvent`s
        // produced by the destroy call; we intentionally drop it
        // here. The destruction itself is what re-establishes
        // the registry invariant, and audit-trail emission for
        // *re-loaded* tombstones is not required by the spec —
        // each tombstone was already audited on its original
        // forget() call.
        //
        // We pass `None` for the `TombstoneStore` parameter — the
        // on-disk tombstone already exists (we are replaying it),
        // so re-persisting would be a duplicate `INSERT OR IGNORE`
        // and a wasted I/O round-trip. The destroy call still
        // populates the in-memory `tombstones` map on the
        // `DekRegistry`.
        let _ = forgetting::destroy_scope_dek(&mut registry, registry_scope, None)
            .expect("None tombstone-store cannot fail");
    }

    // Replay per-epoch tombstones (v8 schema). The
    // `forgotten_scopes` table above only carries scope-grain
    // forgetting; individual epoch DEK destructions (emitted by
    // `crypto::forgetting::destroy_epoch_dek`) live in
    // `epoch_tombstones`. Both must be replayed so post-restart
    // calls for forgotten epochs continue to short-circuit even
    // when the scope itself is still live.
    let epoch_tombstones = store
        .load_epoch_tombstones()
        .map_err(|e| FfiError::Evidence {
            message: e.to_string(),
        })?;
    for (scope_id, epoch_id, _at) in epoch_tombstones {
        // Skip rows whose scope is already scope-forgotten — the
        // earlier `destroy_scope_dek` walk already added every
        // epoch tombstone for that scope, so a per-epoch replay
        // would be redundant.
        if tombstones.contains(&scope_id) {
            continue;
        }
        let registry_scope = forgetting::ScopeId(scope_id.as_uuid());
        let registry_epoch = forgetting::EpochId(epoch_id);
        let _ = forgetting::destroy_epoch_dek(&mut registry, registry_scope, registry_epoch, None)
            .expect("None tombstone-store cannot fail");
    }

    // Re-purge the FTS5 / embedding secondary indexes for every
    // replayed tombstone.
    //
    // `forget()` performs three steps in order: (1) destroy the
    // in-memory DEK, (2) persist the tombstone via
    // `record_forgotten_scope`, (3) purge the FTS / embedding rows
    // via `purge_fts_for_scope`. If the process crashes between
    // steps 2 and 3, the tombstone survives but the plaintext-
    // derived FTS terms persist on disk indefinitely — accessible
    // via raw SQLite without the per-scope DEK and so escaping the
    // cryptographic-forgetting contract. Re-running the purge on
    // every `open_store` closes that window.
    //
    // The FTS purge uses the batched [`EvidenceStore::purge_fts_for_scopes`]
    // entry point so we issue at most a single FTS5 `REBUILD`
    // across the whole replay — not one per scope.
    //
    // Both [`EvidenceStore::purge_fts_for_scope`] and the batch
    // method skip the `REBUILD` when zero FTS rows were actually
    // deleted, so the steady-state replay (every scope already
    // purged on a prior boot) costs `O(N)` zero-row `DELETE`s and
    // *no* rebuilds in either shape. The batch entry point
    // matters in the crash-recovery shape — where `K` of the `N`
    // tombstones still have FTS rows because we crashed between
    // tombstone write and FTS purge. Calling the single-scope
    // method `N` times in that shape would issue `K` separate
    // rebuilds (one per scope that still has data, each
    // O(total_fts_rows)); the batch method coalesces them into a
    // single rebuild at the end of the open transaction.
    //
    // The wrap / blob purges are still issued per scope: each one
    // only touches a small number of rows for the scope it owns,
    // so there is no analogous N-rebuild blow-up to consolidate.
    //
    // A purge failure here surfaces as an `Evidence` error rather
    // than being swallowed — mirroring the `forget()` path's
    // error handling and matching what `record_forgotten_scope`
    // already does for the same conditions. A host that hits this
    // path on startup has a corrupt or unreadable secondary index
    // and needs to know about it.
    // `tombstones` is a `HashSet` for the dedup-on-load and
    // `is_scope_forgotten` lookups above; `purge_fts_for_scopes`
    // wants a slice. The slice's iteration order does not affect
    // correctness — DELETEs are commutative and the single batched
    // REBUILD runs after every per-scope DELETE has landed in the
    // open transaction.
    let tombstones_slice: Vec<ScopeId> = tombstones.iter().copied().collect();
    store
        .purge_fts_for_scopes(&tombstones_slice)
        .map_err(|e| FfiError::Evidence {
            message: e.to_string(),
        })?;
    for scope in &tombstones {
        store
            .purge_body_key_wraps_for_scope(*scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        // Clean up orphaned memory blobs that survived a crash
        // between tombstone write and blob deletion in forget().
        store
            .delete_memory_blobs_for_scope(*scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
    }

    // Populate the DekRegistry from the store's in-memory cache,
    // which was already hydrated from `scope_deks` during
    // `EvidenceStore::open`. No second DB query needed.
    //
    // Defense-in-depth: if `delete_scope_dek` failed during a prior
    // `forget()`, the wrapped DEK may still sit in `scope_deks` and
    // `load_scope_deks` will have loaded it into the store's cache.
    // Evict forgotten keys from the cache AND delete the dangling
    // `scope_deks` row from disk so the wrapped DEK does not persist
    // across restarts.
    for (scope, key) in &store.cached_scope_keys() {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        if registry.is_scope_forgotten(registry_scope) {
            store.evict_cached_scope_key(*scope);
            // Best-effort: delete the dangling wrapped DEK from disk.
            if let Err(e) = store.delete_scope_dek_row(*scope) {
                tracing::warn!(
                    scope = %scope.as_uuid(),
                    error = %e,
                    "failed to clean up dangling scope_deks row; will retry on next open_store"
                );
            }
            continue;
        }
        let dek = forgetting::ScopeDek::new(registry_scope, forgetting::EpochId::zero(), *key);
        registry.insert_scope_dek(dek);
    }

    // Rehydrate persisted user memories from the encrypted
    // `memory_objects` table (v7 schema). Tombstoned scopes are
    // skipped — their memory blobs should have been deleted by
    // `forget()`.
    let mut user_memories = HashMap::new();
    let user_scopes = store
        .list_memory_scopes("user_memory")
        .map_err(|e| FfiError::Evidence {
            message: e.to_string(),
        })?;
    for scope in user_scopes {
        if tombstones.contains(&scope) {
            continue;
        }
        match store.load_memory_blob(scope, "user_memory") {
            Ok(Some(blob)) => match serde_json::from_slice::<UserMemoryObject>(&blob) {
                Ok(umo) => {
                    user_memories.insert(scope, umo);
                }
                Err(e) => {
                    tracing::warn!(
                        scope = %scope.as_uuid(),
                        error = %e,
                        "failed to deserialize user_memory blob; blob dropped"
                    );
                }
            },
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    scope = %scope.as_uuid(),
                    error = %e,
                    "failed to load user_memory blob; skipping"
                );
            }
        }
    }

    let mut channel_memories = HashMap::new();
    let channel_scopes =
        store
            .list_memory_scopes("channel_memory")
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
    for scope in channel_scopes {
        if tombstones.contains(&scope) {
            continue;
        }
        match store.load_memory_blob(scope, "channel_memory") {
            Ok(Some(blob)) => match serde_json::from_slice::<ChannelMemoryObject>(&blob) {
                Ok(cmo) => {
                    channel_memories.insert(scope, cmo);
                }
                Err(e) => {
                    tracing::warn!(
                        scope = %scope.as_uuid(),
                        error = %e,
                        "failed to deserialize channel_memory blob; blob dropped"
                    );
                }
            },
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    scope = %scope.as_uuid(),
                    error = %e,
                    "failed to load channel_memory blob; skipping"
                );
            }
        }
    }

    let runtime = FfiRuntime {
        master_key,
        store,
        registry,
        user_memories,
        channel_memories,
    };

    let handle = RuntimeHandle(next_handle());
    // Defense-in-depth against `AtomicU64` wraparound: `next_handle`
    // starts at 1 and increments via `fetch_add`, so after exactly
    // `u64::MAX` allocations the counter wraps back to 0 — the
    // `RuntimeHandle::NONE` sentinel that every other FFI function
    // treats as "no handle". Refuse to mint that value rather than
    // silently violate the NONE-is-always-invalid contract. In
    // practice this is unreachable (2^64 opens per process), but the
    // check is two instructions and pins the invariant.
    if handle.0 == RuntimeHandle::NONE.0 {
        return Err(FfiError::Evidence {
            message: "runtime handle allocator wrapped to the reserved NONE sentinel".into(),
        });
    }
    let mut guard = write_registry();
    // Allocation is monotonic via `NEXT`, so a collision against an
    // already-open handle would also mean we wrapped. Same reasoning
    // as the sentinel check above — refuse rather than silently
    // overwrite an open runtime.
    if guard.contains_key(&handle.0) {
        return Err(FfiError::Evidence {
            message: format!("runtime handle {} collided during allocation", handle.0),
        });
    }
    guard.insert(handle.0, Arc::new(Mutex::new(runtime)));
    Ok(handle)
}

/// Drop the runtime bound to `handle` and zeroize its master key.
///
/// Idempotent: closing an unknown or already-closed handle is a no-op
/// and returns `Ok(())`. The host can therefore call this in a
/// `try`/`finally` shutdown handler without first probing the
/// runtime state.
///
/// # Concurrency contract
///
/// `close_store` is **synchronous**: it does not return until the
/// SQLite connection and the master key for `handle` have been
/// dropped.
///
/// 1. Remove the entry from the handle map under the registry write
///    lock — this stops any *new* `with_runtime` calls on `handle`
///    from acquiring a fresh `Arc` clone.
/// 2. Drain any *already-in-flight* calls by spinning on
///    [`Arc::try_unwrap`] until we own the only remaining strong
///    reference. The set of outstanding clones is fixed at the moment
///    step 1 completes (since the registry no longer hands out new
///    ones), so this loop is bounded by the longest in-flight call.
/// 3. Drop the unwrapped `Mutex<FfiRuntime>`. Dropping `FfiRuntime`
///    closes the SQLCipher connection and zeroizes the master key on
///    the way out.
///
/// This matches the implicit synchronous-teardown property of the
/// pre-handle singleton design (where `lock_runtime` + `*guard =
/// None` blocked until any in-flight call released the singleton
/// mutex), so Windows hosts can still `move`/`unlink` the database
/// file immediately after `close_store` returns without risking a
/// mandatory-file-lock conflict.
///
/// Idempotent: closing an unknown or already-closed handle is a
/// no-op and returns `Ok(())` without spinning.
pub fn close_store(handle: RuntimeHandle) -> FfiResult<()> {
    let Some(mut entry) = write_registry().remove(&handle.0) else {
        return Ok(());
    };
    // Drain outstanding `with_runtime` calls on this handle. Because
    // step 1 (the `remove` above) already happened, no new clones can
    // be minted — the strong count can only monotonically drop until
    // it reaches 1 (our copy). We use `try_unwrap` rather than
    // `Arc::strong_count` + race-prone manual checking, then yield
    // briefly if another thread still holds a clone so we do not
    // pin a CPU core on long-running calls.
    loop {
        match Arc::try_unwrap(entry) {
            Ok(mutex) => {
                // Drop here closes the SQLCipher connection and
                // zeroizes the master key (`FfiRuntime::Drop`).
                drop(mutex);
                return Ok(());
            }
            Err(returned) => {
                entry = returned;
                // 1 ms sleep balances responsiveness against CPU
                // burn for the common case of short in-flight calls.
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

fn parse_master_key_hex(hex: &str) -> FfiResult<MasterKey> {
    if hex.len() != crypto::MASTER_KEY_LEN * 2 {
        return Err(FfiError::InvalidId {
            message: format!(
                "master_key_hex must be {} hex chars, got {}",
                crypto::MASTER_KEY_LEN * 2,
                hex.len()
            ),
        });
    }
    let mut out: MasterKey = [0u8; crypto::MASTER_KEY_LEN];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|_| FfiError::InvalidId {
            message: "master_key_hex contains non-ascii bytes".into(),
        })?;
        out[i] = u8::from_str_radix(s, 16).map_err(|_| FfiError::InvalidId {
            message: format!("master_key_hex byte {i} is not valid hex"),
        })?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_master_key_hex_accepts_valid_input() {
        let key = parse_master_key_hex(&"ab".repeat(32)).unwrap();
        assert_eq!(key, [0xAB; 32]);
    }

    #[test]
    fn parse_master_key_hex_rejects_short_input() {
        let err = parse_master_key_hex("ab").unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn parse_master_key_hex_rejects_non_hex_input() {
        let err = parse_master_key_hex(&"zz".repeat(32)).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn runtime_handle_round_trips_through_u64() {
        let h = RuntimeHandle(42);
        assert_eq!(h.raw(), 42);
        let v: u64 = h.into();
        assert_eq!(v, 42);
        let back: RuntimeHandle = 42u64.into();
        assert_eq!(back, h);
    }

    #[test]
    fn runtime_handle_none_is_the_zero_sentinel() {
        assert_eq!(RuntimeHandle::NONE.raw(), 0);
        assert_eq!(RuntimeHandle::NONE, RuntimeHandle(0));
    }

    #[test]
    fn with_runtime_returns_unavailable_for_unknown_handle() {
        let err = with_runtime(RuntimeHandle(u64::MAX), |_| Ok(())).unwrap_err();
        assert!(
            matches!(err, FfiError::Unavailable { ref subsystem } if subsystem == "evidence_store")
        );
    }
}
