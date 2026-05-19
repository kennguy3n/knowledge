//! Process-global FFI runtime singleton.
//!
//! The UniFFI / N-API surface is shaped like a flat C-style API: each
//! function takes plain data and returns plain data. The Rust core,
//! however, owns stateful objects (an open SQLCipher database, a
//! `DekRegistry` of destroyed keys, a master key). This module mediates
//! between those two worlds by holding a process-global
//! `OnceLock<Mutex<Option<FfiRuntime>>>`.
//!
//! The runtime is initialized exactly once via [`open_store`] and torn
//! down via [`close_store`]. Subsequent calls to [`open_store`] without
//! an intervening [`close_store`] return [`FfiError::Evidence`] so tests
//! and host shutdown sequences cannot accidentally clobber an open
//! store.
//!
//! Concurrency: every public FFI function acquires the singleton's
//! `Mutex` for the duration of one call. The substrate's per-call
//! workloads are short (single SQL statement, single AEAD seal) so
//! coarse-grained locking is acceptable. If a future update needs
//! finer-grained concurrency (e.g. multi-shard ingest), this is the
//! single seam to replace.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use crypto::forgetting::{self, DekRegistry};
use crypto::MasterKey;
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};
use memory_manager::{ChannelMemoryObject, UserMemoryObject};
use zeroize::Zeroize;

use crate::error::{FfiError, FfiResult};

/// In-memory runtime state carried across FFI calls.
///
/// Holds the open [`EvidenceStore`], the per-user [`MasterKey`] (used
/// to derive [`encrypt`](crate::encrypt) / [`decrypt`](crate::decrypt)
/// keys and to seed [`DekRegistry`] entries lazily on ingest), and the
/// destroyed-key registry that backs [`forget`](crate::forget).
///
/// The struct is intentionally not `Clone` — there must be exactly one
/// per process. Tests reset this by calling
/// [`close_store`](crate::close_store) before re-opening.
pub struct FfiRuntime {
    pub(crate) master_key: MasterKey,
    pub(crate) store: EvidenceStore,
    pub(crate) registry: DekRegistry,
    /// Per-scope user-memory CRUD layer. Kept in process memory
    /// only — persistence to the encrypted evidence plane is not yet
    /// wired.
    pub(crate) user_memories: HashMap<ScopeId, UserMemoryObject>,
    /// Per-scope channel-memory recap home. Also kept in process
    /// memory only.
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
    /// empty `UserMemoryObject` in the per-process map.
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
}

fn singleton() -> &'static Mutex<Option<FfiRuntime>> {
    static RUNTIME: OnceLock<Mutex<Option<FfiRuntime>>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(None))
}

fn lock_runtime() -> MutexGuard<'static, Option<FfiRuntime>> {
    // A poisoned mutex means a previous panic happened inside an FFI
    // call. We recover the inner state so the host can keep
    // functioning rather than propagating the poison.
    singleton()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` against the singleton runtime, returning
/// [`FfiError::Unavailable`] if the store has not been opened.
pub(crate) fn with_runtime<F, T>(f: F) -> FfiResult<T>
where
    F: FnOnce(&mut FfiRuntime) -> FfiResult<T>,
{
    let mut guard = lock_runtime();
    let rt = guard.as_mut().ok_or_else(|| FfiError::Unavailable {
        subsystem: "evidence_store".into(),
    })?;
    f(rt)
}

/// Open the SQLCipher-backed evidence store at `path` using the
/// 32-byte master key encoded as `master_key_hex` (64 lower-case hex
/// chars). Must be called before any other wired FFI function.
///
/// Calling this twice without an intervening [`close_store`] is a
/// programming error: the second call returns [`FfiError::Evidence`]
/// rather than silently replacing the open store.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `master_key_hex` is not exactly 64
///   hex characters.
/// * [`FfiError::Evidence`] if a store is already open or if SQLCipher
///   fails to open the underlying database.
pub fn open_store(path: String, master_key_hex: String) -> FfiResult<()> {
    let master_key = parse_master_key_hex(&master_key_hex)?;
    let mut guard = lock_runtime();
    if guard.is_some() {
        return Err(FfiError::Evidence {
            message: "evidence store is already open; call close_store first".into(),
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
    let tombstones = store
        .load_forgotten_scopes()
        .map_err(|e| FfiError::Evidence {
            message: e.to_string(),
        })?;
    for scope in &tombstones {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        // The return value is the list of `KeyDestructionEvent`s
        // produced by the destroy call; we intentionally drop it
        // here. The destruction itself is what re-establishes
        // the registry invariant, and audit-trail emission for
        // *re-loaded* tombstones is not required by the spec —
        // each tombstone was already audited on its original
        // forget() call.
        let _ = forgetting::destroy_scope_dek(&mut registry, registry_scope);
    }

    // Follow-up — re-purge the FTS5 / embedding
    // secondary indexes for every replayed tombstone.
    //
    // `forget()` performs three steps in order: (1) destroy the
    // in-memory DEK, (2) persist the tombstone via
    // `record_forgotten_scope`, (3) purge the FTS / embedding rows
    // via `purge_fts_for_scope`. If the process crashes between
    // steps 2 and 3, the tombstone survives but the plaintext-
    // derived FTS terms persist on disk indefinitely — accessible
    // via raw SQLite without the per-scope DEK and so escaping the
    // cryptographic-forgetting contract. Re-running the purge on
    // every `open_store` closes that window. The purge is
    // idempotent: dropping already-deleted FTS rows is a no-op.
    //
    // A purge failure here surfaces as an `Evidence` error rather
    // than being swallowed — mirroring the `forget()` path's
    // error handling and matching what `record_forgotten_scope`
    // already does for the same conditions. A host that hits this
    // path on startup has a corrupt or unreadable secondary index
    // and needs to know about it.
    for scope in &tombstones {
        store
            .purge_fts_for_scope(*scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        store
            .purge_body_key_wraps_for_scope(*scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
    }

    // Load independently-generated scope DEKs from the `scope_deks`
    // table (v6 schema). Each persisted DEK is registered in the
    // in-memory `DekRegistry` so the encrypt / decrypt paths find
    // the scope key without re-deriving. Tombstoned scopes are
    // skipped — their DEK rows should already have been deleted by
    // `forget()`, but the filter is defense-in-depth.
    let scope_deks = store
        .load_scope_deks()
        .map_err(|e| FfiError::Evidence {
            message: e.to_string(),
        })?;
    for (scope, key) in &scope_deks {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        if registry.is_scope_forgotten(registry_scope) {
            continue;
        }
        let dek =
            forgetting::ScopeDek::new(registry_scope, forgetting::EpochId::zero(), *key);
        registry.insert_scope_dek(dek);
    }

    *guard = Some(FfiRuntime {
        master_key,
        store,
        registry,
        user_memories: HashMap::new(),
        channel_memories: HashMap::new(),
    });
    Ok(())
}

/// Drop the open evidence store and zeroize the master key.
///
/// Idempotent: closing an already-closed store is a no-op and returns
/// `Ok(())`. The host can therefore call this in a `try`/`finally`
/// shutdown handler without first probing the runtime state.
pub fn close_store() -> FfiResult<()> {
    let mut guard = lock_runtime();
    *guard = None;
    Ok(())
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
}
