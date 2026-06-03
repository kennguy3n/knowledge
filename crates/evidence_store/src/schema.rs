//! SQL schema for the SQLCipher-backed local evidence store.
//!
//! The schema is intentionally append-only on the `evidence` table —
//! UPDATE / DELETE attempts are rejected by triggers (see Task 4 in
//! the spec). Only the `body_store.ref_count` column and the
//! `ring_buffer` table are mutable.

/// Schema version stamped into `PRAGMA user_version`. Knowledge ships
/// a single initial schema, so this is `1`. The bootstrap in
/// [`SCHEMA_SQL`] creates the full on-disk shape directly via
/// idempotent `CREATE * IF NOT EXISTS` statements; there is no
/// migration ladder. The constant is still read on `open` so a
/// database written by a future, higher-versioned build is refused
/// rather than silently downgraded.
///
/// The initial schema covers the full device-side substrate:
/// - Append-only `evidence` plus the deduplicated `body_store`,
///   `ring_buffer`, and the `evidence_fts` / `evidence_fts_cjk` /
///   `evidence_fts_bigram` full-text lanes (unicode61 baseline plus
///   CJK/Thai trigram and bigram recall lanes with symmetric
///   stopword stripping).
/// - `evidence_embeddings` (composite `(evidence_id, model_tag)` key)
///   for the on-device embedding cache.
/// - Cryptographic-forgetting state: `forgotten_scopes`,
///   `epoch_tombstones`, `scope_deks`, and `body_store_key_wraps`.
/// - Persisted per-scope state: `memory_objects`,
///   `synthesis_object_versions`, `approved_document_payloads`
///   (metadata-only; bytes live in `body_store`), and the connector
///   tables `connector_instances` / `connector_tokens`.
pub const SCHEMA_VERSION: i32 = 1;

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
    created_at      INTEGER NOT NULL,
    -- v13: BCP-47 primary language subtag detected on the
    -- plaintext body by `observation_engine::detect_language`.
    -- NULL when the detector either declined to classify or was
    -- bypassed (e.g. pre-v13 ingest paths, embedded binary blobs).
    language_tag    TEXT
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
-- (docs/technical/architecture.md §2.2). This table catches all whitespace-
-- segmented scripts (Latin, Cyrillic, Greek, Arabic, Hebrew,
-- Devanagari, Hangul) including any Latin terms embedded inside a
-- CJK or Thai document. CJK Han / Hiragana / Katakana / Thai
-- substrings are routed *additionally* into `evidence_fts_cjk`
-- below (schema v14) — `unicode61` produces no tokens for
-- those codepoints because it classifies them as separators.
CREATE VIRTUAL TABLE IF NOT EXISTS evidence_fts USING fts5(
    content,
    evidence_id UNINDEXED,
    scope_id    UNINDEXED,
    tokenize    = 'unicode61 remove_diacritics 2'
);

-- v14: trigram-tokenised FTS5 index used for CJK and
-- Thai content where the `unicode61` tokeniser of `evidence_fts`
-- emits zero tokens. The write path inserts a row here *in
-- addition to* `evidence_fts` whenever the body contains any
-- CJK Han / Hiragana / Katakana / Thai codepoint; the read path
-- UNIONs both tables and dedupes on `evidence_id`. Forget /
-- purge / rebuild paths touch both tables in the same
-- transaction so the two indexes can never drift apart.
CREATE VIRTUAL TABLE IF NOT EXISTS evidence_fts_cjk USING fts5(
    content,
    evidence_id UNINDEXED,
    scope_id    UNINDEXED,
    tokenize    = 'trigram'
);

-- v15: CJK / Thai bigram recall lane. The `content`
-- column stores a whitespace-separated string of overlapping
-- 2-codepoint windows over the CJK / Thai portion of the body
-- (computed by `crate::bigram::compute_cjk_bigrams`); the
-- `unicode61 remove_diacritics 2` tokeniser then splits that
-- string into the individual bigram tokens. The write path
-- INSERTs here in addition to `evidence_fts` (always) and
-- `evidence_fts_cjk` (when the body routes CJK / Thai) iff the
-- precomputed bigram string is non-empty. The read path runs
-- a third independent prepared statement against this table
-- with the query bigram-tokenised by
-- `crate::bigram::compute_cjk_bigram_query`, merging into the
-- same `MIN(rank)`-by-`evidence_id` HashMap as the other two
-- branches. This closes the v14 trigram lane's
-- "≥ 3 codepoint query" floor so 2-codepoint CJK queries like
-- `天気` (Japanese "weather") return real recall instead of an
-- empty result set. Forget / purge / rebuild paths touch this
-- table alongside `evidence_fts` and `evidence_fts_cjk` in the
-- same transaction so the three indexes can never drift apart
-- under crash recovery.
CREATE VIRTUAL TABLE IF NOT EXISTS evidence_fts_bigram USING fts5(
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
-- DEK (the unit of forgetting in `docs/technical/design.md` §3.1) makes its
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
-- Each scope's AEAD key is generated from the OS RNG (rand-0.10's
-- `SysRng`; see SECURITY.md §"Random number generation"), not
-- HKDF-derived from the master key. The raw DEK is AEAD-wrapped
-- under a wrapping key
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

-- v9 — persisted connector instances.
-- Each row stores one connector's `(ConnectorConfig, SyncState)`
-- pair as a single AEAD-encrypted JSON blob under the per-scope DEK.
-- The blob is upserted on `create_connector` (initial state) and on
-- every `sync_connector` call (advancing the `SyncState` cursor /
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

-- Defense-in-depth: the runtime check in `create_connector`
-- rejects duplicates with `ConnectorError::DuplicateConnector` under
-- the per-handle mutex (see `crates/ffi/src/connector.rs`). This
-- unique index pins the same contract at the database layer so a
-- future regression of the runtime-side check (or a parallel writer
-- on a different handle pointing at the same SQLCipher file) still
-- cannot create two rows that violate the single-instance-per-
-- `(scope_id, kind)` contract.
CREATE UNIQUE INDEX IF NOT EXISTS idx_connector_instances_scope_kind
    ON connector_instances (scope_id, kind);

-- v9 — persisted OAuth2 token bundles.
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

-- v10 — opaque approved-document payloads.
-- v12 — content-hash dedup via `body_store`.
--
-- Each row attaches metadata to an `ApprovedDocumentRef` previously
-- admitted onto a `TenantMemoryObject`. The ref lives inside the
-- tenant_memory blob; this row carries the *metadata* (content_hash,
-- size_bytes, updated_at) and points — through `content_hash` — at
-- the actual plaintext bytes stored in the deduplicated `body_store`
-- table. The payload bytes themselves are AEAD-encrypted under a
-- random per-row CEK that is wrapped under each referencing scope's
-- DEK in `body_store_key_wraps`, identical to how the evidence
-- body-table content is handled.
--
-- The pre-v12 schema carried inline `nonce` + `payload` columns
-- here, encrypted directly under the scope DEK with AAD binding
-- (scope_id, document_id). That layout could not deduplicate the
-- same content across multiple tenant scopes: admitting the same
-- 1 MiB onboarding doc into N tenants cost N copies of the
-- ciphertext. The v12 layout costs one `body_store` row + N wraps.
-- The destructive v11 -> v12 migration (decrypt every legacy row,
-- admit the plaintext via `body_store`, then ALTER TABLE DROP COLUMN)
-- lives in `migrate_approved_doc_payloads_to_body_store` in
-- `store.rs`.
--
-- Selective read still applies: the metadata-only row is cheap to
-- list (no AEAD), and the actual payload bytes are only decrypted
-- when a tenant synthesis run is about to dispatch the document.
-- Deletion is independent from the ref: `revoke_approved_document`
-- drops the metadata row and the wrap (the body row itself is GCed
-- by the shared `purge_body_key_wraps_for_scope` logic when its
-- ref_count reaches zero).
--
-- `forget(scope)` deletes rows by `scope_id` (defense-in-depth);
-- the durable forgetting comes from destroying the scope DEK + the
-- per-(scope, body) wrap in `body_store_key_wraps`, which makes
-- the body ciphertext unrecoverable even if the body row stays
-- referenced from another scope.
--
-- The composite PK `(scope_id, document_id)` already serves prefix
-- lookups on `scope_id`, so no separate covering index is needed for
-- the `WHERE scope_id = ?` listing query — SQLite's PK index handles
-- it directly.
CREATE TABLE IF NOT EXISTS approved_document_payloads (
    scope_id        BLOB    NOT NULL,
    document_id     BLOB    NOT NULL,
    content_hash    BLOB    NOT NULL,
    size_bytes      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (scope_id, document_id)
);

-- Per-window synthesis-object version history.
--
-- The live `synthesis_objects` blob (keyed by `memory_objects.kind
-- = 'synthesis_object'`, one row per scope) carries only the
-- *latest* version of each window's synthesis output. Each call to
-- `replay_synthesis(scope, window)` archives the previous latest
-- here before installing its own output as the new latest in the
-- per-scope blob, so the blob's read path stays a single
-- decrypt-and-iterate over the current state while the history
-- table grows append-mostly.
--
-- Columns:
--   * `scope_id`     — owning scope (16-byte UUID).
--   * `window_id`    — synthesis window the version belongs to.
--   * `version`      — monotonic stamp; first archived row is the
--                      pre-replay version (e.g. 1 if the window
--                      had never been replayed), subsequent rows
--                      increase by 1 per replay.
--   * `nonce`        — AEAD nonce for this row.
--   * `payload`      — AEAD ciphertext of the serialised
--                      `SynthesisObject` JSON bytes.
--   * `created_at`   — Unix seconds at archive time.
--
-- AAD binds `scope_id` (16) + `window_id` (16) + `version` (u32
-- big-endian) via `synthesis_object_version_aad`, so a ciphertext
-- relocated to a different row fails to decrypt rather than
-- silently surfacing the wrong-version payload to a host reading
-- `list_synthesis_versions`. The magic prefix
-- `synthesis-object-version:v1:` namespaces future AAD format
-- bumps.
--
-- `forget(scope)` deletes rows by `scope_id`. Even if the delete
-- races the scope-DEK destruction, the ciphertext is
-- unrecoverable once the DEK is gone, so the row purge is
-- defense-in-depth rather than the primary security barrier.
--
-- The composite PK `(scope_id, window_id, version)` serves the
-- two read paths we need:
--   * `list_synthesis_object_versions(scope, window)` —
--     `WHERE scope_id = ? AND window_id = ?` is a prefix scan over
--     the PK index; no separate covering index needed.
--   * `load_synthesis_object_version(scope, window, version)` —
--     exact PK lookup.
-- The supplemental `idx_synthesis_object_versions_scope` index
-- supports the orphan-sweep walk at `open_store` time, which lists
-- every `(scope, window)` pair across the table to diff against
-- live window-manager state.
CREATE TABLE IF NOT EXISTS synthesis_object_versions (
    scope_id        BLOB    NOT NULL,
    window_id       BLOB    NOT NULL,
    version         INTEGER NOT NULL,
    nonce           BLOB    NOT NULL,
    payload         BLOB    NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (scope_id, window_id, version)
);

CREATE INDEX IF NOT EXISTS idx_synthesis_object_versions_scope
    ON synthesis_object_versions (scope_id);
"#;
