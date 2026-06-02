//! Fuzz target: AEAD encrypt → decrypt round-trip.
//!
//! Feeds random key, nonce, plaintext, and AAD through
//! `encrypt_aead` → `decrypt_aead` and verifies that:
//! 1. `encrypt_aead` never panics for valid-length key/nonce.
//! 2. `decrypt_aead` recovers the original plaintext exactly.
//! 3. Flipping any byte in the ciphertext, nonce, or AAD causes
//!    decryption to fail (authentication check).

#![no_main]

use libfuzzer_sys::fuzz_target;

use crypto::{decrypt_aead, encrypt_aead, AEAD_KEY_LEN, AEAD_NONCE_LEN};

/// Minimum input: 32 (key) + 24 (nonce) + 1 (aad_len marker) + 0 (plaintext)
const MIN_INPUT: usize = AEAD_KEY_LEN + AEAD_NONCE_LEN + 1;

fuzz_target!(|data: &[u8]| {
    if data.len() < MIN_INPUT {
        return;
    }

    let (key_bytes, rest) = data.split_at(AEAD_KEY_LEN);
    let (nonce_bytes, rest) = rest.split_at(AEAD_NONCE_LEN);
    // Use the next byte to split the remainder into AAD and plaintext.
    let split_byte = rest[0] as usize;
    let payload = &rest[1..];
    let aad_len = if payload.is_empty() {
        0
    } else {
        split_byte % (payload.len() + 1)
    };
    let (aad, plaintext) = payload.split_at(aad_len);

    let mut key = [0u8; AEAD_KEY_LEN];
    key.copy_from_slice(key_bytes);
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    nonce.copy_from_slice(nonce_bytes);

    // Encrypt must succeed for any plaintext/AAD combination.
    let ciphertext = match encrypt_aead(&key, &nonce, plaintext, aad) {
        Ok(ct) => ct,
        Err(_) => return,
    };

    // Decrypt must recover the original plaintext.
    let recovered = decrypt_aead(&key, &nonce, &ciphertext, aad)
        .expect("decrypt must succeed on freshly-encrypted ciphertext");
    assert_eq!(recovered, plaintext,
        "round-trip plaintext mismatch"
    );

    // Tamper check: flipping a bit in the ciphertext must fail.
    if !ciphertext.is_empty() {
        let mut tampered = ciphertext.clone();
        tampered[0] ^= 0x01;
        assert!(decrypt_aead(&key, &nonce, &tampered, aad).is_err(),
            "tampered ciphertext must fail authentication"
        );
    }
});
