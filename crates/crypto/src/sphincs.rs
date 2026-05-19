//! SPHINCS+ stateless backup signer.
//!
//! Real, production SPHINCS+-SHAKE-128f-simple signatures backed by
//! [PQClean] via the [`pqcrypto-sphincsplus`] crate (CC0-licensed C
//! reference implementation wrapped by an MIT/Apache safe Rust
//! façade). The substrate uses this signer as the *stateless,
//! hash-based* counterpart to the lattice-based ML-DSA-65 path —
//! together they form an AND-combiner whose security floor only
//! falls if **both** primitives are broken.
//!
//! Per `docs/DESIGN.md` §9.1 ("Post-quantum signatures"), the substrate
//! ships **two** quantum-resistant signers side-by-side:
//!
//! 1. **ML-DSA-65** ([`crate::signer_backend::MlDsa65Signer`]) — the
//!    primary signer. Lattice-based, ~3.3 kB signatures.
//! 2. **SPHINCS+-SHAKE-128f-simple** (this module) — a *stateless*
//!    hash-based backup signer for archival or "high-assurance"
//!    signing where the security-floor must not depend on lattice
//!    assumptions. Larger signatures (17,088 bytes) but provably
//!    secure under hash-function assumptions only.
//!
//! Both are wrapped under the same [`crate::provenance::ProvenanceSigner`]
//! trait so callers stay algorithm-agnostic. The [`CoSigner`] in this
//! module emits **dual signatures** (one per algorithm) for archival
//! group operations — verification requires *both* halves to validate.
//!
//! # Implementation notes
//!
//! * **Variant.** SPHINCS+-SHAKE-128f-simple is the NIST round-3
//!   "simple" parameter set at 128-bit security with the "fast"
//!   tree-height tuning. The PQClean reference implementation is
//!   public-domain (CC0).
//! * **Signature size.** 17,088 bytes per signature — large but
//!   bounded and predictable. Use SPHINCS+ only on the archival
//!   [`CoSigner`] path, not on per-synthesis provenance (which uses
//!   ML-DSA-65's ~3.3 kB signatures). See `ARCHITECTURE.md` §8.1 for
//!   the policy.
//! * **Key sizes.** Public key 32 B, secret key 64 B (the PQClean
//!   `SK.seed ‖ SK.prf ‖ PK.seed ‖ PK.root` layout).
//! * **`unsafe` boundary.** The `pqcrypto-sphincsplus` crate uses
//!   `unsafe` internally for the C FFI; that's fine — this crate's
//!   `unsafe_code = "forbid"` lint only governs source code within
//!   this crate, not transitive dependencies compiled in isolation.
//! * **Determinism / seeded keygen.** PQClean does not expose
//!   `crypto_sign_seed_keypair` through the safe Rust façade, so we
//!   do not advertise a deterministic 32-byte-seed constructor. Use
//!   [`SphincsPlusSigner::generate`] for fresh keys and
//!   [`SphincsPlusSigner::decode`] (with the full encoded keypair)
//!   to restore from storage.
//! * **Secret-key hygiene.** `pqcrypto_sphincsplus::SecretKey` is an
//!   opaque C-FFI wrapper that does not implement `ZeroizeOnDrop`, so
//!   the long-lived state on [`SphincsPlusSigner`] holds the encoded
//!   secret-key bytes in a `Zeroizing<Vec<u8>>` heap buffer (wiped
//!   on drop) and re-parses to the PQClean type per-operation. This
//!   matches the hygiene provided by `MlDsa65Signer` (whose upstream
//!   `ml-dsa` crate offers `ZeroizeOnDrop` via the workspace's
//!   `features = ["zeroize"]` opt-in). See
//!   [`SphincsPlusSigner`] for the full rationale.
//!
//! [PQClean]: https://github.com/PQClean/PQClean
//! [`pqcrypto-sphincsplus`]: https://docs.rs/pqcrypto-sphincsplus

use pqcrypto_sphincsplus::sphincsshake128fsimple as sphincs_inner;
use pqcrypto_traits::sign::{DetachedSignature as _, PublicKey as _, SecretKey as _};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::errors::CryptoError;
use crate::provenance::{ProvenanceBundle, ProvenanceSignature, ProvenanceSigner, SignedBundle};
use crate::signer_backend::{
    MlDsa65EncodedKeyPair, MlDsa65EncodedVerifyingKey, MlDsa65Signer, MlDsa65Verifier,
    SignerBackend,
};

/// SPHINCS+-SHAKE-128f-simple **public** key length, in bytes.
///
/// Sourced from `pqcrypto-sphincsplus`'s `public_key_bytes()` so it
/// stays in lock-step with the upstream PQClean parameter set. A
/// `const_assert!` below pins the expected value (32) at compile
/// time — an upstream bump that changed the parameter set would
/// surface as a build failure rather than a silent ABI mismatch.
pub const SPHINCS_PLUS_PUBLIC_KEY_LEN: usize = sphincs_inner::public_key_bytes();

/// SPHINCS+-SHAKE-128f-simple **secret** key length, in bytes.
///
/// PQClean stores the secret key as `SK.seed ‖ SK.prf ‖ PK.seed ‖
/// PK.root` — four 16-byte halves at the 128-bit security level —
/// yielding 64 bytes total.
pub const SPHINCS_PLUS_SECRET_KEY_LEN: usize = sphincs_inner::secret_key_bytes();

/// SPHINCS+-SHAKE-128f-simple **signature** length, in bytes (17,088).
///
/// This is the fixed detached-signature size emitted by the PQClean
/// reference implementation; the substrate enforces strict equality
/// at verification time so a truncated or padded transport blob
/// cannot pass through unchallenged.
pub const SPHINCS_PLUS_SIGNATURE_LEN: usize = sphincs_inner::signature_bytes();

// Compile-time guards: catch any upstream bump that silently changes
// the parameter set. The numeric pins are the NIST round-3 SHAKE-128f-
// simple values; flipping these requires a deliberate audit.
const _: () = assert!(SPHINCS_PLUS_PUBLIC_KEY_LEN == 32);
const _: () = assert!(SPHINCS_PLUS_SECRET_KEY_LEN == 64);
const _: () = assert!(SPHINCS_PLUS_SIGNATURE_LEN == 17_088);

/// SPHINCS+ algorithm tag carried in [`crate::provenance::SynthesisActivity`]
/// and audit-log envelopes.
///
/// Uses the canonical PQClean / NIST round-3 final naming
/// (`sphincs-shake-128f-simple`) for the variant — the older form
/// `sphincs-shake256-128f-simple` referred to the same parameter set
/// but was deprecated when PQClean dropped the redundant `256` (the
/// only SHAKE variant SPHINCS+ uses is SHAKE-256 as a variable-output
/// XOF, so naming it explicitly carried no information). The `plus`
/// prefix is retained so downstream consumers can disambiguate this
/// from non-SPHINCS+ SHAKE-based signatures.
pub const SPHINCS_PLUS_ALGORITHM_TAG: &str = "sphincs-plus-shake-128f-simple";

/// SPHINCS+ signing key + verifying key (real PQClean-backed).
///
/// Owns the two halves emitted by `pqcrypto_sphincsplus::keypair()`.
/// The wrapper exposes only `&[u8]` views of the key material so
/// callers can persist / transport the bytes without touching the
/// FFI types directly.
///
/// # Secret-key hygiene
///
/// The long-lived secret-key state is held as `Zeroizing<Vec<u8>>`
/// rather than `pqcrypto_sphincsplus::SecretKey`. The PQClean wrapper
/// type is an opaque C-FFI struct that does **not** implement
/// `ZeroizeOnDrop`, so storing it directly would leak the 64-byte
/// secret-key material on every signer drop. Holding the bytes in a
/// `Zeroizing<Vec<u8>>` ensures the heap buffer is wiped when the
/// signer goes out of scope (consistent with `MlDsa65Signer`, whose
/// upstream `ml-dsa` crate provides `ZeroizeOnDrop` via the
/// workspace's `features = ["zeroize"]` opt-in).
///
/// The trade-off is that signing re-parses the bytes into the PQClean
/// `SecretKey` on every call. Parsing is a 64-byte copy that runs in
/// ~µs whereas SPHINCS+-SHAKE-128f-simple signing itself runs in
/// ~tens of ms, so the overhead is negligible. The transient parsed
/// `SecretKey` lives only for the duration of the sign call; the
/// long-lived sensitive state is the only material we can guarantee
/// to wipe without crossing the workspace `unsafe_code = "forbid"`
/// boundary.
#[derive(Clone)]
pub struct SphincsPlusSigner {
    /// Encoded secret-key bytes, wiped on drop.
    secret_key: Zeroizing<Vec<u8>>,
    public_key: sphincs_inner::PublicKey,
}

impl std::fmt::Debug for SphincsPlusSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SphincsPlusSigner")
            .field("algorithm", &SPHINCS_PLUS_ALGORITHM_TAG)
            .field("public_key_len", &SPHINCS_PLUS_PUBLIC_KEY_LEN)
            .field("secret_key_len", &SPHINCS_PLUS_SECRET_KEY_LEN)
            .finish()
    }
}

impl SphincsPlusSigner {
    /// Generate a fresh SPHINCS+-SHAKE-128f-simple keypair from the
    /// PQClean PRNG (which seeds itself from the OS RNG on first
    /// use).
    pub fn generate() -> Self {
        let (public_key, secret_key) = sphincs_inner::keypair();
        let secret_key = Zeroizing::new(secret_key.as_bytes().to_vec());
        Self {
            secret_key,
            public_key,
        }
    }

    /// Borrow the public key as a byte slice (32 bytes).
    pub fn public_key(&self) -> &[u8] {
        self.public_key.as_bytes()
    }

    /// Borrow the secret key as a byte slice (64 bytes).
    ///
    /// The returned slice borrows from the zeroize-on-drop heap
    /// buffer; callers should not copy the bytes anywhere that does
    /// not provide equivalent zeroization on drop.
    pub fn secret_key(&self) -> &[u8] {
        &self.secret_key
    }

    /// Re-parse the stored secret-key bytes into the PQClean opaque
    /// type. Centralised so every operation that needs a transient
    /// `SecretKey` (sign, coherence probe) goes through one place
    /// and uses the same error mapping.
    fn parsed_secret_key(&self) -> Result<sphincs_inner::SecretKey, CryptoError> {
        sphincs_inner::SecretKey::from_bytes(&self.secret_key)
            .map_err(|_| CryptoError::ProvenanceSerialisation("sphincs+: secret key length"))
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
            public_key: self.public_key.as_bytes().to_vec(),
        }
    }

    /// Decode a previously [`Self::encode`]-d keypair, validating
    /// that the two halves are coherent.
    ///
    /// Coherence is verified end-to-end by signing a fixed probe
    /// message with the decoded secret key and verifying that
    /// signature with the decoded public key. This catches:
    ///
    /// * wrong-length encoded halves (length check up front),
    /// * a public key spliced from a different keypair (probe
    ///   signature fails to verify),
    /// * any silent corruption of the secret-key material that
    ///   produces structurally-valid bytes but does not sign
    ///   correctly.
    ///
    /// We deliberately do not re-derive the public key from the
    /// secret-key seed half: PQClean does not expose seeded keygen
    /// through the safe Rust façade, and an end-to-end probe is
    /// strictly stronger anyway (it exercises both halves through
    /// the real `sign` + `verify` paths).
    pub fn decode(encoded: &SphincsPlusEncodedKeypair) -> Result<Self, CryptoError> {
        // Parse the secret key into the PQClean opaque type to
        // validate length (PQClean's `from_bytes` enforces 64 bytes).
        // The parsed `SecretKey` is held only long enough to drive
        // the coherence probe; the long-lived state below is the
        // raw bytes in a zeroize-on-drop buffer.
        let parsed_secret = sphincs_inner::SecretKey::from_bytes(&encoded.secret_key)
            .map_err(|_| CryptoError::ProvenanceSerialisation("sphincs+: secret key length"))?;
        let public_key = sphincs_inner::PublicKey::from_bytes(&encoded.public_key)
            .map_err(|_| CryptoError::ProvenanceSerialisation("sphincs+: public key length"))?;

        // End-to-end coherence probe: sign+verify a fixed canonical
        // message. Any mismatch between the two halves (wrong pk for
        // this sk, corrupted sk, …) surfaces as a verification
        // failure rather than a silent key swap.
        let probe = b"sphincs-plus keypair coherence probe v1";
        let signature = sphincs_inner::detached_sign(probe, &parsed_secret);
        if sphincs_inner::verify_detached_signature(&signature, probe, &public_key).is_err() {
            return Err(CryptoError::ProvenanceVerification);
        }

        // Store the validated bytes in a zeroize-on-drop heap buffer.
        let secret_key = Zeroizing::new(parsed_secret.as_bytes().to_vec());
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
    public_key: sphincs_inner::PublicKey,
}

impl SphincsPlusVerifier {
    /// Construct a verifier from an encoded verifying key.
    pub fn from_encoded(encoded: &SphincsPlusEncodedVerifyingKey) -> Result<Self, CryptoError> {
        let public_key = sphincs_inner::PublicKey::from_bytes(&encoded.public_key)
            .map_err(|_| CryptoError::ProvenanceSerialisation("sphincs+: public key length"))?;
        Ok(Self { public_key })
    }

    /// Encode the verifying key for persistence / transport.
    pub fn encode(&self) -> SphincsPlusEncodedVerifyingKey {
        SphincsPlusEncodedVerifyingKey {
            public_key: self.public_key.as_bytes().to_vec(),
        }
    }

    /// Verify `signature` against `msg`.
    ///
    /// Returns `Ok(true)` iff the signature is the canonical 17,088
    /// bytes and PQClean's detached-signature verifier accepts it
    /// against `msg` under this public key. Any structural defect
    /// (wrong length, parser rejection) and any cryptographic
    /// failure both surface as `Ok(false)` — the trait contract is
    /// "did this verify?", not "was this well-formed?".
    pub fn verify_bytes(&self, msg: &[u8], signature: &[u8]) -> Result<bool, CryptoError> {
        // Strict length check up front. PQClean's `from_bytes`
        // already rejects oversize blobs but accepts undersize ones
        // by zero-padding, so we enforce equality here to keep the
        // "truncated signature fails verification" contract.
        if signature.len() != SPHINCS_PLUS_SIGNATURE_LEN {
            return Ok(false);
        }
        let Ok(parsed) = sphincs_inner::DetachedSignature::from_bytes(signature) else {
            return Ok(false);
        };
        // `VerificationError` is `#[non_exhaustive]` upstream, so we
        // collapse every error variant to "did not verify" rather
        // than pretending the substrate can distinguish them. Any
        // failure mode — invalid signature, unknown error, future
        // variants — surfaces as `Ok(false)`.
        Ok(sphincs_inner::verify_detached_signature(&parsed, msg, &self.public_key).is_ok())
    }

    /// Verify a [`SignedBundle`] under this verifying key.
    pub fn verify_bundle(&self, signed: &SignedBundle) -> Result<bool, CryptoError> {
        let canonical = signed.bundle.canonical_bytes()?;
        self.verify_bytes(&canonical, signed.signature.as_bytes())
    }
}

/// Wire form of a SPHINCS+ keypair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SphincsPlusEncodedKeypair {
    /// Encoded secret key (64 bytes).
    pub secret_key: Vec<u8>,
    /// Encoded public verifying key (32 bytes).
    pub public_key: Vec<u8>,
}

/// Wire form of a SPHINCS+ verifying key (the only half a verifier
/// needs to validate signatures).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SphincsPlusEncodedVerifyingKey {
    /// Encoded public verifying key (32 bytes).
    pub public_key: Vec<u8>,
}

impl SignerBackend for SphincsPlusSigner {
    fn sign_bytes(&self, msg: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let secret_key = self.parsed_secret_key()?;
        let sig = sphincs_inner::detached_sign(msg, &secret_key);
        Ok(sig.as_bytes().to_vec())
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
    /// SPHINCS+ signature bytes (17,088 bytes).
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
    fn oversize_signature_fails_verification() {
        let signer = SphincsPlusSigner::generate();
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.signature.0.push(0x42);
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn algorithm_tag_is_pinned() {
        assert_eq!(SPHINCS_PLUS_ALGORITHM_TAG, "sphincs-plus-shake-128f-simple");
    }

    /// Regression guard: the long-lived secret-key buffer must
    /// preserve the *exact* PQClean-emitted bytes across a sign
    /// round-trip. This protects against accidental future refactors
    /// that swap the `Zeroizing<Vec<u8>>` field for an opaque
    /// PQClean type whose `as_bytes()` differs in length / layout, or
    /// that introduce truncation in the parse-on-demand path.
    #[test]
    fn zeroizing_secret_key_buffer_preserves_pqclean_bytes() {
        let signer = SphincsPlusSigner::generate();
        let stored = signer.secret_key().to_vec();
        assert_eq!(stored.len(), SPHINCS_PLUS_SECRET_KEY_LEN);

        // Round-trip through encode/decode and confirm the secret
        // bytes survive the Zeroizing<Vec<u8>> -> SecretKey ->
        // Zeroizing<Vec<u8>> trip unchanged.
        let encoded = signer.encode();
        let restored = SphincsPlusSigner::decode(&encoded).expect("decode");
        assert_eq!(restored.secret_key(), stored.as_slice());
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
        assert_eq!(SPHINCS_PLUS_SIGNATURE_LEN, 17_088);
    }

    #[test]
    fn agent_kind_helper_compiles_with_module() {
        assert_eq!(AgentKind::Software.as_str(), "software");
    }
}
