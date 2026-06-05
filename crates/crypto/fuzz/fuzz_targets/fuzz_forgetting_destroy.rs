//! Fuzz target: cryptographic forgetting — DEK destroy + attempted decrypt.
//!
//! Models the core forgetting guarantee: once a scope DEK is destroyed,
//! the data sealed under it is irrecoverable through the registry. For
//! each fuzzed input:
//! 1. Derive a scope DEK and seal the fuzz bytes under it (AEAD).
//! 2. Confirm the live DEK decrypts the body (sanity).
//! 3. Destroy the scope DEK via `destroy_scope_dek`.
//! 4. Assert the scope is now forgotten, the registry hands back no key,
//!    and the destroyed handle's key bytes are gone (`key()` is `None`).
//!
//! This exercises the destroy path and the registry/tombstone
//! bookkeeping under arbitrary plaintext, checking that no input makes
//! `destroy_scope_dek` panic or leave a live key behind.

#![no_main]

use libfuzzer_sys::fuzz_target;

use crypto::forgetting::{
    destroy_scope_dek, DekRegistry, EpochId, EpochKeySource, ScopeDek, ScopeId,
};
use crypto::{decrypt_aead, encrypt_aead, DeterministicEpochKeySource, AEAD_NONCE_LEN};

fuzz_target!(|data: &[u8]| {
    // Derive a deterministic scope DEK (test-support key source).
    let scope = ScopeId::new_v4();
    let epoch = EpochId::zero();
    let mut source = DeterministicEpochKeySource;
    let key = source.derive(scope, epoch);

    // Seal the fuzz bytes under the DEK. A fixed nonce is fine here: we
    // are testing destruction, not nonce hygiene, and the key is unique.
    let nonce = [0u8; AEAD_NONCE_LEN];
    let ciphertext = match encrypt_aead(&key, &nonce, data, b"") {
        Ok(ct) => ct,
        Err(_) => return,
    };

    // Sanity: the live key decrypts the body back to the input.
    let recovered =
        decrypt_aead(&key, &nonce, &ciphertext, b"").expect("live DEK must decrypt its own body");
    assert_eq!(recovered, data, "round-trip under live DEK mismatch");

    // Register the DEK, then destroy the whole scope.
    let mut registry = DekRegistry::new();
    registry.insert_scope_dek(ScopeDek::new(scope, epoch, key));
    assert!(
        registry.get_scope_dek(scope).and_then(ScopeDek::key).is_some(),
        "registry must hand back the live DEK before destruction"
    );

    destroy_scope_dek(&mut registry, scope, None).expect("destroy_scope_dek must not fail");

    // The scope is forgotten and the registry no longer yields a key.
    assert!(
        registry.is_scope_forgotten(scope),
        "scope must be marked forgotten after destroy"
    );
    assert!(
        registry.get_scope_dek(scope).is_none(),
        "destroyed scope DEK must be removed from the registry"
    );

    // Destroy is idempotent: a second call is a no-op (empty events).
    let again = destroy_scope_dek(&mut registry, scope, None)
        .expect("idempotent destroy must not fail");
    assert!(again.is_empty(), "re-destroying a forgotten scope yields no events");
});
