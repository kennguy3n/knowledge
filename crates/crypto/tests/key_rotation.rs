//! Key rotation tests.
//!
//! Verifies the full DEK rotation lifecycle: generate key → encrypt →
//! rotate (generate new key) → old-key decrypt fails → new-key
//! encrypt/decrypt succeeds.

use crypto::{
    decrypt_aead, encrypt_aead, AeadKey, AeadNonce, CryptoError, AEAD_KEY_LEN, AEAD_NONCE_LEN,
};

/// Generate a deterministic key from a seed byte for test reproducibility.
fn test_key(seed: u8) -> AeadKey {
    [seed; AEAD_KEY_LEN]
}

/// Generate a deterministic nonce from a seed byte.
fn test_nonce(seed: u8) -> AeadNonce {
    [seed; AEAD_NONCE_LEN]
}

#[test]
fn key_rotation_old_key_cannot_decrypt_after_rotation() {
    let old_key = test_key(0xAA);
    let nonce = test_nonce(0x01);
    let plaintext = b"sensitive scope data before rotation";
    let aad = b"scope:00000000-0000-0000-0000-000000000001";

    // Encrypt under the old key.
    let ct_old = encrypt_aead(&old_key, &nonce, plaintext, aad).expect("encrypt with old key");

    // Verify old key can decrypt its own ciphertext.
    let pt = decrypt_aead(&old_key, &nonce, &ct_old, aad).expect("decrypt with old key");
    assert_eq!(pt, plaintext);

    // "Rotate": generate a new key (simulating DEK rotation).
    let new_key = test_key(0xBB);
    assert_ne!(old_key, new_key);

    // Old ciphertext is unrecoverable with the new key.
    let err = decrypt_aead(&new_key, &nonce, &ct_old, aad).unwrap_err();
    assert!(
        matches!(err, CryptoError::AeadDecryption),
        "old ciphertext must not decrypt under rotated key"
    );
}

#[test]
fn key_rotation_new_key_encrypts_and_decrypts() {
    let new_key = test_key(0xBB);
    let nonce = test_nonce(0x02);
    let plaintext = b"sensitive scope data after rotation";
    let aad = b"scope:00000000-0000-0000-0000-000000000001";

    let ct_new = encrypt_aead(&new_key, &nonce, plaintext, aad).expect("encrypt with new key");
    let pt = decrypt_aead(&new_key, &nonce, &ct_new, aad).expect("decrypt with new key");
    assert_eq!(pt, plaintext);
}

#[test]
fn key_rotation_full_lifecycle() {
    let old_key = test_key(0x11);
    let new_key = test_key(0x22);
    let nonce_1 = test_nonce(0x01);
    let nonce_2 = test_nonce(0x02);
    let aad = b"scope:lifecycle-test";

    // Step 1: Generate key (old_key) and encrypt.
    let plaintext_1 = b"data encrypted under the original DEK";
    let ct_1 = encrypt_aead(&old_key, &nonce_1, plaintext_1, aad).expect("encrypt step 1");

    // Verify decrypt works with old key.
    let pt_1 = decrypt_aead(&old_key, &nonce_1, &ct_1, aad).expect("decrypt step 1");
    assert_eq!(pt_1, plaintext_1);

    // Step 2: Rotate key → new_key.
    // (In the real substrate, this means destroying the old scope DEK
    // row in `scope_deks` and inserting a new one.)

    // Step 3: Decrypt old ciphertext with old key FAILS (key is "destroyed").
    // Simulate destruction by attempting with the new key.
    let err = decrypt_aead(&new_key, &nonce_1, &ct_1, aad).unwrap_err();
    assert!(matches!(err, CryptoError::AeadDecryption));

    // Step 4: Encrypt new data with new key succeeds.
    let plaintext_2 = b"data encrypted under the rotated DEK";
    let ct_2 = encrypt_aead(&new_key, &nonce_2, plaintext_2, aad).expect("encrypt step 4");

    // Step 5: Decrypt new ciphertext with new key succeeds.
    let pt_2 = decrypt_aead(&new_key, &nonce_2, &ct_2, aad).expect("decrypt step 5");
    assert_eq!(pt_2, plaintext_2);

    // Step 6: Old key cannot decrypt new ciphertext.
    let err = decrypt_aead(&old_key, &nonce_2, &ct_2, aad).unwrap_err();
    assert!(matches!(err, CryptoError::AeadDecryption));
}

#[test]
fn key_rotation_cross_key_isolation() {
    // Encrypt the same plaintext under two different keys and verify
    // the ciphertexts differ (probabilistic, but deterministic nonces
    // here make it guaranteed).
    let key_a = test_key(0xCC);
    let key_b = test_key(0xDD);
    let nonce = test_nonce(0x03);
    let plaintext = b"cross-key isolation check";
    let aad = b"scope:isolation";

    let ct_a = encrypt_aead(&key_a, &nonce, plaintext, aad).expect("encrypt a");
    let ct_b = encrypt_aead(&key_b, &nonce, plaintext, aad).expect("encrypt b");
    assert_ne!(
        ct_a, ct_b,
        "same plaintext under different keys must produce different ciphertext"
    );
}
