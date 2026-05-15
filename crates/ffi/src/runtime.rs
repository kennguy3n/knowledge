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
//! coarse-grained locking is acceptable. If a future phase needs
//! finer-grained concurrency (e.g. multi-shard ingest), this is the
//! single seam to replace.

use std::sync::{Mutex, MutexGuard, OnceLock};

use crypto::forgetting::{self, DekRegistry};
use crypto::MasterKey;
use evidence_store::{EvidenceStore, EvidenceStoreConfig};
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
    pub(crate) fn master_key(&self) -> &MasterKey {
        &self.master_key
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
    let store =
        EvidenceStore::open(&path, &master_key, EvidenceStoreConfig::default()).map_err(|e| {
            FfiError::Evidence {
                message: e.to_string(),
            }
        })?;

    // Phase A.5 Gap 4 — durable cryptographic-forgetting tombstones.
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
    for scope in tombstones {
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

    *guard = Some(FfiRuntime {
        master_key,
        store,
        registry,
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
