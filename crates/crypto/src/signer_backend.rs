//! Signer-backend trait + the post-quantum [`MlDsa65Signer`] backend.
//!
//! The substrate lifts the provenance-signature algorithm from
//! HMAC-SHA256
//! ([`crate::provenance::TestSigner`]) to the FIPS 204 ML-DSA-65
//! lattice signature. This module ships:
//!
//! * [`SignerBackend`] — a small `(sign, verify)` interface mirroring
//!   the [`crate::kem::KemBackend`] pattern. Concrete crypto adapters
//!   plug in here so the rest of the substrate can hold a
//!   `Box<dyn ProvenanceSigner>` and stay algorithm-agnostic.
//! * [`MlDsa65Signer`] / [`MlDsa65Verifier`] — the FIPS 204 ML-DSA-65
//!   implementation backed by the `ml-dsa` crate (RustCrypto). The
//!   signer owns a key pair and emits ~3.3 kB lattice signatures; the
//!   verifier owns only the verifying key and is cheap to clone.
//! * [`MlDsa65EncodedKeyPair`] / [`MlDsa65EncodedVerifyingKey`] — wire
//!   forms used to persist or transmit ML-DSA-65 keys.
//!
//! The `ml-dsa` crate (0.1.0, FIPS 204 stable) backs this module.
//! If the API changes upstream, only this module needs to track the
//! delta — every other crate in the workspace consumes
//! [`crate::provenance::ProvenanceSigner`].

use ml_dsa::{
    signature::{Keypair, Signer, Verifier},
    EncodedVerifyingKey, Generate, MlDsa65, Seed, Signature, SigningKey, VerifyingKey,
};

use crate::errors::CryptoError;
use crate::provenance::{ProvenanceBundle, ProvenanceSignature, ProvenanceSigner, SignedBundle};

/// Minimal `(sign, verify)` interface mirroring [`crate::kem::KemBackend`].
///
/// Implementations are expected to operate on the canonical bundle
/// bytes produced by [`ProvenanceBundle::canonical_bytes`]. They must
/// not interpret or mutate the message — only sign / verify it.
pub trait SignerBackend {
    /// Sign `msg` with this backend's signing key.
    fn sign_bytes(&self, msg: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// Return `Ok(true)` iff `signature` is a valid signature over
    /// `msg` under this backend's verifying key.
    fn verify_bytes(&self, msg: &[u8], signature: &[u8]) -> Result<bool, CryptoError>;
}

/// Length, in bytes, of an encoded ML-DSA-65 signature.
///
/// Pinned to the FIPS 204 ML-DSA-65 spec; checked at runtime in
/// the round-trip test.
pub const ML_DSA_65_SIGNATURE_LEN: usize = 3309;

/// ML-DSA-65 (FIPS 204) signer backend.
///
/// Owns a freshly generated signing + verifying key. Use
/// [`Self::generate`] for a fresh key, or [`Self::from_signing_key_bytes`]
/// to restore one from a previously-encoded signing key.
pub struct MlDsa65Signer {
    signing_key: SigningKey<MlDsa65>,
    verifying_key: VerifyingKey<MlDsa65>,
}

impl MlDsa65Signer {
    /// Generate a fresh ML-DSA-65 key pair from the OS RNG.
    pub fn generate() -> Self {
        let sk = SigningKey::<MlDsa65>::generate();
        let vk = sk.verifying_key();
        Self {
            signing_key: sk,
            verifying_key: vk,
        }
    }

    /// Borrow the underlying [`SigningKey`].
    pub fn signing_key(&self) -> &SigningKey<MlDsa65> {
        &self.signing_key
    }

    /// Borrow the underlying [`VerifyingKey`].
    pub fn verifying_key(&self) -> &VerifyingKey<MlDsa65> {
        &self.verifying_key
    }

    /// Construct a verifier that only carries the verifying key.
    pub fn verifier(&self) -> MlDsa65Verifier {
        MlDsa65Verifier {
            verifying_key: self.verifying_key.clone(),
        }
    }

    /// Encode the signing + verifying keys for persistence / transport.
    pub fn encode(&self) -> MlDsa65EncodedKeyPair {
        MlDsa65EncodedKeyPair {
            signing_seed: self.signing_key.to_seed(),
            verifying_key: self.verifying_key.encode(),
        }
    }

    /// Decode a previously [`Self::encode`]-d key pair. The signing
    /// key is deterministically expanded from its 32-byte seed. As a
    /// safety check the freshly decoded keys are exercised against a
    /// fixed test message; if they do not validate against each
    /// other the caller has supplied mismatched halves and we error
    /// out.
    pub fn decode(encoded: &MlDsa65EncodedKeyPair) -> Result<Self, CryptoError> {
        let signing_key = SigningKey::<MlDsa65>::from_seed(&encoded.signing_seed);
        let verifying_key = VerifyingKey::<MlDsa65>::decode(&encoded.verifying_key);
        let probe = b"ml-dsa-65 keypair coherence probe";
        let signature: Signature<MlDsa65> = signing_key.sign(probe);
        if verifying_key.verify(probe, &signature).is_err() {
            return Err(CryptoError::ProvenanceVerification);
        }
        Ok(Self {
            signing_key,
            verifying_key,
        })
    }
}

/// Pre-encoded key pair for ML-DSA-65, suitable for persisting in
/// the audit trail or wrapping with the master key.
#[derive(Clone)]
pub struct MlDsa65EncodedKeyPair {
    /// 32-byte seed from which the full signing key is derived.
    pub signing_seed: Seed,
    /// Encoded verifying key (~2 kB).
    pub verifying_key: EncodedVerifyingKey<MlDsa65>,
}

/// Pre-encoded verifying key for ML-DSA-65 (the only half a verifier needs).
#[derive(Clone)]
pub struct MlDsa65EncodedVerifyingKey {
    /// Encoded verifying key (~2 kB).
    pub verifying_key: EncodedVerifyingKey<MlDsa65>,
}

/// ML-DSA-65 verifier — owns only the public verifying key.
#[derive(Clone)]
pub struct MlDsa65Verifier {
    verifying_key: VerifyingKey<MlDsa65>,
}

impl MlDsa65Verifier {
    /// Construct a verifier from an encoded verifying key.
    pub fn from_encoded(encoded: &MlDsa65EncodedVerifyingKey) -> Self {
        Self {
            verifying_key: VerifyingKey::<MlDsa65>::decode(&encoded.verifying_key),
        }
    }

    /// Encode the verifying key for persistence / transport.
    pub fn encode(&self) -> MlDsa65EncodedVerifyingKey {
        MlDsa65EncodedVerifyingKey {
            verifying_key: self.verifying_key.encode(),
        }
    }

    /// Verify `signature` against `msg`.
    pub fn verify_bytes(&self, msg: &[u8], signature: &[u8]) -> Result<bool, CryptoError> {
        let Ok(sig) = Signature::<MlDsa65>::try_from(signature) else {
            return Ok(false);
        };
        Ok(self.verifying_key.verify(msg, &sig).is_ok())
    }

    /// Verify a [`SignedBundle`] under this verifying key.
    pub fn verify_bundle(&self, signed: &SignedBundle) -> Result<bool, CryptoError> {
        let canonical = signed.bundle.canonical_bytes()?;
        self.verify_bytes(&canonical, signed.signature.as_bytes())
    }
}

/// `MlDsa65Signer` adapts the FIPS 204 [`MlDsa65`] backend to the
/// substrate-wide [`ProvenanceSigner`] trait.
impl ProvenanceSigner for MlDsa65Signer {
    fn sign(&self, bundle: ProvenanceBundle) -> Result<SignedBundle, CryptoError> {
        let canonical = bundle.canonical_bytes()?;
        let signature: Signature<MlDsa65> = self.signing_key.sign(&canonical);
        let signature_bytes = signature.encode().to_vec();
        Ok(SignedBundle {
            bundle,
            signature: ProvenanceSignature(signature_bytes),
        })
    }

    fn verify(&self, signed: &SignedBundle) -> Result<bool, CryptoError> {
        self.verifier().verify_bundle(signed)
    }
}

/// `SignerBackend` adapter for `MlDsa65Signer` so callers that hold
/// a backend object (rather than a `ProvenanceSigner`) can sign /
/// verify raw bytes.
impl SignerBackend for MlDsa65Signer {
    fn sign_bytes(&self, msg: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let signature: Signature<MlDsa65> = self.signing_key.sign(msg);
        Ok(signature.encode().to_vec())
    }

    fn verify_bytes(&self, msg: &[u8], signature: &[u8]) -> Result<bool, CryptoError> {
        let Ok(sig) = Signature::<MlDsa65>::try_from(signature) else {
            return Ok(false);
        };
        Ok(self.verifying_key.verify(msg, &sig).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::{
        AgentKind, EvidenceRef, ProvenanceAgent, ProvenanceBundle, SynthesisActivity,
    };
    use uuid::Uuid;

    fn fixture_bundle() -> ProvenanceBundle {
        ProvenanceBundle::new(Uuid::nil(),
            SynthesisActivity::new("synth-pipeline:elected:device-7",
                "bonsai-1.7b@q1_0_g128",
                "synth.summary.v1",
                Uuid::nil(),
            ),
            ProvenanceAgent::software("synthesizer:test"),
            vec![EvidenceRef::from_uuid(Uuid::nil())],
        )
    }

    #[test]
    fn keypair_generation_yields_distinct_keys() {
        let a = MlDsa65Signer::generate();
        let b = MlDsa65Signer::generate();
        assert_ne!(a.verifying_key().encode(), b.verifying_key().encode());
    }

    #[test]
    fn round_trip_signs_and_verifies() {
        let signer = MlDsa65Signer::generate();
        let bundle = fixture_bundle();
        let signed = signer.sign(bundle).expect("sign");
        assert!(signer.verify(&signed).expect("verify"));
        assert_eq!(signed.signature.as_bytes().len(), ML_DSA_65_SIGNATURE_LEN);
    }

    #[test]
    fn verifier_round_trips() {
        let signer = MlDsa65Signer::generate();
        let bundle = fixture_bundle();
        let signed = signer.sign(bundle).expect("sign");
        let verifier = signer.verifier();
        assert!(verifier.verify_bundle(&signed).expect("verify"));
    }

    #[test]
    fn encoded_verifying_key_round_trips() {
        let signer = MlDsa65Signer::generate();
        let bundle = fixture_bundle();
        let signed = signer.sign(bundle).expect("sign");
        let encoded = signer.verifier().encode();
        let restored = MlDsa65Verifier::from_encoded(&encoded);
        assert!(restored.verify_bundle(&signed).expect("verify"));
    }

    #[test]
    fn encoded_keypair_round_trips() {
        let signer = MlDsa65Signer::generate();
        let encoded = signer.encode();
        let restored = MlDsa65Signer::decode(&encoded).expect("decode");
        let bundle = fixture_bundle();
        let signed = restored.sign(bundle).expect("sign");
        assert!(signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn decode_rejects_mismatched_keypair() {
        let signer_a = MlDsa65Signer::generate();
        let signer_b = MlDsa65Signer::generate();
        let mismatched = MlDsa65EncodedKeyPair {
            signing_seed: signer_a.signing_key().to_seed(),
            verifying_key: signer_b.verifying_key().encode(),
        };
        match MlDsa65Signer::decode(&mismatched) {
            Err(CryptoError::ProvenanceVerification) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected mismatched keypair to be rejected"),
        }
    }

    #[test]
    fn wrong_key_fails_verification() {
        let alice = MlDsa65Signer::generate();
        let mallory = MlDsa65Signer::generate();
        let signed = alice.sign(fixture_bundle()).expect("sign");
        assert!(!mallory.verify(&signed).expect("verify"));
    }

    #[test]
    fn tampered_entity_id_fails_verification() {
        let signer = MlDsa65Signer::generate();
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.bundle.entity_id = Uuid::from_u128(0xdead_beef_dead_beef);
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn tampered_signature_fails_verification() {
        let signer = MlDsa65Signer::generate();
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.signature.0[0] ^= 0x01;
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn truncated_signature_fails_verification() {
        let signer = MlDsa65Signer::generate();
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.signature.0.truncate(100);
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn signer_backend_trait_round_trips() {
        let signer = MlDsa65Signer::generate();
        let msg = b"hello ml-dsa-65";
        let sig = signer.sign_bytes(msg).expect("sign");
        assert!(signer.verify_bytes(msg, &sig).expect("verify"));
        assert!(!signer.verify_bytes(b"different", &sig).expect("verify"));
    }

    #[test]
    fn agent_kind_tag_helper_smoke() {
        assert_eq!(AgentKind::Software.as_str(), "software");
    }
}
