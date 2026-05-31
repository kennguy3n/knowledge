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

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

pub mod classifier;
pub mod embeddings;
pub mod error;
pub mod ids;
pub mod importance;
pub mod retrieval;
pub mod routing;
pub mod schema;
pub mod script;
pub mod store;

pub use crypto::{ContentHash, MasterKey, MASTER_KEY_LEN};
pub use error::{EvidenceError, Result};
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
