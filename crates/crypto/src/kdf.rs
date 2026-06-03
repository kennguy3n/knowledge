//! HKDF-SHA256 key derivation.
//!
//! Per `docs/technical/architecture.md` §2.2 the SQLCipher master key is derived from a
//! per-user master key via HKDF + hybrid KEM unwrap, and per `docs/technical/design.md`
//! §3.1 every storage path uses per-scope, per-epoch keys. This module
//! exposes a single deterministic [`derive_key`] function so all
//! sub-key derivations across the substrate share one well-defined
//! construction.

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::aead::AEAD_KEY_LEN;
use crate::errors::CryptoError;

/// Length of the canonical user master key (256 bits).
pub const MASTER_KEY_LEN: usize = 32;

/// User master key — the root from which every sub-key in the substrate
/// is derived.
pub type MasterKey = [u8; MASTER_KEY_LEN];

/// 32-byte derived key (suitable for use as an [`crate::AeadKey`] or as
/// a SQLCipher page-encryption key).
pub type DerivedKey = [u8; AEAD_KEY_LEN];

/// Derive a 32-byte sub-key from `master_key` for the given `context`.
///
/// `context` is the HKDF `info` argument and must uniquely identify the
/// purpose of the derived key (for example
/// `b"sqlcipher:store:v1"` or `b"scope:00000000-0000-0000-0000-000000000001:body"`).
///
/// The same `master_key` and `context` always produce the same output
/// (deterministic) — this is essential for SQLCipher master-key derivation
/// and for re-deriving per-scope keys after process restart.
pub fn derive_key(master_key: &MasterKey, context: &[u8]) -> Result<DerivedKey, CryptoError> {
    derive_key_with_salt(master_key, b"knowledge-substrate-v1", context)
}

/// Lower-level variant of [`derive_key`] exposing the HKDF salt.
///
/// Production callers should use [`derive_key`] with the substrate's
/// canonical salt; this variant exists so that integration tests and
/// future epoch-rotation logic can rotate the salt explicitly.
pub fn derive_key_with_salt(
    master_key: &MasterKey,
    salt: &[u8],
    context: &[u8],
) -> Result<DerivedKey, CryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(salt), master_key);
    let mut out = [0u8; AEAD_KEY_LEN];
    hk.expand(context, &mut out)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand rejected output length"))?;
    Ok(out)
}

/// Convenience helper that derives a key and then immediately wipes the
/// caller's master-key buffer.
///
/// Useful at boot when a master key is reconstructed via hybrid-KEM
/// unwrap and should not linger on the stack.
pub fn derive_key_and_zeroize(
    master_key: &mut MasterKey,
    context: &[u8],
) -> Result<DerivedKey, CryptoError> {
    let result = derive_key(master_key, context);
    master_key.zeroize();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mk = [0xAB; MASTER_KEY_LEN];
        let k1 = derive_key(&mk, b"sqlcipher:store:v1").unwrap();
        let k2 = derive_key(&mk, b"sqlcipher:store:v1").unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_contexts_produce_different_keys() {
        let mk = [0xAB; MASTER_KEY_LEN];
        let k1 = derive_key(&mk, b"context:a").unwrap();
        let k2 = derive_key(&mk, b"context:b").unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_master_keys_produce_different_keys() {
        let context = b"context:same";
        let k1 = derive_key(&[0xAB; MASTER_KEY_LEN], context).unwrap();
        let k2 = derive_key(&[0xCD; MASTER_KEY_LEN], context).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn zeroize_helper_clears_master_key() {
        let mut mk = [0xAB; MASTER_KEY_LEN];
        let _k = derive_key_and_zeroize(&mut mk, b"context:zero").unwrap();
        assert_eq!(mk, [0u8; MASTER_KEY_LEN]);
    }
}
