//! Hybrid X25519 + ML-KEM-768 key encapsulation.
//!
//! Per `ARCHITECTURE.md` §8.2 and `docs/DESIGN.md` §9, every new key
//! exchange in the substrate runs a hybrid X25519 + ML-KEM-768
//! construction so the session secret stays classically secure as long
//! as either primitive is unbroken.
//!
//! The combiner is **concatenate-then-KDF**:
//!
//! ```text
//! shared_secret = HKDF-SHA256(
//!     ikm = X25519_dh || MLKEM768_ss,
//!     salt = "knowledge-hybrid-kem-v1",
//!     info = "x25519+mlkem768",
//!     L = 32,
//! )
//! ```
//!
//! Both halves are real on encap and decap; there is no fall-back path
//! that silently drops the PQ side. The PQ side is selected through the
//! [`KemBackend`] trait so it can be swapped (RustCrypto → `liboqs`) in
//! a future update.

use hkdf::Hkdf;
// `OsRng` is imported from `rand_core` (kept at 0.6) rather than
// from `rand::rngs` (now 0.10, where the OS RNG was renamed to
// `SysRng`) because `x25519-dalek 2`'s `X25519Secret::random_from_rng`
// consumes the `rand_core 0.6 RngCore + CryptoRng` trait bound.
// Mixing in a `rand 0.10 SysRng` produces a trait-bound error
// because the two `rand_core` versions have parallel,
// non-interconvertible trait hierarchies. See the workspace
// `Cargo.toml` comment for the full rationale on why
// `rand_core` stays at 0.6 while the rest of the workspace moves
// to `rand` 0.10.
use rand_core::OsRng;
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519Public, StaticSecret as X25519Secret};
use zeroize::Zeroize;

use crate::aead::AEAD_KEY_LEN;
use crate::errors::CryptoError;
use crate::kem::{
    KemBackend, KemCiphertext, KemPublicKey, KemSecretKey, MlKem768Backend, KEM_CIPHERTEXT_LEN,
    KEM_PUBLIC_KEY_LEN, KEM_SECRET_KEY_LEN,
};

/// X25519 public-key length.
const X25519_PUBLIC_LEN: usize = 32;

/// X25519 secret-key length.
const X25519_SECRET_LEN: usize = 32;

/// Length of an X25519 raw shared secret.
const X25519_SHARED_LEN: usize = 32;

/// Hybrid public key — concatenation of an X25519 public key and an
/// ML-KEM-768 public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HybridPublicKey {
    /// X25519 32-byte public key.
    pub x25519: [u8; X25519_PUBLIC_LEN],
    /// ML-KEM-768 1184-byte public key.
    pub mlkem768: KemPublicKey,
}

/// Hybrid secret key — paired secret material for both halves.
#[derive(Clone, Zeroize)]
#[zeroize(drop)]
pub struct HybridSecretKey {
    /// X25519 32-byte secret key.
    pub x25519: [u8; X25519_SECRET_LEN],
    /// ML-KEM-768 2400-byte secret key.
    pub mlkem768: KemSecretKey,
}

impl std::fmt::Debug for HybridSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridSecretKey")
            .field("x25519", &"<redacted>")
            .field("mlkem768", &"<redacted>")
            .finish()
    }
}

/// Output of [`hybrid_kem_encap`] — the wire material the recipient
/// needs to recover the same shared secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HybridCiphertext {
    /// Sender's ephemeral X25519 public key.
    pub x25519_eph_pub: [u8; X25519_PUBLIC_LEN],
    /// ML-KEM-768 ciphertext.
    pub mlkem768_ct: KemCiphertext,
}

/// 32-byte combined shared secret produced by the hybrid combiner.
pub type HybridSharedSecret = [u8; AEAD_KEY_LEN];

/// Generate a fresh hybrid keypair using the default ML-KEM-768
/// backend.
pub fn hybrid_keypair() -> Result<(HybridPublicKey, HybridSecretKey), CryptoError> {
    hybrid_keypair_with_backend(&MlKem768Backend)
}

/// Generate a fresh hybrid keypair with an explicit ML-KEM-768 backend.
pub fn hybrid_keypair_with_backend<B: KemBackend>(
    backend: &B,
) -> Result<(HybridPublicKey, HybridSecretKey), CryptoError> {
    let x25519_sk = X25519Secret::random_from_rng(OsRng);
    let x25519_pk = X25519Public::from(&x25519_sk);

    let (mlkem_pk, mlkem_sk) = backend.keypair()?;

    Ok((
        HybridPublicKey {
            x25519: x25519_pk.to_bytes(),
            mlkem768: mlkem_pk,
        },
        HybridSecretKey {
            x25519: x25519_sk.to_bytes(),
            mlkem768: mlkem_sk,
        },
    ))
}

/// Encapsulate a fresh hybrid shared secret to `recipient_pk` using the
/// default ML-KEM-768 backend.
pub fn hybrid_kem_encap(
    recipient_pk: &HybridPublicKey,
) -> Result<(HybridSharedSecret, HybridCiphertext), CryptoError> {
    hybrid_kem_encap_with_backend(&MlKem768Backend, recipient_pk)
}

/// Encapsulate a fresh hybrid shared secret using an explicit
/// ML-KEM-768 backend.
pub fn hybrid_kem_encap_with_backend<B: KemBackend>(
    backend: &B,
    recipient_pk: &HybridPublicKey,
) -> Result<(HybridSharedSecret, HybridCiphertext), CryptoError> {
    if recipient_pk.x25519.len() != X25519_PUBLIC_LEN {
        return Err(CryptoError::KemBufferLength {
            expected: X25519_PUBLIC_LEN,
            got: recipient_pk.x25519.len(),
        });
    }
    if recipient_pk.mlkem768.len() != KEM_PUBLIC_KEY_LEN {
        return Err(CryptoError::KemBufferLength {
            expected: KEM_PUBLIC_KEY_LEN,
            got: recipient_pk.mlkem768.len(),
        });
    }

    let eph_sk = X25519Secret::random_from_rng(OsRng);
    let eph_pk = X25519Public::from(&eph_sk);

    let mut peer_pk_bytes = [0u8; X25519_PUBLIC_LEN];
    peer_pk_bytes.copy_from_slice(&recipient_pk.x25519);
    let peer_pk = X25519Public::from(peer_pk_bytes);

    let dh = eph_sk.diffie_hellman(&peer_pk);

    let (mlkem_ss, mlkem_ct) = backend.encap(&recipient_pk.mlkem768)?;

    let shared = combine(dh.as_bytes(), &mlkem_ss)?;

    Ok((
        shared,
        HybridCiphertext {
            x25519_eph_pub: eph_pk.to_bytes(),
            mlkem768_ct: mlkem_ct,
        },
    ))
}

/// Decapsulate a hybrid ciphertext with the default ML-KEM-768 backend.
pub fn hybrid_kem_decap(
    recipient_sk: &HybridSecretKey,
    ciphertext: &HybridCiphertext,
) -> Result<HybridSharedSecret, CryptoError> {
    hybrid_kem_decap_with_backend(&MlKem768Backend, recipient_sk, ciphertext)
}

/// Decapsulate a hybrid ciphertext with an explicit ML-KEM-768 backend.
pub fn hybrid_kem_decap_with_backend<B: KemBackend>(
    backend: &B,
    recipient_sk: &HybridSecretKey,
    ciphertext: &HybridCiphertext,
) -> Result<HybridSharedSecret, CryptoError> {
    if recipient_sk.x25519.len() != X25519_SECRET_LEN {
        return Err(CryptoError::KemBufferLength {
            expected: X25519_SECRET_LEN,
            got: recipient_sk.x25519.len(),
        });
    }
    if recipient_sk.mlkem768.len() != KEM_SECRET_KEY_LEN {
        return Err(CryptoError::KemBufferLength {
            expected: KEM_SECRET_KEY_LEN,
            got: recipient_sk.mlkem768.len(),
        });
    }
    if ciphertext.mlkem768_ct.len() != KEM_CIPHERTEXT_LEN {
        return Err(CryptoError::KemBufferLength {
            expected: KEM_CIPHERTEXT_LEN,
            got: ciphertext.mlkem768_ct.len(),
        });
    }

    let mut sk_bytes = [0u8; X25519_SECRET_LEN];
    sk_bytes.copy_from_slice(&recipient_sk.x25519);
    let sk = X25519Secret::from(sk_bytes);
    let eph_pk = X25519Public::from(ciphertext.x25519_eph_pub);
    let dh = sk.diffie_hellman(&eph_pk);

    let mlkem_ss = backend.decap(&recipient_sk.mlkem768, &ciphertext.mlkem768_ct)?;

    combine(dh.as_bytes(), &mlkem_ss)
}

/// Concatenate-then-KDF combiner shared between encap and decap.
fn combine(
    x25519_dh: &[u8; X25519_SHARED_LEN],
    mlkem768_ss: &[u8; AEAD_KEY_LEN],
) -> Result<HybridSharedSecret, CryptoError> {
    let mut ikm = [0u8; X25519_SHARED_LEN + AEAD_KEY_LEN];
    ikm[..X25519_SHARED_LEN].copy_from_slice(x25519_dh);
    ikm[X25519_SHARED_LEN..].copy_from_slice(mlkem768_ss);

    let hk = Hkdf::<Sha256>::new(Some(b"knowledge-hybrid-kem-v1"), &ikm);
    let mut out = [0u8; AEAD_KEY_LEN];
    hk.expand(b"x25519+mlkem768", &mut out)
        .map_err(|_| CryptoError::HybridCombiner("HKDF expand rejected output length"))?;

    // Best-effort wipe of the concatenated material on the stack.
    ikm.zeroize();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kem::StubKemBackend;

    #[test]
    fn hybrid_roundtrip_real_backend() {
        let (pk, sk) = hybrid_keypair().expect("keypair");
        let (ss_send, ct) = hybrid_kem_encap(&pk).expect("encap");
        let ss_recv = hybrid_kem_decap(&sk, &ct).expect("decap");
        assert_eq!(ss_send, ss_recv);
        assert_eq!(ss_send.len(), AEAD_KEY_LEN);
    }

    #[test]
    fn hybrid_roundtrip_stub_backend() {
        let backend = StubKemBackend;
        let (pk, sk) = hybrid_keypair_with_backend(&backend).expect("keypair");
        let (ss_send, ct) = hybrid_kem_encap_with_backend(&backend, &pk).expect("encap");
        let ss_recv = hybrid_kem_decap_with_backend(&backend, &sk, &ct).expect("decap");
        assert_eq!(ss_send, ss_recv);
    }

    #[test]
    fn distinct_keypairs_yield_distinct_secrets() {
        let (pk1, _sk1) = hybrid_keypair().unwrap();
        let (pk2, _sk2) = hybrid_keypair().unwrap();
        let (ss1, _) = hybrid_kem_encap(&pk1).unwrap();
        let (ss2, _) = hybrid_kem_encap(&pk2).unwrap();
        assert_ne!(ss1, ss2);
    }

    #[test]
    fn wrong_secret_key_fails_to_recover_secret() {
        let (pk, _sk) = hybrid_keypair().unwrap();
        let (_, sk_other) = hybrid_keypair().unwrap();
        let (ss_send, ct) = hybrid_kem_encap(&pk).unwrap();
        // Decapsulating with a different secret key should produce a
        // different shared secret (or fail outright). Either way, it
        // must not equal the sender's secret.
        match hybrid_kem_decap(&sk_other, &ct) {
            Ok(ss_recv) => assert_ne!(ss_recv, ss_send),
            Err(_) => {}
        }
    }
}
