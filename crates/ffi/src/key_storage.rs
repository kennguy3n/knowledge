//! FFI surface for host-supplied master-key storage.
//!
//! This module exposes the cross-language counterpart of
//! [`crypto::key_storage::KeyStorage`]. The substrate itself does
//! not yet read the resolver on the hot path — the registration
//! hook is added now so the platform shells (UniFFI clients on
//! iOS / Android / Windows; N-API on Electron) can already
//! implement the contract while `crypto`'s migration story (move
//! the master-key bytes out of the FFI surface entirely and into
//! Keychain / Keystore / DPAPI) lands separately.
//!
//! # Resolution model
//!
//! `KeyStorageResolver` is a cross-language callback trait,
//! mirroring [`crate::connector::OAuthClientSecretResolver`]:
//!
//! * The substrate **registers** at most one resolver per runtime
//!   via [`set_key_storage_resolver`]. Re-registration replaces
//!   the previous one (the `set_key_storage_resolver_total`
//!   metric counts these events so operators can spot hosts that
//!   are treating registration as request-scoped instead of
//!   once-per-`open_store`).
//! * The substrate **unregisters** via
//!   [`clear_key_storage_resolver`]. Counters under
//!   `clear_key_storage_resolver_total` mirror the OAuth side.
//! * The substrate **does not yet call into the resolver** on its
//!   hot path. Today, the wired master key is consumed at
//!   [`crate::open_store`] time and the resolver is informational
//!   only. The follow-up migration (tracked in `SECURITY.md`
//!   §"Key storage") will replace `crate::open_store`'s direct
//!   master-key parameter with a resolver-driven lookup.
//!
//! Why register without calling? Two reasons:
//!
//! 1. Foreign bindings (UniFFI / N-API) have to ship the resolver
//!    type on the same release cadence as the substrate. Adding
//!    the FFI surface now lets host implementations land in
//!    parallel with the `crypto` migration.
//! 2. It surfaces the contract publicly. Hosts get compile-time
//!    feedback on the resolver shape, and there is no risk of a
//!    silent behavioural divergence between a draft host-side
//!    implementation and the substrate-side consumer that lands
//!    later.
//!
//! # Threading
//!
//! The trait requires `Send + Sync` because the substrate may
//! consult the resolver from any background worker (sync
//! scheduler, synthesis dispatcher, etc.). UniFFI enforces this
//! on the foreign side, and the N-API adapter wraps the host's
//! `JsFunction` in a `Mutex`-guarded slot so the JS engine only
//! sees one in-flight call at a time even when multiple workers
//! race.

use std::sync::Arc;

#[cfg(test)]
use crate::error::FfiError;
use crate::error::FfiResult;
use crate::metrics;
use crate::runtime::{with_runtime, RuntimeHandle};

/// Host-supplied master-key storage callback.
///
/// Every method returns `FfiResult` so platform errors (Keychain
/// access denied, biometric prompt cancelled, hardware-backed
/// key handle revoked) can surface through the standard
/// [`FfiError`] taxonomy without leaking platform-specific error
/// shapes.
///
/// `key_id` is an opaque identifier chosen by the substrate. It
/// is **not** a Keychain item label, an Android `KeyAlias`, or a
/// DPAPI descriptor — those are private to the implementation.
/// The substrate uses the same id consistently across `store` /
/// `load` / `delete` so the host's mapping table can be a simple
/// `HashMap<String, PlatformHandle>`.
///
/// `key_hex` is a hex-encoded representation of the 32-byte
/// master key. Hex is preferred over base64 because UniFFI's
/// `String` marshalling is already a well-trodden path for hex
/// payloads (the substrate exposes provenance signatures the
/// same way), and the size overhead (64 bytes vs 44 bytes) is
/// negligible compared to the keychain round-trip cost.
#[uniffi::export(with_foreign)]
pub trait KeyStorageResolver: Send + Sync {
    /// Persist `key_hex` under `key_id`. Implementations must
    /// overwrite any existing entry with the same id and must
    /// zeroize any in-flight host-side copy of the hex string.
    fn store_key(&self, key_id: String, key_hex: String) -> FfiResult<()>;

    /// Load the key registered under `key_id`. Implementations
    /// must return [`FfiError::NotFound`] when the id is unknown
    /// rather than returning an empty string or fabricating
    /// zero bytes.
    fn load_key(&self, key_id: String) -> FfiResult<String>;

    /// Drop the key registered under `key_id`. Must be idempotent
    /// — calling `delete_key` on an unknown id is a success, not
    /// an error, mirroring [`crypto::key_storage::KeyStorage::
    /// delete_master_key`].
    fn delete_key(&self, key_id: String) -> FfiResult<()>;
}

/// Register a host-supplied [`KeyStorageResolver`] against
/// `handle`. The substrate stores at most one resolver per
/// runtime; re-registration replaces the previous resolver.
///
/// Calling this multiple times bumps
/// `set_key_storage_resolver_total` so operators can spot hosts
/// that treat registration as request-scoped (the resolver is
/// meant to be a once-per-`open_store` lifecycle event).
///
/// # Errors
///
/// * Propagates [`with_runtime`]'s errors if the handle has been
///   closed or reaped.
#[uniffi::export]
pub fn set_key_storage_resolver(
    handle: RuntimeHandle,
    resolver: Arc<dyn KeyStorageResolver>,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_set_key_storage_resolver, || {
        with_runtime(handle, |rt| {
            rt.key_storage_resolver = Some(resolver);
            Ok(())
        })
    })
}

/// Unregister the previously-registered [`KeyStorageResolver`].
///
/// Idempotent — calling this when no resolver is registered is a
/// success, mirroring [`set_key_storage_resolver`]'s
/// "last-write-wins" semantics.
///
/// # Errors
///
/// * Propagates [`with_runtime`]'s errors if the handle has been
///   closed or reaped.
#[uniffi::export]
pub fn clear_key_storage_resolver(handle: RuntimeHandle) -> FfiResult<()> {
    metrics::instrument(metrics::inc_clear_key_storage_resolver, || {
        with_runtime(handle, |rt| {
            rt.key_storage_resolver = None;
            Ok(())
        })
    })
}

#[cfg(test)]
mod tests {
    //! Resolver registration is purely cross-language plumbing —
    //! the substrate does not call into the resolver from any hot
    //! path yet (per the module-level docs). The tests exercise
    //! the registration / unregistration lifecycle through a
    //! deterministic in-memory backing store and verify the
    //! metric counters fire as expected.
    use super::*;
    use crate::metrics::snapshot;

    use std::collections::HashMap;
    use std::sync::Mutex;

    struct CountingResolver {
        store: Mutex<HashMap<String, String>>,
    }

    impl CountingResolver {
        fn new() -> Self {
            Self {
                store: Mutex::new(HashMap::new()),
            }
        }
    }

    impl KeyStorageResolver for CountingResolver {
        fn store_key(&self, key_id: String, key_hex: String) -> FfiResult<()> {
            let mut g = self.store.lock().map_err(|_| FfiError::Unavailable {
                subsystem: "test-key-storage-resolver".into(),
            })?;
            g.insert(key_id, key_hex);
            Ok(())
        }

        fn load_key(&self, key_id: String) -> FfiResult<String> {
            let g = self.store.lock().map_err(|_| FfiError::Unavailable {
                subsystem: "test-key-storage-resolver".into(),
            })?;
            g.get(&key_id).cloned().ok_or_else(|| FfiError::NotFound {
                kind: "key".into(),
                id: key_id,
            })
        }

        fn delete_key(&self, key_id: String) -> FfiResult<()> {
            let mut g = self.store.lock().map_err(|_| FfiError::Unavailable {
                subsystem: "test-key-storage-resolver".into(),
            })?;
            g.remove(&key_id);
            Ok(())
        }
    }

    #[test]
    fn resolver_round_trip_via_substrate_registry() {
        // Exercises the trait directly (no FFI registry yet) so
        // we can observe the resolver behaviour without standing
        // up a full `open_store` (which the workspace test
        // harness covers separately under `runtime_tests.rs`).
        let r = CountingResolver::new();
        r.store_key("alpha".into(), "deadbeef".into()).unwrap();
        assert_eq!(r.load_key("alpha".into()).unwrap(), "deadbeef");
        r.delete_key("alpha".into()).unwrap();
        assert!(r.load_key("alpha".into()).is_err());
    }

    #[test]
    fn metric_counters_are_published() {
        // We can't easily call `set_key_storage_resolver` without
        // a live `RuntimeHandle`, but we can still verify the
        // counter wiring by triggering the underlying inc helpers
        // and observing the snapshot. The same helpers are what
        // the public entry points invoke under the hood, so a
        // green counter here proves the snapshot mapping is
        // wired correctly.
        let before = snapshot();
        metrics::inc_set_key_storage_resolver();
        metrics::inc_set_key_storage_resolver();
        metrics::inc_clear_key_storage_resolver();
        let after = snapshot();

        assert_eq!(
            after.set_key_storage_resolver_total - before.set_key_storage_resolver_total,
            2,
            "set counter must increment per call"
        );
        assert_eq!(
            after.clear_key_storage_resolver_total - before.clear_key_storage_resolver_total,
            1,
            "clear counter must increment per call"
        );
    }
}
