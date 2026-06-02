//! `knowledge_evidence_store` — SQLCipher-backed encrypted evidence
//! store for the Knowledge substrate.
//!
//! This crate implements the **evidence plane** described in
//! `docs/DESIGN.md` §3.1 and `ARCHITECTURE.md` §2.1 / §2.2:
//!
//! * Append-only encrypted ingestion of message / file / chunk bodies.
//! * Content-hash deduplication (BLAKE3) with a size-threshold routing
//!   strategy: inline rows for `≤ 512 B`, deduplicated body table for
//!   `> 512 B`, ring buffer for noise-class messages.
//! * SQLCipher page-encryption keyed off a per-user master key via
//!   HKDF (the master key itself is unwrapped by the hybrid X25519 +
//!   ML-KEM-768 KEM at boot — see the `crypto` crate).
//! * SQLite FTS5 index (`unicode61 remove_diacritics 2` tokenizer) for
//!   lexical retrieval over plaintext content.
//! * A lexicon-only [`importance::ImportanceClassifier`] fallback used
//!   when the SLM is not available (low-tier devices, bootstrap,
//!   degraded-mode operation).
//!
//! # Test-only types (`test-support` feature)
//!
//! `CONTRIBUTING.md` requires that test-only types be gated behind
//! `cfg(any(test, feature = "test-support"))` AND documented in the
//! crate's top-level doc comment. The `test-support` feature is
//! declared in `Cargo.toml` as a no-op feature flag (no transitive
//! dependencies); enabling it exposes the following deterministic
//! hooks for unit tests, integration tests, and the `ffi` crate's
//! end-to-end tests:
//!
//! * `EvidenceStore::search_fts_with_weighted_ranks_for_tests` —
//!   exposes the cross-lane merged BM25 ranks (after lane-weight
//!   application) so  tests can assert the precision
//!   hierarchy `unicode61 < trigram < bigram` rather than just the
//!   evidence-id ordering surfaced by the public retrieval path.
//! * `EvidenceStore::inject_with_transaction_failure_for_tests` —
//!   forces the next `EvidenceStore::with_transaction` call to fail
//!   with a synthetic `EvidenceError::Sqlite(SQLITE_FULL)` so
//!   commit-failure recovery paths (e.g. `apply_dispatch_outcome`,
//!   `replace_approved_document`) can be exercised without a real
//!   SQLCipher I/O error. Paired with a private
//!   `take_injected_with_transaction_failure` consumer so the
//!   injection fires exactly once.
//! * `EvidenceStore::write_legacy_approved_doc_payload_for_tests` —
//!   surgically reshapes `approved_document_payloads` back to its
//!   pre-v12 ( / v10) inline layout and writes a single
//!   legacy-shape row so the v12-onwards re-migration code path
//!   has a controllable starting state. The next
//!   `EvidenceStore::open` silently re-migrates the row.
//!
//! The three hooks above are referenced as plain code spans rather
//! than intra-doc links because their `pub` symbols are themselves
//! gated behind `cfg(any(test, feature = "test-support"))`, so the
//! links would be unresolved under default-features `cargo doc`.
//! This mirrors the `crypto` crate's `Test-only types` precedent.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

pub mod bigram;
pub mod classifier;
pub mod embedding_routing;
pub mod embeddings;
pub mod error;
pub mod fts_stopwords;
pub mod fts_telemetry;
pub mod fts_weights;
pub mod ids;
pub mod importance;
pub mod retrieval;
pub mod routing;
pub mod schema;
pub mod script;
pub mod store;
pub mod vector_telemetry;

pub use crypto::{ContentHash, MasterKey, MASTER_KEY_LEN};
pub use error::{EvidenceError, Result};
pub use fts_telemetry::{snapshot as fts_telemetry_snapshot, FtsTelemetrySnapshot};
pub use ids::{EvidenceId, ScopeId};
pub use importance::{ImportanceClass, ImportanceClassifier, Lexicon, LexiconClassifier};
pub use retrieval::{HybridRetriever, HybridWeights, RetrievalResult};
pub use routing::{
    route_storage, route_storage_with_threshold, StoragePath, DEFAULT_INLINE_THRESHOLD_BYTES,
};
pub use store::{
    ApprovedDocumentPayloadMeta, EvidenceRow, EvidenceStore, EvidenceStoreConfig, IngestResult,
    RingBufferEntry, DEFAULT_RING_BUFFER_MAX_BYTES,
};
pub use vector_telemetry::{snapshot as vector_telemetry_snapshot, VectorTelemetrySnapshot};
