//! `knowledge_evidence_store` — SQLCipher-backed encrypted evidence
//! store for the Knowledge substrate.
//!
//! This crate implements the **evidence plane** described in
//! `docs/technical/design.md` §3.1 and `docs/technical/architecture.md` §2.1 / §2.2:
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
//!   application) so tests can assert the precision
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
//!
//! The two hooks above are referenced as plain code spans rather
//! than intra-doc links because their `pub` symbols are themselves
//! gated behind `cfg(any(test, feature = "test-support"))`, so the
//! links would be unresolved under default-features `cargo doc`.
//! This mirrors the `crypto` crate's `Test-only types` precedent.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

// UNSTABLE — internal bigram tokeniser; not part of consumer API.
#[doc(hidden)]
pub mod bigram;
// UNSTABLE — internal classifier plumbing; not part of consumer API.
#[doc(hidden)]
pub mod classifier;
// UNSTABLE — internal embedding routing; not part of consumer API.
#[doc(hidden)]
pub mod embedding_routing;
// STABLE
pub mod embeddings;
// STABLE
pub mod error;
// UNSTABLE — internal FTS stopword lists.
#[doc(hidden)]
pub mod fts_stopwords;
// UNSTABLE — internal FTS telemetry.
#[doc(hidden)]
pub mod fts_telemetry;
// UNSTABLE — internal FTS weight tuning.
#[doc(hidden)]
pub mod fts_weights;
// STABLE
pub mod ids;
// STABLE
pub mod importance;
// STABLE
pub mod retrieval;
// STABLE
pub mod routing;
// UNSTABLE — internal schema migrations; not part of consumer API.
#[doc(hidden)]
pub mod schema;
// UNSTABLE — internal Unicode script detection.
#[doc(hidden)]
pub mod script;
// STABLE
pub mod store;
// UNSTABLE — internal vector telemetry.
#[doc(hidden)]
pub mod vector_telemetry;

// STABLE
pub use crypto::{ContentHash, MasterKey, MASTER_KEY_LEN};
// STABLE
pub use error::{EvidenceError, Result};
// UNSTABLE — internal telemetry; signatures may change.
#[doc(hidden)]
pub use fts_telemetry::{snapshot as fts_telemetry_snapshot, FtsTelemetrySnapshot};
// STABLE
pub use ids::{EvidenceId, ScopeId};
// STABLE
pub use importance::{
    ImportanceClass, ImportanceClassifier, Lexicon, LexiconClassifier, NegationVerdict,
    SemanticNegationDetector,
};
// STABLE
pub use retrieval::{ClusteredRetrievalResult, HybridRetriever, HybridWeights, RetrievalResult};
// STABLE
pub use script::{detect_mixed_language, MixedLanguageResult, ScriptKind};
// STABLE
pub use routing::{
    route_storage, route_storage_with_threshold, StoragePath, DEFAULT_INLINE_THRESHOLD_BYTES,
};
// STABLE
pub use store::{
    ApprovedDocumentPayloadMeta, EvidenceRow, EvidenceStore, EvidenceStoreConfig, IngestResult,
    MasterKeyRotationReport, MemoryProfile, RingBufferEntry, SecureDeletionReport, TrimReport,
    DEFAULT_RING_BUFFER_MAX_BYTES, LOW_MEMORY_PAGE_CACHE_KIB, MEDIUM_MEMORY_PAGE_CACHE_KIB,
};
// UNSTABLE — internal telemetry; signatures may change.
#[doc(hidden)]
pub use vector_telemetry::{snapshot as vector_telemetry_snapshot, VectorTelemetrySnapshot};
