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
#[cfg(feature = "http-client")]
use connector_framework::{BlockingHttpTransport, OAuth2Client};
use connector_framework::{Connector, ConnectorInstance, ConnectorInstanceId, OAuth2TokenVault};
use crypto::forgetting::{self, DekRegistry, TombstoneStore};
use crypto::{CryptoError, MasterKey};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};
use inference_router::{
    FallbackAdapter, InferenceAdapter, InferenceRouter, LlamaCppAdapter, MlxAdapter, RouterConfig,
};
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

// Tell uniffi how to round-trip `RuntimeHandle` across the FFI
// boundary. The newtype is `#[repr(transparent)]` over `u64`, so
// uniffi's standard `u64` codec is byte-compatible — `custom_newtype!`
// just wires the lift / lower into the existing `From<u64>` /
// `From<RuntimeHandle> for u64` impls above so Swift and Kotlin see
// the handle as a plain native `UInt64` / `ULong` instead of a Record
// wrapper with a `raw` field. The N-API binding in `crates/napi/src/bindings.rs`
// continues to map this as a `BigInt` over the same `u64` underneath.
uniffi::custom_newtype!(RuntimeHandle, u64);

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
    /// On-device SLM inference router. Constructed eagerly at
    /// `open_store` time so classification tasks (which the
    /// [`FallbackAdapter`] handles even without an SLM) are always
    /// available; synthesis tasks dispatch through the MLX +
    /// llama.cpp adapters when those are wired by the platform shell.
    ///
    /// No interior `Mutex` is needed: `InferenceRouter::dispatch`
    /// takes `&self`, and each adapter manages its own probe state
    /// / activity tracking through `OnceLock`, `AtomicBool`, or
    /// `RwLock` as appropriate. The whole `FfiRuntime` is itself
    /// held inside `Arc<Mutex<FfiRuntime>>` at the handle registry,
    /// which already serialises calls against the same handle —
    /// adding an inner mutex would double-lock without buying any
    /// extra safety.
    ///
    /// Stored behind an `Arc` so [`open_store`] can hand a clone to
    /// the background bootstrap thread without giving up ownership.
    /// The `Arc<InferenceRouter>` itself is `Sync`, and dispatch /
    /// idle-sweep paths only need `&InferenceRouter`, which the
    /// `Deref` impl on `Arc` provides transparently.
    pub(crate) inference_router: Arc<InferenceRouter>,
    /// Per-runtime connector registry — every
    /// [`create_connector`](crate::create_connector) call inserts a
    /// fresh [`ConnectorInstance`] (config + sync state) keyed by
    /// its [`ConnectorInstanceId`].
    ///
    /// The struct is held by value (not behind a lock) because the
    /// entire `FfiRuntime` is already wrapped in `Arc<Mutex<…>>` at
    /// the handle registry, which serialises every FFI call against
    /// the same handle. Connector lifecycle calls (`create_connector`
    /// / `authenticate_connector` / `sync_connector` /
    /// `remove_connector`) all run with that mutex held, so adding
    /// an inner lock would double-lock without buying any extra
    /// safety.
    ///
    /// Mirrored to disk via the v9 `connector_instances` SQLCipher
    /// table: every `create_connector` / `sync_connector` /
    /// `remove_connector` call writes through to the AEAD-encrypted
    /// row before the in-memory map is touched, and `open_store`
    /// rehydrates this map by deserialising every row (see
    /// `crate::connector::rehydrate_connectors`). Tombstoned scopes
    /// are skipped on rehydrate so a forgotten scope's connector
    /// state never resurrects across restarts.
    pub(crate) connector_instances: HashMap<ConnectorInstanceId, ConnectorInstance>,
    /// Live connector implementors keyed by instance id. Built by
    /// the connector factory at `create_connector` time and dropped
    /// at `remove_connector` / `close_store`.
    ///
    /// Stored as `Arc<dyn Connector>` so the FFI surface can
    /// **clone the handle** out of the per-runtime mutex and run
    /// the connector's HTTP round-trip with the mutex released —
    /// every other FFI call on the same handle (evidence queries,
    /// memory reads, health checks, other connector lifecycle
    /// calls) stays unblocked while the sync is in flight. See
    /// `crates/ffi/src/connector.rs::sync_connector` for the
    /// three-phase locking pattern that consumes these `Arc`s. The
    /// `Send + Sync` supertraits on the [`Connector`] trait are
    /// what make the `Arc<dyn Connector>` clone safe to ship across
    /// the FFI surface's cross-thread call pattern; see the
    /// `Connector` trait docs in `crates/connector_framework`.
    pub(crate) connectors: HashMap<ConnectorInstanceId, Arc<dyn Connector>>,
    /// OAuth2 token bundles keyed by connector instance id. Mirrored
    /// to disk via the v9 `connector_tokens` SQLCipher table:
    /// `authenticate_connector` writes the AEAD-encrypted token
    /// payload through to the row before updating the in-memory
    /// vault, and `open_store` rehydrates the vault by deserialising
    /// every row's plaintext. The on-disk ciphertext is sealed under
    /// the per-scope DEK so tokens for a forgotten scope are
    /// cryptographically unrecoverable even if the row delete races
    /// against the scope DEK destruction.
    pub(crate) token_vault: OAuth2TokenVault,
    /// Shared HTTP transport for every connector on this runtime.
    ///
    /// `reqwest::blocking::Client` (which `BlockingHttpTransport`
    /// wraps) manages a connection pool, TLS session cache, and a
    /// thread pool internally — building one per `create_connector`
    /// call would multiply those pools by the number of connectors
    /// on the runtime even when several connectors target the same
    /// provider host. Allocating a single transport at
    /// [`open_store`] time and handing every connector an
    /// `Arc<dyn HttpTransport>` clone lets reqwest re-use one pool
    /// across the whole substrate; per-provider host isolation is
    /// still preserved at the TLS / Host-header layer.
    ///
    /// Held as `Arc<dyn HttpTransport>` (rather than the concrete
    /// `BlockingHttpTransport`) so the same field type can later
    /// host an `AsyncHttpTransport`-backed implementation without
    /// reshaping the runtime; today only the blocking transport is
    /// wired in. Only present when the `http-client` feature is on
    /// — without it, the connector factory short-circuits to
    /// `FfiError::Unavailable { subsystem: "connector-http-client" }`
    /// before reaching this field.
    ///
    /// Wrapped in `Option` so a failure to build the transport at
    /// [`open_store`] time degrades **only** the connector
    /// subsystem rather than refusing to open the store at all.
    /// Hosts that do not need connectors (e.g. ingest-only test
    /// fixtures, offline builds where the network is unavailable)
    /// stay fully functional; connector calls surface
    /// [`FfiError::Unavailable { subsystem: "connector-http-client" }`]
    /// — the same path the `not(http-client)` build takes — so
    /// hosts see one uniform recovery contract.
    #[cfg(feature = "http-client")]
    pub(crate) http_transport: Option<Arc<BlockingHttpTransport>>,
    /// Shared OAuth2 token-exchange client, wired through
    /// [`Self::http_transport`]. Same allocation-reuse rationale as
    /// the transport above — the OAuth2 client itself is a thin
    /// stateless wrapper that builds requests through whichever
    /// `Arc<T: HttpTransport>` it was constructed with, so a
    /// single per-runtime instance is sufficient and saves us a
    /// fresh `Arc` per connector.
    ///
    /// Held as the concrete `OAuth2Client<BlockingHttpTransport>`
    /// so [`crate::connector::build_connector`] can up-cast the
    /// `Arc` to the `Arc<dyn OAuth2CodeExchange>` that the
    /// connector constructors expect via the unsized-coercion the
    /// standard library provides for `Arc<T>` → `Arc<dyn Trait>`.
    ///
    /// `Option<…>` lock-step with [`Self::http_transport`] — the
    /// OAuth2 client is constructed from the same shared transport
    /// so the two either populate together or stay `None` together.
    /// See [`Self::http_transport`] for the soft-fail rationale.
    #[cfg(feature = "http-client")]
    pub(crate) oauth_client: Option<Arc<OAuth2Client<BlockingHttpTransport>>>,
    /// Running webhook receiver servers (Phase 5).
    ///
    /// Each entry is one independently-running tokio runtime +
    /// axum server hosted on a dedicated OS thread; the
    /// [`crate::webhook::RunningWebhookServer`] struct holds the
    /// shutdown oneshot, the per-server
    /// [`crate::webhook::FfiWebhookRouter`] (the single
    /// [`connector_framework::WebhookDispatcher`] every framework
    /// route entry points at), and the thread join handle.
    ///
    /// **Not** behind an additional lock: the whole `FfiRuntime`
    /// already lives inside `Arc<Mutex<…>>` at the handle registry,
    /// which serialises every FFI call against the same handle. The
    /// dispatcher closures running on each server's tokio runtime
    /// thread re-enter the substrate through
    /// [`with_runtime`] (which acquires the same mutex), so the
    /// only lock the map ever sits under is the per-handle one —
    /// good enough.
    ///
    /// Lifetime contract: every entry is taken out of this map and
    /// synchronously joined either by an explicit
    /// [`crate::stop_webhook_server`] call OR by
    /// [`crate::close_store`]'s pre-drain step
    /// ([`crate::webhook::drain_all_servers`]) BEFORE the
    /// `Arc::try_unwrap` spin loop. Without that ordering the spin
    /// loop would race the in-flight tokio task that's still calling
    /// [`with_runtime`].
    pub(crate) webhook_servers:
        HashMap<crate::types::WebhookServerHandle, crate::webhook::RunningWebhookServer>,
    /// Singleton background sync scheduler (Phase 6).
    ///
    /// Exactly one [`crate::sync_scheduler::RunningSyncScheduler`]
    /// per runtime; [`crate::start_sync_scheduler`] populates this
    /// slot and [`crate::stop_sync_scheduler`] /
    /// [`crate::close_store`] consume it. The dispatch worker
    /// thread re-enters [`with_runtime`] on every tick (briefly
    /// cloning the entry `Arc`), so the slot MUST be drained
    /// BEFORE the `Arc::try_unwrap` spin loop the same way
    /// [`Self::webhook_servers`] is — see the
    /// [`crate::sync_scheduler::drain_scheduler`] call in
    /// [`crate::close_store`].
    ///
    /// `None` until the host explicitly opts in; most ingest-only
    /// substrate clients (offline CLI batch tools, Electron status
    /// panels) never start a scheduler.
    pub(crate) sync_scheduler: Option<crate::sync_scheduler::RunningSyncScheduler>,
}

impl Drop for FfiRuntime {
    fn drop(&mut self) {
        // Reap the inference-router bootstrap thread before any
        // other teardown work. The bootstrap thread holds an `Arc`
        // clone of `inference_router`, so without an explicit join
        // the thread can outlive `close_store` by however long the
        // probe takes (e.g. a multi-second `GET /health` against
        // the llama.cpp loopback server when the http-client
        // feature is enabled). That window is memory-safe — the
        // thread's `Arc` keeps the router alive — but it leaks
        // OS resources (open sockets, file descriptors) past the
        // point at which the host has been told the store is
        // closed, which the substrate's lifecycle contract
        // ([`close_store`] docs at the top of this module)
        // explicitly forbids. `shutdown()` is idempotent and
        // returns immediately when no background thread is in
        // flight, so the cost on the fast path is one
        // uncontended mutex acquisition.
        self.inference_router.shutdown();
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
    /// cannot leak it across the FFI boundary. Read by the Phase 6
    /// health probe to verify the master key is non-zero (an
    /// all-zero key signals an uninitialised runtime).
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

    /// Number of distinct scopes with a rehydrated
    /// [`UserMemoryObject`]. Used by the Phase 6 health probe.
    pub(crate) fn user_memory_count(&self) -> usize {
        self.user_memories.len()
    }

    /// Number of distinct scopes with a rehydrated
    /// [`ChannelMemoryObject`]. Used by the Phase 6 health probe.
    pub(crate) fn channel_memory_count(&self) -> usize {
        self.channel_memories.len()
    }

    /// Persist a fully-built [`ChannelMemoryObject`] to disk and,
    /// only on disk-save success, install it into the per-scope
    /// in-memory map.
    ///
    /// `trigger_synthesis` builds the next recap off-the-side via
    /// [`Self::channel_memory`] (cloning the existing entry if one
    /// exists) so that any failure between the inference dispatch
    /// and the final flush leaves the substrate's observable state
    /// untouched. This pins the
    /// `trigger_synthesis_failure_does_not_allocate_channel_memory`
    /// invariant: no `channel_memories` map entry is created until
    /// `save_memory_blob` returns `Ok`.
    pub(crate) fn save_channel_memory(
        &mut self,
        scope: ScopeId,
        cmo: ChannelMemoryObject,
    ) -> crate::error::FfiResult<()> {
        let json = serde_json::to_vec(&cmo).map_err(|e| crate::error::FfiError::Memory {
            message: format!("failed to serialize channel memory: {e}"),
        })?;
        self.store
            .save_memory_blob(scope, "channel_memory", &json)
            .map_err(|e| crate::error::FfiError::Evidence {
                message: e.to_string(),
            })?;
        self.channel_memories.insert(scope, cmo);
        Ok(())
    }

    /// Return an owned `Arc` clone of the on-device SLM inference
    /// router so callers can keep a handle alive after dropping the
    /// surrounding [`with_runtime`](crate::runtime::with_runtime)
    /// frame.
    ///
    /// This is the entry point that [`crate::trigger_synthesis`] uses
    /// to split the synthesis call into a `gather → dispatch → apply`
    /// pipeline: it clones the router here while the runtime mutex is
    /// held, then drops the mutex before issuing the (potentially
    /// multi-second) `wait_for_bootstrap` + SLM dispatch. The owned
    /// clone keeps the underlying [`InferenceRouter`] alive across the
    /// unlocked phase even if every other holder were torn down — the
    /// `FfiRuntime` itself can't be dropped concurrently because
    /// `close_store` synchronises with in-flight `with_runtime` frames
    /// via `WITH_RUNTIME_STACK` + the per-handle drain loop, but a
    /// background sweep that legitimately rebuilds the router would
    /// otherwise race the dispatch caller.
    ///
    /// The clone itself is a single atomic increment on the strong
    /// count; no allocation, no contention with concurrent dispatch.
    pub(crate) fn inference_router_arc(&self) -> Arc<InferenceRouter> {
        Arc::clone(&self.inference_router)
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

thread_local! {
    /// Stack of [`RuntimeHandle`] values whose `with_runtime` frames
    /// the current thread is currently executing.
    ///
    /// Pushed on `with_runtime` entry, popped on exit (via the
    /// [`WithRuntimeGuard`] RAII helper so panics still pop).
    /// [`close_store`] reads this stack to detect the *same-handle*
    /// reentrant-call case: a closure passed to `with_runtime(H, ...)`
    /// that calls `close_store(H)` would otherwise deadlock in the
    /// `Arc::try_unwrap` spin loop, because the caller itself is the
    /// outstanding clone the loop is waiting on.
    ///
    /// Tracking handles individually (rather than a single depth
    /// counter) preserves the documented contract that closing a
    /// **different** handle from inside `with_runtime` is supported:
    /// `close_store(H2)` from inside `with_runtime(H1, ...)` is safe
    /// because the thread's outstanding `Arc` clone is for `H1`, not
    /// `H2`, so the drain loop on `H2` terminates immediately.
    ///
    /// `Vec` (rather than `HashSet`) because the expected stack depth
    /// is 0 or 1 (nested `with_runtime` is rare) and linear scan over
    /// a tiny `Vec` is cheaper than a hash lookup. The push/pop
    /// discipline also surfaces any guard imbalance loudly: the next
    /// `close_store` check would observe the wrong handle on top.
    static WITH_RUNTIME_STACK: std::cell::RefCell<Vec<u64>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

struct WithRuntimeGuard {
    handle: u64,
}

impl WithRuntimeGuard {
    fn enter(handle: u64) -> Self {
        WITH_RUNTIME_STACK.with(|s| s.borrow_mut().push(handle));
        WithRuntimeGuard { handle }
    }
}

impl Drop for WithRuntimeGuard {
    fn drop(&mut self) {
        WITH_RUNTIME_STACK.with(|s| {
            let mut stack = s.borrow_mut();
            // Stacks are LIFO: the most recent `enter` pushed this
            // guard's `handle`, so the top should match. Pop
            // unconditionally to keep the invariant tight; the
            // `debug_assert_eq!` flags any imbalance (e.g. a future
            // refactor that lets a guard outlive its frame).
            let popped = stack.pop();
            debug_assert_eq!(
                popped,
                Some(self.handle),
                "WithRuntimeGuard pop/push imbalance"
            );
        });
    }
}

fn thread_holds_runtime_handle(handle: u64) -> bool {
    WITH_RUNTIME_STACK.with(|s| s.borrow().contains(&handle))
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
///
/// Pushes `handle` onto [`WITH_RUNTIME_STACK`] for the current
/// thread for the duration of the closure (via a RAII guard so
/// panics still pop). [`close_store`] reads that stack to detect
/// the same-handle reentrant-call deadlock case.
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
    let _depth_guard = WithRuntimeGuard::enter(handle.0);
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
#[uniffi::export]
pub fn open_store(path: String, master_key_hex: String) -> FfiResult<RuntimeHandle> {
    crate::metrics::instrument(crate::metrics::inc_open_store, || {
        open_store_inner(path, master_key_hex)
    })
}

#[allow(clippy::needless_pass_by_value)] // Mirror of [`open_store`]: forwards the owned strings the FFI boundary handed to the outer wrapper.
fn open_store_inner(path: String, master_key_hex: String) -> FfiResult<RuntimeHandle> {
    let master_key = parse_master_key_hex(&master_key_hex)?;

    // Allocate and validate the handle *before* doing any expensive
    // SQLCipher work. `next_handle` is `AtomicU64::fetch_add`, so
    // every successful caller observes a unique value, but after
    // exactly `u64::MAX` allocations the counter wraps to 0 — the
    // `RuntimeHandle::NONE` sentinel every other FFI function treats
    // as "no handle". Reject that case up front so the wraparound
    // path short-circuits without opening the SQLCipher connection,
    // replaying tombstones, or running the FTS purge sweep (all O(N)
    // in stored scopes). The collision-against-existing-handle check
    // still has to run after we have the registry write lock, but
    // that lock is taken only once at the end of `open_store` so it
    // does not gate the long-running store-construction work.
    //
    // In practice the wraparound branch is unreachable (2^64 opens
    // per process), but the check is two instructions and pins the
    // invariant for the cost of one branch.
    let handle = RuntimeHandle(next_handle());
    if handle.0 == RuntimeHandle::NONE.0 {
        return Err(FfiError::Evidence {
            message: "runtime handle allocator wrapped to the reserved NONE sentinel".into(),
        });
    }

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

    let router_config = router_config_from_env();
    let inference_router = Arc::new(build_inference_router(router_config));
    // Spawn the adapter probe on a background thread so `open_store`
    // returns immediately even when an adapter's probe hits the
    // network (e.g. the `http-client`-backed llama.cpp adapter
    // pings `GET /health` with a multi-second timeout). FFI calls
    // that need the router (`trigger_synthesis`) join on
    // `wait_for_bootstrap` before dispatch.
    Arc::clone(&inference_router).spawn_bootstrap();

    // Build the per-runtime HTTP transport + OAuth2 client up front
    // so every connector on this runtime shares one reqwest
    // connection pool / TLS session cache / thread pool.
    // Constructing `BlockingHttpTransport` is non-trivial (it builds
    // a `reqwest::blocking::Client`, which spins up an internal
    // thread pool, initialises the TLS backend, and resolves the
    // platform CA bundle) and can legitimately fail on hosts where
    // the TLS provider is broken, the network stack is sandboxed
    // away, or the system entropy source is unreachable.
    //
    // Soft-fail: degrade ONLY the connector subsystem rather than
    // refusing to open the store entirely. Hosts that do not need
    // connectors (ingest-only test fixtures, offline / air-gapped
    // builds) stay fully functional, and connector calls surface
    // `FfiError::Unavailable { subsystem: "connector-http-client" }`
    // — the same path the `not(http-client)` build takes — so the
    // host sees one uniform recovery contract regardless of whether
    // the feature is off, the transport failed to build, or the
    // host hasn't called `create_connector` yet. Logged at WARN so
    // the host's tracing subscriber surfaces the failure for
    // diagnostics even though the open succeeds.
    //
    // ### Confidential-client `client_secret` wiring
    //
    // The `OAuth2Client::new(...)` constructor below intentionally
    // does NOT call `.with_client_secret(...)`. The framework
    // resolves the `client_secret` form field at every grant
    // through a three-layer fallback ladder (see
    // `connector_framework::oauth`'s module-level rustdoc):
    //
    // 1. Host-supplied
    //    [`OAuthClientSecretResolver`](crate::connector::OAuthClientSecretResolver)
    //    registered via
    //    [`set_oauth_client_secret_resolver`](crate::connector::set_oauth_client_secret_resolver).
    //    Production path — secret lives in the host's OS keychain
    //    and never persists in the substrate.
    // 2. `auth_config_json["client_secret"]` — fallback for
    //    tests / single-tenant CLI hosts. Secret persists encrypted
    //    under the scope DEK in SQLCipher (see the field's doc on
    //    `ConnectorConfig` for the deliberate-deviation warning).
    // 3. Static `OAuth2Client::with_client_secret` — legacy / unit-
    //    test convenience. Not used by this constructor.
    // 4. Field omitted entirely — public-client / PKCE-only flows.
    //
    // The same per-runtime `OAuth2Client` is shared by every
    // connector via `Arc<dyn OAuth2CodeExchange>` unsized
    // coercion, so registering a resolver once after `open_store`
    // applies to ALL connectors on the runtime — both for
    // `authenticate_connector`'s `exchange_code` grant and
    // `refresh_connector_token` / `sync_connector`'s
    // `refresh_token` grant.
    #[cfg(feature = "http-client")]
    let (http_transport, oauth_client) = match BlockingHttpTransport::new() {
        Ok(transport) => {
            let transport_arc: Arc<BlockingHttpTransport> = Arc::new(transport);
            let oauth = Arc::new(OAuth2Client::new(Arc::clone(&transport_arc)));
            (Some(transport_arc), Some(oauth))
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "open_store: BlockingHttpTransport construction failed; \
                 connector subsystem disabled for this runtime (connector calls \
                 will surface FfiError::Unavailable {{ subsystem: \"connector-http-client\" }})",
            );
            (None, None)
        }
    };

    let mut runtime = FfiRuntime {
        master_key,
        store,
        registry,
        user_memories,
        channel_memories,
        inference_router,
        connector_instances: HashMap::new(),
        connectors: HashMap::new(),
        token_vault: OAuth2TokenVault::new(),
        #[cfg(feature = "http-client")]
        http_transport,
        #[cfg(feature = "http-client")]
        oauth_client,
        webhook_servers: HashMap::new(),
        sync_scheduler: None,
    };

    // Rehydrate persisted connector state from the v9
    // `connector_instances` + `connector_tokens` tables. This walks
    // both tables, skips tombstoned scopes (whose AEAD payloads are
    // already unrecoverable from the destroyed scope DEK), and
    // rebuilds every `Arc<dyn Connector>` via the same factory the
    // create-time path uses. Rows that fail to decrypt or deserialise
    // are skipped with a `tracing::warn!` rather than blocking the
    // open — matching the user_memory / channel_memory rehydration
    // discipline above.
    //
    // The rehydrate call mutates the runtime's `connector_instances`
    // / `connectors` / `token_vault` maps, so it has to run before
    // the runtime is moved into the registry below. The mutation is
    // single-threaded here (no other handle reaches this runtime
    // until the registry insert lands), so we can simply call it on
    // the local `runtime` variable without acquiring any mutex.
    crate::connector::rehydrate_connectors(&mut runtime, &tombstones);

    // Capture the post-replay tombstone count before moving `runtime`
    // into the registry; we publish it only after the insert succeeds
    // (see below) so the gauge can never reflect a runtime that was
    // rejected by the collision check.
    let tombstones_after_replay = runtime.registry.tombstones().count() as u64;

    let mut guard = write_registry();
    // Allocation is monotonic via `NEXT`, so a collision against an
    // already-open handle would also mean we wrapped. Same reasoning
    // as the sentinel check on `handle` above — refuse rather than
    // silently overwrite an open runtime.
    //
    // The collision check is inside the write lock to ensure no other
    // `open_store` racing with this one can insert at the same key
    // between our `contains_key` test and our `insert`.
    if guard.contains_key(&handle.0) {
        return Err(FfiError::Evidence {
            message: format!("runtime handle {} collided during allocation", handle.0),
        });
    }
    guard.insert(handle.0, Arc::new(Mutex::new(runtime)));
    // Publish the open_handles + tombstone_count gauges inside the
    // write lock so both gauges are atomically consistent with the
    // registry mutation. The tombstone gauge mirrors the freshly-
    // replayed set so the health envelope reports the post-replay
    // count even before any FFI call has touched `forget`.
    crate::metrics::set_open_handles(guard.len() as u64);
    crate::metrics::set_tombstone_count(tombstones_after_replay);
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
///
/// # Reentrance
///
/// `close_store` **must not** be called from inside a [`with_runtime`]
/// closure for the *same* handle. The calling thread would be holding
/// one of the `Arc<Mutex<FfiRuntime>>` clones the spin loop is
/// waiting to drop, which would cause the loop to spin forever.
///
/// To make that misuse loud instead of silent, `close_store` checks
/// the per-thread [`WITH_RUNTIME_STACK`] that [`with_runtime`]
/// maintains via [`WithRuntimeGuard`]. If the stack contains the
/// *specific* handle being closed, `close_store` returns an explicit
/// [`FfiError::Evidence`] without removing the entry from the
/// registry — preserving the contract that *no* state changes when
/// the call fails.
///
/// Closing a *different* handle from inside `with_runtime` is
/// supported and behaves correctly: the calling thread's `Arc` clone
/// is for a different registry entry, so the per-handle reentrance
/// guard does not fire and the drain loop on the closed handle
/// terminates immediately.
#[uniffi::export]
pub fn close_store(handle: RuntimeHandle) -> FfiResult<()> {
    crate::metrics::instrument(crate::metrics::inc_close_store, || {
        // Reentrance guard: bail before any state change if this thread
        // is currently executing inside a `with_runtime` closure on
        // **this specific** handle. Removing the entry and then spinning
        // while the calling thread itself holds the outstanding clone
        // would produce a silent infinite loop — the explicit error
        // makes the misuse diagnosable. Cross-handle calls fall through
        // because the calling thread's clone is for a different
        // registry entry.
        if thread_holds_runtime_handle(handle.0) {
            return Err(FfiError::Evidence {
                message: "close_store called from within a with_runtime frame for the same handle on this thread; this would deadlock the synchronous-teardown spin loop".into(),
            });
        }
        let entry_opt = {
            let mut guard = write_registry();
            let removed = guard.remove(&handle.0);
            // Refresh the open-handles gauge to match the post-remove
            // size whether or not the handle was present. On the
            // "unknown handle" / idempotent path the gauge is
            // unchanged; on the real-close path it ticks down.
            crate::metrics::set_open_handles(guard.len() as u64);
            removed
        };
        let Some(mut entry) = entry_opt else {
            return Ok(());
        };
        // Pre-drain phase: synchronously shut down every webhook
        // receiver server attached to this runtime BEFORE the
        // `Arc::try_unwrap` spin loop. The dispatcher closures
        // running on each server's tokio runtime thread re-enter
        // the substrate through `with_runtime`, briefly cloning the
        // entry `Arc` (and so blocking the spin loop) every time a
        // webhook lands. Without this step a server that keeps
        // receiving webhooks during shutdown would turn the
        // try_unwrap loop into an unbounded busy-wait.
        //
        // We take the inner mutex briefly here — the registry write
        // lock has already released, so other in-flight FFI calls
        // can still run; they just block on the mutex for the
        // duration of the take. Once the webhook_servers map is
        // taken out of the runtime, the mutex releases and the
        // shutdown_and_join calls run UNLOCKED so they cannot
        // deadlock against in-flight dispatchers that are themselves
        // calling `with_runtime`. Joined threads' last act is to
        // drop their `Arc` clones of the entry, which lets the
        // try_unwrap loop below succeed.
        {
            let mut rt_guard = match entry.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let servers = std::mem::take(&mut rt_guard.webhook_servers);
            // Also take the sync scheduler out of its slot under
            // the same locked section so its worker thread —
            // which re-enters `with_runtime` on every tick — also
            // gets joined BEFORE the `Arc::try_unwrap` spin loop
            // below. Without this the scheduler would race the
            // unwrap exactly the way an undrained webhook server
            // would: each tick briefly clones the entry `Arc`,
            // bumping the strong count and pinning the unwrap at
            // `Err(returned)`.
            let scheduler = rt_guard.sync_scheduler.take();
            // Drop the runtime lock BEFORE joining the runtime
            // threads — the runtime mutex is what the joined
            // threads' dispatchers were trying to acquire.
            drop(rt_guard);
            crate::webhook::drain_all_servers(servers);
            crate::sync_scheduler::drain_scheduler(scheduler);
        }
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
    })
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

// ──────────────────────── Inference router ────────────────────────

/// Environment variable consulted for the llama.cpp loopback server
/// URL. The default targets the upstream `llama-server` listening on
/// `http://127.0.0.1:8081` (matches [`RouterConfig::default`]).
pub(crate) const ENV_SLM_SERVER_URL: &str = "KNOWLEDGE_SLM_SERVER_URL";

/// Environment variable consulted for the SLM model artifact path.
pub(crate) const ENV_SLM_MODEL_PATH: &str = "KNOWLEDGE_SLM_MODEL_PATH";

/// Environment variable consulted for the device tier
/// (`low` / `medium` / `high`).
pub(crate) const ENV_SLM_DEVICE_TIER: &str = "KNOWLEDGE_SLM_DEVICE_TIER";

/// Read a [`RouterConfig`] from the well-known `KNOWLEDGE_SLM_*`
/// environment variables, falling back to [`RouterConfig::default`]
/// for any variable that is absent or malformed.
///
/// Exposed at `pub(crate)` so the FFI surface can call it from
/// `open_store` and the unit tests can exercise the env-parsing
/// branches.
pub(crate) fn router_config_from_env() -> RouterConfig {
    use inference_router::DeviceTier;
    let mut cfg = RouterConfig::default();
    if let Ok(url) = std::env::var(ENV_SLM_SERVER_URL) {
        if !url.is_empty() {
            cfg.server_url = url;
        }
    }
    if let Ok(path) = std::env::var(ENV_SLM_MODEL_PATH) {
        if !path.is_empty() {
            cfg.model_path = path;
        }
    }
    if let Ok(tier) = std::env::var(ENV_SLM_DEVICE_TIER) {
        cfg.device_tier = match tier.to_ascii_lowercase().as_str() {
            "low" => DeviceTier::Low,
            "high" => DeviceTier::High,
            // Default and the explicit "medium" both land here so a
            // typo in the env var degrades to the documented default
            // rather than a silent device-tier downgrade.
            _ => DeviceTier::Medium,
        };
    }
    cfg
}

/// Build an [`InferenceRouter`] from `config`.
///
/// The router holds adapters in priority order:
///
/// 1. **MLX** — Apple Silicon native SLM. Returns `Unavailable`
///    until the iOS / macOS native shell calls
///    [`inference_router::adapters::mlx::set_mlx_runtime_linked`]
///    and registers a real generate callback at boot. Always
///    listed first so when the runtime *is* linked the router
///    prefers on-device hardware acceleration.
/// 2. **llama.cpp** — loopback HTTP server. Only constructed when
///    the `http-client` feature is enabled (the real
///    [`HttpLlamaServerClient`] is gated behind that feature so the
///    substrate's default `cargo build` stays free of network deps).
///    When the feature is off, the slot is skipped — the fallback
///    adapter then handles classification tasks and synthesis
///    surfaces as `Unavailable`.
/// 3. **Fallback** — encoder-only classifier. Always available;
///    serves classification tasks (`TagImportance`,
///    `ExtractEntities`, `PromoteObservation`) when MLX and
///    llama.cpp are both absent.
///
/// The returned router is *not yet bootstrapped* — callers must
/// invoke [`InferenceRouter::bootstrap`] (which `open_store` does)
/// before [`InferenceRouter::dispatch`].
pub(crate) fn build_inference_router(config: RouterConfig) -> InferenceRouter {
    let mut adapters: Vec<Box<dyn InferenceAdapter>> = Vec::with_capacity(3);
    adapters.push(Box::new(MlxAdapter::new(config.clone())));

    #[cfg(feature = "http-client")]
    {
        match inference_router::HttpLlamaServerClient::new(config.server_url.clone()) {
            Ok(client) => {
                adapters.push(Box::new(LlamaCppAdapter::new(
                    config.clone(),
                    Box::new(client),
                )));
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to construct HttpLlamaServerClient; llama.cpp adapter disabled",
                );
            }
        }
    }
    // Suppress unused-import warning for LlamaCppAdapter on builds
    // without the http-client feature — the adapter type is still
    // referenced via the slot above but only at conditional code.
    #[cfg(not(feature = "http-client"))]
    {
        let _ = std::marker::PhantomData::<LlamaCppAdapter>;
    }

    adapters.push(Box::new(FallbackAdapter::new()));
    InferenceRouter::new(config, adapters)
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

    /// Calling `close_store(H)` from inside a `with_runtime(H, ...)`
    /// frame must fail loudly with `FfiError::Evidence` rather than
    /// silently deadlocking in the `Arc::try_unwrap` spin loop.
    ///
    /// The reentrance detection is keyed on the thread-local
    /// `WITH_RUNTIME_STACK`, so we drive it directly here (without
    /// an actual open store) by pushing the same handle onto the
    /// stack and then calling `close_store` with it. The
    /// `Err(FfiError::Evidence)` path must trip *before* the
    /// registry lookup; otherwise an unknown handle would fast-path
    /// to `Ok(())` and mask the bug.
    #[test]
    fn close_store_rejects_same_handle_reentrant_call_on_same_thread() {
        // Synthesise a `with_runtime(H, ...)` frame on this thread
        // without having to hold an actual `Arc<Mutex<FfiRuntime>>`.
        // The per-handle stack is what `close_store` checks, not the
        // registry. Bind the guard to a regular name so we can drop
        // it explicitly later (an `_`-prefixed binding would be
        // dropped at end of scope, missing the post-drop assertion).
        let h = RuntimeHandle(u64::MAX);
        let guard = WithRuntimeGuard::enter(h.0);

        let err = close_store(h).unwrap_err();
        match err {
            FfiError::Evidence { ref message } => {
                assert!(
                    message.contains("close_store called from within a with_runtime frame"),
                    "expected reentrance error, got: {message}"
                );
                assert!(
                    message.contains("same handle"),
                    "expected same-handle qualifier in error message, got: {message}"
                );
            }
            other => panic!("expected FfiError::Evidence, got: {other:?}"),
        }

        // Sanity: after the guard drops, `close_store` on the same
        // unknown handle succeeds as a no-op (idempotent path).
        drop(guard);
        close_store(h).expect("non-reentrant close_store on unknown handle");
    }

    /// Closing a *different* handle from inside a `with_runtime(H1, ...)`
    /// frame is supported (the calling thread's `Arc` clone is for
    /// `H1`, not `H2`, so the drain loop on `H2` terminates
    /// immediately). The reentrance guard must NOT reject it.
    ///
    /// We pin this directly against the per-handle stack: enter a
    /// frame for `H1`, then call `close_store(H2)` and expect it to
    /// reach the registry lookup (and fast-path to `Ok(())` for the
    /// unknown handle) rather than failing with the reentrance
    /// error.
    #[test]
    fn close_store_allows_cross_handle_call_from_with_runtime_frame() {
        let h1 = RuntimeHandle(0xAAAA_AAAA_AAAA_AAAA);
        let h2 = RuntimeHandle(0xBBBB_BBBB_BBBB_BBBB);
        assert_ne!(h1.0, h2.0);

        let _frame = WithRuntimeGuard::enter(h1.0);
        // `H2` is not in the registry, so the call should reach the
        // `write_registry().remove(&h2.0)` branch and return Ok(())
        // because the entry is absent. The reentrance guard must not
        // fire because the thread is NOT inside a `with_runtime(H2, ...)`
        // frame.
        close_store(h2).expect("cross-handle close_store from within with_runtime frame");
    }

    /// The `WITH_RUNTIME_STACK` is thread-local: another thread
    /// entering and leaving a guard does not affect this thread's
    /// stack. A full multi-thread close_store test lives in
    /// `crates/ffi/tests/ffi_integration_tests.rs`; here we just
    /// pin the per-thread scoping directly.
    #[test]
    fn with_runtime_stack_is_thread_local() {
        let stack_empty = || WITH_RUNTIME_STACK.with(|s| s.borrow().is_empty());
        assert!(stack_empty(), "this thread's stack starts empty");
        let join = std::thread::spawn(|| {
            let _g = WithRuntimeGuard::enter(42);
            WITH_RUNTIME_STACK.with(|s| s.borrow().len())
        });
        let other_len = join.join().expect("thread join");
        assert_eq!(other_len, 1, "other thread observed depth 1 inside guard");
        assert!(
            stack_empty(),
            "this thread's stack still empty after the other thread"
        );
    }

    /// `WithRuntimeGuard` is LIFO. Nested guards on the same thread
    /// must pop in the reverse order they were pushed.
    #[test]
    fn with_runtime_stack_is_lifo() {
        let a = WithRuntimeGuard::enter(1);
        let b = WithRuntimeGuard::enter(2);
        let c = WithRuntimeGuard::enter(3);

        assert!(thread_holds_runtime_handle(1));
        assert!(thread_holds_runtime_handle(2));
        assert!(thread_holds_runtime_handle(3));
        assert!(!thread_holds_runtime_handle(4));

        drop(c);
        assert!(thread_holds_runtime_handle(1));
        assert!(thread_holds_runtime_handle(2));
        assert!(!thread_holds_runtime_handle(3));

        drop(b);
        assert!(thread_holds_runtime_handle(1));
        assert!(!thread_holds_runtime_handle(2));

        drop(a);
        assert!(!thread_holds_runtime_handle(1));
    }
}
