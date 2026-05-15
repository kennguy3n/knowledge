//! SQL schema for the SQLCipher-backed local evidence store.
//!
//! The schema is intentionally append-only on the `evidence` table —
//! UPDATE / DELETE attempts are rejected by triggers (see Task 4 in
//! the Phase 0 spec). Only the `body_store.ref_count` column and the
//! `ring_buffer` table are mutable.

/// Schema version stamped into `PRAGMA user_version`. Bumped on every
/// breaking schema change.
///
/// History:
/// - v1: initial evidence / body_store / ring_buffer / evidence_fts.
/// - v2 (Phase B): added `evidence_embeddings` for the on-device ONNX
///   embedding cache used by the hybrid retriever's semantic-vector
///   lane.
pub const SCHEMA_VERSION: i32 = 2;

/// Schema bootstrap statements executed inside a transaction at
/// `EvidenceStore::open`.
pub const SCHEMA_SQL: &str = r#"
-- Evidence rows are append-only. UPDATE / DELETE attempts are rejected
-- by triggers below.
CREATE TABLE IF NOT EXISTS evidence (
    id              BLOB    PRIMARY KEY,
    scope_id        BLOB    NOT NULL,
    content_hash    BLOB    NOT NULL,
    body            BLOB,
    body_ref        BLOB,
    nonce           BLOB,
    source_ref      TEXT,
    acl_pointer     TEXT,
    importance      INTEGER NOT NULL,
    storage_path    INTEGER NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_evidence_scope_created
    ON evidence (scope_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_evidence_content_hash
    ON evidence (content_hash);

-- Append-only enforcement.
CREATE TRIGGER IF NOT EXISTS evidence_no_update
BEFORE UPDATE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'evidence is append-only');
END;

CREATE TRIGGER IF NOT EXISTS evidence_no_delete
BEFORE DELETE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'evidence is append-only');
END;

-- Deduplicated body table (BLAKE3 content-hash keyed).
CREATE TABLE IF NOT EXISTS body_store (
    content_hash    BLOB PRIMARY KEY,
    body            BLOB    NOT NULL,
    nonce           BLOB    NOT NULL,
    ref_count       INTEGER NOT NULL DEFAULT 0
);

-- Ring buffer for noise-class messages. Configurable size cap is
-- enforced in code; entries are FIFO-overwritten when the cap is hit.
CREATE TABLE IF NOT EXISTS ring_buffer (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_id        BLOB    NOT NULL,
    body            BLOB    NOT NULL,
    nonce           BLOB    NOT NULL,
    payload_size    INTEGER NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_ring_buffer_scope_created
    ON ring_buffer (scope_id, created_at DESC);

-- FTS5 index over plaintext content for non-noise rows. Tokenizer is
-- the substrate canonical 'unicode61 remove_diacritics 2'
-- (ARCHITECTURE.md §2.2).
CREATE VIRTUAL TABLE IF NOT EXISTS evidence_fts USING fts5(
    content,
    evidence_id UNINDEXED,
    scope_id    UNINDEXED,
    tokenize    = 'unicode61 remove_diacritics 2'
);

-- Embedding cache used by the hybrid retriever's semantic-vector
-- lane (Phase B). Populated on write when an `EmbeddingModel` has
-- been wired into the store; queried by `HybridRetriever` instead of
-- re-embedding the plaintext body on every search. The `embedding`
-- column stores the `f32` vector as little-endian raw bytes.
CREATE TABLE IF NOT EXISTS evidence_embeddings (
    evidence_id     BLOB    PRIMARY KEY,
    embedding       BLOB    NOT NULL,
    model_tag       TEXT    NOT NULL,
    created_at      INTEGER NOT NULL
);
"#;
