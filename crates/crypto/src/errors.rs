//! Error type surfaced by every public function in this crate.

use thiserror::Error;

/// All errors that can be produced by `knowledge_crypto`.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// AEAD encryption failed (extremely rare for XChaCha20-Poly1305).
    #[error("AEAD encryption failed")]
    AeadEncryption,
    /// AEAD decryption failed — typically a wrong key, wrong nonce, or
    /// tampered ciphertext / AAD.
    #[error("AEAD decryption / authentication failed")]
    AeadDecryption,
    /// HKDF expand step rejected the requested output length.
    #[error("HKDF key derivation failed: {0}")]
    KeyDerivation(&'static str),
    /// A KEM key, ciphertext, or shared-secret buffer had the wrong length.
    #[error("KEM buffer length mismatch (expected {expected}, got {got})")]
    KemBufferLength {
        /// Expected length in bytes.
        expected: usize,
        /// Actual length in bytes.
        got: usize,
    },
    /// The KEM backend rejected the encapsulation / decapsulation call —
    /// usually a malformed key.
    #[error("KEM backend operation failed: {0}")]
    KemBackend(&'static str),
    /// The hybrid KEM combiner rejected its inputs.
    #[error("hybrid KEM combiner failed: {0}")]
    HybridCombiner(&'static str),
    /// Provenance bundle (de)serialization failed.
    #[error("provenance bundle serialisation failed: {0}")]
    ProvenanceSerialisation(&'static str),
    /// Provenance bundle signature verification failed (wrong key,
    /// tampered bundle, or wrong signing algorithm).
    #[error("provenance signature verification failed")]
    ProvenanceVerification,
    /// A key-exchange operation violated the active hybrid policy
    /// (e.g. classical-only when [`HybridMode::HybridTransition`] is
    /// enforced).
    #[error("hybrid policy violation: expected {expected}, got {got}")]
    HybridPolicyViolation {
        /// Primitive(s) the policy required.
        expected: &'static str,
        /// Primitive(s) the operation actually used.
        got: &'static str,
    },
    /// A [`crate::forgetting::TombstoneStore`] implementation
    /// rejected a tombstone persist or load call.
    ///
    /// `crypto` itself owns no on-disk storage — the
    /// [`crate::forgetting::TombstoneStore`] trait is the durability
    /// boundary that host crates (typically `ffi` /
    /// `evidence_store`) implement against their own SQLCipher
    /// tables. When that underlying I/O fails, the host wraps the
    /// driver-specific error into this string and surfaces it
    /// through the trait so the destroy code path can decide
    /// whether to retry on next open or alert.
    ///
    /// Unlike [`Self::KeyDerivation`] (which takes a `&'static
    /// str` for compile-time-known KDF tags), this variant holds
    /// an owned `String` because the underlying error message is
    /// supplied at runtime by whichever store implementation the
    /// host has wired in.
    #[error("tombstone persistence failed: {0}")]
    TombstonePersistence(String),
    /// An epoch counter overflowed `u64::MAX`.
    ///
    /// Surfaced by both [`crate::mls::MlsEpoch::next`] and
    /// [`crate::forgetting::EpochId::next`] — the substrate refuses
    /// to silently saturate at the terminal epoch because that would
    /// allow an unbounded sequence of "epoch advances" that did not
    /// actually advance, breaking forward-secrecy invariants
    /// (forgotten epochs could appear to coexist with new ones at
    /// the same id).
    ///
    /// For MLS groups, callers should treat this as a terminal
    /// condition and start a fresh group. For forgetting epochs,
    /// callers should rotate the scope to a fresh, smaller numbering
    /// (or escalate — reaching `u64::MAX` epochs on a single scope
    /// is overwhelmingly indicative of a logic bug).
    #[error("epoch counter overflow at u64::MAX")]
    EpochOverflow,
    /// Attestation verification was requested for a real TEE platform
    /// whose quote-signature / vendor-CA-chain verification is not yet
    /// implemented, so the substrate **fails closed** rather than
    /// trusting the report.
    ///
    /// [`crate::attestation::verify_attestation`] only knows how to
    /// fully validate the `Mock` platform today. For a real platform
    /// (`intel_tdx`, `amd_sev_snp`, `nitro_enclaves`) the only signal
    /// available without the platform-specific verification library is
    /// the `measurement` field — but that field is copied verbatim out
    /// of the (as-yet-unverified) quote document, so an untrusted host
    /// operator could forge a report carrying the expected measurement
    /// and no valid platform signature. Returning `Ok(true)` from a
    /// bare measurement comparison would therefore be fail-*open* in
    /// the exact threat model TEE attestation exists to defend against.
    /// The substrate refuses the attestation with this error until real
    /// quote verification lands, mirroring the fail-closed posture used
    /// everywhere else for not-yet-implemented production paths.
    ///
    /// The held string is the platform tag
    /// ([`crate::attestation::TeePlatform::as_str`]) that was rejected.
    #[error("attestation verification unsupported for TEE platform: {0}")]
    AttestationUnsupported(&'static str),
}
