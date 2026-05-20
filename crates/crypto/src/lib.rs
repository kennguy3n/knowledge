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

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("test-support must not be enabled in release builds");

pub mod aead;
pub mod attestation;
pub mod errors;
pub mod forgetting;
pub mod hash;
pub mod hybrid_enforcement;
pub mod hybrid_kem;
pub mod kdf;
pub mod kem;
pub mod mls;
pub mod provenance;
pub mod signer_backend;
pub mod sphincs;

pub use aead::{
    decrypt_aead, encrypt_aead, AeadCiphertext, AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN,
};
pub use errors::CryptoError;
pub use hash::{content_hash, ContentHash, CONTENT_HASH_LEN};
pub use hybrid_kem::{
    hybrid_kem_decap, hybrid_kem_decap_with_backend, hybrid_kem_encap,
    hybrid_kem_encap_with_backend, hybrid_keypair, hybrid_keypair_with_backend, HybridCiphertext,
    HybridPublicKey, HybridSecretKey, HybridSharedSecret,
};
pub use kdf::{derive_key, DerivedKey, MasterKey, MASTER_KEY_LEN};
#[cfg(any(test, feature = "test-support"))]
pub use kem::StubKemBackend;
pub use kem::{
    KemBackend, KemCiphertext, KemPublicKey, KemSecretKey, KemSharedSecret, MlKem768Backend,
    KEM_CIPHERTEXT_LEN, KEM_PUBLIC_KEY_LEN, KEM_SECRET_KEY_LEN, KEM_SHARED_SECRET_LEN,
};
pub use provenance::{
    AgentKind, EvidenceRef, ProvenanceAgent, ProvenanceBundle, ProvenanceSignature,
    ProvenanceSigner, SignedBundle, SynthesisActivity,
};
#[cfg(any(test, feature = "test-support"))]
pub use provenance::{TestSigner, TEST_SIGNER_KEY_LEN};
