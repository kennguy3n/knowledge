//! XChaCha20-Poly1305 AEAD wrapper.
//!
//! Per `docs/DESIGN.md` §3.1 and §9 and `ARCHITECTURE.md` §2.2 / §8.4, every
//! evidence body and every cold archive segment is encrypted with
//! XChaCha20-Poly1305 using a per-scope, per-epoch symmetric key. This
//! module exposes the small, opinionated `encrypt_aead` /
//! `decrypt_aead` API consumed by the rest of the substrate.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

use crate::errors::CryptoError;

/// Length of an XChaCha20-Poly1305 key (256 bits).
pub const AEAD_KEY_LEN: usize = 32;

/// Length of an XChaCha20-Poly1305 nonce (192 bits).
pub const AEAD_NONCE_LEN: usize = 24;

/// Symmetric key used for [`encrypt_aead`] / [`decrypt_aead`].
pub type AeadKey = [u8; AEAD_KEY_LEN];

/// Nonce used for [`encrypt_aead`] / [`decrypt_aead`].
///
/// XChaCha20 nonces are 192 bits, large enough to be safely chosen at
/// random (nonce reuse is essentially impossible at substrate scale).
pub type AeadNonce = [u8; AEAD_NONCE_LEN];

/// Ciphertext produced by [`encrypt_aead`] (Poly1305 tag appended).
pub type AeadCiphertext = Vec<u8>;

/// Encrypt `plaintext` under `key` and `nonce`, binding `aad` into the
/// authentication tag.
///
/// The output is the XChaCha20-Poly1305 ciphertext with the 16-byte tag
/// appended. The caller is responsible for storing the nonce and AAD
/// alongside the ciphertext (typically the evidence row id, scope id,
/// and content hash).
pub fn encrypt_aead(key: &AeadKey,
    nonce: &AeadNonce,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<AeadCiphertext, CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XNonce::from_slice(nonce);
    let payload = chacha20poly1305::aead::Payload {
        msg: plaintext,
        aad,
    };
    cipher
        .encrypt(nonce, payload)
        .map_err(|_| CryptoError::AeadEncryption)
}

/// Decrypt `ciphertext` produced by [`encrypt_aead`].
///
/// Returns [`CryptoError::AeadDecryption`] if the key is wrong, the
/// nonce is wrong, the AAD is wrong, or the ciphertext was tampered with.
pub fn decrypt_aead(key: &AeadKey,
    nonce: &AeadNonce,
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XNonce::from_slice(nonce);
    let payload = chacha20poly1305::aead::Payload {
        msg: ciphertext,
        aad,
    };
    cipher
        .decrypt(nonce, payload)
        .map_err(|_| CryptoError::AeadDecryption)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_key() -> AeadKey {
        let mut k = [0u8; AEAD_KEY_LEN];
        for (i, byte) in k.iter_mut().enumerate() {
            *byte = u8::try_from(i).expect("AEAD_KEY_LEN fits in u8");
        }
        k
    }

    fn fixture_nonce() -> AeadNonce {
        let mut n = [0u8; AEAD_NONCE_LEN];
        for (i, byte) in n.iter_mut().enumerate() {
            *byte = u8::try_from(i)
                .expect("AEAD_NONCE_LEN fits in u8")
                .wrapping_mul(7);
        }
        n
    }

    #[test]
    fn roundtrip() {
        let key = fixture_key();
        let nonce = fixture_nonce();
        let plaintext = b"hello world, this is a sensitive evidence body";
        let aad = b"evidence:scope:00000000-0000-0000-0000-000000000001";
        let ct = encrypt_aead(&key, &nonce, plaintext, aad).expect("encrypt");
        let pt = decrypt_aead(&key, &nonce, &ct, aad).expect("decrypt");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key = fixture_key();
        let nonce = fixture_nonce();
        let ct = encrypt_aead(&key, &nonce, b"secret", b"aad").unwrap();

        let mut wrong_key = key;
        wrong_key[0] ^= 0xff;
        let err = decrypt_aead(&wrong_key, &nonce, &ct, b"aad").unwrap_err();
        assert!(matches!(err, CryptoError::AeadDecryption));
    }

    #[test]
    fn wrong_aad_fails() {
        let key = fixture_key();
        let nonce = fixture_nonce();
        let ct = encrypt_aead(&key, &nonce, b"secret", b"good-aad").unwrap();
        let err = decrypt_aead(&key, &nonce, &ct, b"BAD-aad").unwrap_err();
        assert!(matches!(err, CryptoError::AeadDecryption));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = fixture_key();
        let nonce = fixture_nonce();
        let mut ct = encrypt_aead(&key, &nonce, b"secret", b"aad").unwrap();
        // Flip a byte.
        ct[0] ^= 0x01;
        let err = decrypt_aead(&key, &nonce, &ct, b"aad").unwrap_err();
        assert!(matches!(err, CryptoError::AeadDecryption));
    }
}
