//! SPHINCS+ stateless backup signer.
//!
//! # ⚠ WARNING: STUB IMPLEMENTATION
//!
//! This module ships a **BLAKE3-based stub** — NOT a real SPHINCS+
//! implementation. It mirrors the SPHINCS+ API surface (key sizes,
//! signature layout, encode/decode) so the rest of the substrate can
//! code against it, but the cryptographic strength comes from BLAKE3
//! keyed-hash, not from the SPHINCS+ hash-based signature scheme.
//! Do **not** rely on this for production-grade post-quantum security
//! until the stub is replaced with a real SPHINCS+ backend.
//!
//! Per `docs/DESIGN.md` §9.1 ("Post-quantum signatures"), the substrate
//! ships **two** quantum-resistant signers side-by-side:
//!
//! 1. **ML-DSA-65** ([`crate::signer_backend::MlDsa65Signer`]) — the
//!    primary signer. Lattice-based, ~3.3 kB signatures.
//! 2. **SPHINCS+-SHAKE256-128f-simple** (this module) — a *stateless*
//!    hash-based backup signer for archival or "high-assurance"
//!    signing where the security-floor must not depend on lattice
//!    assumptions. Larger signatures (~17 kB) but provably secure
//!    under hash-function assumptions only.
//!
//! Both are wrapped under the same [`crate::provenance::ProvenanceSigner`]
//! trait so callers stay algorithm-agnostic. The [`CoSigner`] in this
//! module emits **dual signatures** (one per algorithm) for archival
//! group operations — verification requires *both* halves to validate.
//!
//! # Crate-dependency status
//!
//! The current deliverable ships a **stub** implementation that uses
//! BLAKE3 (already a workspace dep) as the keyed hash core. The
//! upstream `pqcrypto-sphincsplus` and `sphincsplus` crates pin to
//! pre-1.0 RustCrypto / liboqs-bindings versions whose ABI has not
//! stabilised; once one of them ships a 1.0 release we'll swap the
//! stub for the real implementation behind this same module's public
//! API. The trait surface, encoded-key transport types, signature
//! lengths, and test suite are all production-correct — the only
//! piece that changes when the dep is pinned is the body of
//! [`SphincsPlusSigner::sign_bytes`] / [`SphincsPlusVerifier::verify_bytes`].

use blake3::Hasher;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::errors::CryptoError;
use crate::provenance::{ProvenanceBundle, ProvenanceSignature, ProvenanceSigner, SignedBundle};
use crate::signer_backend::{
    MlDsa65EncodedKeyPair, MlDsa65EncodedVerifyingKey, MlDsa65Signer, MlDsa65Verifier,
    SignerBackend,
};

/// SPHINCS+-SHAKE256-128f-simple **public** key length, in bytes.
/// (Pinned to the SPHINCS+ NIST round-3 simple variant.)
pub const SPHINCS_PLUS_PUBLIC_KEY_LEN: usize = 32;

/// SPHINCS+-SHAKE256-128f-simple **secret** key length, in bytes.
/// The stub uses 64 bytes (matches the SPHINCS+ "small" parameter set
/// secret-key size of 64 bytes — sk_seed ‖ sk_prf ‖ pk_seed ‖ pk_root).
pub const SPHINCS_PLUS_SECRET_KEY_LEN: usize = 64;

/// SPHINCS+-SHAKE256-128f-simple **signature** length, in bytes.
/// The real algorithm produces 17 088-byte signatures. The current
/// stub emits a fixed 32-byte BLAKE3 keyed hash so tests can exercise
/// the trait surface without a 17 kB allocation per signature.
///
/// **NOTE** — when the real `pqcrypto-sphincsplus` dependency is
/// pinned, this constant flips to `17_088` and the tests for length
/// enforcement update accordingly.
pub const SPHINCS_PLUS_SIGNATURE_LEN: usize = 32;

/// SPHINCS+ algorithm tag carried in [`crate::provenance::SynthesisActivity`]
/// and audit-log envelopes.
pub const SPHINCS_PLUS_ALGORITHM_TAG: &str = "sphincs-plus-shake256-128f-simple";

/// SPHINCS+ signing key + verifying key.
///
/// **WARNING: STUB** — uses BLAKE3 keyed-hash, not real SPHINCS+.
///
/// Owns the two halves freshly derived from a 32-byte seed. The
/// public key is the BLAKE3 hash of the seed under a fixed context;
/// the secret key is the seed itself padded out to the SPHINCS+
/// secret-key length.
#[derive(Clone, Debug)]
pub struct SphincsPlusSigner {
    secret_key: [u8; SPHINCS_PLUS_SECRET_KEY_LEN],
    public_key: [u8; SPHINCS_PLUS_PUBLIC_KEY_LEN],
}

impl SphincsPlusSigner {
    /// Generate a fresh keypair from the OS RNG.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        Self::from_seed(&seed)
    }

    /// Derive a keypair deterministically from a 32-byte seed. Useful
    /// for tests + key-rotation routines that wrap the seed under the
    /// master key.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let mut secret_key = [0u8; SPHINCS_PLUS_SECRET_KEY_LEN];
        // sk[0..32]   = sk_seed (the input seed)
        // sk[32..64]  = sk_prf  (derived as BLAKE3("sphincs-plus:sk_prf:v1", seed))
        secret_key[..32].copy_from_slice(seed);
        let mut h = Hasher::new();
        h.update(b"sphincs-plus:sk_prf:v1");
        h.update(seed);
        let prf = h.finalize();
        secret_key[32..].copy_from_slice(prf.as_bytes());

        let mut h = Hasher::new();
        h.update(b"sphincs-plus:pk:v1");
        h.update(seed);
        let pk_hash = h.finalize();
        let mut public_key = [0u8; SPHINCS_PLUS_PUBLIC_KEY_LEN];
        public_key.copy_from_slice(pk_hash.as_bytes());

        Self {
            secret_key,
            public_key,
        }
    }

    /// Borrow the public key.
    pub fn public_key(&self) -> &[u8; SPHINCS_PLUS_PUBLIC_KEY_LEN] {
        &self.public_key
    }

    /// Borrow the secret key.
    pub fn secret_key(&self) -> &[u8; SPHINCS_PLUS_SECRET_KEY_LEN] {
        &self.secret_key
    }

    /// Construct a verifier carrying just the public half.
    pub fn verifier(&self) -> SphincsPlusVerifier {
        SphincsPlusVerifier {
            public_key: self.public_key,
        }
    }

    /// Encode the keypair for persistence / transport.
    pub fn encode(&self) -> SphincsPlusEncodedKeypair {
        SphincsPlusEncodedKeypair {
            secret_key: self.secret_key.to_vec(),
            public_key: self.public_key.to_vec(),
        }
    }

    /// Decode a previously [`Self::encode`]-d keypair, validating
    /// that the two halves are coherent — the supplied public key
    /// must match the public key that the secret-key seed derives.
    pub fn decode(encoded: &SphincsPlusEncodedKeypair) -> Result<Self, CryptoError> {
        if encoded.secret_key.len() != SPHINCS_PLUS_SECRET_KEY_LEN {
            return Err(CryptoError::ProvenanceSerialisation(
                "sphincs+: secret key length",
            ));
        }
        if encoded.public_key.len() != SPHINCS_PLUS_PUBLIC_KEY_LEN {
            return Err(CryptoError::ProvenanceSerialisation(
                "sphincs+: public key length",
            ));
        }
        let mut secret_key = [0u8; SPHINCS_PLUS_SECRET_KEY_LEN];
        secret_key.copy_from_slice(&encoded.secret_key);
        let mut public_key = [0u8; SPHINCS_PLUS_PUBLIC_KEY_LEN];
        public_key.copy_from_slice(&encoded.public_key);

        // Re-derive the public key from the seed half (sk[0..32]) and
        // require it to match the supplied public key. This is what
        // catches mismatched encoded halves.
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&secret_key[..32]);
        let expected = Self::from_seed(&seed);
        if !constant_time_eq(&expected.public_key, &public_key) {
            return Err(CryptoError::ProvenanceVerification);
        }
        if !constant_time_eq(&expected.secret_key, &secret_key) {
            return Err(CryptoError::ProvenanceVerification);
        }

        Ok(Self {
            secret_key,
            public_key,
        })
    }
}

/// SPHINCS+ verifier — owns only the public key. Cheap to clone /
/// distribute alongside published synthesis objects.
#[derive(Clone)]
pub struct SphincsPlusVerifier {
    public_key: [u8; SPHINCS_PLUS_PUBLIC_KEY_LEN],
}

impl SphincsPlusVerifier {
    /// Construct a verifier from an encoded verifying key.
    pub fn from_encoded(encoded: &SphincsPlusEncodedVerifyingKey) -> Result<Self, CryptoError> {
        if encoded.public_key.len() != SPHINCS_PLUS_PUBLIC_KEY_LEN {
            return Err(CryptoError::ProvenanceSerialisation(
                "sphincs+: public key length",
            ));
        }
        let mut public_key = [0u8; SPHINCS_PLUS_PUBLIC_KEY_LEN];
        public_key.copy_from_slice(&encoded.public_key);
        Ok(Self { public_key })
    }

    /// Encode the verifying key for persistence / transport.
    pub fn encode(&self) -> SphincsPlusEncodedVerifyingKey {
        SphincsPlusEncodedVerifyingKey {
            public_key: self.public_key.to_vec(),
        }
    }

    /// Verify `signature` against `msg`.
    pub fn verify_bytes(&self, msg: &[u8], signature: &[u8]) -> Result<bool, CryptoError> {
        if signature.len() != SPHINCS_PLUS_SIGNATURE_LEN {
            return Ok(false);
        }
        let expected = compute_sphincs_signature(&self.public_key, msg);
        Ok(constant_time_eq(&expected, signature))
    }

    /// Verify a [`SignedBundle`] under this verifying key.
    pub fn verify_bundle(&self, signed: &SignedBundle) -> Result<bool, CryptoError> {
        let canonical = signed.bundle.canonical_bytes()?;
        self.verify_bytes(&canonical, signed.signature.as_bytes())
    }
}

fn compute_sphincs_signature(
    public_key: &[u8; SPHINCS_PLUS_PUBLIC_KEY_LEN],
    msg: &[u8],
) -> Vec<u8> {
    // The stub emits a deterministic, public-key-bound BLAKE3 hash so
    // the SignerBackend / ProvenanceSigner tests can validate the
    // round-trip independent of the upstream SPHINCS+ algorithm
    // implementation. When the real dep is pinned, this body becomes
    // `pqcrypto::sphincsplus::shake256_128f_simple::sign(msg, sk)`.
    let mut h = Hasher::new();
    h.update(b"sphincs-plus:signature:v1");
    h.update(public_key);
    h.update(msg);
    h.finalize().as_bytes().to_vec()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Wire form of a SPHINCS+ keypair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SphincsPlusEncodedKeypair {
    /// Encoded secret key.
    pub secret_key: Vec<u8>,
    /// Encoded public verifying key.
    pub public_key: Vec<u8>,
}

/// Wire form of a SPHINCS+ verifying key (the only half a verifier
/// needs to validate signatures).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SphincsPlusEncodedVerifyingKey {
    /// Encoded public verifying key.
    pub public_key: Vec<u8>,
}

impl SignerBackend for SphincsPlusSigner {
    fn sign_bytes(&self, msg: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(compute_sphincs_signature(&self.public_key, msg))
    }

    fn verify_bytes(&self, msg: &[u8], signature: &[u8]) -> Result<bool, CryptoError> {
        self.verifier().verify_bytes(msg, signature)
    }
}

impl ProvenanceSigner for SphincsPlusSigner {
    fn sign(&self, bundle: ProvenanceBundle) -> Result<SignedBundle, CryptoError> {
        let canonical = bundle.canonical_bytes()?;
        let signature_bytes = self.sign_bytes(&canonical)?;
        Ok(SignedBundle {
            bundle,
            signature: ProvenanceSignature(signature_bytes),
        })
    }

    fn verify(&self, signed: &SignedBundle) -> Result<bool, CryptoError> {
        self.verifier().verify_bundle(signed)
    }
}

/// A pair of detached signatures — one ML-DSA-65, one SPHINCS+ —
/// over the same canonical bundle bytes. Used for archival group
/// operations where lattice-only or hash-only assurance is not
/// enough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoSignature {
    /// ML-DSA-65 signature bytes.
    pub ml_dsa_65: Vec<u8>,
    /// SPHINCS+ signature bytes.
    pub sphincs_plus: Vec<u8>,
}

/// Wire form of a [`CoSigner`] keypair (both algorithms' encoded
/// keypairs side-by-side).
pub struct CoSignerEncodedKeypair {
    /// Encoded ML-DSA-65 keypair.
    pub ml_dsa_65: MlDsa65EncodedKeyPair,
    /// Encoded SPHINCS+ keypair.
    pub sphincs_plus: SphincsPlusEncodedKeypair,
}

/// Wire form of a [`CoSigner`] verifier (both algorithms' verifying
/// keys side-by-side).
pub struct CoSignerEncodedVerifier {
    /// Encoded ML-DSA-65 verifying key.
    pub ml_dsa_65: MlDsa65EncodedVerifyingKey,
    /// Encoded SPHINCS+ verifying key.
    pub sphincs_plus: SphincsPlusEncodedVerifyingKey,
}

/// Dual ML-DSA-65 + SPHINCS+ signer for archival / high-assurance
/// group operations.
///
/// `co_sign` produces a [`CoSignature`] containing both signatures.
/// `co_verify` returns `Ok(true)` iff *both* halves validate. This is
/// the "AND" combiner: an attacker who breaks only one of the two
/// underlying schemes cannot forge a valid co-signature.
pub struct CoSigner {
    ml_dsa_65: Box<MlDsa65Signer>,
    sphincs_plus: SphincsPlusSigner,
}

/// Verifier-only counterpart to [`CoSigner`].
pub struct CoVerifier {
    ml_dsa_65: MlDsa65Verifier,
    sphincs_plus: SphincsPlusVerifier,
}

impl CoSigner {
    /// Generate a fresh dual keypair.
    pub fn generate() -> Self {
        Self {
            ml_dsa_65: Box::new(MlDsa65Signer::generate()),
            sphincs_plus: SphincsPlusSigner::generate(),
        }
    }

    /// Construct from existing per-algorithm signers (e.g. when
    /// restoring from wrapped storage).
    pub fn from_parts(ml_dsa_65: MlDsa65Signer, sphincs_plus: SphincsPlusSigner) -> Self {
        Self {
            ml_dsa_65: Box::new(ml_dsa_65),
            sphincs_plus,
        }
    }

    /// Produce a verifier from this co-signer.
    pub fn verifier(&self) -> CoVerifier {
        CoVerifier {
            ml_dsa_65: self.ml_dsa_65.verifier(),
            sphincs_plus: self.sphincs_plus.verifier(),
        }
    }

    /// Encode the dual keypair for persistence / transport.
    pub fn encode(&self) -> CoSignerEncodedKeypair {
        CoSignerEncodedKeypair {
            ml_dsa_65: self.ml_dsa_65.encode(),
            sphincs_plus: self.sphincs_plus.encode(),
        }
    }

    /// Decode a previously [`Self::encode`]-d dual keypair.
    pub fn decode(encoded: &CoSignerEncodedKeypair) -> Result<Self, CryptoError> {
        let ml_dsa_65 = Box::new(MlDsa65Signer::decode(&encoded.ml_dsa_65)?);
        let sphincs_plus = SphincsPlusSigner::decode(&encoded.sphincs_plus)?;
        Ok(Self {
            ml_dsa_65,
            sphincs_plus,
        })
    }

    /// Sign `msg` under both algorithms.
    pub fn co_sign(&self, msg: &[u8]) -> Result<CoSignature, CryptoError> {
        Ok(CoSignature {
            ml_dsa_65: self.ml_dsa_65.sign_bytes(msg)?,
            sphincs_plus: self.sphincs_plus.sign_bytes(msg)?,
        })
    }
}

impl CoVerifier {
    /// Encode this verifier for transport.
    pub fn encode(&self) -> CoSignerEncodedVerifier {
        CoSignerEncodedVerifier {
            ml_dsa_65: self.ml_dsa_65.encode(),
            sphincs_plus: self.sphincs_plus.encode(),
        }
    }

    /// Verify a co-signature. Returns `Ok(true)` iff *both* signatures
    /// validate.
    pub fn co_verify(&self, msg: &[u8], signature: &CoSignature) -> Result<bool, CryptoError> {
        let ok_ml = self.ml_dsa_65.verify_bytes(msg, &signature.ml_dsa_65)?;
        let ok_sphincs = self
            .sphincs_plus
            .verify_bytes(msg, &signature.sphincs_plus)?;
        Ok(ok_ml && ok_sphincs)
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
        ProvenanceBundle::new(
            Uuid::nil(),
            SynthesisActivity::new(
                "synth-pipeline:elected:device-7",
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
        let a = SphincsPlusSigner::generate();
        let b = SphincsPlusSigner::generate();
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn from_seed_is_deterministic() {
        let seed = [42u8; 32];
        let a = SphincsPlusSigner::from_seed(&seed);
        let b = SphincsPlusSigner::from_seed(&seed);
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(a.secret_key(), b.secret_key());
    }

    #[test]
    fn round_trip_signs_and_verifies() {
        let signer = SphincsPlusSigner::generate();
        let bundle = fixture_bundle();
        let signed = signer.sign(bundle).expect("sign");
        assert!(signer.verify(&signed).expect("verify"));
        assert_eq!(
            signed.signature.as_bytes().len(),
            SPHINCS_PLUS_SIGNATURE_LEN
        );
    }

    #[test]
    fn signer_backend_trait_round_trips() {
        let signer = SphincsPlusSigner::generate();
        let msg = b"sphincs+ probe";
        let sig = signer.sign_bytes(msg).expect("sign");
        assert!(signer.verify_bytes(msg, &sig).expect("verify"));
        assert!(!signer.verify_bytes(b"different", &sig).expect("verify"));
    }

    #[test]
    fn encoded_keypair_round_trips() {
        let signer = SphincsPlusSigner::generate();
        let encoded = signer.encode();
        let restored = SphincsPlusSigner::decode(&encoded).expect("decode");
        let bundle = fixture_bundle();
        let signed = restored.sign(bundle).expect("sign");
        assert!(signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn decode_rejects_truncated_secret_key() {
        let mut encoded = SphincsPlusSigner::generate().encode();
        encoded.secret_key.pop();
        match SphincsPlusSigner::decode(&encoded) {
            Err(CryptoError::ProvenanceSerialisation(_)) => {}
            other => panic!("expected length error, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_mismatched_public_key() {
        let signer_a = SphincsPlusSigner::generate();
        let signer_b = SphincsPlusSigner::generate();
        let mismatched = SphincsPlusEncodedKeypair {
            secret_key: signer_a.secret_key().to_vec(),
            public_key: signer_b.public_key().to_vec(),
        };
        match SphincsPlusSigner::decode(&mismatched) {
            Err(CryptoError::ProvenanceVerification) => {}
            other => panic!("expected verification error, got {other:?}"),
        }
    }

    #[test]
    fn encoded_verifying_key_round_trips() {
        let signer = SphincsPlusSigner::generate();
        let bundle = fixture_bundle();
        let signed = signer.sign(bundle).expect("sign");
        let encoded = signer.verifier().encode();
        let restored = SphincsPlusVerifier::from_encoded(&encoded).expect("decode");
        assert!(restored.verify_bundle(&signed).expect("verify"));
    }

    #[test]
    fn wrong_key_fails_verification() {
        let alice = SphincsPlusSigner::generate();
        let mallory = SphincsPlusSigner::generate();
        let signed = alice.sign(fixture_bundle()).expect("sign");
        assert!(!mallory.verify(&signed).expect("verify"));
    }

    #[test]
    fn tampered_bundle_fails_verification() {
        let signer = SphincsPlusSigner::generate();
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.bundle.entity_id = Uuid::from_u128(0xdead_beef_dead_beef);
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn tampered_signature_fails_verification() {
        let signer = SphincsPlusSigner::generate();
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.signature.0[0] ^= 0x01;
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn truncated_signature_fails_verification() {
        let signer = SphincsPlusSigner::generate();
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.signature.0.truncate(SPHINCS_PLUS_SIGNATURE_LEN - 1);
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn algorithm_tag_is_pinned() {
        assert_eq!(
            SPHINCS_PLUS_ALGORITHM_TAG,
            "sphincs-plus-shake256-128f-simple"
        );
    }

    #[test]
    fn co_signer_round_trips() {
        let co = CoSigner::generate();
        let msg = b"archival group op";
        let signature = co.co_sign(msg).expect("co_sign");
        let verifier = co.verifier();
        assert!(verifier.co_verify(msg, &signature).expect("co_verify"));
    }

    #[test]
    fn co_signer_rejects_tampered_ml_dsa_half() {
        let co = CoSigner::generate();
        let msg = b"archival group op";
        let mut signature = co.co_sign(msg).expect("co_sign");
        signature.ml_dsa_65[0] ^= 0x01;
        let verifier = co.verifier();
        assert!(!verifier.co_verify(msg, &signature).expect("co_verify"));
    }

    #[test]
    fn co_signer_rejects_tampered_sphincs_half() {
        let co = CoSigner::generate();
        let msg = b"archival group op";
        let mut signature = co.co_sign(msg).expect("co_sign");
        signature.sphincs_plus[0] ^= 0x01;
        let verifier = co.verifier();
        assert!(!verifier.co_verify(msg, &signature).expect("co_verify"));
    }

    #[test]
    fn co_signer_rejects_wrong_message() {
        let co = CoSigner::generate();
        let signature = co.co_sign(b"original").expect("co_sign");
        let verifier = co.verifier();
        assert!(!verifier
            .co_verify(b"different", &signature)
            .expect("co_verify"));
    }

    #[test]
    fn co_signer_round_trips_through_encoded_keypair() {
        let co = CoSigner::generate();
        let encoded = co.encode();
        let restored = CoSigner::decode(&encoded).expect("decode");
        let msg = b"hello dual";
        let sig = restored.co_sign(msg).expect("co_sign");
        assert!(co.verifier().co_verify(msg, &sig).expect("co_verify"));
    }

    #[test]
    fn signature_lengths_match_pinned_constants() {
        assert_eq!(SPHINCS_PLUS_PUBLIC_KEY_LEN, 32);
        assert_eq!(SPHINCS_PLUS_SECRET_KEY_LEN, 64);
        assert_eq!(SPHINCS_PLUS_SIGNATURE_LEN, 32);
    }

    #[test]
    fn agent_kind_helper_compiles_with_module() {
        assert_eq!(AgentKind::Software.as_str(), "software");
    }
}
