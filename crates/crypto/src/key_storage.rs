//! Hardware-backed master-key storage trait.
//!
//! The substrate ultimately treats the per-user master key (see
//! [`crate::MasterKey`]) as opaque bytes — derive-only — but the
//! *storage* of that key is platform-specific:
//!
//! * iOS / macOS: Keychain (`SecItemAdd` with
//!   `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`)
//! * Android: Keystore (`AndroidKeyStore`) / StrongBox-backed
//!   key material on Pixel 6+
//! * Windows: DPAPI (`CryptProtectData` / `CryptUnprotectData`)
//!   or, on Windows 11 with Pluton, the TPM via
//!   `NCryptOpenStorageProvider`
//! * Linux: libsecret (`SecretService`) when present, otherwise
//!   a host-supplied fallback
//! * Server / TEE: Nitro / SEV-SNP sealed memory
//!
//! Each platform integration owns its own attestation surface, key
//! handle lifetime, and biometric/PIN gate semantics — none of
//! which `crypto` can encode generically. The
//! [`KeyStorage`] trait is therefore the boundary:
//!
//! * `crypto` owns the byte-level representation of [`MasterKey`]
//!   and the zeroize discipline on its own copies.
//! * Host shells (FFI / N-API addon) implement [`KeyStorage`]
//!   against their platform secure store and pass the
//!   implementation into the substrate at startup.
//! * The [`InMemoryKeyStorage`] reference implementation in this
//!   module is the one and only test-grade backend; it is **not**
//!   suitable for production use because the key material lives in
//!   process heap (and so is exposed to any process-level memory
//!   disclosure bug, never the platform's secure enclave).
//!
//! Platform integrations should be registered via FFI callback by
//! the host shell — see `crates/ffi/src/key_storage.rs`'s
//! `KeyStorageResolver` for the cross-language contract.

use std::collections::HashMap;
use std::sync::Mutex;

use zeroize::Zeroize;

use crate::errors::CryptoError;
use crate::kdf::MasterKey;
#[cfg(test)]
use crate::kdf::MASTER_KEY_LEN;

/// Host-supplied backing store for per-user [`MasterKey`] material.
///
/// All three methods are blocking — they assume the underlying
/// platform call is synchronous, which is the case for every
/// keystore listed in the module docs. Implementations that fan
/// out to an async runtime must own that bridging internally.
///
/// `key_id` is an opaque identifier chosen by the substrate; it
/// is **not** a Keychain item label or an Android alias, those
/// are private to the implementation.
pub trait KeyStorage: Send + Sync {
    /// Persist `key` under `key_id`.
    ///
    /// Implementations must overwrite any existing entry under the
    /// same id (no silent collision behaviour). The on-wire copy
    /// of `key` is the caller's responsibility to zeroize; the
    /// storage backend must zeroize any in-flight copy it makes
    /// internally.
    fn store_master_key(&self, key_id: &str, key: &MasterKey) -> Result<(), CryptoError>;

    /// Load the master key registered under `key_id`.
    ///
    /// Returns [`CryptoError::TombstonePersistence`] (re-used as
    /// the generic "host storage I/O failed" variant — `crypto`
    /// does not own a richer error taxonomy and adding one now
    /// would be a breaking change for every downstream crate) if
    /// the key is missing or the host store rejected the read.
    fn load_master_key(&self, key_id: &str) -> Result<MasterKey, CryptoError>;

    /// Drop the master key registered under `key_id`.
    ///
    /// Idempotent — calling `delete_master_key` on an unknown id
    /// must succeed silently, mirroring the FFI contract where
    /// the host shell may have already evicted the entry. Backends
    /// must zeroize the in-memory copy *and* invoke whatever
    /// platform-level shred is appropriate (Keychain delete,
    /// `NCryptDeleteKey`, etc.).
    fn delete_master_key(&self, key_id: &str) -> Result<(), CryptoError>;
}

/// Reference [`KeyStorage`] implementation backed by an in-process
/// `HashMap` guarded by a `Mutex`. Intended for tests and as the
/// reference for what a platform backend must guarantee — **not**
/// for production use, because heap-resident keys are exposed to
/// process-level memory disclosure bugs.
///
/// Drop discipline: every value held in the inner map is zeroized
/// on `delete_master_key`, and the entire map is zeroized when
/// [`InMemoryKeyStorage`] is dropped. The substrate never relies
/// on observed bytes after `delete`, but the zeroize is part of
/// the trait contract and so the reference implementation honours
/// it explicitly.
#[derive(Default)]
pub struct InMemoryKeyStorage {
    keys: Mutex<HashMap<String, MasterKey>>,
}

impl InMemoryKeyStorage {
    /// Allocate an empty [`InMemoryKeyStorage`].
    pub fn new() -> Self {
        Self {
            keys: Mutex::new(HashMap::new()),
        }
    }

    /// Number of keys currently held. Test-only convenience —
    /// production code paths never need to introspect the size of
    /// the host store.
    pub fn len(&self) -> usize {
        // Poisoned mutex => treat as empty; the alternative
        // (panicking) would propagate a poisoning event into
        // arbitrary call sites.
        self.keys.lock().map_or(0, |g| g.len())
    }

    /// True iff [`Self::len`] is zero.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl KeyStorage for InMemoryKeyStorage {
    fn store_master_key(&self, key_id: &str, key: &MasterKey) -> Result<(), CryptoError> {
        let mut guard = self.keys.lock().map_err(|_| {
            CryptoError::TombstonePersistence(
                "in-memory key storage mutex poisoned during store".to_string(),
            )
        })?;
        // Overwrite-on-collision: zeroize any pre-existing entry
        // so the old byte pattern is not left lingering in the
        // map's value slot.
        if let Some(prev) = guard.insert(key_id.to_owned(), *key) {
            let mut prev = prev;
            prev.zeroize();
        }
        Ok(())
    }

    fn load_master_key(&self, key_id: &str) -> Result<MasterKey, CryptoError> {
        let guard = self.keys.lock().map_err(|_| {
            CryptoError::TombstonePersistence(
                "in-memory key storage mutex poisoned during load".to_string(),
            )
        })?;
        guard.get(key_id).copied().ok_or_else(|| {
            CryptoError::TombstonePersistence(format!(
                "in-memory key storage: no key registered under id {key_id:?}"
            ))
        })
    }

    fn delete_master_key(&self, key_id: &str) -> Result<(), CryptoError> {
        let mut guard = self.keys.lock().map_err(|_| {
            CryptoError::TombstonePersistence(
                "in-memory key storage mutex poisoned during delete".to_string(),
            )
        })?;
        if let Some(mut victim) = guard.remove(key_id) {
            victim.zeroize();
        }
        // Idempotent: deleting an unknown id is a success.
        Ok(())
    }
}

impl Drop for InMemoryKeyStorage {
    fn drop(&mut self) {
        // Zeroize every value before the `HashMap` is freed, so the
        // master-key bytes do not linger in heap memory after the
        // store goes out of scope. We do not own the keys (the
        // ids are arbitrary `String`s with no secret content) so
        // only the values are zeroized.
        if let Ok(mut guard) = self.keys.lock() {
            for (_id, value) in guard.iter_mut() {
                value.zeroize();
            }
            guard.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_key(seed: u8) -> MasterKey {
        let mut k = [0u8; MASTER_KEY_LEN];
        for (i, slot) in k.iter_mut().enumerate() {
            *slot = seed.wrapping_add(u8::try_from(i & 0xFF).unwrap_or(0));
        }
        k
    }

    #[test]
    fn store_then_load_round_trips_the_key() {
        let store = InMemoryKeyStorage::new();
        let key = fixture_key(0x11);
        store.store_master_key("user-1", &key).unwrap();
        assert_eq!(store.len(), 1);
        let loaded = store.load_master_key("user-1").unwrap();
        assert_eq!(loaded, key, "load must return exactly the stored bytes");
    }

    #[test]
    fn load_unknown_id_errors() {
        let store = InMemoryKeyStorage::new();
        let err = store.load_master_key("ghost").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ghost"),
            "error must mention the missing key id; got {msg:?}"
        );
    }

    #[test]
    fn delete_is_idempotent_for_unknown_ids() {
        let store = InMemoryKeyStorage::new();
        store
            .delete_master_key("never-existed")
            .expect("delete of unknown id is a no-op success");
        assert!(store.is_empty());
    }

    #[test]
    fn overwrite_zeroizes_the_previous_value_slot() {
        // We can't directly observe the freed bytes, but we can
        // observe that `store` overwrites cleanly: the second
        // `load` returns the new value, not the old one.
        let store = InMemoryKeyStorage::new();
        let a = fixture_key(0x22);
        let b = fixture_key(0x33);
        store.store_master_key("user-2", &a).unwrap();
        store.store_master_key("user-2", &b).unwrap();
        assert_eq!(store.load_master_key("user-2").unwrap(), b);
        assert_eq!(store.len(), 1, "overwrite must not duplicate the entry");
    }

    #[test]
    fn delete_removes_the_entry() {
        let store = InMemoryKeyStorage::new();
        let key = fixture_key(0x44);
        store.store_master_key("user-3", &key).unwrap();
        store.delete_master_key("user-3").unwrap();
        assert!(store.is_empty());
        assert!(store.load_master_key("user-3").is_err());
    }

    #[test]
    fn concurrent_callers_can_share_the_store_via_arc() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(InMemoryKeyStorage::new());
        let mut handles = Vec::new();
        for tid in 0..8u8 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                let id = format!("user-{tid}");
                let key = fixture_key(tid);
                store.store_master_key(&id, &key).unwrap();
                let loaded = store.load_master_key(&id).unwrap();
                assert_eq!(loaded, key);
            }));
        }
        for h in handles {
            h.join().expect("worker join");
        }
        assert_eq!(store.len(), 8);
    }
}
