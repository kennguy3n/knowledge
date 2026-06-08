//! ML-KEM-768 Known Answer Test (KAT) vectors.
//!
//! The `ml-kem 0.2` RustCrypto crate does not ship NIST KAT vectors in
//! its public API, and embedding the full NIST `.rsp` files
//! (hundreds of KiB each) would add significant maintenance burden for
//! a downstream consumer crate. Instead, this file exercises
//! **round-trip consistency tests** that verify the ML-KEM-768 backend
//! produces self-consistent keypairs and that the hybrid KEM combiner
//! is deterministic for a given (X25519 DH, ML-KEM shared secret)
//! pair.
//!
//! If/when upstream publishes a `kat` feature or test vectors become
//! available as a standalone crate, replace these with true KAT
//! assertions.

use crypto::kem::{KemBackend, MlKem768Backend};
use crypto::{hybrid_kem_decap, hybrid_kem_encap, hybrid_keypair, AEAD_KEY_LEN};

// -----------------------------------------------------------------------
// 1. ML-KEM-768 round-trip consistency
// -----------------------------------------------------------------------

#[test]
fn mlkem768_keypair_roundtrip_consistent() {
    let backend = MlKem768Backend;
    // Generate multiple keypairs and verify encap → decap agreement.
    for _ in 0..10 {
        let (pk, sk) = backend.keypair().expect("keypair");
        let (ss_enc, ct) = backend.encap(&pk).expect("encap");
        let ss_dec = backend.decap(&sk, &ct).expect("decap");
        assert_eq!(ss_enc, ss_dec, "encap/decap shared secrets must agree");
        assert_eq!(ss_enc.len(), 32, "ML-KEM-768 shared secret is 32 bytes");
    }
}

#[test]
fn mlkem768_distinct_keypairs_distinct_secrets() {
    let backend = MlKem768Backend;
    let (pk_a, _sk_a) = backend.keypair().expect("keypair a");
    let (pk_b, _sk_b) = backend.keypair().expect("keypair b");
    let (ss_a, _ct_a) = backend.encap(&pk_a).expect("encap a");
    let (ss_b, _ct_b) = backend.encap(&pk_b).expect("encap b");
    assert_ne!(
        ss_a, ss_b,
        "distinct public keys should produce distinct shared secrets"
    );
}

#[test]
fn mlkem768_wrong_secret_key_fails() {
    let backend = MlKem768Backend;
    let (pk, _sk) = backend.keypair().expect("keypair");
    let (_pk2, sk2) = backend.keypair().expect("keypair 2");
    let (ss_send, ct) = backend.encap(&pk).expect("encap");

    // Decapsulating with the wrong secret key must not recover the
    // same shared secret (ML-KEM's implicit rejection returns a
    // pseudorandom value rather than an error).
    let ss_wrong = backend
        .decap(&sk2, &ct)
        .expect("implicit rejection returns Ok");
    assert_ne!(
        ss_send, ss_wrong,
        "wrong SK must not recover the correct shared secret"
    );
}

// -----------------------------------------------------------------------
// 2. Hybrid KEM combiner consistency
// -----------------------------------------------------------------------

#[test]
fn hybrid_kem_roundtrip_multiple_iterations() {
    // Run 20 iterations to exercise the OS RNG across multiple keypairs.
    for _ in 0..20 {
        let (pk, sk) = hybrid_keypair().expect("keypair");
        let (ss_send, ct) = hybrid_kem_encap(&pk).expect("encap");
        let ss_recv = hybrid_kem_decap(&sk, &ct).expect("decap");
        assert_eq!(ss_send, ss_recv);
        assert_eq!(ss_send.len(), AEAD_KEY_LEN);
    }
}

#[test]
fn hybrid_shared_secret_is_32_bytes() {
    let (pk, _sk) = hybrid_keypair().expect("keypair");
    let (ss, _ct) = hybrid_kem_encap(&pk).expect("encap");
    assert_eq!(ss.len(), 32);
}
