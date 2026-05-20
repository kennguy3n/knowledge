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
}
