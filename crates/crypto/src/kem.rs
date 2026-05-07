//! ML-KEM-768 (Kyber) post-quantum KEM with a swappable backend trait.
//!
//! The substrate eventually wants to call into `liboqs` for a formally
//! audited ML-KEM-768 implementation (per `ARCHITECTURE.md` §2.5). For
//! this Phase 0 batch we provide:
//!
//! * [`MlKem768Backend`] — a real implementation backed by the
//!   pure-Rust `ml-kem` crate from RustCrypto. This is the default
//!   backend used by [`crate::hybrid_kem_encap`] / [`crate::hybrid_kem_decap`].
//! * [`StubKemBackend`] — a deterministic mock backend used in tests
//!   and by callers that explicitly want to defer the PQ side. It
//!   produces well-typed but cryptographically-meaningless output and
//!   is **not** suitable for production.
//!
//! The trait surface is small on purpose: `keypair`, `encap`, `decap`,
//! plus the four buffer-length constants. Swapping in an `liboqs`
//! backend later is a one-file change.

use rand::rngs::OsRng;

use crate::errors::CryptoError;

/// ML-KEM-768 public-key length in bytes.
pub const KEM_PUBLIC_KEY_LEN: usize = 1184;

/// ML-KEM-768 secret-key length in bytes.
pub const KEM_SECRET_KEY_LEN: usize = 2400;

/// ML-KEM-768 ciphertext length in bytes.
pub const KEM_CIPHERTEXT_LEN: usize = 1088;

/// ML-KEM-768 shared-secret length in bytes (256 bits).
pub const KEM_SHARED_SECRET_LEN: usize = 32;

/// ML-KEM-768 public key.
pub type KemPublicKey = [u8; KEM_PUBLIC_KEY_LEN];

/// ML-KEM-768 secret key.
pub type KemSecretKey = [u8; KEM_SECRET_KEY_LEN];

/// ML-KEM-768 ciphertext (encapsulated key material).
pub type KemCiphertext = [u8; KEM_CIPHERTEXT_LEN];

/// ML-KEM-768 shared secret.
pub type KemSharedSecret = [u8; KEM_SHARED_SECRET_LEN];

/// Pluggable backend trait for the ML-KEM-768 side of the hybrid KEM.
///
/// The substrate consumes ML-KEM-768 only through this trait so the
/// underlying implementation can be swapped (RustCrypto pure-Rust →
/// `liboqs` FFI → hardware-accelerated backend) without touching the
/// rest of the core.
pub trait KemBackend {
    /// Generate a fresh ML-KEM-768 keypair.
    fn keypair(&self) -> Result<(KemPublicKey, KemSecretKey), CryptoError>;

    /// Encapsulate a fresh shared secret to `recipient_pk`.
    fn encap(
        &self,
        recipient_pk: &KemPublicKey,
    ) -> Result<(KemSharedSecret, KemCiphertext), CryptoError>;

    /// Decapsulate `ciphertext` with `recipient_sk` and recover the
    /// shared secret.
    fn decap(
        &self,
        recipient_sk: &KemSecretKey,
        ciphertext: &KemCiphertext,
    ) -> Result<KemSharedSecret, CryptoError>;
}

/// Real ML-KEM-768 backend backed by the `ml-kem` RustCrypto crate.
///
/// This is the default backend wired into [`crate::hybrid_kem_encap`]
/// and [`crate::hybrid_kem_decap`].
#[derive(Debug, Default, Clone, Copy)]
pub struct MlKem768Backend;

impl KemBackend for MlKem768Backend {
    fn keypair(&self) -> Result<(KemPublicKey, KemSecretKey), CryptoError> {
        use ml_kem::{EncodedSizeUser, KemCore, MlKem768};
        let mut rng = OsRng;
        let (dk, ek) = MlKem768::generate(&mut rng);

        let ek_bytes = ek.as_bytes();
        let dk_bytes = dk.as_bytes();
        let mut pk = [0u8; KEM_PUBLIC_KEY_LEN];
        let mut sk = [0u8; KEM_SECRET_KEY_LEN];
        check_len("ek", ek_bytes.len(), KEM_PUBLIC_KEY_LEN)?;
        check_len("dk", dk_bytes.len(), KEM_SECRET_KEY_LEN)?;
        pk.copy_from_slice(&ek_bytes);
        sk.copy_from_slice(&dk_bytes);
        Ok((pk, sk))
    }

    fn encap(
        &self,
        recipient_pk: &KemPublicKey,
    ) -> Result<(KemSharedSecret, KemCiphertext), CryptoError> {
        use ml_kem::kem::Encapsulate;
        use ml_kem::{Encoded, EncodedSizeUser, KemCore, MlKem768};

        let encoded =
            Encoded::<<MlKem768 as KemCore>::EncapsulationKey>::try_from(recipient_pk.as_slice())
                .map_err(|_| CryptoError::KemBackend("malformed encapsulation key"))?;
        let ek = <MlKem768 as KemCore>::EncapsulationKey::from_bytes(&encoded);
        let mut rng = OsRng;
        let (ct, ss) = ek
            .encapsulate(&mut rng)
            .map_err(|_| CryptoError::KemBackend("encapsulation failed"))?;
        let mut ct_out = [0u8; KEM_CIPHERTEXT_LEN];
        let mut ss_out = [0u8; KEM_SHARED_SECRET_LEN];
        check_len("ciphertext", ct.len(), KEM_CIPHERTEXT_LEN)?;
        check_len("shared secret", ss.len(), KEM_SHARED_SECRET_LEN)?;
        ct_out.copy_from_slice(&ct);
        ss_out.copy_from_slice(&ss);
        Ok((ss_out, ct_out))
    }

    fn decap(
        &self,
        recipient_sk: &KemSecretKey,
        ciphertext: &KemCiphertext,
    ) -> Result<KemSharedSecret, CryptoError> {
        use ml_kem::kem::Decapsulate;
        use ml_kem::{Encoded, EncodedSizeUser, KemCore, MlKem768};

        let encoded_dk =
            Encoded::<<MlKem768 as KemCore>::DecapsulationKey>::try_from(recipient_sk.as_slice())
                .map_err(|_| CryptoError::KemBackend("malformed decapsulation key"))?;
        let dk = <MlKem768 as KemCore>::DecapsulationKey::from_bytes(&encoded_dk);

        let encoded_ct = ml_kem::Ciphertext::<MlKem768>::try_from(ciphertext.as_slice())
            .map_err(|_| CryptoError::KemBackend("malformed ciphertext"))?;
        let ss = dk
            .decapsulate(&encoded_ct)
            .map_err(|_| CryptoError::KemBackend("decapsulation failed"))?;
        let mut ss_out = [0u8; KEM_SHARED_SECRET_LEN];
        check_len("shared secret", ss.len(), KEM_SHARED_SECRET_LEN)?;
        ss_out.copy_from_slice(&ss);
        Ok(ss_out)
    }
}

/// Deterministic mock KEM backend for tests and for callers that want
/// to defer the PQ side.
///
/// Outputs are well-typed but **not** cryptographically meaningful —
/// shared secrets are derived purely from a BLAKE3 digest of the public
/// key and ciphertext. Never use this in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubKemBackend;

impl KemBackend for StubKemBackend {
    fn keypair(&self) -> Result<(KemPublicKey, KemSecretKey), CryptoError> {
        use rand::RngCore;
        let mut rng = OsRng;
        let mut pk = [0u8; KEM_PUBLIC_KEY_LEN];
        let mut sk = [0u8; KEM_SECRET_KEY_LEN];
        rng.fill_bytes(&mut pk);
        // Bind sk to pk so decap(sk) and encap(pk) can recover the same
        // shared secret deterministically.
        sk[..KEM_PUBLIC_KEY_LEN].copy_from_slice(&pk);
        rng.fill_bytes(&mut sk[KEM_PUBLIC_KEY_LEN..]);
        Ok((pk, sk))
    }

    fn encap(
        &self,
        recipient_pk: &KemPublicKey,
    ) -> Result<(KemSharedSecret, KemCiphertext), CryptoError> {
        use rand::RngCore;
        let mut rng = OsRng;
        let mut ct = [0u8; KEM_CIPHERTEXT_LEN];
        rng.fill_bytes(&mut ct);
        let ss = stub_shared_secret(recipient_pk, &ct);
        Ok((ss, ct))
    }

    fn decap(
        &self,
        recipient_sk: &KemSecretKey,
        ciphertext: &KemCiphertext,
    ) -> Result<KemSharedSecret, CryptoError> {
        // Recover the embedded public key from the secret key and
        // recompute the deterministic shared secret.
        let mut pk = [0u8; KEM_PUBLIC_KEY_LEN];
        pk.copy_from_slice(&recipient_sk[..KEM_PUBLIC_KEY_LEN]);
        Ok(stub_shared_secret(&pk, ciphertext))
    }
}

fn stub_shared_secret(pk: &KemPublicKey, ct: &KemCiphertext) -> KemSharedSecret {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"knowledge-stub-kem-v1");
    hasher.update(pk);
    hasher.update(ct);
    *hasher.finalize().as_bytes()
}

fn check_len(label: &'static str, got: usize, expected: usize) -> Result<(), CryptoError> {
    if got == expected {
        Ok(())
    } else {
        let _ = label;
        Err(CryptoError::KemBufferLength { expected, got })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ml_kem_768_roundtrip() {
        let backend = MlKem768Backend;
        let (pk, sk) = backend.keypair().expect("keypair");
        let (ss_send, ct) = backend.encap(&pk).expect("encap");
        let ss_recv = backend.decap(&sk, &ct).expect("decap");
        assert_eq!(ss_send, ss_recv);
    }

    #[test]
    fn ml_kem_768_keypair_lengths() {
        let backend = MlKem768Backend;
        let (pk, sk) = backend.keypair().unwrap();
        assert_eq!(pk.len(), KEM_PUBLIC_KEY_LEN);
        assert_eq!(sk.len(), KEM_SECRET_KEY_LEN);
    }

    #[test]
    fn ml_kem_768_two_keypairs_differ() {
        let backend = MlKem768Backend;
        let (pk_a, _) = backend.keypair().unwrap();
        let (pk_b, _) = backend.keypair().unwrap();
        assert_ne!(pk_a, pk_b);
    }

    #[test]
    fn stub_backend_roundtrip() {
        let backend = StubKemBackend;
        let (pk, sk) = backend.keypair().unwrap();
        let (ss_send, ct) = backend.encap(&pk).unwrap();
        let ss_recv = backend.decap(&sk, &ct).unwrap();
        assert_eq!(ss_send, ss_recv);
    }
}
