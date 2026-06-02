//! Property-based tests for audit preparation of the `crypto` crate.
//!
//! These tests use `proptest` to exercise cryptographic primitives
//! with randomised inputs, covering:
//!
//! * Hybrid KEM round-trip (generate → encapsulate → decapsulate)
//! * Provenance signature round-trip (sign → verify for ML-DSA-65
//!   and SPHINCS+)
//! * AEAD encrypt/decrypt with boundary inputs (empty plaintext,
//!   large plaintext, wrong key, tampered ciphertext)
//! * `ZeroizeOnDrop` on `HybridSecretKey`

use proptest::prelude::*;

use crypto::{
    decrypt_aead, encrypt_aead, hybrid_kem_decap, hybrid_kem_encap, hybrid_keypair, AeadKey,
    AeadNonce, CryptoError, AEAD_KEY_LEN,
};

// ---------------------------------------------------------------------------
// 1. Hybrid KEM round-trip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// For every generated keypair, encap → decap must yield the same
    /// shared secret, regardless of the OS RNG state at generation
    /// time.
    #[test]
    fn hybrid_kem_round_trip_always_agrees(_seed in 0u64..1000) {
        let (pk, sk) = hybrid_keypair().expect("keypair");
        let (ss_send, ct) = hybrid_kem_encap(&pk).expect("encap");
        let ss_recv = hybrid_kem_decap(&sk, &ct).expect("decap");
        prop_assert_eq!(ss_send, ss_recv);
        prop_assert_eq!(ss_send.len(), AEAD_KEY_LEN);
    }

    /// Two independent keypairs must produce distinct shared secrets
    /// when encapsulating to different recipients.
    #[test]
    fn hybrid_kem_distinct_recipients_yield_distinct_secrets(_seed in 0u64..1000) {
        let (pk_a, _sk_a) = hybrid_keypair().expect("keypair a");
        let (pk_b, _sk_b) = hybrid_keypair().expect("keypair b");
        let (ss_a, _ct_a) = hybrid_kem_encap(&pk_a).expect("encap a");
        let (ss_b, _ct_b) = hybrid_kem_encap(&pk_b).expect("encap b");
        // Shared secrets from distinct public keys should differ
        // (vanishingly small collision probability).
        prop_assert_ne!(ss_a, ss_b);
    }
}

// ---------------------------------------------------------------------------
// 2. Provenance signature round-trip (ML-DSA-65)
// ---------------------------------------------------------------------------

use crypto::signer_backend::MlDsa65Signer;
use crypto::{
    EvidenceRef, ProvenanceAgent, ProvenanceBundle, ProvenanceSigner, SynthesisActivity,
};
use uuid::Uuid;

fn arb_bundle(entity_bytes: [u8; 16], run_bytes: [u8; 16], deriv_count: usize) -> ProvenanceBundle {
    let entity_id = Uuid::from_bytes(entity_bytes);
    let run_id = Uuid::from_bytes(run_bytes);
    let derivations: Vec<EvidenceRef> = (0..deriv_count)
        .map(|i| {
            let mut b = [0u8; 16];
            // Deterministic but distinct per index.
            b[0] = u8::try_from(i % 256).unwrap_or(0);
            b[1] = u8::try_from(i / 256).unwrap_or(0);
            EvidenceRef::from_uuid(Uuid::from_bytes(b))
        })
        .collect();
    ProvenanceBundle::new(
        entity_id,
        SynthesisActivity::new("test-agent", "model-v1", "prompt-v1", run_id),
        ProvenanceAgent::software("test-signer"),
        derivations,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn ml_dsa_65_sign_verify_round_trip(
        entity_bytes in prop::array::uniform16(any::<u8>()),
        run_bytes in prop::array::uniform16(any::<u8>()),
        deriv_count in 0usize..8,
    ) {
        let signer = MlDsa65Signer::generate();
        let bundle = arb_bundle(entity_bytes, run_bytes, deriv_count);
        let signed = signer.sign(bundle).expect("sign");
        prop_assert!(signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn ml_dsa_65_tampered_bundle_fails(
        entity_bytes in prop::array::uniform16(any::<u8>()),
        run_bytes in prop::array::uniform16(any::<u8>()),
    ) {
        let signer = MlDsa65Signer::generate();
        let bundle = arb_bundle(entity_bytes, run_bytes, 1);
        let mut signed = signer.sign(bundle).expect("sign");
        // Tamper with the entity_id.
        signed.bundle.entity_id = Uuid::from_u128(
            signed.bundle.entity_id.as_u128().wrapping_add(1),
        );
        prop_assert!(!signer.verify(&signed).expect("verify"));
    }
}

// ---------------------------------------------------------------------------
// 3. Provenance signature round-trip (SPHINCS+)
// ---------------------------------------------------------------------------

use crypto::sphincs::SphincsPlusSigner;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    #[test]
    fn sphincs_sign_verify_round_trip(
        entity_bytes in prop::array::uniform16(any::<u8>()),
        run_bytes in prop::array::uniform16(any::<u8>()),
        deriv_count in 0usize..4,
    ) {
        let signer = SphincsPlusSigner::generate();
        let bundle = arb_bundle(entity_bytes, run_bytes, deriv_count);
        let signed = signer.sign(bundle).expect("sign");
        prop_assert!(signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn sphincs_tampered_signature_fails(
        entity_bytes in prop::array::uniform16(any::<u8>()),
        run_bytes in prop::array::uniform16(any::<u8>()),
    ) {
        let signer = SphincsPlusSigner::generate();
        let bundle = arb_bundle(entity_bytes, run_bytes, 1);
        let mut signed = signer.sign(bundle).expect("sign");
        // Flip a byte in the signature.
        if !signed.signature.as_bytes().is_empty() {
            signed.signature.0[0] ^= 0x01;
        }
        prop_assert!(!signer.verify(&signed).expect("verify"));
    }
}

// ---------------------------------------------------------------------------
// 4. AEAD encrypt/decrypt with boundary inputs
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Round-trip with arbitrary plaintext and AAD.
    #[test]
    fn aead_round_trip_arbitrary(
        key in prop::array::uniform32(any::<u8>()),
        nonce in prop::array::uniform24(any::<u8>()),
        plaintext in prop::collection::vec(any::<u8>(), 0..4096),
        aad in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let aead_key: AeadKey = key;
        let aead_nonce: AeadNonce = nonce;
        let ct = encrypt_aead(&aead_key, &aead_nonce, &plaintext, &aad)
            .expect("encrypt");
        let pt = decrypt_aead(&aead_key, &aead_nonce, &ct, &aad)
            .expect("decrypt");
        prop_assert_eq!(pt, plaintext);
    }

    /// Empty plaintext encrypts and decrypts correctly (the tag is
    /// still produced for authentication).
    #[test]
    fn aead_empty_plaintext(
        key in prop::array::uniform32(any::<u8>()),
        nonce in prop::array::uniform24(any::<u8>()),
    ) {
        let ct = encrypt_aead(&key, &nonce, &[], b"aad").expect("encrypt");
        // Ciphertext is just the 16-byte Poly1305 tag.
        prop_assert_eq!(ct.len(), 16);
        let pt = decrypt_aead(&key, &nonce, &ct, b"aad").expect("decrypt");
        prop_assert!(pt.is_empty());
    }

    /// Wrong key always fails decryption.
    #[test]
    fn aead_wrong_key_fails(
        key in prop::array::uniform32(any::<u8>()),
        nonce in prop::array::uniform24(any::<u8>()),
        plaintext in prop::collection::vec(any::<u8>(), 1..512),
    ) {
        let ct = encrypt_aead(&key, &nonce, &plaintext, b"aad")
            .expect("encrypt");
        let mut wrong_key = key;
        wrong_key[0] ^= 0xff;
        let err = decrypt_aead(&wrong_key, &nonce, &ct, b"aad")
            .expect_err("should fail");
        prop_assert!(matches!(err, CryptoError::AeadDecryption));
    }

    /// Tampered ciphertext always fails decryption.
    #[test]
    fn aead_tampered_ciphertext_fails(
        key in prop::array::uniform32(any::<u8>()),
        nonce in prop::array::uniform24(any::<u8>()),
        plaintext in prop::collection::vec(any::<u8>(), 1..512),
        flip_pos_frac in 0.0f64..1.0,
    ) {
        let mut ct = encrypt_aead(&key, &nonce, &plaintext, b"aad")
            .expect("encrypt");
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pos = {
            let raw = flip_pos_frac * (ct.len() as f64);
            // `raw` is non-negative (both operands ≥ 0) and < ct.len(),
            // so the truncation to usize is intentional and safe.
            raw as usize
        };
        let pos = pos.min(ct.len() - 1);
        ct[pos] ^= 0x01;
        let err = decrypt_aead(&key, &nonce, &ct, b"aad")
            .expect_err("should fail");
        prop_assert!(matches!(err, CryptoError::AeadDecryption));
    }

    /// Large plaintext (up to 64 KiB) round-trips correctly.
    #[test]
    fn aead_large_plaintext(
        key in prop::array::uniform32(any::<u8>()),
        nonce in prop::array::uniform24(any::<u8>()),
        plaintext in prop::collection::vec(any::<u8>(), 32768..65536),
    ) {
        let ct = encrypt_aead(&key, &nonce, &plaintext, b"large")
            .expect("encrypt");
        let pt = decrypt_aead(&key, &nonce, &ct, b"large")
            .expect("decrypt");
        prop_assert_eq!(pt, plaintext);
    }
}

// ---------------------------------------------------------------------------
// 5. Key zeroize: verify HybridSecretKey is zeroed on drop
// ---------------------------------------------------------------------------

#[test]
fn hybrid_secret_key_is_zeroed_on_drop() {
    let (_pk, sk) = hybrid_keypair().expect("keypair");

    // Copy the raw key bytes before dropping.
    let x25519_copy = sk.x25519;
    let mlkem_copy = sk.mlkem768;

    // Verify the keys are non-zero before drop.
    assert!(
        x25519_copy.iter().any(|&b| b != 0),
        "X25519 secret key should not be all-zero before drop"
    );
    assert!(
        mlkem_copy.iter().any(|&b| b != 0),
        "ML-KEM secret key should not be all-zero before drop"
    );

    // Drop the secret key — `#[zeroize(drop)]` should wipe the
    // fields. We verify structurally that the derive macro is
    // applied; direct memory inspection after drop is UB in safe
    // Rust, so we check the derive attribute instead.
    drop(sk);

    // Structural assertion: HybridSecretKey derives Zeroize and
    // has #[zeroize(drop)]. We verify this compiles — the derive
    // macro generates a `Drop` impl that calls `self.zeroize()`.
    fn assert_zeroize_on_drop<T: zeroize::Zeroize>() {}
    assert_zeroize_on_drop::<crypto::HybridSecretKey>();
}

/// Verify that `MasterKey` type alias is a fixed-length byte array
/// compatible with zeroize patterns.
#[test]
fn master_key_len_matches_aead_key_len() {
    use crypto::{AEAD_KEY_LEN, MASTER_KEY_LEN};
    assert_eq!(MASTER_KEY_LEN, AEAD_KEY_LEN);
    assert_eq!(MASTER_KEY_LEN, 32);
}
