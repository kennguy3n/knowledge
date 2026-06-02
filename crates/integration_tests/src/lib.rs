//! Cross-crate integration tests for the Knowledge substrate.
//!
//! This crate re-exports shared test constants and helpers used by
//! the per-file integration-test binaries under `tests/`.
//!
//! See:
//!
//! * `tests/ingest_to_query.rs` — evidence-store ingest, query, and
//!   cryptographic forgetting across two scopes.
//! * `tests/concept_graph_pipeline.rs` — evidence → observation
//!   pipeline → concept graph supersession.
//! * `tests/permission_check.rs` — Zanzibar-style `check_permission`
//!   with the default `Owner ⇒ Admin ⇒ Editor ⇒ Member ⇒ Viewer`
//!   inheritance chain and tuple-via userset rewrites.
//! * `tests/crypto_round_trip.rs` — hybrid KEM encap/decap, ML-DSA-65
//!   sign/verify, SPHINCS+ sign/verify, co-sign/co-verify, and
//!   AEAD ciphertext-after-forgetting failure.
//! * `tests/synthesis_round_trip.rs` — full pipeline from evidence
//!   ingest through synthesis (channel → domain → tenant) to export.
//! * `tests/connector_lifecycle.rs` — connector attach/detach with
//!   scope inheritance and DEK destroy.
//! * `tests/memory_decay.rs` — retention scoring, decay state
//!   transitions, and cryptographic forgetting.
//! * `tests/sync_merge.rs` — CRDT convergence via delta exchange
//!   with add-wins, supersession, and idempotent merge.
//! * `tests/agent_proposal.rs` — proposal lifecycle, canonical
//!   promotion, and audit log entries.
//! * `tests/multi_scope_isolation.rs` — cross-scope evidence,
//!   observation, and permission isolation.
//!
//! # Test-support items (feature-gated)
//!
//! When the `test-support` feature is enabled the crate exposes a
//! [`test_helpers`] module with:
//!
//! * [`test_helpers::MASTER_KEY`] — fixed 32-byte key for all test stores.
//! * [`test_helpers::BODY_SIZE`] — body size above the inline threshold.
//! * [`test_helpers::open_store`] — open a fresh [`EvidenceStore`](evidence_store::EvidenceStore) at a path.
//! * [`test_helpers::padded_body`] — create a body of [`BODY_SIZE`](test_helpers::BODY_SIZE) bytes.
//! * Re-exports: [`ScopeId`](evidence_store::ScopeId), [`ImportanceClass`](evidence_store::ImportanceClass), etc.

#![deny(missing_docs)]

#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("test-support must not be enabled in release builds");

/// Shared test constants and helpers for integration tests.
#[cfg(any(test, feature = "test-support"))]
pub mod test_helpers {
    pub use evidence_store::{
        EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId,
        DEFAULT_INLINE_THRESHOLD_BYTES,
    };

    /// Fixed master key for all test stores.
    pub const MASTER_KEY: [u8; 32] = [0xA5; 32];

    /// Body size above the inline threshold so evidence takes the
    /// body-table path where cryptographic forgetting actually shreds.
    pub const BODY_SIZE: usize = DEFAULT_INLINE_THRESHOLD_BYTES * 4;

    /// Open a fresh [`EvidenceStore`] at `path` using [`MASTER_KEY`].
    pub fn open_store(path: &std::path::Path) -> EvidenceStore {
        EvidenceStore::open(path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open evidence store")
    }

    /// Create a body of [`BODY_SIZE`] bytes prefixed with `prefix`.
    pub fn padded_body(prefix: &str) -> Vec<u8> {
        let mut body = prefix.as_bytes().to_vec();
        body.resize(BODY_SIZE, b'.');
        body
    }
}
