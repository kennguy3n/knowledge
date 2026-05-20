//! Cross-crate integration tests for the Knowledge substrate.
//!
//! This crate intentionally exports nothing — it only exists so the
//! files under `tests/` (each one a standalone cargo integration-test
//! binary) can pull in `crypto`, `evidence_store`, `concept_graph`,
//! and `permission_service` together and exercise the full pipeline.
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

#![deny(missing_docs)]
