//! Fuzz target: hybrid KEM encapsulate → decapsulate round-trip.
//!
//! Uses a single thread-local hybrid keypair (X25519 + ML-KEM-768) and,
//! for each fuzzed input:
//! 1. Encapsulates a fresh shared secret to the public key.
//! 2. Decapsulates with the secret key and asserts the secret matches
//!    (round-trip correctness).
//! 3. Uses the fuzz bytes to flip a byte in the ciphertext (either the
//!    X25519 ephemeral public key or the ML-KEM-768 ciphertext) and
//!    asserts decapsulation never panics and never reproduces the
//!    original shared secret — i.e. a mauled ciphertext cannot yield the
//!    real key (ML-KEM-768 implicit rejection / a different X25519 DH).

#![no_main]

use libfuzzer_sys::fuzz_target;

use crypto::hybrid_kem::{
    hybrid_kem_decap, hybrid_kem_encap, hybrid_keypair, HybridPublicKey, HybridSecretKey,
};

use std::cell::RefCell;

thread_local! {
    static KEYPAIR: RefCell<(HybridPublicKey, HybridSecretKey)> =
        RefCell::new(hybrid_keypair().expect("hybrid keypair generation must succeed"));
}

fuzz_target!(|data: &[u8]| {
    KEYPAIR.with(|cell| {
        let pair = cell.borrow();
        let (pk, sk) = (&pair.0, &pair.1);

        // Encapsulate a fresh secret. Must succeed for a valid key.
        let (shared, ciphertext) =
            hybrid_kem_encap(pk).expect("encap must succeed for a valid public key");

        // Honest decapsulation recovers the same secret.
        let recovered =
            hybrid_kem_decap(sk, &ciphertext).expect("decap must succeed on honest ciphertext");
        assert_eq!(shared, recovered, "hybrid KEM round-trip mismatch");

        // Tamper: use the first fuzz byte to choose which half of the
        // ciphertext to maul, and the rest to choose an offset/mask.
        if data.is_empty() {
            return;
        }
        let mut mauled = ciphertext.clone();
        let mask = data.get(1).copied().unwrap_or(0x01) | 0x01;
        if data[0] & 1 == 0 {
            // Flip a byte in the X25519 ephemeral public key.
            let idx = data.get(2).copied().unwrap_or(0) as usize % mauled.x25519_eph_pub.len();
            mauled.x25519_eph_pub[idx] ^= mask;
        } else {
            // Flip a byte in the ML-KEM-768 ciphertext (fixed-length,
            // always non-empty).
            let idx = data.get(2).copied().unwrap_or(0) as usize % mauled.mlkem768_ct.len();
            mauled.mlkem768_ct[idx] ^= mask;
        }

        // Decap of a mauled ciphertext must not panic and must not
        // reproduce the honest shared secret.
        if let Ok(bad) = hybrid_kem_decap(sk, &mauled) {
            assert_ne!(
                bad, shared,
                "mauled hybrid ciphertext must not recover the honest shared secret"
            );
        }
    });
});
