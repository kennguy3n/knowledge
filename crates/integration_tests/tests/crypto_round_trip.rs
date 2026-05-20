//! Cross-primitive crypto round-trip test.
//!
//! Drives every advertised algorithm in the `crypto` crate through a
//! single end-to-end scenario:
//!
//! 1. Hybrid X25519 + ML-KEM-768 keypair → encap → decap. Recipient
//!    must reconstruct the same shared secret; tampering with the
//!    ciphertext must change the recipient's shared secret.
//! 2. ML-DSA-65 sign → verify; tampered message rejected.
//! 3. SPHINCS+-SHAKE-128f-simple sign → verify; tampered message
//!    rejected.
//! 4. Co-sign / co-verify — the hybrid ML-DSA-65 ⊕ SPHINCS+ bundle.
//! 5. AEAD `(encrypt, decrypt)` round-trip + "cryptographic
//!    forgetting" — destroying the scope key after encryption makes
//!    the ciphertext unrecoverable.

use crypto::signer_backend::{MlDsa65Signer, SignerBackend};
use crypto::sphincs::{CoSigner, SphincsPlusSigner};
use crypto::{
    decrypt_aead, derive_key, encrypt_aead, hybrid_kem_decap, hybrid_kem_encap, hybrid_keypair,
    AEAD_KEY_LEN, AEAD_NONCE_LEN, MASTER_KEY_LEN,
};

#[test]
fn hybrid_kem_encap_decap_round_trip() {
    let (pk, sk) = hybrid_keypair().expect("keypair");

    let (shared_a, ct) = hybrid_kem_encap(&pk).expect("encap");
    let shared_b = hybrid_kem_decap(&sk, &ct).expect("decap");
    assert_eq!(
        shared_a, shared_b,
        "sender's and recipient's shared secret must match"
    );

    // Tampering with the X25519 ephemeral public key must produce a
    // *different* shared secret on decap — verifies the X25519 leg
    // is actually consumed by the combiner (not just appended).
    let mut tampered = ct.clone();
    tampered.x25519_eph_pub[0] ^= 0x01;
    let tampered_shared = hybrid_kem_decap(&sk, &tampered).expect("decap of tampered ct");
    assert_ne!(
        tampered_shared, shared_a,
        "flipping the X25519 ephemeral pub must change the combined shared secret"
    );
}

#[test]
fn ml_dsa_65_sign_verify_round_trip() {
    let signer = MlDsa65Signer::generate();
    let verifier = signer.verifier();

    let msg = b"ml-dsa-65 integration message body";
    let sig = signer.sign_bytes(msg).expect("sign");
    assert!(
        verifier.verify_bytes(msg, &sig).expect("verify_bytes"),
        "freshly-signed message must verify"
    );

    // Tampered message — verify must return Ok(false), not error.
    let mut tampered = msg.to_vec();
    tampered[0] ^= 0x80;
    assert!(
        !verifier
            .verify_bytes(&tampered, &sig)
            .expect("verify on tampered msg"),
        "ML-DSA-65 must reject a signature against a tampered message"
    );

    // Tampered signature — also must return Ok(false).
    let mut bad_sig = sig.clone();
    bad_sig[0] ^= 0x01;
    assert!(
        !verifier
            .verify_bytes(msg, &bad_sig)
            .expect("verify with tampered sig"),
        "ML-DSA-65 must reject a tampered signature"
    );
}

#[test]
fn sphincs_plus_sign_verify_round_trip() {
    let signer = SphincsPlusSigner::generate();
    let verifier = signer.verifier();

    let msg = b"sphincs+ integration message body";
    let sig = signer.sign_bytes(msg).expect("sign");
    assert!(
        verifier.verify_bytes(msg, &sig).expect("verify_bytes"),
        "freshly-signed message must verify"
    );

    let mut tampered = msg.to_vec();
    tampered[0] ^= 0x80;
    assert!(
        !verifier
            .verify_bytes(&tampered, &sig)
            .expect("verify on tampered msg"),
        "SPHINCS+ must reject a signature against a tampered message"
    );
}

#[test]
fn co_sign_co_verify_round_trip() {
    let signer = CoSigner::generate();
    let verifier = signer.verifier();

    let msg = b"co-signed message: ml-dsa-65 + sphincs+";
    let sig = signer.co_sign(msg).expect("co_sign");
    assert!(
        verifier.co_verify(msg, &sig).expect("co_verify"),
        "freshly-cosigned message must verify"
    );

    // Tampering with either half must invalidate the whole bundle.
    let mut tampered_ml = sig.clone();
    tampered_ml.ml_dsa_65[0] ^= 0x01;
    assert!(
        !verifier
            .co_verify(msg, &tampered_ml)
            .expect("co_verify ml-dsa tamper"),
        "tampered ML-DSA-65 half must invalidate the co-signature"
    );

    let mut tampered_sp = sig.clone();
    tampered_sp.sphincs_plus[0] ^= 0x01;
    assert!(
        !verifier
            .co_verify(msg, &tampered_sp)
            .expect("co_verify sphincs tamper"),
        "tampered SPHINCS+ half must invalidate the co-signature"
    );
}

#[test]
fn aead_round_trip_then_forget_scope_makes_ciphertext_unrecoverable() {
    let mut master_key = [0u8; MASTER_KEY_LEN];
    for (i, byte) in master_key.iter_mut().enumerate() {
        // `i` ≤ MASTER_KEY_LEN (= 32) so try_from is exact.
        *byte = u8::try_from(i).unwrap_or(0xAA).wrapping_mul(31).wrapping_add(7);
    }

    // Derive a per-scope AEAD key under a representative HKDF
    // context. This mirrors what `EvidenceStore::scope_key` does
    // when no random DEK has been provisioned yet.
    let scope_label = b"scope:00000000-0000-0000-0000-000000000001:body:v1";
    let mut scope_key = derive_key(&master_key, scope_label).expect("derive scope key");

    let plaintext = b"sensitive evidence body, encrypted under the scope key";
    let nonce = [0xC3u8; AEAD_NONCE_LEN];
    let aad = b"integration:aad";

    assert_eq!(scope_key.len(), AEAD_KEY_LEN);
    let ciphertext = encrypt_aead(&scope_key, &nonce, plaintext, aad).expect("encrypt");
    let recovered = decrypt_aead(&scope_key, &nonce, &ciphertext, aad).expect("decrypt");
    assert_eq!(recovered, plaintext, "AEAD round-trip must succeed");

    // "Forget" the scope: overwrite the in-memory key material with
    // zeros. With a fresh, randomly-generated DEK that's never been
    // re-derivable, this would be sufficient. Because `derive_key`
    // is deterministic (HKDF), re-deriving from the master would
    // still recover the same bytes — so to model true cryptographic
    // forgetting we also overwrite the master key. The `crypto`
    // crate uses `zeroize` for this in production paths; the
    // integration test crate stays free of that dep by writing the
    // zeros directly so the test exercises only the public API.
    scope_key.fill(0);
    master_key.fill(0);
    assert_eq!(
        scope_key,
        [0u8; AEAD_KEY_LEN],
        "scope key buffer must be all-zero after destruction"
    );

    // Decrypting with the zeroized key must fail — XChaCha20-Poly1305
    // is an authenticated cipher, so the tag check is the gate.
    let err = decrypt_aead(&scope_key, &nonce, &ciphertext, aad);
    assert!(
        err.is_err(),
        "decrypt with destroyed scope key must fail (Poly1305 tag mismatch)"
    );
}
