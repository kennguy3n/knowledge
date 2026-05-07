//! Integration tests for the public API of the `crypto` crate.
//!
//! These tests live alongside the inline unit tests in each module
//! and exercise the high-level surface (BLAKE3 hash, AEAD,
//! key-derivation, hybrid KEM) end-to-end.

use crypto::{
    content_hash, decrypt_aead, derive_key, encrypt_aead, hybrid_kem_decap, hybrid_kem_encap,
    hybrid_keypair, AEAD_KEY_LEN, AEAD_NONCE_LEN, MASTER_KEY_LEN,
};

#[test]
fn content_hash_matches_blake3_independently() {
    let data = b"the quick brown fox";
    let our = content_hash(data);
    let theirs: [u8; 32] = blake3::hash(data).into();
    assert_eq!(our, theirs);
}

#[test]
fn aead_roundtrip_via_public_api() {
    let key = [0xA5; AEAD_KEY_LEN];
    let nonce = [0x11; AEAD_NONCE_LEN];
    let aad = b"some-aad";
    let plaintext = b"please don't read this in transit";
    let ct = encrypt_aead(&key, &nonce, plaintext, aad).expect("encrypt");
    let pt = decrypt_aead(&key, &nonce, &ct, aad).expect("decrypt");
    assert_eq!(pt, plaintext);
}

#[test]
fn derive_key_is_deterministic() {
    let master = [0x42; MASTER_KEY_LEN];
    let a = derive_key(&master, b"label-a").unwrap();
    let b = derive_key(&master, b"label-a").unwrap();
    let c = derive_key(&master, b"label-b").unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn hybrid_kem_roundtrip_via_public_api() {
    let (pk, sk) = hybrid_keypair().expect("keypair");
    let (ss_send, ct) = hybrid_kem_encap(&pk).expect("encap");
    let ss_recv = hybrid_kem_decap(&sk, &ct).expect("decap");
    assert_eq!(ss_send, ss_recv);
    assert_eq!(ss_send.len(), AEAD_KEY_LEN);
}
