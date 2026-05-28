//! SQL schema for the SQLCipher-backed local evidence store.
//!
//! The schema is intentionally append-only on the `evidence` table —
//! UPDATE / DELETE attempts are rejected by triggers (see Task 4 in
//! the spec). Only the `body_store.ref_count` column and the
//! `ring_buffer` table are mutable.

/// Schema version stamped into `PRAGMA user_version`. Bumped on every
/// breaking schema change.
///
/// History:
/// - v1: initial evidence / body_store / ring_buffer / evidence_fts.
/// - v2: added `evidence_embeddings` for the on-device ONNX
///   embedding cache used by the hybrid retriever's semantic-vector
///   lane.
/// - v3: widened the `evidence_embeddings`
///   primary key from `evidence_id` alone to the composite
///   `(evidence_id, model_tag)`. This lets multiple cached vectors
///   coexist for the same evidence row when the embedding model is
///   swapped — a model upgrade keeps the old rows warm for any
///   retriever still running under the previous tag instead of
///   destroying them via `INSERT OR REPLACE`. The upgrade is
///   destructive (cannot be expressed with `CREATE * IF NOT EXISTS`)
///   so the migration is implemented in `apply_migration(3)`.
/// - v4: added `forgotten_scopes` tombstone table
///   so cryptographic-forgetting tombstones survive process restarts
///   — the in-memory `DekRegistry` is rebuilt from this table on
///   `open_store`. Purely additive.
/// - v5 (WS1): added `body_store_key_wraps` for per-scope CEK
///   wrapping of deduplicated body-table rows. Bodies in
///   `body_store` are now encrypted under a random per-row Content
///   Encryption Key (CEK); each scope that references the body
///   wraps the CEK under its per-scope AEAD key. `forget()` deletes
///   wraps for the forgotten scope; when no wraps remain the body
///   is cryptographically unrecoverable. Purely additive.
/// - v6 (C2): added `scope_deks` for independently generated scope
///   Data Encryption Keys. Each scope key is now randomly generated
///   via `OsRng` rather than HKDF-derived from the master key.
///   The DEK is AEAD-wrapped under a master-derived wrapping key and
///   stored in this table. `forget()` deletes the row, making the
///   scope key truly unrecoverable even if the master key is
///   compromised. Purely additive.
/// - v7 (C10): added `memory_objects` for persisted per-scope
///   memory state. Each scope's `UserMemoryObject` (or
///   `ChannelMemoryObject`) is JSON-serialized and AEAD-encrypted
///   under the scope key, so memory state survives process
///   restarts. Mutations (pin, unpin, decay_sweep) flush the
///   updated state to this table. Purely additive.
/// - v8: added `epoch_tombstones` for per-`(scope, epoch)`
///   cryptographic-forgetting tombstones. The existing
///   `forgotten_scopes` table is scope-grain only; epoch DEK
///   destruction — emitted by
///   [`crypto::forgetting::destroy_epoch_dek`] — was previously
///   in-memory only and lost across restarts. The substrate now
///   replays this table into the in-process [`DekRegistry`] on
///   every `open_store` so post-restart calls for forgotten epochs
///   continue to short-circuit. Purely additive.
/// - v9 (Phase 3 connector persistence): added `connector_instances`
///   for AEAD-encrypted per-instance `(ConnectorConfig, SyncState)`
///   blobs and `connector_tokens` for AEAD-encrypted per-instance
///   `OAuth2Token` bundles. Both encrypted under the same per-scope
///   DEK that protects `memory_objects` and `body_store_key_wraps`,
///   so `forget(scope)`'s destruction of the scope DEK makes both
///   tables' ciphertexts cryptographically unrecoverable even if the
///   row deletion races against the DEK delete. A unique index on
///   `connector_instances(scope_id, kind)` pins the
///   single-instance-per-(scope, kind) contract at the DB layer
///   (defense-in-depth against future regressions of the runtime-
///   side check). Purely additive.
pub const SCHEMA_VERSION: i32 = 9;

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
-- lane. Populated on write when an `EmbeddingModel` has
-- been wired into the store; queried by `HybridRetriever` instead of
-- re-embedding the plaintext body on every search. The `embedding`
-- column stores the `f32` vector as little-endian raw bytes.
--
-- The primary key is the composite (`evidence_id`, `model_tag`) so a
-- single evidence row can have multiple cached vectors — one per
-- model the store has been wired into. This is the v3 shape (
-- follow-up); the destructive v2 -> v3 migration that rewrites a
-- pre-existing single-PK table into this shape lives in
-- `apply_migration` in `store.rs`. For an already-v3 database this
-- statement is a no-op via `IF NOT EXISTS`; for a fresh database it
-- creates the v3 shape directly.
CREATE TABLE IF NOT EXISTS evidence_embeddings (
    evidence_id     BLOB    NOT NULL,
    embedding       BLOB    NOT NULL,
    model_tag       TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (evidence_id, model_tag)
);

-- Durable cryptographic-forgetting tombstones.
-- Each row records that the runtime destroyed the per-scope DEK for
-- `scope_id` at `forgotten_at` (Unix epoch seconds). The substrate
-- replays these rows into the in-process `DekRegistry` on every
-- `open_store` so post-restart calls for the same scope continue
-- to short-circuit with `NotFound { kind: "scope" }`.
--
-- This table is the *only* mutable store of forgetting state. The
-- `evidence` table itself is append-only — destroying the per-scope
-- DEK (the unit of forgetting in `docs/DESIGN.md` §3.1) makes its
-- bodies unrecoverable; the tombstone here makes that decision
-- durable across process restarts. Re-inserts for an already-
-- forgotten scope are no-ops by way of `INSERT OR IGNORE`.
CREATE TABLE IF NOT EXISTS forgotten_scopes (
    scope_id        BLOB    PRIMARY KEY,
    forgotten_at    INTEGER NOT NULL
);

-- v5 (WS1) — per-scope CEK wraps for deduplicated body-table rows.
-- Each row wraps the random Content Encryption Key (CEK) of a body_store
-- row under the per-scope AEAD key so that `forget()` can destroy a
-- scope's access to shared bodies without affecting other scopes.
-- When no wraps remain for a given `content_hash`, the body is
-- cryptographically unrecoverable.
CREATE TABLE IF NOT EXISTS body_store_key_wraps (
    content_hash    BLOB    NOT NULL,
    scope_id        BLOB    NOT NULL,
    wrapped_cek     BLOB    NOT NULL,
    nonce           BLOB    NOT NULL,
    PRIMARY KEY (content_hash, scope_id)
);

-- Forgetting a scope queries body_store_key_wraps by scope_id alone.
-- The composite PK only supports prefix lookups on content_hash;
-- without this index those queries require a full table scan.
CREATE INDEX IF NOT EXISTS idx_body_wraps_scope
    ON body_store_key_wraps (scope_id);

-- v6 (C2) — independently generated per-scope DEKs.
-- Each scope's AEAD key is generated via OsRng (not HKDF-derived from
-- the master key). The raw DEK is AEAD-wrapped under a wrapping key
-- derived from the master key, so it can be unwrapped at open_store
-- time. On forget(), the row is deleted — without the wrapped DEK
-- the scope key is truly unrecoverable even if the master key is
-- later compromised.
CREATE TABLE IF NOT EXISTS scope_deks (
    scope_id        BLOB    PRIMARY KEY,
    wrapped_dek     BLOB    NOT NULL,
    nonce           BLOB    NOT NULL,
    created_at      INTEGER NOT NULL
);

-- v7 (C10) — encrypted per-scope memory objects.
-- Each row stores a scope's memory objects (user or channel)
-- as a single AEAD-encrypted JSON blob. The `kind` column
-- discriminates between user_memory and channel_memory.
-- Mutations (pin, unpin, decay_sweep) upsert the entire blob.
CREATE TABLE IF NOT EXISTS memory_objects (
    scope_id        BLOB    NOT NULL,
    kind            TEXT    NOT NULL,
    nonce           BLOB    NOT NULL,
    payload         BLOB    NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (scope_id, kind)
);

-- v8 — per-(scope, epoch) cryptographic-forgetting tombstones.
-- Each row records that the runtime destroyed the epoch DEK for
-- `(scope_id, epoch_id)` at `forgotten_at` (Unix epoch seconds).
-- Scope-wide forgetting still goes through `forgotten_scopes`;
-- this table makes per-epoch destruction (emitted by
-- `crypto::forgetting::destroy_epoch_dek`) durable across process
-- restarts so post-restart calls for the same epoch continue to
-- short-circuit. Like `forgotten_scopes`, re-inserts for an
-- already-forgotten (scope, epoch) are no-ops via the
-- TombstoneStore implementation's `INSERT OR IGNORE`.
CREATE TABLE IF NOT EXISTS epoch_tombstones (
    scope_id        BLOB    NOT NULL,
    epoch_id        INTEGER NOT NULL,
    forgotten_at    INTEGER NOT NULL,
    PRIMARY KEY (scope_id, epoch_id)
);

-- v9 (Phase 3) — persisted connector instances.
-- Each row stores one connector's `(ConnectorConfig, SyncState)`
-- pair as a single AEAD-encrypted JSON blob under the per-scope DEK.
-- The blob is upserted on `create_connector` (initial state) and on
-- every `sync_connector` Phase 3 (advancing the `SyncState` cursor /
-- status). The `kind` column is denormalised out of the encrypted
-- payload so the unique index below can pin the single-instance-per-
-- `(scope_id, kind)` contract at the DB layer without first having
-- to decrypt every row to read the kind tag.
--
-- Forgetting deletes rows by `scope_id`; even if that delete fails,
-- the AEAD payload is unrecoverable once the scope DEK is destroyed
-- (step 1 of the cryptographic-forgetting sequence in
-- `crates/ffi/src/lib.rs::forget_scope_state`), so the row purge is
-- best-effort defense in depth.
CREATE TABLE IF NOT EXISTS connector_instances (
    instance_id     BLOB    PRIMARY KEY,
    scope_id        BLOB    NOT NULL,
    kind            TEXT    NOT NULL,
    nonce           BLOB    NOT NULL,
    payload         BLOB    NOT NULL,
    updated_at      INTEGER NOT NULL
);

-- `forget_scope_state` deletes connector rows by `scope_id`; the PK
-- only supports point lookup on `instance_id`, so without this index
-- the scope-grain delete would degrade to a full table scan.
CREATE INDEX IF NOT EXISTS idx_connector_instances_scope
    ON connector_instances (scope_id);

-- Defense-in-depth: the Phase 2 runtime check in `create_connector`
-- rejects duplicates with `ConnectorError::DuplicateConnector` under
-- the per-handle mutex (see `crates/ffi/src/connector.rs`). This
-- unique index pins the same contract at the database layer so a
-- future regression of the runtime-side check (or a parallel writer
-- on a different handle pointing at the same SQLCipher file) still
-- cannot create two rows that violate the single-instance-per-
-- `(scope_id, kind)` contract.
CREATE UNIQUE INDEX IF NOT EXISTS idx_connector_instances_scope_kind
    ON connector_instances (scope_id, kind);

-- v9 (Phase 3) — persisted OAuth2 token bundles.
-- Held in a separate table from `connector_instances` because the
-- token lifecycle is independent: created by `authenticate_connector`,
-- mutated by future background-refresh flows, and dropped by
-- `remove_connector`. The `scope_id` is denormalised onto the row
-- (rather than read out of the encrypted payload) so the
-- `forget(scope)` delete can issue a single indexed scan instead of
-- decrypting every row first.
--
-- AEAD-encrypted under the per-scope DEK with AAD binding both
-- `scope_id` and `instance_id`, so a ciphertext relocated to a
-- different row (different scope or different instance) fails to
-- decrypt and surfaces a structured error rather than silently
-- returning a stale token from the wrong context.
CREATE TABLE IF NOT EXISTS connector_tokens (
    instance_id     BLOB    PRIMARY KEY,
    scope_id        BLOB    NOT NULL,
    nonce           BLOB    NOT NULL,
    payload         BLOB    NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_connector_tokens_scope
    ON connector_tokens (scope_id);
"#;
