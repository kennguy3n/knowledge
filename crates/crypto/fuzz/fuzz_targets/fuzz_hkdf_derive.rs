//! Fuzz target: HKDF key derivation.
//!
//! Feeds random master keys and context labels through `derive_key`
//! and verifies:
//! 1. The function never panics for any 32-byte master key + arbitrary
//!    context.
//! 2. The derived key is always exactly `AEAD_KEY_LEN` (32) bytes.
//! 3. The derivation is deterministic (same inputs → same output).

#![no_main]

use libfuzzer_sys::fuzz_target;

use crypto::{derive_key, AEAD_KEY_LEN, MASTER_KEY_LEN};

fuzz_target!(|data: &[u8]| {
    if data.len() < MASTER_KEY_LEN {
        return;
    }

    let (key_bytes, context) = data.split_at(MASTER_KEY_LEN);
    let mut master_key = [0u8; MASTER_KEY_LEN];
    master_key.copy_from_slice(key_bytes);

    let derived = match derive_key(&master_key, context) {
        Ok(k) => k,
        // CryptoError is acceptable (e.g. if HKDF rejects), but
        // panics are not.
        Err(_) => return,
    };

    // The derived key must be exactly AEAD_KEY_LEN.
    assert_eq!(derived.len(), AEAD_KEY_LEN);

    // Determinism: re-deriving with the same inputs must produce the
    // same output.
    let derived2 = derive_key(&master_key, context)
        .expect("second derivation must succeed if first did");
    assert_eq!(derived, derived2, "HKDF derivation must be deterministic");
});
