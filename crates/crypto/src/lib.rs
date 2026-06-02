//! `knowledge_crypto` — post-quantum cryptographic primitives for the
//! Knowledge substrate.
//!
//! This crate implements the high-level cryptographic API consumed by the
//! rest of the Rust shared core (see `ARCHITECTURE.md` §2.5 and §8). It
//! deliberately exposes a small, opinionated surface so that the rest of
//! the substrate never touches raw cryptographic state directly.
//!
//! # What is implemented
//!
//! * **BLAKE3** content hashing for evidence-body integrity framing.
//! * **XChaCha20-Poly1305 AEAD** for per-scope, per-epoch symmetric
//!   encryption of evidence bodies and cold archive segments.
//! * **HKDF-SHA256** key derivation from a per-user master key, scoped to
//!   a context label.
//! * **Hybrid X25519 + ML-KEM-768** key encapsulation with a
//!   concatenate-then-KDF combiner (`HKDF-SHA256`) — both halves are
//!   real, post-quantum ML-KEM-768 is provided by the `ml-kem` RustCrypto
//!   crate.
//! * **ML-DSA-65** (FIPS 204 lattice signatures) for per-synthesis
//!   provenance, backed by the `ml-dsa` RustCrypto crate with
//!   `ZeroizeOnDrop` on long-lived secret-key state. See
//!   [`signer_backend::MlDsa65Signer`].
//! * **SPHINCS+-SHAKE-128f-simple** (stateless hash-based signatures)
//!   for the archival AND-combiner co-signing path, backed by the
//!   PQClean reference implementation via the `pqcrypto-sphincsplus`
//!   crate, with zeroize-on-drop on long-lived secret-key state. See
//!   [`sphincs::SphincsPlusSigner`] and [`sphincs::CoSigner`].
//! * A [`KemBackend`] trait so the ML-KEM-768 side can be swapped for an
//!   FFI-backed implementation (`liboqs`) later without touching the rest
//!   of the substrate. A [`signer_backend::SignerBackend`] trait
//!   abstracts the two signature backends so [`sphincs::CoSigner`] can
//!   AND-combine the lattice (ML-DSA-65) and hash-based (SPHINCS+) halves
//!   without coupling to either implementation.
//!
//! # Test-only types (`test-support` feature)
//!
//! `CONTRIBUTING.md` requires that test-only types be gated behind
//! `cfg(any(test, feature = "test-support"))` AND documented in the
//! crate's top-level doc comment. The `test-support` feature is
//! declared in `Cargo.toml` as a no-op feature flag (no transitive
//! dependencies); enabling it exposes the following deterministic
//! mocks for unit tests, integration tests, fuzzers, and the
//! `demo` binary:
//!
//! * `kem::StubKemBackend` — fixed-output [`KemBackend`] used to
//!   make hybrid-KEM tests reproducible.
//! * `provenance::TestSigner` / `provenance::TEST_SIGNER_KEY_LEN` —
//!   deterministic [`provenance::ProvenanceSigner`] for provenance
//!   round-trip tests.
//! * `forgetting::DeterministicEpochKeySource` — deterministic
//!   [`forgetting::EpochKeySource`] for `EpochManager` rotation
//!   tests; derives keys as
//!   `BLAKE3("test-epoch-key" || scope_uuid || epoch_id_le_u64)`.
//! * `key_storage::InMemoryKeyStorage` — reference
//!   [`key_storage::KeyStorage`] implementation backed by an
//!   in-process `HashMap`. The substrate itself is platform-
//!   agnostic, so production hosts register a platform-backed
//!   `KeyStorage` (iOS Keychain, Android Keystore, Windows DPAPI,
//!   etc.) via the FFI `KeyStorageResolver`; the in-process
//!   implementation is *only* appropriate for tests and the demo
//!   binary because heap-resident master keys are exposed to any
//!   process-level memory-disclosure bug.
//!
//! The four types above are referenced as plain code spans rather
//! than intra-doc links because their re-exports are themselves
//! gated behind `cfg(any(test, feature = "test-support"))`, so the
//! links would be unresolved under default-features `cargo doc`.
//!
//! A `compile_error!` below enforces that `test-support` is never
//! enabled in release builds, so production binaries cannot
//! accidentally ship the mocks.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("test-support must not be enabled in release builds");

// STABLE
pub mod aead;
// UNSTABLE — TEE attestation surface; API may change.
pub mod attestation;
// STABLE
pub mod errors;
// STABLE
pub mod forgetting;
// STABLE
pub mod hash;
// UNSTABLE — internal enforcement helper; not part of consumer API.
#[doc(hidden)]
pub mod hybrid_enforcement;
// STABLE
pub mod hybrid_kem;
// STABLE
pub mod kdf;
// STABLE
pub mod kem;
// STABLE
pub mod key_storage;
// UNSTABLE — MLS keying; API may change.
pub mod mls;
// STABLE
pub mod provenance;
// STABLE
pub mod signer_backend;
// STABLE
pub mod sphincs;

// STABLE
pub use aead::{
    decrypt_aead, encrypt_aead, AeadCiphertext, AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN,
};
// STABLE
pub use errors::CryptoError;
#[cfg(any(test, feature = "test-support"))]
pub use forgetting::DeterministicEpochKeySource;
// STABLE
pub use hash::{content_hash, ContentHash, CONTENT_HASH_LEN};
// STABLE
pub use hybrid_kem::{
    hybrid_kem_decap, hybrid_kem_decap_with_backend, hybrid_kem_encap,
    hybrid_kem_encap_with_backend, hybrid_keypair, hybrid_keypair_with_backend, HybridCiphertext,
    HybridPublicKey, HybridSecretKey, HybridSharedSecret,
};
// STABLE
pub use kdf::{derive_key, DerivedKey, MasterKey, MASTER_KEY_LEN};
#[cfg(any(test, feature = "test-support"))]
pub use kem::StubKemBackend;
// STABLE
pub use kem::{
    KemBackend, KemCiphertext, KemPublicKey, KemSecretKey, KemSharedSecret, MlKem768Backend,
    KEM_CIPHERTEXT_LEN, KEM_PUBLIC_KEY_LEN, KEM_SECRET_KEY_LEN, KEM_SHARED_SECRET_LEN,
};
#[cfg(any(test, feature = "test-support"))]
pub use key_storage::InMemoryKeyStorage;
// STABLE
pub use key_storage::KeyStorage;
// STABLE
pub use provenance::{
    AgentKind, EvidenceRef, ProvenanceAgent, ProvenanceBundle, ProvenanceSignature,
    ProvenanceSigner, SignedBundle, SynthesisActivity,
};
#[cfg(any(test, feature = "test-support"))]
pub use provenance::{TestSigner, TEST_SIGNER_KEY_LEN};
