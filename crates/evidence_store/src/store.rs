//! Top-level [`EvidenceStore`] type — opens the SQLCipher database,
//! runs the schema, and exposes the append-only ingestion + read API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crypto::{
    content_hash, decrypt_aead, derive_key, encrypt_aead, AeadKey, AeadNonce, ContentHash,
    MasterKey, AEAD_KEY_LEN, AEAD_NONCE_LEN, MASTER_KEY_LEN,
};

use crate::embeddings::EmbeddingModel;
use crate::error::{EvidenceError, Result};
use crate::fts_weights::{
    bm25_select_fragment, EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS, EVIDENCE_FTS_BIGRAM_LANE_WEIGHT,
    EVIDENCE_FTS_CJK_COLUMN_WEIGHTS, EVIDENCE_FTS_CJK_LANE_WEIGHT, EVIDENCE_FTS_COLUMN_WEIGHTS,
    EVIDENCE_FTS_LANE_WEIGHT,
};
use crate::ids::{EvidenceId, ScopeId};
use crate::importance::ImportanceClass;
use crate::routing::{route_storage_with_threshold, StoragePath, DEFAULT_INLINE_THRESHOLD_BYTES};
use crate::schema::{SCHEMA_SQL, SCHEMA_VERSION};

/// Default ring-buffer size cap (`docs/technical/design.md` §3.1, `docs/technical/architecture.md`
/// §9.1).
pub const DEFAULT_RING_BUFFER_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Maximum number of `?` placeholders we pack into a single `IN (...)`
/// clause before chunking. SQLite's default
/// `SQLITE_MAX_VARIABLE_NUMBER` is 999; we stay well below it so the
/// statement compiles even on builds that lower the cap.
const DELETE_BATCH: usize = 256;

/// Configuration for [`EvidenceStore::open`].
#[derive(Debug, Clone)]
pub struct EvidenceStoreConfig {
    /// Inline body threshold in bytes. Bodies `≤` this length are
    /// stored inline in the evidence row when their importance class
    /// is non-noise.
    pub inline_threshold_bytes: usize,
    /// Hard cap on the ring buffer. When exceeded, oldest entries are
    /// FIFO-evicted on insert.
    pub ring_buffer_max_bytes: usize,
}

impl Default for EvidenceStoreConfig {
    fn default() -> Self {
        Self {
            inline_threshold_bytes: DEFAULT_INLINE_THRESHOLD_BYTES,
            ring_buffer_max_bytes: DEFAULT_RING_BUFFER_MAX_BYTES,
        }
    }
}

/// One row in the evidence table.
#[derive(Debug, Clone)]
pub struct EvidenceRow {
    /// Unique identifier (UUID v4).
    pub id: EvidenceId,
    /// Scope that owns this row.
    pub scope_id: ScopeId,
    /// BLAKE3 content hash of the *plaintext* body.
    pub content_hash: ContentHash,
    /// Optional source reference (connector id, message id, etc.).
    pub source_ref: Option<String>,
    /// Optional ACL pointer (Zanzibar tuple key, etc.).
    pub acl_pointer: Option<String>,
    /// Importance class assigned at ingest time.
    pub importance: ImportanceClass,
    /// Storage path actually taken.
    pub storage_path: StoragePath,
    /// Unix epoch seconds at ingest.
    pub created_at: i64,
    /// BCP-47 primary language subtag detected on the plaintext
    /// body at ingest time (schema v13). `None` when
    /// the row was ingested via the legacy
    /// [`EvidenceStore::ingest`] shim, when the language detector
    /// declined to classify, or when the row predates schema v13.
    /// Downstream consumers (multilingual lexicon registry, per-
    /// locale FTS5 tokenizer) MUST treat `None` as "unknown"
    /// rather than substitute a default — see
    /// [`EvidenceStore::ingest_with_language`].
    pub language_tag: Option<String>,
}

/// Returned by [`EvidenceStore::ingest`].
#[derive(Debug, Clone)]
pub struct IngestResult {
    /// Identifier of the freshly-inserted evidence row.
    pub evidence_id: EvidenceId,
    /// Storage path actually taken.
    pub storage_path: StoragePath,
    /// BLAKE3 content hash of the plaintext body.
    pub content_hash: ContentHash,
}

/// Outcome of an offline master-key rotation
/// ([`EvidenceStore::rotate_master_key`]).
///
/// The counts double as the audit log line for a rotation: how many
/// scope keys were re-wrapped under the new master, how many evidence
/// rows the rotated copy carries, and how many of those bodies were
/// decrypted from *both* the source and the rotated copy and confirmed
/// byte-identical before the copy was accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterKeyRotationReport {
    /// Number of live (non-forgotten) scope keys re-wrapped under the
    /// new master-derived wrapping key in the rotated copy. Legacy
    /// HKDF-derived scope keys are persisted as explicit wrapped DEKs
    /// as part of this step, decoupling every body from the master key
    /// going forward.
    pub scopes_rewrapped: usize,
    /// Total evidence rows in the source store, verified to match the
    /// rotated copy exactly.
    pub evidence_rows: usize,
    /// Evidence bodies whose plaintext was decrypted from both the
    /// source and the rotated copy and confirmed byte-identical. Rows
    /// belonging to forgotten scopes (whose DEK was destroyed) are
    /// excluded — their ciphertext is copied verbatim but is, by
    /// design, no longer decryptable.
    pub bodies_verified: usize,
}

/// One entry returned from the ring buffer.
#[derive(Debug, Clone)]
pub struct RingBufferEntry {
    /// SQLite rowid (monotonic, primary key).
    pub id: i64,
    /// Scope that owns this entry.
    pub scope_id: ScopeId,
    /// Plaintext body (decrypted on read).
    pub body: Vec<u8>,
    /// Unix epoch seconds at insert.
    pub created_at: i64,
}

/// Plaintext metadata returned by
/// [`EvidenceStore::list_approved_document_payload_meta_for_scope`].
///
/// Lets the FFI surface (`list_approved_documents`) display
/// document id / size / content-hash without paying the AEAD
/// decryption cost on every list call. The ciphertext itself is
/// only decrypted by [`EvidenceStore::load_approved_document_payload`]
/// when the tenant-synthesis dispatch is about to send the bundle
/// to the SLM endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedDocumentPayloadMeta {
    /// Document id (matches `ApprovedDocumentRef::id` on the
    /// tenant-memory side).
    pub document_id: uuid::Uuid,
    /// BLAKE3 content hash of the plaintext payload bytes. Same
    /// hash function used by `body_store` rows for evidence claims.
    pub content_hash: ContentHash,
    /// Plaintext payload size in bytes (NOT the AEAD ciphertext
    /// size, which is `payload + 16` for the AES-GCM auth tag).
    pub size_bytes: u64,
    /// Unix epoch seconds at last upsert. Mostly diagnostic.
    pub updated_at: i64,
}

/// SQLCipher-backed encrypted local evidence store.
pub struct EvidenceStore {
    conn: Connection,
    config: EvidenceStoreConfig,
    /// Cached per-scope AEAD key derivations. Re-derived from the
    /// master key + scope context label. Wrapped in `RwLock` so the
    /// read paths (e.g. [`Self::read_body`]) can populate the cache
    /// while only borrowing `&self`, which lets the hybrid retriever
    /// fan-in semantic similarity over an immutable store handle.
    /// Unlike `RefCell`, `RwLock` is `Sync` and will block (rather
    /// than panic) if shared across threads.
    scope_keys: std::sync::RwLock<std::collections::HashMap<ScopeId, AeadKey>>,
    /// Master key — wiped on drop.
    master_key: MasterKey,
    /// Optional [`EmbeddingModel`] used by the ingest path to populate
    /// the `evidence_embeddings` cache. When `None` no
    /// embedding is persisted on write — the hybrid retriever then
    /// falls back to re-embedding the body on each search.
    embedding_model: Option<Box<dyn EmbeddingModel>>,
    /// Free-form tag (e.g. `"xlm-r-v1"`) stamped alongside every row
    /// inserted into `evidence_embeddings`. Used to invalidate the
    /// cache when the model is swapped.
    embedding_model_tag: String,
    /// Test-only "next [`Self::with_transaction`] call should fail"
    /// hook. Populated by
    /// [`Self::inject_with_transaction_failure_for_tests`] and
    /// consumed (one-shot) by [`Self::with_transaction`] before it
    /// opens a real SQLCipher transaction. Lets downstream crates
    /// (chiefly the `ffi` crate's `apply_dispatch_outcome` commit-
    /// failure regression test) exercise the tx-failure path without
    /// inducing a real SQLCipher I/O error — see the `test-support`
    /// feature comment in `Cargo.toml` for the contract.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) injected_with_transaction_failure: std::sync::Mutex<Option<String>>,
}

impl Drop for EvidenceStore {
    fn drop(&mut self) {
        self.master_key.zeroize();
        // `get_mut()` bypasses locking on `&mut self` but still
        // checks the poison flag — recover gracefully so Drop never
        // panics (a panic during unwinding would abort).
        for (_id, key) in self
            .scope_keys
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
        {
            key.zeroize();
        }
    }
}

impl EvidenceStore {
    /// Open or create the SQLCipher database at `path`.
    ///
    /// `master_key` is the per-user master key from which the
    /// SQLCipher page key and every per-scope AEAD key is HKDF-derived.
    /// Per `docs/technical/architecture.md` §2.2, in a real deployment the master key
    /// is itself unwrapped by the hybrid X25519 + ML-KEM-768 KEM at
    /// boot.
    pub fn open<P: AsRef<Path>>(
        path: P,
        master_key: &MasterKey,
        config: EvidenceStoreConfig,
    ) -> Result<Self> {
        if config.ring_buffer_max_bytes == 0 {
            return Err(EvidenceError::InvalidConfig(
                "ring_buffer_max_bytes must be > 0",
            ));
        }
        if config.inline_threshold_bytes == 0 {
            return Err(EvidenceError::InvalidConfig(
                "inline_threshold_bytes must be > 0",
            ));
        }

        let path: PathBuf = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;

        // Derive the SQLCipher page-encryption key from the master
        // key. This is the deterministic HKDF wrap-around — see
        // docs/technical/architecture.md §2.2. `Zeroizing<String>`
        // zeroes the heap-allocated bytes when dropped — without this
        // wrapper the hex-encoded SQLCipher page key would linger in
        // freed heap memory after `String`'s default `Drop`. The same
        // wrap is applied to the `format!("x'…'")` SQL pragma value
        // below.
        let key_hex: Zeroizing<String> = page_key_hex(master_key)?;

        // Apply SQLCipher PRAGMAs. `cipher_page_size = 4096` and
        // `kdf_iter = 256000` are the SQLCipher 4.x defaults; we set
        // them explicitly so the schema is portable across versions.
        let key_pragma: Zeroizing<String> = Zeroizing::new(format!("x'{}'", &*key_hex));
        conn.pragma_update(None, "key", key_pragma.as_str())?;
        conn.pragma_update(None, "cipher_page_size", 4096_i64)?;
        conn.pragma_update(None, "kdf_iter", 256_000_i64)?;
        // Foreign keys are off by default; we don't use FK constraints
        // (body_ref is a soft pointer with manual ref_count book-keeping).

        // Verify the key works — issuing any SELECT before the schema
        // exists will surface a "file is not a database" if the key is
        // wrong.
        {
            let _: i32 = conn
                .query_row("SELECT 1", [], |row| row.get(0))
                .map_err(|_| EvidenceError::Schema("SQLCipher key did not unlock the database"))?;
        }

        // Schema initialization. Knowledge ships a single initial
        // schema (v1): the idempotent `CREATE * IF NOT EXISTS`
        // bootstrap in [`SCHEMA_SQL`] builds the full on-disk shape
        // directly, so there is no migration ladder to walk. We still
        // read the existing `user_version` BEFORE the bootstrap so a
        // database written by a *newer* build is refused rather than
        // silently downgraded:
        //
        //   * `user_version == 0`              → fresh database; run
        //                                         the bootstrap and
        //                                         stamp the version.
        //   * `user_version == SCHEMA_VERSION` → already current.
        //   * `user_version > SCHEMA_VERSION`  → either a newer build or
        //                                         a pre-release internal
        //                                         database (which used
        //                                         higher version stamps);
        //                                         refuse to open rather
        //                                         than corrupt it. 1.0
        //                                         ships no upgrade path.
        let detected_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);
        if detected_version > SCHEMA_VERSION {
            return Err(EvidenceError::Schema(
                "evidence_store database has an unsupported schema version: it was \
                 written either by a newer build or by a pre-release internal build, \
                 neither of which has an upgrade path to the 1.0 baseline; recreate \
                 the database from source data",
            ));
        }

        // Run the schema bootstrap. Every statement is
        // `CREATE * IF NOT EXISTS`, which makes it safe to re-run
        // against an already-initialised database.
        conn.execute_batch(SCHEMA_SQL)?;

        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        let mut store = Self {
            conn,
            config,
            scope_keys: std::sync::RwLock::new(std::collections::HashMap::new()),
            master_key: *master_key,
            embedding_model: None,
            embedding_model_tag: String::new(),
            #[cfg(any(test, feature = "test-support"))]
            injected_with_transaction_failure: std::sync::Mutex::new(None),
        };
        // No-op for now, but keeps the borrow checker happy if we add
        // post-open prepared statements.
        store.preflight()?;

        // Hydrate the in-memory scope-key cache from the durable
        // `scope_deks` table. Scopes that have an independently
        // generated DEK store it wrapped here; loading them on open
        // means `scope_key()` finds that key in cache rather than
        // falling back to HKDF derivation.
        {
            let deks = store.load_scope_deks()?;
            let mut cache = store.scope_keys.write().unwrap();
            for (scope, key) in deks {
                cache.insert(scope, key);
            }
        }

        Ok(store)
    }

    /// Post-bootstrap sanity check: after [`Self::open`] runs the
    /// schema bootstrap the on-disk schema version must equal
    /// [`SCHEMA_VERSION`]. A mismatch here is a bug in the bootstrap
    /// logic, not a user-recoverable condition.
    fn preflight(&mut self) -> Result<()> {
        let version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(EvidenceError::Schema(
                "user_version does not match SCHEMA_VERSION after bootstrap",
            ));
        }
        Ok(())
    }

    /// Look up the AEAD key for the given scope from the in-memory
    /// cache. New scopes should have their DEK provisioned via
    /// [`Self::ensure_scope_dek`] *before* any read/write path needs
    /// it. For databases upgraded from pre-v6 schema, this method
    /// falls back to HKDF derivation so that existing encrypted
    /// bodies remain readable without a bulk re-encryption migration.
    ///
    /// Takes `&self` so the read paths (e.g. [`Self::read_body`]) can
    /// populate the per-scope key cache without an exclusive borrow.
    /// The cache lives behind a [`std::sync::RwLock`].
    fn scope_key(&self, scope_id: ScopeId) -> Result<AeadKey> {
        if let Some(k) = self.scope_keys.read().unwrap().get(&scope_id) {
            return Ok(*k);
        }
        // Legacy fallback: pre-v6 databases have bodies encrypted
        // under HKDF-derived keys. Derive the key so those rows
        // remain readable. New scopes go through `ensure_scope_dek`
        // which generates a random DEK stored in `scope_deks`.
        //
        // INVARIANT: this HKDF fallback is only correct for genuinely
        // legacy scopes. Any scope with an explicitly stored random DEK
        // would get the WRONG key here — but `open()` hydrates the cache
        // with every `scope_deks` row, so such scopes always hit the
        // cache above and never reach this branch. `rotate_master_key`
        // relies on this: it resolves each scope's key via this method,
        // so the cache must be fully hydrated for the rotation to copy
        // the actual per-body key rather than a mis-derived one.
        let label = format!("scope:{}:body:v1", scope_id.as_uuid());
        let key = derive_key(&self.master_key, label.as_bytes())?;
        self.scope_keys.write().unwrap().insert(scope_id, key);
        Ok(key)
    }

    /// Append-only ingest a fresh evidence row.
    ///
    /// Per `docs/technical/design.md` §3.1 / §4.3:
    ///
    /// * If `importance == Noise`, the body is written to the ring
    ///   buffer and **no** evidence row is created.
    /// * Else if `body.len() ≤ inline_threshold_bytes`, the encrypted
    ///   body lives inline in the evidence row.
    /// * Else the encrypted body lives in the deduplicated `body_store`
    ///   table keyed by its BLAKE3 content hash.
    pub fn ingest(
        &mut self,
        scope_id: ScopeId,
        body: &[u8],
        source_ref: Option<&str>,
        importance: ImportanceClass,
    ) -> Result<IngestResult> {
        self.ingest_with_language(scope_id, body, source_ref, importance, None)
    }

    /// Same contract as [`Self::ingest`], but additionally stamps
    /// the row's `language_tag` column (schema v13) with
    /// a BCP-47 primary subtag.
    ///
    /// The substrate's ingest path runs
    /// [`observation_engine::detect_language`] on the plaintext
    /// body before this call; the detected tag (or `None` when the
    /// detector declined to classify) flows through here so the
    /// multilingual lexicon registry and per-locale FTS5 tokenizer
    /// can pick the right per-locale assets without re-running
    /// detection on every downstream consumer. Noise-class rows go
    /// to the ring buffer and therefore do not retain the language
    /// tag (the ring buffer is plaintext-only, append-and-evict).
    pub fn ingest_with_language(
        &mut self,
        scope_id: ScopeId,
        body: &[u8],
        source_ref: Option<&str>,
        importance: ImportanceClass,
        language_tag: Option<&str>,
    ) -> Result<IngestResult> {
        let path = route_storage_with_threshold(
            body.len(),
            importance,
            self.config.inline_threshold_bytes,
        );
        let hash = content_hash(body);

        match path {
            StoragePath::RingBuffer => {
                self.ring_buffer_insert(scope_id, body)?;
                // Noise rows do not produce an evidence row. We still
                // return an IngestResult with a fresh id so the caller
                // can correlate; the row itself does not exist in
                // `evidence`.
                Ok(IngestResult {
                    evidence_id: EvidenceId::new_v4(),
                    storage_path: StoragePath::RingBuffer,
                    content_hash: hash,
                })
            }
            StoragePath::Inline => {
                self.ingest_inline(scope_id, body, source_ref, importance, hash, language_tag)
            }
            StoragePath::BodyTable => {
                self.ingest_body_table(scope_id, body, source_ref, importance, hash, language_tag)
            }
        }
    }

    fn ingest_inline(
        &mut self,
        scope_id: ScopeId,
        body: &[u8],
        source_ref: Option<&str>,
        importance: ImportanceClass,
        hash: ContentHash,
        language_tag: Option<&str>,
    ) -> Result<IngestResult> {
        let evidence_id = EvidenceId::new_v4();
        let key = self.scope_key(scope_id)?;
        let nonce = random_nonce();
        let aad = ingest_aad(scope_id, evidence_id, &hash);
        let ciphertext = encrypt_aead(&key, &nonce, body, &aad)?;

        let now = Utc::now().timestamp();
        let path_tag = StoragePath::Inline as i64;

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO evidence
             (id, scope_id, content_hash, body, body_ref, nonce,
              source_ref, acl_pointer, importance, storage_path,
              created_at, language_tag)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, ?7, ?8, ?9, ?10)",
            params![
                evidence_id.as_uuid().as_bytes().as_slice(),
                scope_id.as_uuid().as_bytes().as_slice(),
                hash.as_slice(),
                ciphertext,
                nonce.as_slice(),
                source_ref,
                importance.as_tag(),
                path_tag,
                now,
                language_tag,
            ],
        )?;
        Self::index_fts(&tx, evidence_id, scope_id, body)?;
        Self::index_embedding(
            &tx,
            evidence_id,
            body,
            self.embedding_model.as_deref(),
            &self.embedding_model_tag,
            now,
        );
        tx.commit()?;

        Ok(IngestResult {
            evidence_id,
            storage_path: StoragePath::Inline,
            content_hash: hash,
        })
    }

    fn ingest_body_table(
        &mut self,
        scope_id: ScopeId,
        body: &[u8],
        source_ref: Option<&str>,
        importance: ImportanceClass,
        hash: ContentHash,
        language_tag: Option<&str>,
    ) -> Result<IngestResult> {
        let evidence_id = EvidenceId::new_v4();
        let scope_key = self.scope_key(scope_id)?;

        // Pre-transaction read: if a dedup hit exists and this scope
        // does not yet have a CEK wrap, we need to derive the donor
        // scope's key *before* starting the transaction (the borrow
        // checker forbids calling `self.scope_key` while the
        // transaction holds `&mut self.conn`).
        let dedup_donor: Option<(Vec<u8>, Vec<u8>, AeadKey)> = {
            let existing_wrap: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = self
                .conn
                .query_row(
                    "SELECT w.wrapped_cek, w.nonce, w.scope_id \
                     FROM body_store_key_wraps w \
                     WHERE w.content_hash = ?1 \
                       AND w.scope_id != ?2 \
                     LIMIT 1",
                    params![hash.as_slice(), scope_id.as_uuid().as_bytes().as_slice(),],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                        ))
                    },
                )
                .optional()?;
            match existing_wrap {
                Some((wrapped_cek, wrap_nonce, donor_scope_bytes)) => {
                    let donor_scope = ScopeId::from_uuid(slice_to_uuid(&donor_scope_bytes)?);
                    let donor_key = self.scope_key(donor_scope)?;
                    Some((wrapped_cek, wrap_nonce, donor_key))
                }
                None => None,
            }
        };

        let tx = self.conn.transaction()?;
        // Dedup index lookup.
        let existing: Option<i64> = tx
            .query_row(
                "SELECT ref_count FROM body_store WHERE content_hash = ?1",
                params![hash.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;

        if existing.is_some() {
            tx.execute(
                "UPDATE body_store SET ref_count = ref_count + 1 WHERE content_hash = ?1",
                params![hash.as_slice()],
            )?;
            // Dedup hit — create a CEK wrap for this scope if one
            // does not already exist (INSERT OR IGNORE makes
            // same-scope re-ingest idempotent).
            let already_has_wrap: bool = tx
                .query_row(
                    "SELECT 1 FROM body_store_key_wraps \
                     WHERE content_hash = ?1 AND scope_id = ?2",
                    params![hash.as_slice(), scope_id.as_uuid().as_bytes().as_slice(),],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !already_has_wrap {
                if let Some((donor_wrapped, donor_nonce, donor_key)) = &dedup_donor {
                    let cek = unwrap_cek(donor_key, donor_wrapped, donor_nonce, &hash)?;
                    let wrap_nonce = random_nonce();
                    let wrapped = wrap_cek(&scope_key, &cek, &wrap_nonce, &hash)?;
                    tx.execute(
                        "INSERT OR IGNORE INTO body_store_key_wraps \
                         (content_hash, scope_id, wrapped_cek, nonce) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            hash.as_slice(),
                            scope_id.as_uuid().as_bytes().as_slice(),
                            wrapped,
                            wrap_nonce.as_slice(),
                        ],
                    )?;
                } else {
                    // Orphaned body_store row: all previous CEK wraps
                    // have been purged (every scope that referenced it
                    // was forgotten). The ciphertext is unrecoverable,
                    // so delete the stale row and fall through to the
                    // new-body path below.
                    tx.execute(
                        "DELETE FROM body_store WHERE content_hash = ?1",
                        params![hash.as_slice()],
                    )?;
                    // Re-encrypt from scratch.
                    let cek = random_cek();
                    let body_nonce = random_nonce();
                    let aad = body_table_aad(&hash);
                    let ciphertext = encrypt_aead(&cek, &body_nonce, body, &aad)?;
                    tx.execute(
                        "INSERT INTO body_store (content_hash, body, nonce, ref_count) \
                         VALUES (?1, ?2, ?3, 1)",
                        params![hash.as_slice(), ciphertext, body_nonce.as_slice()],
                    )?;
                    let wrap_nonce = random_nonce();
                    let wrapped = wrap_cek(&scope_key, &cek, &wrap_nonce, &hash)?;
                    tx.execute(
                        "INSERT INTO body_store_key_wraps \
                         (content_hash, scope_id, wrapped_cek, nonce) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            hash.as_slice(),
                            scope_id.as_uuid().as_bytes().as_slice(),
                            wrapped,
                            wrap_nonce.as_slice(),
                        ],
                    )?;
                }
            }
        } else {
            // New body — generate a random CEK, encrypt the body
            // under it, then wrap the CEK under the ingesting scope's
            // key.
            let cek = random_cek();
            let body_nonce = random_nonce();
            let aad = body_table_aad(&hash);
            let ciphertext = encrypt_aead(&cek, &body_nonce, body, &aad)?;
            tx.execute(
                "INSERT INTO body_store (content_hash, body, nonce, ref_count)
                 VALUES (?1, ?2, ?3, 1)",
                params![hash.as_slice(), ciphertext, body_nonce.as_slice()],
            )?;
            let wrap_nonce = random_nonce();
            let wrapped = wrap_cek(&scope_key, &cek, &wrap_nonce, &hash)?;
            tx.execute(
                "INSERT INTO body_store_key_wraps \
                 (content_hash, scope_id, wrapped_cek, nonce) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    hash.as_slice(),
                    scope_id.as_uuid().as_bytes().as_slice(),
                    wrapped,
                    wrap_nonce.as_slice(),
                ],
            )?;
        }

        let now = Utc::now().timestamp();
        let path_tag = StoragePath::BodyTable as i64;
        tx.execute(
            "INSERT INTO evidence
             (id, scope_id, content_hash, body, body_ref, nonce,
              source_ref, acl_pointer, importance, storage_path,
              created_at, language_tag)
             VALUES (?1, ?2, ?3, NULL, ?4, NULL, ?5, NULL, ?6, ?7, ?8, ?9)",
            params![
                evidence_id.as_uuid().as_bytes().as_slice(),
                scope_id.as_uuid().as_bytes().as_slice(),
                hash.as_slice(),
                hash.as_slice(),
                source_ref,
                importance.as_tag(),
                path_tag,
                now,
                language_tag,
            ],
        )?;
        Self::index_fts(&tx, evidence_id, scope_id, body)?;
        Self::index_embedding_or_copy_dedup(
            &tx,
            evidence_id,
            &hash,
            body,
            self.embedding_model.as_deref(),
            &self.embedding_model_tag,
            now,
        );
        tx.commit()?;

        Ok(IngestResult {
            evidence_id,
            storage_path: StoragePath::BodyTable,
            content_hash: hash,
        })
    }

    /// Write-side dual entry point for all three FTS5 shadow tables
    /// (`evidence_fts` / `evidence_fts_cjk` / `evidence_fts_bigram`).
    ///
    /// **Error-propagation contract** (read this if you're tempted
    /// to "match the read path" and swallow errors on the CJK lanes):
    ///
    /// All three INSERT statements propagate `rusqlite::Error` to
    /// the caller via `?`. This is **asymmetric** with the read-path
    /// contract documented on [`Self::search_fts`], which silently
    /// treats CJK-lane errors as empty results to preserve the
    /// "unicode61 is the source of truth for query validity"
    /// invariant. The asymmetry is intentional:
    ///
    /// * `index_fts` runs **inside the same SQLCipher transaction**
    ///   as the `evidence` row INSERT (see [`Self::ingest_message`]
    ///   / [`Self::ingest_document`]). A swallowed INSERT failure
    ///   on `evidence_fts_cjk` or `evidence_fts_bigram` would
    ///   commit the `evidence` row + `evidence_fts` row without
    ///   the matching CJK / bigram shadow row — a permanent stale
    ///   state with no REBUILD opportunity (REBUILD re-tokenises
    ///   the existing `content` column; it cannot synthesise rows
    ///   that were never inserted). That would silently degrade
    ///   recall on the affected scope forever with no signal to
    ///   the caller or operator.
    /// * `search_fts` runs **outside any transaction** as a
    ///   read-only query. If the engine is transiently broken
    ///   (e.g. an old `sqlite_stat1` regression on a specific
    ///   bundled build), the read path can safely degrade recall
    ///   for the duration of the broken read and recover on the
    ///   next call. There is no committed state to corrupt.
    ///
    /// Hence: propagate on write, swallow on read. Both paths are
    /// part of the same architectural invariant — the source of
    /// truth (`evidence_fts`) is always consistent with the
    /// additive lanes whenever every write transaction commits.
    fn index_fts(
        tx: &rusqlite::Transaction<'_>,
        evidence_id: EvidenceId,
        scope_id: ScopeId,
        body: &[u8],
    ) -> Result<()> {
        // Only index UTF-8 content. Binary blobs (files, media) are
        // not indexed at this layer — observation-plane extraction
        // handles those in a future update.
        let Ok(text) = std::str::from_utf8(body) else {
            return Ok(());
        };
        let evidence_uuid = evidence_id.as_uuid();
        let scope_uuid = scope_id.as_uuid();
        let evidence_id_bytes = evidence_uuid.as_bytes().as_slice();
        let scope_id_bytes = scope_uuid.as_bytes().as_slice();
        // Every UTF-8 row goes into `evidence_fts` (unicode61). The
        // tokeniser segments whitespace-bounded scripts (Latin,
        // Cyrillic, Greek, Arabic, Hangul, Devanagari) including any
        // Latin terms embedded inside an otherwise-CJK document, so
        // this is the universal lexical index.
        tx.execute(
            "INSERT INTO evidence_fts (content, evidence_id, scope_id) VALUES (?1, ?2, ?3)",
            params![text, evidence_id_bytes, scope_id_bytes],
        )?;
        // schema v14: rows whose body contains any CJK
        // Han / Hiragana / Katakana / Thai codepoint *additionally*
        // go into `evidence_fts_cjk` (trigram). `unicode61` emits
        // zero tokens for those codepoints, so without this branch
        // a pure-CJK or pure-Thai document is invisible to lexical
        // search. The routing decision is body-derived (not
        // language-tag-derived) so a row with `language_tag = NULL`
        // or a mis-detected dominant tag still lands in the right
        // table. See `crate::script::contains_cjk_or_thai` for the
        // codepoint membership rationale.
        if crate::script::contains_cjk_or_thai(text) {
            // schema v16: strip recall-lane stopwords
            // (Japanese / Chinese / Thai / Tibetan / Khmer /
            // Myanmar / Lao function words) BEFORE the trigram and
            // bigram lanes index the body. Stripping replaces each
            // matched stopword with a single ASCII space; the
            // trigram and bigram tokenisers treat whitespace as a
            // hard separator so the spurious "particle window"
            // pseudo-matches (e.g. body `今日の鬼ヶ島` matching a
            // query about `今日のオリンピック` on the
            // cross-particle trigram `日のオ`) are eliminated. The
            // unicode61 `evidence_fts.content` insert above is
            // intentionally untouched — the baseline lane is the
            // universal source of truth for plaintext, and BM25's
            // idf weighting already discounts high-frequency
            // particles for whitespace-tokenised scripts. The same
            // strip is symmetrically applied on the query side by
            // [`merged_fts_search`], which is what makes the
            // index- and query-time tokenisations match: stripping
            // on only one side would silently destroy recall on
            // any body that contains a stopword between two
            // content tokens (the stripped side would no longer
            // produce the bridging trigram that the unstripped
            // side still expects). See [`crate::fts_stopwords`]
            // for the symmetric-stripping rationale.
            // Counted variant feeds the index-write
            // stopword strip telemetry — `strip_count` is the
            // number of stopword instances replaced.
            let (stripped, strip_count) =
                crate::fts_stopwords::strip_recall_lane_stopwords_counted(text);
            crate::fts_telemetry::record_stopwords_stripped(
                crate::fts_telemetry::StripSite::IndexWrite,
                strip_count,
            );
            tx.execute(
                "INSERT INTO evidence_fts_cjk (content, evidence_id, scope_id) \
                 VALUES (?1, ?2, ?3)",
                params![stripped.as_ref(), evidence_id_bytes, scope_id_bytes],
            )?;
            // schema v15: rows that route to
            // `evidence_fts_cjk` *additionally* go into
            // `evidence_fts_bigram`, which stores the
            // whitespace-separated overlapping 2-codepoint
            // windows of the CJK / Thai portion of the body
            // under the same `unicode61` tokeniser as
            // `evidence_fts`. The bigram lane is the recall
            // floor that the v14 trigram lane cannot serve —
            // queries with only 2 CJK codepoints (`天気`,
            // `良い`) MATCH here even though they hit the
            // FTS5 trigram 3-codepoint minimum on
            // `evidence_fts_cjk`. We skip the INSERT entirely
            // when the precomputed bigram string is empty
            // (which can only happen if the body has a single
            // CJK / Thai codepoint and no others — extremely
            // rare but possible on a fragmentary OCR ingest);
            // an empty `content` row would still consume an
            // FTS5 docid and inflate the
            // `evidence_fts_bigram_docsize` shadow without
            // contributing any recall.
            //
            // The bigram windowing happens over the SAME stripped
            // text as the trigram lane above. `compute_cjk_bigrams`
            // additionally filters to CJK / Thai codepoints, so
            // the ASCII spaces produced by stripping are dropped
            // before windowing — which means the bigram lane DOES
            // bridge across stripped-particle gaps (a body
            // `日本のオリンピック` after stripping is
            // `日本 オリンピック` then filtered to
            // `日本オリンピック`, yielding the bridging bigram
            // `本オ`). This is symmetric with the query-side
            // bigram path: `compute_cjk_bigram_query` runs over
            // the stripped query and applies the same CJK / Thai
            // filter, so the bridging bigram is requested by the
            // query whenever it would be produced by the body.
            let bigrams = crate::bigram::compute_cjk_bigrams(stripped.as_ref());
            if !bigrams.is_empty() {
                tx.execute(
                    "INSERT INTO evidence_fts_bigram (content, evidence_id, scope_id) \
                     VALUES (?1, ?2, ?3)",
                    params![bigrams, evidence_id_bytes, scope_id_bytes],
                )?;
            }
        }
        Ok(())
    }

    /// Populate the `evidence_embeddings` cache for `evidence_id`.
    /// No-op when no model is wired in, when `body` is not valid UTF-8,
    /// or when the model returns an error — the hybrid retriever falls
    /// back to re-embedding on the read side in those cases. The
    /// embedding is serialised as little-endian raw `f32` bytes.
    ///
    /// SQL-level failures from the cache `INSERT` are also swallowed
    /// for the same reason: the cache row is a performance hint, not
    /// load-bearing data. An out-of-disk / I/O error here would
    /// otherwise abort the surrounding `ingest_*` transaction and
    /// take down the evidence row itself, which has no functional
    /// dependency on the cache.
    fn index_embedding(
        tx: &rusqlite::Transaction<'_>,
        evidence_id: EvidenceId,
        body: &[u8],
        model: Option<&dyn EmbeddingModel>,
        model_tag: &str,
        created_at: i64,
    ) {
        let Some(model) = model else { return };
        let Ok(text) = std::str::from_utf8(body) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        // pre-embedding routing gate. A body
        // classified as noise-only (pure punctuation / emoji /
        // digits / whitespace) is still indexed via FTS5 by the
        // caller — we just skip writing a vector row for it.
        // The retriever's `candidate_embedding` path on a later
        // search will short-circuit the same way, so an absent
        // `evidence_embeddings` row for a noise body is
        // consistent end-to-end.
        let route = crate::embedding_routing::classify_for_embedding(text);
        crate::vector_telemetry::record_pre_embed_decision(route);
        if matches!(route, crate::embedding_routing::EmbeddingRoute::Skip(_)) {
            return;
        }
        // A model failure should not block ingestion — the row is
        // still recoverable via FTS and the retriever's re-embedding
        // fallback. Both the success and the per-variant error are
        // counted in `crate::vector_telemetry` so the operator can
        // see the adapter-health signal alongside the lane invocations.
        let vec = match model.embed(text) {
            Ok(v) => {
                crate::vector_telemetry::record_embedding_computed(
                    crate::vector_telemetry::EmbedSite::IndexWrite,
                );
                v
            }
            Err(err) => {
                crate::vector_telemetry::record_embedding_error_from(&err);
                return;
            }
        };
        // Surface the (model_tag, dimension) observation so a rotation-
        // rule violation (same tag, different dim) is operator-visible
        // via `model_tag_dimension_violations_total`. Best-effort.
        crate::vector_telemetry::record_observed_dimension(model_tag, vec.len());
        let bytes = embedding_to_bytes(&vec);
        // Best-effort write. See the doc-comment above for the
        // rationale — propagating this error would abort the
        // `ingest_*` transaction and lose the evidence row over a
        // cache-table hiccup. We log on `Err` so operators running
        // with `RUST_LOG=evidence_store=debug` can spot a degraded
        // cache (out-of-disk, schema drift, SQLite I/O hiccup)
        // instead of having the failure stay completely invisible.
        if let Err(err) = tx.execute(
            "INSERT OR REPLACE INTO evidence_embeddings
                 (evidence_id, embedding, model_tag, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                evidence_id.as_uuid().as_bytes().as_slice(),
                bytes,
                model_tag,
                created_at,
            ],
        ) {
            tracing::debug!(evidence_id = %evidence_id.as_uuid(),
                model_tag,
                error = %err,
                "evidence_embeddings INSERT swallowed; the row is still recoverable via FTS + the retriever re-embed fallback",
            );
        }
    }

    /// Body-table variant of [`Self::index_embedding`] that takes
    /// advantage of dedup: when an existing evidence row references
    /// the same `content_hash` and was embedded under the same
    /// `model_tag`, the cached vector is byte-for-byte identical to
    /// what re-embedding would produce, so we copy it directly and
    /// skip the model invocation entirely. This is the key win for
    /// high-dedup workloads (e.g. mailing-list threads, replayed
    /// payloads) where re-running the ONNX runtime over identical
    /// content would otherwise be pure waste.
    ///
    /// Falls back to [`Self::index_embedding`] (a fresh embed) when
    /// no prior row matches both the content hash and the active
    /// model tag, or when no model is wired in.
    ///
    /// Like the underlying helper, the cache INSERT is best-effort
    /// and never fails the surrounding ingest transaction.
    fn index_embedding_or_copy_dedup(
        tx: &rusqlite::Transaction<'_>,
        evidence_id: EvidenceId,
        hash: &ContentHash,
        body: &[u8],
        model: Option<&dyn EmbeddingModel>,
        model_tag: &str,
        created_at: i64,
    ) {
        if model.is_none() {
            return;
        }

        // Look for any prior evidence row with the same content hash
        // whose cached embedding was produced by the active model.
        // The join filters out rows from older model tags so we never
        // copy a stale vector that the retriever would have rejected
        // on dimension mismatch.
        let copied: Option<Vec<u8>> = tx
            .query_row(
                "SELECT ee.embedding
                 FROM evidence_embeddings ee
                 JOIN evidence e ON e.id = ee.evidence_id
                 WHERE e.content_hash = ?1 AND ee.model_tag = ?2
                 LIMIT 1",
                params![hash.as_slice(), model_tag],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .unwrap_or(None);

        if let Some(bytes) = copied {
            // Same content + same model ⇒ identical vector. Reuse.
            // Bump the dedup-copy counter so the operator can see
            // how often this short-circuit pays off on real corpora
            // (the dominant write-path optimisation for high-dedup
            // workloads like mailing-list threads / replayed payloads).
            crate::vector_telemetry::record_dedup_copy_hit();
            // Same swallow-and-log discipline as `index_embedding`:
            // the cache row is non-load-bearing data so an INSERT
            // failure must not abort the surrounding transaction,
            // but it should at least show up under `RUST_LOG=debug`.
            if let Err(err) = tx.execute(
                "INSERT OR REPLACE INTO evidence_embeddings
                     (evidence_id, embedding, model_tag, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    evidence_id.as_uuid().as_bytes().as_slice(),
                    bytes,
                    model_tag,
                    created_at,
                ],
            ) {
                tracing::debug!(evidence_id = %evidence_id.as_uuid(),
                    model_tag,
                    error = %err,
                    "evidence_embeddings dedup-copy INSERT swallowed; the row is still recoverable via FTS + the retriever re-embed fallback",
                );
            }
            return;
        }

        // No prior embedding to copy from — fall through to a fresh
        // embed via the standard helper.
        Self::index_embedding(tx, evidence_id, body, model, model_tag, created_at);
    }

    /// Read the plaintext body of an evidence row.
    pub fn read_body(&self, evidence_id: EvidenceId) -> Result<Vec<u8>> {
        let row: Option<(
            Vec<u8>,
            Vec<u8>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Vec<u8>,
            i64,
        )> = self
            .conn
            .query_row(
                "SELECT scope_id, content_hash, body, body_ref, COALESCE(nonce, X''),
                            storage_path
                     FROM evidence WHERE id = ?1",
                params![evidence_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;

        let (scope_bytes, hash_bytes, inline_body, body_ref, nonce_bytes, path_tag) =
            row.ok_or_else(|| EvidenceError::NotFound(format!("evidence_id={evidence_id}")))?;

        let scope_id = ScopeId::from_uuid(slice_to_uuid(&scope_bytes)?);
        let mut content_hash_arr = [0u8; 32];
        if hash_bytes.len() != 32 {
            return Err(EvidenceError::Schema("content_hash column has wrong width"));
        }
        content_hash_arr.copy_from_slice(&hash_bytes);

        match path_tag {
            t if t == StoragePath::Inline as i64 => {
                let key = self.scope_key(scope_id)?;
                let body =
                    inline_body.ok_or(EvidenceError::Schema("inline row missing body column"))?;
                if nonce_bytes.len() != AEAD_NONCE_LEN {
                    return Err(EvidenceError::Schema("inline row has malformed nonce"));
                }
                let mut nonce = [0u8; AEAD_NONCE_LEN];
                nonce.copy_from_slice(&nonce_bytes);
                let aad = ingest_aad(scope_id, evidence_id, &content_hash_arr);
                let pt = decrypt_aead(&key, &nonce, &body, &aad)?;
                Ok(pt)
            }
            t if t == StoragePath::BodyTable as i64 => {
                let body_ref =
                    body_ref.ok_or(EvidenceError::Schema("body-table row missing body_ref"))?;
                let (ct, body_nonce_bytes): (Vec<u8>, Vec<u8>) = self
                    .conn
                    .query_row(
                        "SELECT body, nonce FROM body_store WHERE content_hash = ?1",
                        params![body_ref.as_slice()],
                        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
                    )
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => EvidenceError::DanglingBodyRef,
                        other => EvidenceError::Sqlite(other),
                    })?;
                if body_nonce_bytes.len() != AEAD_NONCE_LEN {
                    return Err(EvidenceError::Schema("body_store row has malformed nonce"));
                }
                let mut body_nonce = [0u8; AEAD_NONCE_LEN];
                body_nonce.copy_from_slice(&body_nonce_bytes);

                // Unwrap the per-scope CEK wrap for this scope, then
                // decrypt the body under the recovered CEK.
                let scope_key = self.scope_key(scope_id)?;
                let (wrapped_cek, wrap_nonce_bytes): (Vec<u8>, Vec<u8>) = self
                    .conn
                    .query_row(
                        "SELECT wrapped_cek, nonce FROM body_store_key_wraps \
                         WHERE content_hash = ?1 AND scope_id = ?2",
                        params![
                            body_ref.as_slice(),
                            scope_id.as_uuid().as_bytes().as_slice(),
                        ],
                        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
                    )
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => EvidenceError::DanglingBodyRef,
                        other => EvidenceError::Sqlite(other),
                    })?;
                let cek = unwrap_cek(
                    &scope_key,
                    &wrapped_cek,
                    &wrap_nonce_bytes,
                    &content_hash_arr,
                )?;
                let aad = body_table_aad(&content_hash_arr);
                let pt = decrypt_aead(&cek, &body_nonce, &ct, &aad)?;
                Ok(pt)
            }
            _ => Err(EvidenceError::Schema(
                "evidence row has unknown storage_path tag",
            )),
        }
    }

    /// Read all metadata for an evidence row (no body decryption).
    pub fn get(&self, evidence_id: EvidenceId) -> Result<Option<EvidenceRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT scope_id, content_hash, source_ref, acl_pointer, importance,
                        storage_path, created_at, language_tag
                 FROM evidence WHERE id = ?1",
                params![evidence_id.as_uuid().as_bytes().as_slice()],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i32>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;

        let Some((
            scope_bytes,
            hash_bytes,
            source_ref,
            acl_pointer,
            imp_tag,
            path_tag,
            created,
            language_tag,
        )) = row
        else {
            return Ok(None);
        };

        let scope_id = ScopeId::from_uuid(slice_to_uuid(&scope_bytes)?);
        if hash_bytes.len() != 32 {
            return Err(EvidenceError::Schema("content_hash column has wrong width"));
        }
        let mut content_hash_arr = [0u8; 32];
        content_hash_arr.copy_from_slice(&hash_bytes);

        let importance = ImportanceClass::from_tag(imp_tag).ok_or(EvidenceError::Schema(
            "evidence row has unknown importance tag",
        ))?;
        let storage_path = match path_tag {
            t if t == StoragePath::Inline as i64 => StoragePath::Inline,
            t if t == StoragePath::BodyTable as i64 => StoragePath::BodyTable,
            _ => {
                return Err(EvidenceError::Schema(
                    "evidence row has unknown storage_path tag",
                ))
            }
        };

        Ok(Some(EvidenceRow {
            id: evidence_id,
            scope_id,
            content_hash: content_hash_arr,
            source_ref,
            acl_pointer,
            importance,
            storage_path,
            created_at: created,
            language_tag,
        }))
    }

    /// List the most recent `limit` evidence row ids for `scope_id`,
    /// ordered newest → oldest by `created_at` (ties broken by `id`
    /// for determinism). Uses the `(scope_id, created_at DESC)`
    /// covering index added in schema v1, so the query is index-only
    /// and does not scan the table.
    ///
    /// Returns `Ok(vec![])` for scopes with no rows. Callers feed the
    /// returned ids through [`Self::read_body`] when they need the
    /// plaintext payloads — the synthesis pipeline uses this to build
    /// SLM prompts from a scope's recent evidence window.
    pub fn recent_evidence_ids_for_scope(
        &self,
        scope_id: ScopeId,
        limit: usize,
    ) -> Result<Vec<EvidenceId>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM evidence
             WHERE scope_id = ?1
             ORDER BY created_at DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                clamp_limit_to_sqlite(limit),
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(EvidenceId(slice_to_uuid(&row?)?));
        }
        Ok(out)
    }

    /// Run an FTS5 search scoped to `scope_id`.
    ///
    /// The query is passed straight through to FTS5; callers should
    /// pre-process per FTS5's syntax (e.g. quote phrases). The result
    /// is the matching evidence ids ordered by FTS5 rank.
    ///
    /// As of schema v15 the search fans out across
    /// **three** lexical indexes and de-duplicates on `evidence_id`,
    /// taking the best (smallest, since FTS5 rank is negative-and-
    /// smaller-is-better) of whichever ranks the row appears under:
    ///
    /// * `evidence_fts` (unicode61, schema v1) — universal lane for
    ///   whitespace / punctuation-segmented scripts.
    /// * `evidence_fts_cjk` (trigram, schema v14) — CJK /
    ///   Thai substring lane for queries of ≥ 3 codepoints (FTS5's
    ///   built-in trigram tokeniser cannot serve shorter queries).
    /// * `evidence_fts_bigram` (precomputed bigrams under
    ///   `unicode61`, schema v15) — CJK / Thai recall
    ///   lane for **2-codepoint** queries like `天気` (Japanese
    ///   "weather") that the trigram lane cannot serve. Bigrams are
    ///   computed at write time over the CJK / Thai portion of the
    ///   body (see [`crate::bigram`]) and stored as whitespace-
    ///   separated tokens, so the same `unicode61` tokeniser that
    ///   powers `evidence_fts` matches them word-by-word.
    ///
    /// **Query-syntax compatibility, not equivalence.** All three
    /// branches accept the same FTS5 query *grammar* (the bareword
    /// / `"phrase"` / `term1 OR term2` / `NEAR(…)` / column-filter /
    /// prefix-star syntax described in
    /// <https://sqlite.org/fts5.html#full_text_query_syntax>).
    /// They differ in what each tokeniser is able to match, and the
    /// `trigram` branch rejects some queries that `unicode61`
    /// accepts. Per the [`trigram` tokeniser documentation][trigram-doc]:
    ///
    /// * `unicode61` (`evidence_fts`) splits on Unicode whitespace
    ///   and punctuation and is happy with single-codepoint terms.
    ///   A query like `"to OR deadline"` is well-formed and may
    ///   match real rows.
    /// * `trigram` (`evidence_fts_cjk`) only stores overlapping
    ///   3-codepoint windows of `content`. It returns a SQLite
    ///   error — not an empty result set — when given a query term
    ///   shorter than 3 characters, a `NEAR(…)` expression, a
    ///   column filter, or a prefix-star (`term*`) match shorter
    ///   than 3 codepoints.
    /// * `unicode61` over precomputed bigrams (`evidence_fts_bigram`)
    ///   only sees rows whose body contributed bigrams (i.e. bodies
    ///   that contain at least one CJK / Thai codepoint). Queries
    ///   below 2 CJK codepoints, or pure-ASCII queries, route to
    ///   it as empty matches that are folded into the merge.
    ///
    /// To preserve the **architectural invariant that a syntactically
    /// valid `unicode61` query never breaks the substrate's search
    /// API** — even when that query happens to be a `trigram`-
    /// rejected shape — the implementation runs each branch as an
    /// independent prepared statement and merges in Rust:
    ///
    /// * The `unicode61` branch (`evidence_fts`) is the **source of
    ///   truth for query validity**: any error from
    ///   `evidence_fts MATCH ?1` propagates to the caller (genuine
    ///   FTS5 syntax error, schema corruption, etc).
    /// * The `trigram` branch (`evidence_fts_cjk`) is **purely
    ///   additive recall**: any error from `evidence_fts_cjk
    ///   MATCH ?1` — including the short-term / `NEAR(…)` /
    ///   column-filter rejections — is silently treated as an
    ///   empty result set, so the caller sees the `unicode61`
    ///   results unchanged.
    /// * The `unicode61`-over-bigrams branch (`evidence_fts_bigram`)
    ///   is **purely additive recall** on the same contract as the
    ///   trigram branch. Queries with fewer than 2 CJK / Thai
    ///   codepoints round-trip as empty bigram matches, so adding
    ///   the branch never changes the result set of an earlier
    ///   pure-Latin query.
    ///
    /// Concretely, a 2-codepoint CJK query like `天気` — which the
    /// v14 trigram lane could not serve — now returns matches via
    /// the bigram lane (closing the documented 2-codepoint recall
    /// floor). A mixed query like `"to OR 良い天気"` still returns
    /// the `unicode61` hit on `to` even though the trigram branch
    /// rejects the 2-char `to` token, AND additionally picks up CJK
    /// matches via both the trigram lane (3+ codepoint `良い天`,
    /// `い天気`) and the bigram lane (2-codepoint windows of
    /// `良い天気`).
    ///
    /// This design also makes the substrate robust to the exact
    /// bundled SQLite version's lenience — older builds may silently
    /// return empty for short-term trigram queries while the
    /// documented contract is to error. Either way, the public
    /// `search_fts` contract is unchanged.
    ///
    /// [trigram-doc]: <https://www.sqlite.org/fts5.html#the_trigram_tokenizer>
    pub fn search_fts(
        &self,
        scope_id: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceId>> {
        let merged = merged_fts_search(&self.conn, scope_id, query, limit)?;
        Ok(merged.into_iter().map(|(id, _rank)| id).collect())
    }

    /// Test-only variant of [`Self::search_fts`] that exposes the
    /// post-lane-weighting BM25 rank for every returned row, so
    /// integration tests can pin the cross-lane
    /// precision-vs-recall hierarchy at the rank-arithmetic level
    /// (and not just at the recall-preservation level that
    /// [`Self::search_fts`] would surface).
    ///
    /// The ranks are the **post-weight** scores the public
    /// surface uses internally for cross-lane MIN-merge — so
    /// e.g. a 2-char CJK query that only hits the bigram lane
    /// returns the raw BM25 rank multiplied by
    /// [`crate::fts_weights::EVIDENCE_FTS_BIGRAM_LANE_WEIGHT`].
    /// This is the integration-test counterpart to the unit-test
    /// pinning in [`crate::fts_weights::tests`] and the SQL-
    /// shape pinning in [`lane_sql_tests`].
    ///
    /// Only available with the `test-support` feature (or in unit
    /// tests of this crate). The public surface remains the
    /// ID-only [`Self::search_fts`] because cross-lane raw ranks
    /// are not directly comparable to BM25 numbers a caller might
    /// produce against the FTS5 tables directly — exposing them
    /// in the public API would leak the lane-weight constants as
    /// a stable interface, which they are not.
    #[cfg(any(test, feature = "test-support"))]
    pub fn search_fts_with_weighted_ranks_for_tests(
        &self,
        scope_id: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(EvidenceId, f64)>> {
        merged_fts_search(&self.conn, scope_id, query, limit)
    }

    /// Insert a noise-class body into the ring buffer.
    ///
    /// Routinely exceeds the configured cap is fine — older entries
    /// are evicted FIFO until the buffer fits within the cap again.
    pub fn ring_buffer_insert(&mut self, scope_id: ScopeId, body: &[u8]) -> Result<()> {
        let key = self.scope_key(scope_id)?;
        let nonce = random_nonce();
        let aad = ring_buffer_aad(scope_id);
        let ciphertext = encrypt_aead(&key, &nonce, body, &aad)?;
        // Payload size fits in SQLite's signed 64-bit INTEGER on any
        // realistic deployment (ciphertext + nonce << 2^63). Clamp
        // defensively rather than overflow if the bound ever changes.
        let payload_size = i64::try_from(ciphertext.len() + nonce.len()).unwrap_or(i64::MAX);
        // Unix epoch *seconds*, matching the documented type of
        // [`RingBufferEntry::created_at`] and the `evidence` table's
        // `created_at` column.
        let now = Utc::now().timestamp();

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO ring_buffer (scope_id, body, nonce, payload_size, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                ciphertext,
                nonce.as_slice(),
                payload_size,
                now,
            ],
        )?;

        // Evict oldest until we fit within the cap. Clamp the
        // configured cap into SQLite's signed 64-bit range — a
        // value > i64::MAX would mean "unbounded", which is what
        // saturating to i64::MAX expresses.
        let cap = i64::try_from(self.config.ring_buffer_max_bytes).unwrap_or(i64::MAX);
        loop {
            let total: i64 = tx.query_row(
                "SELECT COALESCE(SUM(payload_size), 0) FROM ring_buffer",
                [],
                |r| r.get(0),
            )?;
            if total <= cap {
                break;
            }
            // Delete the oldest row by created_at, then by id as a
            // tiebreaker.
            let rows_changed = tx.execute("DELETE FROM ring_buffer WHERE id = (SELECT id FROM ring_buffer ORDER BY created_at ASC, id ASC LIMIT 1
                 )",
                [],
            )?;
            if rows_changed == 0 {
                // Nothing to evict but we're still over cap — bail to
                // avoid an infinite loop. This can only happen if a
                // single entry exceeds the cap on its own.
                break;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Read all ring buffer entries for `scope_id`, ordered oldest →
    /// newest. Bodies are decrypted on read.
    pub fn ring_buffer_read_window(&mut self, scope_id: ScopeId) -> Result<Vec<RingBufferEntry>> {
        let key = self.scope_key(scope_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT id, body, nonce, created_at
             FROM ring_buffer WHERE scope_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let aad = ring_buffer_aad(scope_id);
        let rows = stmt.query_map(params![scope_id.as_uuid().as_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, ct, nonce_bytes, created_at) = row?;
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                return Err(EvidenceError::Schema("ring_buffer row has malformed nonce"));
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&nonce_bytes);
            let pt = decrypt_aead(&key, &nonce, &ct, &aad)?;
            out.push(RingBufferEntry {
                id,
                scope_id,
                body: pt,
                created_at,
            });
        }
        Ok(out)
    }

    /// Return the current ring-buffer size in bytes (sum of encrypted
    /// payloads + nonces across all scopes).
    pub fn ring_buffer_current_size(&self) -> Result<usize> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(payload_size), 0) FROM ring_buffer",
            [],
            |r| r.get(0),
        )?;
        Ok(i64_count_to_usize(total))
    }

    /// Return the number of ring-buffer entries (across all scopes).
    pub fn ring_buffer_len(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM ring_buffer", [], |r| r.get(0))?;
        Ok(i64_count_to_usize(n))
    }

    /// Drop every ring-buffer entry across all scopes.
    pub fn ring_buffer_clear(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM ring_buffer", [])?;
        Ok(())
    }

    /// Number of evidence rows in the store. Useful in tests.
    pub fn evidence_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get(0))?;
        Ok(i64_count_to_usize(n))
    }

    /// Number of distinct body-table rows. Useful in tests of dedup.
    pub fn body_store_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM body_store", [], |r| r.get(0))?;
        Ok(i64_count_to_usize(n))
    }

    /// Reference count for a body in the body-store. Useful in tests
    /// of dedup.
    pub fn body_ref_count(&self, hash: &ContentHash) -> Result<Option<i64>> {
        let val: Option<i64> = self
            .conn
            .query_row(
                "SELECT ref_count FROM body_store WHERE content_hash = ?1",
                params![hash.as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(val)
    }

    /// Borrow the underlying connection for advanced callers (mostly
    /// for tests verifying append-only behaviour through raw SQL).
    pub fn raw_conn(&self) -> &Connection {
        &self.conn
    }

    /// Record a durable cryptographic-forgetting tombstone for
    /// `scope_id`.
    ///
    /// Inserts into the `forgotten_scopes` table with `forgotten_at`
    /// set to the current wall-clock (Unix epoch seconds). `INSERT
    /// OR IGNORE` makes the operation idempotent — re-recording an
    /// already-forgotten scope is a no-op rather than a failure, so
    /// callers can replay this from a host-side persisted log
    /// without special-casing duplicates.
    ///
    /// This **only** persists the tombstone; the in-memory
    /// [`crypto::forgetting::DekRegistry`] zeroize and the FTS
    /// purge are separate concerns owned by the FFI runtime and
    /// [`Self::purge_fts_for_scope`] respectively.
    pub fn record_forgotten_scope(&mut self, scope_id: ScopeId) -> Result<()> {
        let now = Utc::now().timestamp();
        self.record_forgotten_scope_at(scope_id, now)
    }

    /// Like [`Self::record_forgotten_scope`], but persists `at`
    /// (Unix epoch seconds) as the `forgotten_at` column instead of
    /// calling `Utc::now()` internally.
    ///
    /// The FFI runtime's `TombstoneStore` impl uses this entry
    /// point so the on-disk tombstone records *the exact instant*
    /// the in-memory [`crypto::forgetting::destroy_scope_dek`] call
    /// produced, rather than a slightly-later wall-clock reading.
    /// Like its sibling, this is idempotent — re-recording an
    /// existing tombstone is a no-op via `INSERT OR IGNORE`.
    pub fn record_forgotten_scope_at(&mut self, scope_id: ScopeId, at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO forgotten_scopes (scope_id, forgotten_at) VALUES (?1, ?2)",
            params![scope_id.as_uuid().as_bytes().as_slice(), at],
        )?;
        Ok(())
    }

    /// Load every persisted cryptographic-forgetting tombstone.
    ///
    /// The FFI runtime calls this once on `open_store` and replays
    /// every returned [`ScopeId`] through
    /// [`crypto::forgetting::destroy_scope_dek`] so that the
    /// in-memory `DekRegistry` matches the on-disk record. The
    /// resulting registry is what every `is_scope_forgotten` check
    /// short-circuits on.
    ///
    /// The returned ordering is unspecified; callers must not rely
    /// on it.
    pub fn load_forgotten_scopes(&self) -> Result<Vec<ScopeId>> {
        let mut stmt = self.conn.prepare("SELECT scope_id FROM forgotten_scopes")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = row?;
            out.push(ScopeId::from_uuid(slice_to_uuid(&bytes)?));
        }
        Ok(out)
    }

    /// Same as [`Self::load_forgotten_scopes`] but returns the
    /// `forgotten_at` column alongside the scope id. The FFI
    /// runtime's `TombstoneStore::load_forgotten_scopes` impl uses
    /// this entry point.
    pub fn load_forgotten_scopes_with_timestamps(&self) -> Result<Vec<(ScopeId, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT scope_id, forgotten_at FROM forgotten_scopes")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (bytes, ts) = row?;
            out.push((ScopeId::from_uuid(slice_to_uuid(&bytes)?), ts));
        }
        Ok(out)
    }

    /// Persist a per-`(scope, epoch)` cryptographic-forgetting
    /// tombstone produced by
    /// [`crypto::forgetting::destroy_epoch_dek`]. The on-disk
    /// record makes the destruction durable across restarts; the
    /// FFI runtime replays it through `destroy_epoch_dek` on
    /// `open_store` so post-restart calls for the forgotten epoch
    /// short-circuit.
    ///
    /// Idempotent via `INSERT OR IGNORE` on the `(scope_id,
    /// epoch_id)` primary key — re-recording an existing tombstone
    /// is a no-op rather than an error.
    pub fn record_epoch_tombstone(
        &mut self,
        scope_id: ScopeId,
        epoch_id: u64,
        at: i64,
    ) -> Result<()> {
        // SQLite REAL/INTEGER columns are i64. Epoch ids are u64
        // by definition (per `crypto::forgetting::EpochId`), but
        // in practice we never get anywhere near 2^63 — the
        // rotation policy default is 24h or 16 GiB per epoch, so
        // a single scope would need ~2.5e13 years of continuous
        // rotation to overflow. We reject the impossible case
        // explicitly so a corrupt host-side counter cannot silently
        // wrap into negative ids.
        let epoch_signed = i64::try_from(epoch_id).map_err(|_| {
            EvidenceError::Schema("epoch_id overflows i64 — refusing to persist tombstone")
        })?;
        self.conn.execute(
            "INSERT OR IGNORE INTO epoch_tombstones (scope_id, epoch_id, forgotten_at) \
             VALUES (?1, ?2, ?3)",
            params![scope_id.as_uuid().as_bytes().as_slice(), epoch_signed, at],
        )?;
        Ok(())
    }

    /// Load every persisted per-`(scope, epoch)` tombstone. The
    /// FFI runtime's `TombstoneStore::load_tombstones` impl uses
    /// this entry point on `open_store` to rebuild the in-memory
    /// [`crypto::forgetting::DekRegistry`] so post-restart calls
    /// for forgotten epochs continue to short-circuit.
    ///
    /// The returned ordering is unspecified.
    pub fn load_epoch_tombstones(&self) -> Result<Vec<(ScopeId, u64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT scope_id, epoch_id, forgotten_at FROM epoch_tombstones")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (bytes, epoch_signed, ts) = row?;
            let epoch = u64::try_from(epoch_signed).map_err(|_| {
                EvidenceError::Schema("epoch_tombstones.epoch_id is negative — database corruption")
            })?;
            out.push((ScopeId::from_uuid(slice_to_uuid(&bytes)?), epoch, ts));
        }
        Ok(out)
    }

    /// Return a snapshot of the in-memory scope-key cache. Used by
    /// `open_store` to populate the `DekRegistry` without a second
    /// DB round-trip.
    pub fn cached_scope_keys(&self) -> std::collections::HashMap<ScopeId, AeadKey> {
        self.scope_keys.read().unwrap().clone()
    }

    /// Count the entries in the in-memory scope-key cache without
    /// cloning every key. Used by the FFI `health_check` crypto
    /// probe (which only needs the count, not the keys) so the
    /// per-probe cost stays O(1) instead of O(N · key-bytes). The
    /// underlying lock is a `std::sync::RwLock` shared read guard,
    /// so concurrent count-readers do not serialize against each
    /// other; a writer (e.g. a `register_scope_key` path) will
    /// still briefly block readers, but the critical section is a
    /// single `HashMap::len()`.
    pub fn cached_scope_key_count(&self) -> usize {
        self.scope_keys.read().unwrap().len()
    }

    /// Remove a scope key from the in-memory cache only (does NOT
    /// touch the `scope_deks` table). Used during `open_store` to
    /// evict keys for forgotten scopes as a defense-in-depth measure.
    pub fn evict_cached_scope_key(&self, scope_id: ScopeId) {
        self.scope_keys.write().unwrap().remove(&scope_id);
    }

    // ───────────────── Independently-generated scope DEKs (C2) ─────

    /// Derive the wrapping key used to AEAD-encrypt scope DEKs at
    /// rest. The wrapping key itself is HKDF-derived from the master
    /// key — that is fine because the *wrapped* material (the scope
    /// DEK) is independently generated. An attacker with the master
    /// key can derive the wrapping key, but after a `forget()` the
    /// wrapped DEK row is deleted and the scope key is truly
    /// unrecoverable.
    fn dek_wrapping_key(&self) -> Result<AeadKey> {
        derive_key(&self.master_key, b"scope-dek-wrap:v1").map_err(Into::into)
    }

    /// Persist a new independently-generated scope DEK.
    ///
    /// `dek` is the raw 32-byte AEAD key. It is wrapped (encrypted)
    /// under the master-derived wrapping key and stored in the
    /// `scope_deks` table. Idempotent: re-inserting an existing
    /// scope DEK is a no-op via `INSERT OR IGNORE`.
    pub fn store_scope_dek(&mut self, scope_id: ScopeId, dek: &AeadKey) -> Result<()> {
        let wrap_key = self.dek_wrapping_key()?;
        let nonce = random_nonce();
        let aad = scope_dek_aad(scope_id);
        let wrapped = encrypt_aead(&wrap_key, &nonce, dek.as_slice(), &aad)?;
        let now = Utc::now().timestamp();
        let rows_changed = self.conn.execute(
            "INSERT OR IGNORE INTO scope_deks (scope_id, wrapped_dek, nonce, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                wrapped,
                nonce.as_slice(),
                now
            ],
        )?;
        // Only update the in-memory cache when the DB row was actually
        // inserted. When INSERT OR IGNORE is a no-op (row already
        // exists), the DB retains the original key and the cache must
        // stay consistent with it.
        if rows_changed > 0 {
            self.scope_keys.write().unwrap().insert(scope_id, *dek);
        }
        Ok(())
    }

    /// Load every persisted scope DEK, unwrap it, and return the map.
    ///
    /// Called once by `open_store` to hydrate the in-memory key cache
    /// from the durable `scope_deks` table.
    pub fn load_scope_deks(&self) -> Result<std::collections::HashMap<ScopeId, AeadKey>> {
        let wrap_key = self.dek_wrapping_key()?;
        let mut stmt = self
            .conn
            .prepare("SELECT scope_id, wrapped_dek, nonce FROM scope_deks")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (scope_bytes, wrapped, nonce_bytes) = row?;
            let scope = ScopeId::from_uuid(slice_to_uuid(&scope_bytes)?);
            let aad = scope_dek_aad(scope);
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                return Err(EvidenceError::Schema(
                    "scope DEK nonce has wrong length in scope_deks table",
                ));
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&nonce_bytes);
            let plain = decrypt_aead(&wrap_key, &nonce, &wrapped, &aad)?;
            if plain.len() != AEAD_KEY_LEN {
                return Err(EvidenceError::Schema(
                    "unwrapped scope DEK has wrong length",
                ));
            }
            let mut key = [0u8; AEAD_KEY_LEN];
            key.copy_from_slice(&plain);
            out.insert(scope, key);
        }
        Ok(out)
    }

    /// Delete the wrapped scope DEK for `scope_id` from the
    /// `scope_deks` table and remove it from the in-memory cache.
    ///
    /// After this call, the scope key is truly unrecoverable — the
    /// master-derived wrapping key can no longer reconstruct it
    /// because the wrapped material has been deleted.
    pub fn delete_scope_dek(&mut self, scope_id: ScopeId) -> Result<()> {
        self.delete_scope_dek_row(scope_id)?;
        self.scope_keys.write().unwrap().remove(&scope_id);
        Ok(())
    }

    /// Delete just the `scope_deks` DB row for `scope_id` without
    /// touching the in-memory cache. Takes `&self` so it can be
    /// called during `open_store` iteration where the cache is
    /// managed separately via [`Self::evict_cached_scope_key`].
    pub fn delete_scope_dek_row(&self, scope_id: ScopeId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM scope_deks WHERE scope_id = ?1",
            params![scope_id.as_uuid().as_bytes().as_slice()],
        )?;
        Ok(())
    }

    /// Ensure the scope has an AEAD key in the in-memory cache,
    /// generating and persisting a new random DEK only when the scope
    /// is genuinely new.
    ///
    /// Resolution order:
    /// 1. In-memory cache (fast path).
    /// 2. `scope_deks` DB table — the wrapped DEK may have been
    ///    persisted by a prior session but not yet loaded into cache.
    /// 3. Legacy HKDF fallback — if evidence rows exist for this
    ///    scope (pre-v6 database), the scope was encrypted under an
    ///    HKDF-derived key. We adopt that key and persist it in
    ///    `scope_deks` so future opens find it directly.
    /// 4. Fresh random DEK drawn from the OS RNG (`SysRng`) — only
    ///    for genuinely new scopes with no prior evidence.
    pub fn ensure_scope_dek(&mut self, scope_id: ScopeId) -> Result<AeadKey> {
        // 1. Fast path: already in memory.
        if let Some(k) = self.scope_keys.read().unwrap().get(&scope_id) {
            return Ok(*k);
        }

        // 2. Check the durable scope_deks table.
        let wrap_key = self.dek_wrapping_key()?;
        let db_row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT wrapped_dek, nonce FROM scope_deks WHERE scope_id = ?1",
                params![scope_id.as_uuid().as_bytes().as_slice()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        if let Some((wrapped, nonce_bytes)) = db_row {
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                return Err(EvidenceError::Schema(
                    "scope DEK nonce has wrong length in scope_deks table",
                ));
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&nonce_bytes);
            let aad = scope_dek_aad(scope_id);
            let plain = decrypt_aead(&wrap_key, &nonce, &wrapped, &aad)?;
            if plain.len() != AEAD_KEY_LEN {
                return Err(EvidenceError::Schema(
                    "unwrapped scope DEK has wrong length",
                ));
            }
            let mut key = [0u8; AEAD_KEY_LEN];
            key.copy_from_slice(&plain);
            self.scope_keys.write().unwrap().insert(scope_id, key);
            return Ok(key);
        }

        // 3. Legacy check: if evidence rows exist for this scope
        //    they were encrypted under the HKDF-derived key. Adopt
        //    that key and persist it so future opens find it.
        let has_evidence: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM evidence WHERE scope_id = ?1)",
            params![scope_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if has_evidence {
            let label = format!("scope:{}:body:v1", scope_id.as_uuid());
            let key = derive_key(&self.master_key, label.as_bytes())?;
            self.store_scope_dek(scope_id, &key)?;
            return Ok(key);
        }

        // 4. Genuinely new scope: generate a fresh random DEK.
        let mut dek = [0u8; AEAD_KEY_LEN];
        // `SysRng` is fallible (rand 0.10's `TryRng` trait); calling
        // `try_fill_bytes(...).expect(...)` panics on OS RNG failure
        // — the correct behavior for DEK generation, because a
        // substrate that cannot draw entropy cannot safely create
        // new encrypted scopes. Panicking surfaces the breakage
        // rather than silently producing weak keys. Called via UFCS
        // (`rand::TryRng::try_fill_bytes(&mut rand::rngs::SysRng, …)`)
        // to avoid a mid-function `use` that clippy's
        // `items-after-statements` lint would flag.
        rand::TryRng::try_fill_bytes(&mut rand::rngs::SysRng, &mut dek).expect("OS RNG failure");
        self.store_scope_dek(scope_id, &dek)?;
        Ok(dek)
    }

    // ───────────────── Offline master-key rotation ─────────────────

    /// Rotate the store from its current master key (the one it was
    /// opened with) to `new_master_key`, writing the rotated database
    /// to `dest_path`.
    ///
    /// This is the cryptographic core of the offline rotation tool.
    /// **The caller is responsible for the surrounding choreography**
    /// (stopping writers, then atomically swapping `dest_path` over the
    /// live store) — see `substrate_server::key_rotation`. This method
    /// neither mutates the source store nor touches the live file; it
    /// only produces a verified rotated copy.
    ///
    /// # What rotates, and what does not
    ///
    /// Only two pieces of key material are derived from the master key:
    /// the SQLCipher page key (`sqlcipher:store:v1`) and the scope-DEK
    /// wrapping key (`scope-dek-wrap:v1`). Evidence bodies are encrypted
    /// under per-scope DEKs that are *independent* of the master key, so
    /// they never need re-encryption. The steps are:
    ///
    /// 1. Resolve the current key for every scope that owns encrypted
    ///    data. `scope_key` resolves the in-memory cache, the stored
    ///    `scope_deks` table, and the legacy HKDF-derived fallback
    ///    uniformly, so the map holds the *actual* key each body is
    ///    encrypted under. Scopes that have been cryptographically
    ///    forgotten are excluded (their DEK is gone; their leftover
    ///    append-only ciphertext is copied verbatim but stays
    ///    unreadable by design).
    /// 2. `VACUUM INTO` the source into `dest_path` — a consistent,
    ///    defragmented copy still encrypted under the *old* page key.
    /// 3. Rekey the copy's SQLCipher page key to the new master and
    ///    re-wrap every resolved scope DEK under the new wrapping key
    ///    (`INSERT OR REPLACE`, which both re-wraps existing rows and
    ///    persists previously-legacy scopes as explicit DEKs).
    /// 4. Re-open the copy under the new master key and verify
    ///    integrity: every DEK unwraps to the same bytes, the evidence
    ///    row count matches, and every live body decrypts to identical
    ///    plaintext.
    ///
    /// # Errors
    ///
    /// Returns [`EvidenceError::KeyRotation`] if `dest_path` already
    /// exists or any integrity check fails, and propagates SQLite /
    /// crypto errors from the underlying operations. On any error the
    /// caller MUST discard `dest_path` and keep the original store.
    pub fn rotate_master_key(
        &self,
        new_master_key: &MasterKey,
        dest_path: &Path,
    ) -> Result<MasterKeyRotationReport> {
        if dest_path.exists() {
            return Err(EvidenceError::KeyRotation(format!(
                "destination path already exists: {}",
                dest_path.display()
            )));
        }
        let dest_str = dest_path.to_str().ok_or_else(|| {
            EvidenceError::KeyRotation("destination path is not valid UTF-8".to_string())
        })?;

        // 1. Resolve every live scope's key (cache / stored DEK /
        //    legacy HKDF), excluding cryptographically forgotten scopes.
        let forgotten: std::collections::HashSet<ScopeId> =
            self.load_forgotten_scopes()?.into_iter().collect();
        let mut scope_keys: HashMap<ScopeId, AeadKey> = HashMap::new();
        for scope in self.all_data_scopes()? {
            if forgotten.contains(&scope) {
                continue;
            }
            scope_keys.insert(scope, self.scope_key(scope)?);
        }

        // 2. Snapshot the source into `dest_path` under the old page key.
        self.conn
            .execute("VACUUM main INTO ?1", params![dest_str])?;

        // 3. Rekey the copy and re-wrap the scope DEKs. A raw connection
        //    lets us drive the exact `PRAGMA key`/`rekey` sequence.
        {
            let mut conn = Connection::open(dest_path)?;
            let old_key_hex = page_key_hex(&self.master_key)?;
            // Wrap the `x'…'` pragma value in `Zeroizing` so the full
            // page key does not linger in freed heap — same rationale as
            // `EvidenceStore::open`. Both the old and new page keys are
            // handled this way.
            let old_key_pragma: Zeroizing<String> = Zeroizing::new(format!("x'{}'", &*old_key_hex));
            conn.pragma_update(None, "key", old_key_pragma.as_str())?;
            conn.pragma_update(None, "cipher_page_size", 4096_i64)?;
            conn.pragma_update(None, "kdf_iter", 256_000_i64)?;
            // Confirm the old key actually unlocks the vacuumed copy
            // before we rekey it.
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|_| {
                EvidenceError::KeyRotation(
                    "rotated copy did not unlock with the existing key".to_string(),
                )
            })?;

            // Rekey the page encryption to the new master.
            let new_key_hex = page_key_hex(new_master_key)?;
            let new_key_pragma: Zeroizing<String> = Zeroizing::new(format!("x'{}'", &*new_key_hex));
            conn.pragma_update(None, "rekey", new_key_pragma.as_str())?;

            // Re-wrap (and, for legacy scopes, first-time persist) every
            // live scope DEK under the new master-derived wrapping key, and
            // defensively purge DEK rows belonging to cryptographically
            // forgotten scopes. Both run inside a single transaction so the
            // `scope_deks` table moves to the new key atomically (one commit
            // /fsync regardless of scope count) and can never be left half
            // re-wrapped if the process dies mid-loop.
            //
            // The DELETE guards a *pre-existing* inconsistent state, not one
            // this tool creates: `forget` deletes a scope's DEK row before
            // writing its tombstone, but a crash in between could leave an
            // orphaned row still wrapped under the OLD key. Step 1 excludes
            // forgotten scopes from `scope_keys`, so without this purge that
            // stale row would survive the VACUUM untouched and step 4's
            // re-open would fail trying to unwrap it under the new key.
            // Purging it both upholds the forgetting guarantee (no DEK for a
            // forgotten scope survives rotation) and lets rotation succeed
            // cleanly instead of failing closed on a recoverable edge case.
            let new_wrap = derive_key(new_master_key, b"scope-dek-wrap:v1")?;
            let now = Utc::now().timestamp();
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM scope_deks WHERE scope_id IN \
                 (SELECT scope_id FROM forgotten_scopes)",
                [],
            )?;
            for (scope, dek) in &scope_keys {
                let nonce = random_nonce();
                let aad = scope_dek_aad(*scope);
                let wrapped = encrypt_aead(&new_wrap, &nonce, dek.as_slice(), &aad)?;
                tx.execute(
                    "INSERT OR REPLACE INTO scope_deks \
                     (scope_id, wrapped_dek, nonce, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        scope.as_uuid().as_bytes().as_slice(),
                        wrapped,
                        nonce.as_slice(),
                        now
                    ],
                )?;
            }
            tx.commit()?;
        }

        // 4. Re-open under the new master and verify integrity.
        let rotated = EvidenceStore::open(dest_path, new_master_key, self.config.clone())?;

        let loaded = rotated.load_scope_deks()?;
        for (scope, dek) in &scope_keys {
            match loaded.get(scope) {
                Some(k) if k == dek => {}
                _ => {
                    return Err(EvidenceError::KeyRotation(format!(
                        "scope DEK for {} did not round-trip under the new master key",
                        scope.as_uuid()
                    )));
                }
            }
        }

        let src_rows = self.all_evidence_rows()?;
        let rotated_count = rotated.evidence_count()?;
        if rotated_count != src_rows.len() {
            return Err(EvidenceError::KeyRotation(format!(
                "evidence row count mismatch after rotation: source={}, rotated={}",
                src_rows.len(),
                rotated_count
            )));
        }

        let mut bodies_verified = 0usize;
        for (id, scope) in &src_rows {
            // Skip rows whose scope was forgotten: their ciphertext is
            // copied verbatim but is no longer decryptable on either side.
            if !scope_keys.contains_key(scope) {
                continue;
            }
            let src_body = self.read_body(*id)?;
            let rotated_body = rotated.read_body(*id)?;
            if src_body != rotated_body {
                return Err(EvidenceError::KeyRotation(format!(
                    "body plaintext mismatch after rotation for evidence row {}",
                    id.as_uuid()
                )));
            }
            bodies_verified += 1;
        }

        Ok(MasterKeyRotationReport {
            scopes_rewrapped: scope_keys.len(),
            evidence_rows: src_rows.len(),
            bodies_verified,
        })
    }

    /// Collect the distinct scope ids that own scope-key-encrypted data
    /// across every table whose rows are sealed under a per-scope key.
    /// Used by [`Self::rotate_master_key`] to enumerate the keys that
    /// must survive a master-key rotation. `forgotten_scopes` /
    /// `epoch_tombstones` are intentionally excluded — they record
    /// destroyed keys, not live encrypted payloads.
    fn all_data_scopes(&self) -> Result<Vec<ScopeId>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope_id FROM evidence \
             UNION SELECT scope_id FROM ring_buffer \
             UNION SELECT scope_id FROM body_store_key_wraps \
             UNION SELECT scope_id FROM memory_objects \
             UNION SELECT scope_id FROM connector_instances \
             UNION SELECT scope_id FROM connector_tokens \
             UNION SELECT scope_id FROM approved_document_payloads \
             UNION SELECT scope_id FROM synthesis_object_versions \
             UNION SELECT scope_id FROM scope_deks",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(ScopeId::from_uuid(slice_to_uuid(&row?)?));
        }
        Ok(out)
    }

    /// Return `(evidence_id, scope_id)` for every row in the append-only
    /// evidence table. Used by [`Self::rotate_master_key`] to verify the
    /// rotated copy round-trips every body.
    fn all_evidence_rows(&self) -> Result<Vec<(EvidenceId, ScopeId)>> {
        let mut stmt = self.conn.prepare("SELECT id, scope_id FROM evidence")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id_bytes, scope_bytes) = row?;
            out.push((
                EvidenceId(slice_to_uuid(&id_bytes)?),
                ScopeId::from_uuid(slice_to_uuid(&scope_bytes)?),
            ));
        }
        Ok(out)
    }

    // ─────────────── Memory-object persistence (C10) ───────────────

    /// Run `f` inside a SQLCipher transaction, committing on `Ok` and
    /// rolling back on `Err`. Uses `unchecked_transaction` so the
    /// caller only needs `&self` (the runtime mutex already serialises
    /// access; `Connection` is not `Sync` so the borrow checker can't
    /// see this externally).
    ///
    /// Intended for grouping multiple `*_in_tx` writes that must
    /// either all land on disk or none — see
    /// `apply_dispatch_outcome` in the FFI crate for the canonical
    /// caller. Returning `Err` from `f` aborts the transaction;
    /// returning `Ok` commits before the result is propagated.
    pub fn with_transaction<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<R>,
    {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(reason) = self.take_injected_with_transaction_failure() {
            // Surface the synthetic failure through the same variant
            // path that real disk-write failures use so callers
            // (`apply_dispatch_outcome`, `replace_approved_document`)
            // exercise their `EvidenceError::Sqlite` handling rather
            // than a synthetic-only code path. `SQLITE_FULL` mirrors
            // the most plausible cause of a real commit failure
            // (disk full); the injected `reason` is preserved in the
            // `Option<String>` slot so it surfaces in the rendered
            // error message.
            return Err(EvidenceError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                Some(reason),
            )));
        }
        let tx = self.conn.unchecked_transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Arm a one-shot failure on the next [`Self::with_transaction`]
    /// call. The next invocation pops the slot, returns
    /// [`EvidenceError::Schema(reason)`] without opening a real
    /// transaction, and subsequent calls behave normally until the
    /// slot is armed again.
    ///
    /// Only available with the `test-support` feature (or in unit
    /// tests of this crate). Used by the `ffi` crate's
    /// `apply_dispatch_outcome_tx_failure_marks_window_failed`
    /// regression test to verify the commit-failure recovery path.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_with_transaction_failure_for_tests(&self, reason: impl Into<String>) {
        *self
            .injected_with_transaction_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason.into());
    }

    /// Atomically take the armed failure (if any). Internal helper
    /// for the `with_transaction` hot path — holds the mutex only
    /// long enough to swap `None` into the slot so the caller never
    /// double-fires.
    #[cfg(any(test, feature = "test-support"))]
    fn take_injected_with_transaction_failure(&self) -> Option<String> {
        self.injected_with_transaction_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Persist a serializable memory object (user or channel) for
    /// `scope_id`. The `kind` tag discriminates between different
    /// memory types ("user_memory" / "channel_memory"). The object
    /// is JSON-serialized and AEAD-encrypted under the scope key.
    ///
    /// Upserts: calling this with the same `(scope_id, kind)` pair
    /// overwrites the previous blob.
    ///
    /// Wraps the single-statement insert in an implicit autocommit;
    /// callers that need to bundle this write with others under one
    /// transaction must use [`Self::with_transaction`] +
    /// [`Self::save_memory_blob_in_tx`].
    pub fn save_memory_blob(
        &self,
        scope_id: ScopeId,
        kind: &str,
        plaintext_json: &[u8],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.save_memory_blob_in_tx(&tx, scope_id, kind, plaintext_json)?;
        tx.commit()?;
        Ok(())
    }

    /// Transactional variant of [`Self::save_memory_blob`] that runs
    /// inside an existing `Transaction<'_>` so multiple writes can be
    /// grouped atomically. Identical AEAD framing (scope-bound AAD,
    /// random nonce, `INSERT OR REPLACE`) to the autocommit path.
    pub fn save_memory_blob_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        scope_id: ScopeId,
        kind: &str,
        plaintext_json: &[u8],
    ) -> Result<()> {
        let key = self.scope_key(scope_id)?;
        let nonce = random_nonce();
        // b"memory:" (7) + kind + b':' (1) + UUID (16) = 24 + kind.len()
        let mut aad = Vec::with_capacity(24 + kind.len());
        aad.extend_from_slice(b"memory:");
        aad.extend_from_slice(kind.as_bytes());
        aad.push(b':');
        aad.extend_from_slice(scope_id.as_uuid().as_bytes());
        let ciphertext = encrypt_aead(&key, &nonce, plaintext_json, &aad)?;
        let now = chrono::Utc::now().timestamp();
        tx.execute(
            "INSERT OR REPLACE INTO memory_objects \
             (scope_id, kind, nonce, payload, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                kind,
                nonce.as_slice(),
                ciphertext,
                now
            ],
        )?;
        Ok(())
    }

    /// Load a previously-persisted memory blob for `(scope_id, kind)`.
    /// Returns `None` if no row exists.
    pub fn load_memory_blob(&self, scope_id: ScopeId, kind: &str) -> Result<Option<Vec<u8>>> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT nonce, payload FROM memory_objects \
                 WHERE scope_id = ?1 AND kind = ?2",
                params![scope_id.as_uuid().as_bytes().as_slice(), kind],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((nonce_bytes, ciphertext)) = row else {
            return Ok(None);
        };
        if nonce_bytes.len() != AEAD_NONCE_LEN {
            return Err(EvidenceError::Schema(
                "memory_objects nonce has wrong length",
            ));
        }
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        nonce.copy_from_slice(&nonce_bytes);
        let key = self.scope_key(scope_id)?;
        // b"memory:" (7) + kind + b':' (1) + UUID (16) = 24 + kind.len()
        let mut aad = Vec::with_capacity(24 + kind.len());
        aad.extend_from_slice(b"memory:");
        aad.extend_from_slice(kind.as_bytes());
        aad.push(b':');
        aad.extend_from_slice(scope_id.as_uuid().as_bytes());
        let plaintext = decrypt_aead(&key, &nonce, &ciphertext, &aad)?;
        Ok(Some(plaintext))
    }

    /// Delete all memory blobs for `scope_id` (all kinds).
    pub fn delete_memory_blobs_for_scope(&self, scope_id: ScopeId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM memory_objects WHERE scope_id = ?1",
            params![scope_id.as_uuid().as_bytes().as_slice()],
        )?;
        Ok(())
    }

    /// List all scope IDs that have persisted memory blobs of the
    /// given `kind`. Used at startup to rehydrate the in-memory maps.
    pub fn list_memory_scopes(&self, kind: &str) -> Result<Vec<ScopeId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT scope_id FROM memory_objects WHERE kind = ?1")?;
        let rows = stmt.query_map(params![kind], |row| row.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = row?;
            out.push(ScopeId::from_uuid(slice_to_uuid(&bytes)?));
        }
        Ok(out)
    }

    // ─────────────────── Connector persistence (v9) ───────────────────
    //
    // The connector lifecycle (`create_connector`, `authenticate_connector`,
    // `sync_connector`, `remove_connector` over in the FFI surface) needs
    // three pieces of state to survive process restarts:
    //
    //   1. `ConnectorInstance` = `(config, sync_state)` per instance.
    //   2. `OAuth2Token` bundle per authenticated instance.
    //   3. The single-instance-per-`(scope_id, kind)` contract — pinned
    //      by the unique index in `SCHEMA_SQL` so a runtime-side bug
    //      cannot insert a duplicate even on a different handle.
    //
    // Both encrypted blobs are AEAD-sealed under the per-scope DEK so
    // `forget(scope)` makes them cryptographically unrecoverable even
    // if the row-level `DELETE` races against the DEK destruction. AAD
    // binds `scope_id` (both tables) plus `instance_id` (tokens) and
    // the kind tag (instance row) so a relocated ciphertext fails to
    // decrypt instead of silently aliasing onto a different identity.
    //
    // The methods here intentionally take/return raw JSON byte
    // payloads so the schema can live in `evidence_store` without
    // pulling in `connector_framework` as a build dependency — the
    // FFI runtime is the one place that knows the JSON shape and
    // owns the `serde_json` round-trip.

    /// Upsert a connector instance row (encrypted `(config, sync_state)`
    /// JSON blob) for `instance_id`. The `kind_tag` is the stable
    /// snake_case `ConnectorKind` tag (`"google_drive"`, `"slack"`, …)
    /// and is stored unencrypted on the row so the unique index on
    /// `(scope_id, kind)` can enforce the dedup contract without
    /// decrypting every row first; the same tag is also folded into the
    /// AAD so a ciphertext relocated to a row with a different `kind`
    /// fails to decrypt.
    ///
    /// Upserts: calling this with the same `instance_id` overwrites the
    /// previous blob (used by `sync_connector` to advance the
    /// stored `SyncState` cursor).
    pub fn save_connector_instance(
        &self,
        instance_id: uuid::Uuid,
        scope_id: ScopeId,
        kind_tag: &str,
        plaintext_json: &[u8],
    ) -> Result<()> {
        let key = self.scope_key(scope_id)?;
        let nonce = random_nonce();
        let aad = connector_instance_aad(scope_id, instance_id, kind_tag);
        let ciphertext = encrypt_aead(&key, &nonce, plaintext_json, &aad)?;
        let now = chrono::Utc::now().timestamp();
        // Upsert keyed on `instance_id` only. We do NOT use
        // `INSERT OR REPLACE` because the table has a *secondary*
        // unique index on `(scope_id, kind)` — `OR REPLACE` would
        // silently delete the conflicting row on EITHER unique
        // constraint, which means a collision against the dedup
        // index would silently wipe the existing instance row
        // instead of surfacing as an error. The runtime-side check
        // in `create_connector` already rejects duplicates, but
        // this on-conflict spelling is the defense-in-depth: a
        // future regression of that check (or a stray writer on a
        // different handle) will hit `UNIQUE constraint failed:
        // connector_instances.scope_id, connector_instances.kind`
        // and bubble up as a structured `rusqlite::Error` instead
        // of silently destroying the existing row.
        self.conn.execute(
            "INSERT INTO connector_instances \
             (instance_id, scope_id, kind, nonce, payload, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(instance_id) DO UPDATE SET \
               scope_id = excluded.scope_id, \
               kind = excluded.kind, \
               nonce = excluded.nonce, \
               payload = excluded.payload, \
               updated_at = excluded.updated_at",
            params![
                instance_id.as_bytes().as_slice(),
                scope_id.as_uuid().as_bytes().as_slice(),
                kind_tag,
                nonce.as_slice(),
                ciphertext,
                now,
            ],
        )?;
        Ok(())
    }

    /// Delete the persisted instance row for `instance_id`. No-op if
    /// the row does not exist (idempotent — matches the
    /// `remove_connector` contract on the FFI surface).
    pub fn delete_connector_instance(&self, instance_id: uuid::Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM connector_instances WHERE instance_id = ?1",
            params![instance_id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    /// Delete every persisted connector instance row bound to
    /// `scope_id`. Used by `forget_scope_state` to tear down a
    /// forgotten scope's connector state from disk. Returns the count
    /// of rows deleted so the caller can log it for diagnostics.
    pub fn delete_connector_instances_for_scope(&self, scope_id: ScopeId) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM connector_instances WHERE scope_id = ?1",
            params![scope_id.as_uuid().as_bytes().as_slice()],
        )?;
        Ok(n)
    }

    /// Load every persisted connector instance row, decrypting under
    /// each row's per-scope DEK. Rows that fail to decrypt (corrupt
    /// payload, tampered ciphertext, missing scope key) are skipped
    /// with a `tracing::warn!`, so a single bad row never blocks
    /// `open_store`. Returns `(instance_id, scope_id, kind_tag,
    /// plaintext_json)` tuples in unspecified order.
    pub fn load_connector_instances(&self) -> Result<Vec<(uuid::Uuid, ScopeId, String, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT instance_id, scope_id, kind, nonce, payload \
             FROM connector_instances",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (instance_bytes, scope_bytes, kind_tag, nonce_bytes, ciphertext) = row?;
            let instance_id = match slice_to_uuid(&instance_bytes) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(instance_bytes_len = instance_bytes.len(),
                        error = %e,
                        "connector_instances row has malformed instance_id; skipping",
                    );
                    continue;
                }
            };
            let scope_id = match slice_to_uuid(&scope_bytes) {
                Ok(id) => ScopeId::from_uuid(id),
                Err(e) => {
                    tracing::warn!(instance = %instance_id,
                        scope_bytes_len = scope_bytes.len(),
                        error = %e,
                        "connector_instances row has malformed scope_id; skipping",
                    );
                    continue;
                }
            };
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                tracing::warn!(instance = %instance_id,
                    "connector_instances row has malformed nonce; skipping",
                );
                continue;
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&nonce_bytes);
            let key = match self.scope_key(scope_id) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(instance = %instance_id,
                        scope = %scope_id.as_uuid(),
                        error = %e,
                        "connector_instances scope key unavailable; skipping row",
                    );
                    continue;
                }
            };
            let aad = connector_instance_aad(scope_id, instance_id, &kind_tag);
            match decrypt_aead(&key, &nonce, &ciphertext, &aad) {
                Ok(plaintext) => out.push((instance_id, scope_id, kind_tag, plaintext)),
                Err(e) => {
                    tracing::warn!(instance = %instance_id,
                        scope = %scope_id.as_uuid(),
                        error = %e,
                        "connector_instances row failed to decrypt; skipping",
                    );
                }
            }
        }
        Ok(out)
    }

    /// Upsert a connector OAuth2 token blob for `instance_id`. The
    /// caller supplies an already-JSON-encoded `OAuth2Token`; the AAD
    /// binds both `scope_id` and `instance_id` so a ciphertext copied
    /// to a different row fails to decrypt.
    pub fn save_connector_token(
        &self,
        instance_id: uuid::Uuid,
        scope_id: ScopeId,
        plaintext_json: &[u8],
    ) -> Result<()> {
        let key = self.scope_key(scope_id)?;
        let nonce = random_nonce();
        let aad = connector_token_aad(scope_id, instance_id);
        let ciphertext = encrypt_aead(&key, &nonce, plaintext_json, &aad)?;
        let now = chrono::Utc::now().timestamp();
        // Upsert keyed on `instance_id` only. `connector_tokens` has
        // no secondary unique indexes today, so `INSERT OR REPLACE`
        // would be functionally equivalent — but using
        // `ON CONFLICT(instance_id) DO UPDATE` matches the spelling
        // used by `save_connector_instance` (where the secondary
        // `(scope_id, kind)` UNIQUE makes the distinction
        // safety-critical) and pre-empts a future schema migration
        // that adds a secondary unique constraint here from silently
        // becoming a defense-in-depth regression.
        self.conn.execute(
            "INSERT INTO connector_tokens \
             (instance_id, scope_id, nonce, payload, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(instance_id) DO UPDATE SET \
               scope_id = excluded.scope_id, \
               nonce = excluded.nonce, \
               payload = excluded.payload, \
               updated_at = excluded.updated_at",
            params![
                instance_id.as_bytes().as_slice(),
                scope_id.as_uuid().as_bytes().as_slice(),
                nonce.as_slice(),
                ciphertext,
                now,
            ],
        )?;
        Ok(())
    }

    /// Delete the persisted token row for `instance_id`. No-op if the
    /// row does not exist.
    pub fn delete_connector_token(&self, instance_id: uuid::Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM connector_tokens WHERE instance_id = ?1",
            params![instance_id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    /// Delete every persisted token row bound to `scope_id`. Used by
    /// `forget_scope_state` to drop tokens for a forgotten scope from
    /// disk. Returns the count of rows deleted so the caller can log
    /// it for diagnostics.
    pub fn delete_connector_tokens_for_scope(&self, scope_id: ScopeId) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM connector_tokens WHERE scope_id = ?1",
            params![scope_id.as_uuid().as_bytes().as_slice()],
        )?;
        Ok(n)
    }

    /// Load every persisted token row, decrypting under the
    /// corresponding scope DEK. Rows that fail to decrypt are skipped
    /// with a `tracing::warn!`. Returns `(instance_id, scope_id,
    /// plaintext_json)` tuples.
    pub fn load_connector_tokens(&self) -> Result<Vec<(uuid::Uuid, ScopeId, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT instance_id, scope_id, nonce, payload \
             FROM connector_tokens",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (instance_bytes, scope_bytes, nonce_bytes, ciphertext) = row?;
            let instance_id = match slice_to_uuid(&instance_bytes) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(instance_bytes_len = instance_bytes.len(),
                        error = %e,
                        "connector_tokens row has malformed instance_id; skipping",
                    );
                    continue;
                }
            };
            let scope_id = match slice_to_uuid(&scope_bytes) {
                Ok(id) => ScopeId::from_uuid(id),
                Err(e) => {
                    tracing::warn!(instance = %instance_id,
                        scope_bytes_len = scope_bytes.len(),
                        error = %e,
                        "connector_tokens row has malformed scope_id; skipping",
                    );
                    continue;
                }
            };
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                tracing::warn!(instance = %instance_id,
                    "connector_tokens row has malformed nonce; skipping",
                );
                continue;
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&nonce_bytes);
            let key = match self.scope_key(scope_id) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(instance = %instance_id,
                        scope = %scope_id.as_uuid(),
                        error = %e,
                        "connector_tokens scope key unavailable; skipping row",
                    );
                    continue;
                }
            };
            let aad = connector_token_aad(scope_id, instance_id);
            match decrypt_aead(&key, &nonce, &ciphertext, &aad) {
                Ok(plaintext) => out.push((instance_id, scope_id, plaintext)),
                Err(e) => {
                    tracing::warn!(instance = %instance_id,
                        scope = %scope_id.as_uuid(),
                        error = %e,
                        "connector_tokens row failed to decrypt; skipping",
                    );
                }
            }
        }
        Ok(out)
    }

    // ───────────── Approved-document payloads (v10 / ;
    //               v12 / : body-store dedup) ──────────
    //
    // Tenant memory carries the *reference* (id / label / approver /
    // approved_at) for every admitted approved document, but the
    // payload bytes themselves are too large to keep inline in the
    // tenant_memory JSON blob (every mutation would force a full
    // read / encrypt / write of every doc payload).
    //
    // **v12 layout (current).** A row in `approved_document_payloads`
    // is metadata-only: `(scope_id, document_id) -> (content_hash,
    // size_bytes, updated_at)`. The plaintext bytes live in the
    // content-hash-deduplicated `body_store` table (encrypted under
    // a random per-row CEK), and each scope that references the
    // content owns a CEK wrap in `body_store_key_wraps` (encrypted
    // under that scope's DEK). Admitting the same content into N
    // tenant scopes therefore costs one body row + N wraps instead
    // of N inline ciphertexts. The body row's `ref_count` is
    // informational; the orphan-body GC trigger is "no wraps exist
    // for this content_hash", implemented in
    // [`Self::purge_body_key_wraps_for_scope`].
    //
    // `forget(scope)` calls
    // [`Self::purge_body_key_wraps_for_scope`] (drops every wrap
    // owned by the scope and GCs orphan body rows) followed by
    // [`Self::delete_approved_document_payloads_for_scope`] (drops
    // the metadata rows). Even if either delete fails, the scope-DEK
    // destruction step makes any retained ciphertext
    // cryptographically unrecoverable — the row purges are
    // defense-in-depth rather than the primary security barrier.

    /// Upsert an opaque approved-document payload for
    /// `(scope_id, document_id)`.
    ///
    /// The plaintext is stored in the content-hash-deduplicated
    /// `body_store` table — admitting the same content into N tenant
    /// scopes costs one body row + N per-scope CEK wraps in
    /// `body_store_key_wraps` instead of N inline ciphertexts. The
    /// `approved_document_payloads` row itself is metadata-only
    /// (content_hash + size_bytes + updated_at); it joins to the body
    /// via `content_hash`. See
    /// [`Self::admit_approved_doc_body_in_tx`] for the body-store
    /// admission logic.
    ///
    /// Re-calling with the same `(scope_id, document_id)` overwrites
    /// the previous metadata row (e.g. a host that re-uploads a
    /// corrected PDF for an existing ref) and, if the content_hash
    /// changes, admits the new body separately — the old body's
    /// wrap is retained until [`Self::purge_body_key_wraps_for_scope`]
    /// runs (at `forget_scope` time), at which point the body row
    /// is GCed if no other scope still wraps it. The caller is
    /// responsible for enforcing any size cap before invoking this
    /// method — the store does not impose one.
    pub fn save_approved_document_payload(
        &self,
        scope_id: ScopeId,
        document_id: uuid::Uuid,
        plaintext: &[u8],
        content_hash: &ContentHash,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.save_approved_document_payload_in_tx(
            &tx,
            scope_id,
            document_id,
            plaintext,
            content_hash,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Transaction-bound variant of
    /// [`Self::save_approved_document_payload`] so callers that need
    /// to bundle this write with the tenant memory blob (e.g. the
    /// FFI `replace_approved_document` entry point) can group both
    /// under one SQLCipher transaction via [`Self::with_transaction`].
    ///
    /// Admits the body to the deduplicated `body_store` table
    /// (creating a per-scope CEK wrap if this is the first time
    /// `scope_id` references the content), then upserts the
    /// metadata row. Both writes happen under the caller's `tx`,
    /// so a host-level crash between the two — or any sub-call
    /// failure — rolls everything back atomically.
    pub fn save_approved_document_payload_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        scope_id: ScopeId,
        document_id: uuid::Uuid,
        plaintext: &[u8],
        content_hash: &ContentHash,
    ) -> Result<()> {
        self.admit_approved_doc_body_in_tx(tx, scope_id, plaintext, content_hash)?;
        let now = chrono::Utc::now().timestamp();
        let size_bytes = i64::try_from(plaintext.len()).unwrap_or(i64::MAX);
        tx.execute(
            "INSERT INTO approved_document_payloads \
             (scope_id, document_id, content_hash, size_bytes, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(scope_id, document_id) DO UPDATE SET \
               content_hash = excluded.content_hash, \
               size_bytes = excluded.size_bytes, \
               updated_at = excluded.updated_at",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                document_id.as_bytes().as_slice(),
                content_hash.as_slice(),
                size_bytes,
                now,
            ],
        )?;
        Ok(())
    }

    /// Admit `plaintext` (with pre-computed BLAKE3 `content_hash`)
    /// into the deduplicated `body_store` table on behalf of
    /// `scope_id`, creating a per-scope CEK wrap in
    /// `body_store_key_wraps` if one does not already exist.
    ///
    /// Three cases:
    ///   1. **New body** — no existing `body_store` row. Generate a
    ///      random CEK, AEAD-encrypt the body under it (AAD binds
    ///      the content_hash via [`body_table_aad`]), insert the
    ///      row with `ref_count = 1`, and wrap the CEK under the
    ///      ingesting scope's DEK.
    ///   2. **Dedup hit, scope already wrapped** — the same scope
    ///      previously admitted (or ingested) this content. The
    ///      ciphertext is already decryptable; do nothing. `ref_count`
    ///      is intentionally NOT bumped because the existing wrap
    ///      counts the scope's reference.
    ///   3. **Dedup hit, scope not yet wrapped** — another scope
    ///      already admitted this content. Read any existing wrap
    ///      from `body_store_key_wraps` to find the donor scope,
    ///      derive the donor's DEK, unwrap the CEK, re-wrap under
    ///      this scope's DEK, and insert the new wrap. `ref_count`
    ///      is incremented so the body row's counter reflects the
    ///      total per-scope references. (`ref_count` is
    ///      informational; the orphan-body GC trigger is
    ///      "no wraps exist for this content_hash", as implemented
    ///      in [`Self::purge_body_key_wraps_for_scope`].)
    ///
    /// A pathological fourth case — body row present but ALL wraps
    /// have been purged (forgetting the only admitting scope races
    /// the body GC) — falls through to the new-body path after
    /// deleting the stale body row. This matches the same
    /// defense-in-depth handling in [`Self::ingest_body_table`].
    fn admit_approved_doc_body_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        scope_id: ScopeId,
        plaintext: &[u8],
        hash: &ContentHash,
    ) -> Result<()> {
        let scope_key = self.scope_key(scope_id)?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT ref_count FROM body_store WHERE content_hash = ?1",
                params![hash.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;

        if existing.is_some() {
            let already_has_wrap: bool = tx
                .query_row(
                    "SELECT 1 FROM body_store_key_wraps \
                     WHERE content_hash = ?1 AND scope_id = ?2",
                    params![hash.as_slice(), scope_id.as_uuid().as_bytes().as_slice(),],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if already_has_wrap {
                // Case 2: same-scope re-admit, ciphertext already
                // decryptable under existing wrap. No change.
                return Ok(());
            }

            // Case 3: cross-scope dedup. Locate any donor wrap.
            let donor: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = tx
                .query_row(
                    "SELECT w.wrapped_cek, w.nonce, w.scope_id \
                     FROM body_store_key_wraps w \
                     WHERE w.content_hash = ?1 \
                       AND w.scope_id != ?2 \
                     LIMIT 1",
                    params![hash.as_slice(), scope_id.as_uuid().as_bytes().as_slice(),],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                        ))
                    },
                )
                .optional()?;

            if let Some((wrapped_cek, donor_wrap_nonce_bytes, donor_scope_bytes)) = donor {
                if donor_wrap_nonce_bytes.len() != AEAD_NONCE_LEN {
                    return Err(EvidenceError::Schema(
                        "body_store_key_wraps row has malformed nonce",
                    ));
                }
                let mut donor_wrap_nonce = [0u8; AEAD_NONCE_LEN];
                donor_wrap_nonce.copy_from_slice(&donor_wrap_nonce_bytes);
                let donor_scope = ScopeId::from_uuid(slice_to_uuid(&donor_scope_bytes)?);
                let donor_key = self.scope_key(donor_scope)?;
                let cek = unwrap_cek(&donor_key, &wrapped_cek, &donor_wrap_nonce, hash)?;
                let new_wrap_nonce = random_nonce();
                let new_wrapped = wrap_cek(&scope_key, &cek, &new_wrap_nonce, hash)?;
                tx.execute(
                    "INSERT INTO body_store_key_wraps \
                     (content_hash, scope_id, wrapped_cek, nonce) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        hash.as_slice(),
                        scope_id.as_uuid().as_bytes().as_slice(),
                        new_wrapped,
                        new_wrap_nonce.as_slice(),
                    ],
                )?;
                tx.execute(
                    "UPDATE body_store SET ref_count = ref_count + 1 \
                     WHERE content_hash = ?1",
                    params![hash.as_slice()],
                )?;
            } else {
                // Pathological: body row exists but every wrap has
                // been purged. Treat as orphan — delete the stale
                // ciphertext and admit the new plaintext from
                // scratch under a fresh CEK.
                tx.execute(
                    "DELETE FROM body_store WHERE content_hash = ?1",
                    params![hash.as_slice()],
                )?;
                Self::insert_new_approved_doc_body_in_tx(
                    tx, scope_id, &scope_key, plaintext, hash,
                )?;
            }
        } else {
            // Case 1: new body.
            Self::insert_new_approved_doc_body_in_tx(tx, scope_id, &scope_key, plaintext, hash)?;
        }

        Ok(())
    }

    /// Insert a fresh `body_store` row + a wrap for `scope_id` under
    /// the supplied `scope_key`. Used by
    /// [`Self::admit_approved_doc_body_in_tx`] for the new-body and
    /// orphan-recovery paths.
    ///
    /// Free-function rather than `&self`-method because this is pure
    /// SQL + crypto over the borrowed `tx` — taking `&self` would
    /// confuse the borrow checker for no benefit (clippy flags it as
    /// `unused_self`).
    fn insert_new_approved_doc_body_in_tx(
        tx: &rusqlite::Transaction<'_>,
        scope_id: ScopeId,
        scope_key: &AeadKey,
        plaintext: &[u8],
        hash: &ContentHash,
    ) -> Result<()> {
        let cek = random_cek();
        let body_nonce = random_nonce();
        let aad = body_table_aad(hash);
        let ciphertext = encrypt_aead(&cek, &body_nonce, plaintext, &aad)?;
        tx.execute(
            "INSERT INTO body_store (content_hash, body, nonce, ref_count) \
             VALUES (?1, ?2, ?3, 1)",
            params![hash.as_slice(), ciphertext, body_nonce.as_slice()],
        )?;
        let wrap_nonce = random_nonce();
        let wrapped = wrap_cek(scope_key, &cek, &wrap_nonce, hash)?;
        tx.execute(
            "INSERT INTO body_store_key_wraps \
             (content_hash, scope_id, wrapped_cek, nonce) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                hash.as_slice(),
                scope_id.as_uuid().as_bytes().as_slice(),
                wrapped,
                wrap_nonce.as_slice(),
            ],
        )?;
        Ok(())
    }

    /// Load the plaintext payload bytes for `(scope_id, document_id)`.
    /// Returns `None` if no metadata row exists.
    ///
    /// As of v12 the read path joins
    /// `approved_document_payloads` (metadata: content_hash, size,
    /// updated_at) against the deduplicated `body_store` table via
    /// the per-scope CEK wrap in `body_store_key_wraps`. Returns
    /// [`EvidenceError::Schema`] on a malformed nonce / hash row or
    /// when the metadata row references a content_hash that the
    /// scope no longer wraps (defensive — a healthy DB should never
    /// produce this, but a forget-races-rehydrate edge case would
    /// surface here rather than silently returning corrupt bytes).
    pub fn load_approved_document_payload(
        &self,
        scope_id: ScopeId,
        document_id: uuid::Uuid,
    ) -> Result<Option<Vec<u8>>> {
        let row: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT content_hash FROM approved_document_payloads \
                 WHERE scope_id = ?1 AND document_id = ?2",
                params![
                    scope_id.as_uuid().as_bytes().as_slice(),
                    document_id.as_bytes().as_slice(),
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let Some(content_hash_bytes) = row else {
            return Ok(None);
        };
        if content_hash_bytes.len() != crypto::CONTENT_HASH_LEN {
            return Err(EvidenceError::Schema(
                "approved_document_payloads content_hash has wrong length",
            ));
        }
        let mut content_hash = [0u8; crypto::CONTENT_HASH_LEN];
        content_hash.copy_from_slice(&content_hash_bytes);
        let plaintext = self
            .load_approved_doc_body(scope_id, &content_hash)?
            .ok_or(EvidenceError::Schema(
                "approved_document_payloads row references a body that is no longer wrapped \
                 for this scope (body_store row gone, or wrap purged before metadata)",
            ))?;
        Ok(Some(plaintext))
    }

    /// Decrypt the `body_store` row for `content_hash` under
    /// `scope_id`'s CEK wrap. Returns `None` when the wrap or the
    /// body row is absent — caller decides whether absence is an
    /// error (e.g. read path) or normal (e.g. cleanup path).
    fn load_approved_doc_body(
        &self,
        scope_id: ScopeId,
        content_hash: &ContentHash,
    ) -> Result<Option<Vec<u8>>> {
        let wrap_row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT wrapped_cek, nonce FROM body_store_key_wraps \
                 WHERE content_hash = ?1 AND scope_id = ?2",
                params![
                    content_hash.as_slice(),
                    scope_id.as_uuid().as_bytes().as_slice(),
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((wrapped_cek, wrap_nonce_bytes)) = wrap_row else {
            return Ok(None);
        };
        if wrap_nonce_bytes.len() != AEAD_NONCE_LEN {
            return Err(EvidenceError::Schema(
                "body_store_key_wraps row has malformed nonce",
            ));
        }
        let mut wrap_nonce = [0u8; AEAD_NONCE_LEN];
        wrap_nonce.copy_from_slice(&wrap_nonce_bytes);
        let scope_key = self.scope_key(scope_id)?;
        let cek = unwrap_cek(&scope_key, &wrapped_cek, &wrap_nonce, content_hash)?;

        let body_row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT body, nonce FROM body_store WHERE content_hash = ?1",
                params![content_hash.as_slice()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((ciphertext, body_nonce_bytes)) = body_row else {
            return Ok(None);
        };
        if body_nonce_bytes.len() != AEAD_NONCE_LEN {
            return Err(EvidenceError::Schema("body_store row has malformed nonce"));
        }
        let mut body_nonce = [0u8; AEAD_NONCE_LEN];
        body_nonce.copy_from_slice(&body_nonce_bytes);
        let aad = body_table_aad(content_hash);
        let plaintext = decrypt_aead(&cek, &body_nonce, &ciphertext, &aad)?;
        Ok(Some(plaintext))
    }

    /// List every persisted approved-document payload row for
    /// `scope_id`, returning plaintext metadata only (no
    /// ciphertext decryption). Order is unspecified — the caller
    /// joins against the tenant-memory ref list (which IS ordered)
    /// to produce a stable host-facing view.
    pub fn list_approved_document_payload_meta_for_scope(
        &self,
        scope_id: ScopeId,
    ) -> Result<Vec<ApprovedDocumentPayloadMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, content_hash, size_bytes, updated_at \
             FROM approved_document_payloads WHERE scope_id = ?1",
        )?;
        let rows = stmt.query_map(params![scope_id.as_uuid().as_bytes().as_slice()], |row| {
            let document_id_bytes: Vec<u8> = row.get(0)?;
            let content_hash_bytes: Vec<u8> = row.get(1)?;
            let size_bytes: i64 = row.get(2)?;
            let updated_at: i64 = row.get(3)?;
            Ok((
                document_id_bytes,
                content_hash_bytes,
                size_bytes,
                updated_at,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (document_id_bytes, content_hash_bytes, size_bytes, updated_at) = row?;
            let document_id = slice_to_uuid(&document_id_bytes)?;
            if content_hash_bytes.len() != crypto::CONTENT_HASH_LEN {
                return Err(EvidenceError::Schema(
                    "approved_document_payloads content_hash has wrong length",
                ));
            }
            let mut content_hash = [0u8; crypto::CONTENT_HASH_LEN];
            content_hash.copy_from_slice(&content_hash_bytes);
            out.push(ApprovedDocumentPayloadMeta {
                document_id,
                content_hash,
                size_bytes: u64::try_from(size_bytes).unwrap_or(0),
                updated_at,
            });
        }
        Ok(out)
    }

    /// Delete the payload row for `(scope_id, document_id)`. No-op
    /// if the row does not exist. Returns the count of rows deleted
    /// so the caller can log it for diagnostics.
    pub fn delete_approved_document_payload(
        &self,
        scope_id: ScopeId,
        document_id: uuid::Uuid,
    ) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM approved_document_payloads \
             WHERE scope_id = ?1 AND document_id = ?2",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                document_id.as_bytes().as_slice(),
            ],
        )?;
        Ok(n)
    }

    /// Delete every payload row bound to `scope_id`. Called from
    /// the FFI layer's `forget_scope_state` after the scope DEK has
    /// already been destroyed, as a best-effort byte purge.
    /// Returns the count of rows deleted so the caller can log it.
    pub fn delete_approved_document_payloads_for_scope(&self, scope_id: ScopeId) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM approved_document_payloads WHERE scope_id = ?1",
            params![scope_id.as_uuid().as_bytes().as_slice()],
        )?;
        Ok(n)
    }

    /// List every `(scope_id, document_id)` composite key present in
    /// `approved_document_payloads`. This is a cheap metadata-only
    /// scan (no AEAD decryption) used by the orphan-sweep at
    /// `open_store` time to compare against the set of refs
    /// rehydrated from tenant memory.
    ///
    /// **Malformed-row policy:** because the caller is a best-effort
    /// orphan sweep — not a rehydration path that requires every row
    /// — rows whose `scope_id` or `document_id` columns do not parse
    /// as 16-byte UUIDs are logged at WARN and skipped rather than
    /// aborting the whole scan. The alternative (hard error) would
    /// let a single corrupt row block cleanup of every legitimate
    /// orphan until the row was manually purged, which is a worse
    /// operational outcome than tolerating the corrupt row's payload
    /// staying behind (it is already unreachable through the
    /// tenant-memory join). Surfaceable error paths (the SQL
    /// `prepare` / `query_map` / per-row `Result`) still bubble up
    /// because those indicate the table or connection itself is in
    /// a state the sweep cannot reason about.
    pub fn list_all_approved_document_payload_keys(&self) -> Result<Vec<(ScopeId, uuid::Uuid)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT scope_id, document_id FROM approved_document_payloads")?;
        let rows = stmt.query_map([], |row| {
            let scope_bytes: Vec<u8> = row.get(0)?;
            let doc_bytes: Vec<u8> = row.get(1)?;
            Ok((scope_bytes, doc_bytes))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (scope_bytes, doc_bytes) = row?;
            let Ok(scope_id) = uuid::Uuid::from_slice(&scope_bytes) else {
                tracing::warn!(scope_bytes_len = scope_bytes.len(),
                    "list_all_approved_document_payload_keys: skipping row with non-UUID scope_id; \
                     orphan sweep will leave this row untouched (manual purge required to recover)",
                );
                continue;
            };
            let Ok(doc_id) = uuid::Uuid::from_slice(&doc_bytes) else {
                tracing::warn!(scope = %scope_id,
                    doc_bytes_len = doc_bytes.len(),
                    "list_all_approved_document_payload_keys: skipping row with non-UUID document_id; \
                     orphan sweep will leave this row untouched (manual purge required to recover)",
                );
                continue;
            };
            result.push((ScopeId::from_uuid(scope_id), doc_id));
        }
        Ok(result)
    }

    // ────────── synthesis_object_versions ──────────
    //
    // The live `synthesis_objects` blob (memory_objects row keyed by
    // `kind = 'synthesis_object'`) carries only the latest version
    // of each window's synthesis output. `replay_synthesis(scope,
    // window)` archives the previous latest into the
    // `synthesis_object_versions` table before installing its own
    // output as the new latest in the per-scope blob.
    //
    // AAD binds `scope_id` + `window_id` + `version` (u32 BE) via
    // `synthesis_object_version_aad`, so a ciphertext relocated to
    // a different row fails to decrypt rather than surfacing the
    // wrong-version payload to a host calling
    // `list_synthesis_versions`.
    //
    // `forget(scope)` calls
    // [`Self::delete_synthesis_object_versions_for_scope`] from the
    // FFI layer's `forget_scope_state`. Even if the delete fails,
    // the scope-DEK destruction step makes the ciphertext
    // cryptographically unrecoverable, so the row purge is
    // defense-in-depth rather than the primary security barrier.

    /// Archive a prior version of a synthesis object so a future
    /// `list_synthesis_versions(scope, window)` can replay the
    /// history. The plaintext is the serialised
    /// [`synthesis_pipeline::SynthesisObject`] JSON bytes; AEAD AAD
    /// binds `(scope, window, version)` so cross-row relocation
    /// fails to decrypt.
    pub fn save_synthesis_object_version(
        &self,
        scope_id: ScopeId,
        window_id: uuid::Uuid,
        version: u32,
        plaintext: &[u8],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.save_synthesis_object_version_in_tx(&tx, scope_id, window_id, version, plaintext)?;
        tx.commit()?;
        Ok(())
    }

    /// Transaction-bound variant of [`Self::save_synthesis_object_version`]
    /// so callers that need to bundle the version archive with the
    /// updated `synthesis_objects` blob and `synthesis_windows`
    /// blob (e.g. the FFI `replay_synthesis` entry point) can group
    /// all three writes under one SQLCipher transaction via
    /// [`Self::with_transaction`].
    pub fn save_synthesis_object_version_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        scope_id: ScopeId,
        window_id: uuid::Uuid,
        version: u32,
        plaintext: &[u8],
    ) -> Result<()> {
        let key = self.scope_key(scope_id)?;
        let nonce = random_nonce();
        let aad = synthesis_object_version_aad(scope_id, window_id, version);
        let ciphertext = encrypt_aead(&key, &nonce, plaintext, &aad)?;
        let now = chrono::Utc::now().timestamp();
        tx.execute(
            "INSERT INTO synthesis_object_versions \
             (scope_id, window_id, version, nonce, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(scope_id, window_id, version) DO UPDATE SET \
               nonce = excluded.nonce, \
               payload = excluded.payload, \
               created_at = excluded.created_at",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                window_id.as_bytes().as_slice(),
                i64::from(version),
                nonce.as_slice(),
                ciphertext,
                now,
            ],
        )?;
        Ok(())
    }

    /// Load and decrypt the synthesis-object-version row at
    /// `(scope_id, window_id, version)`. Returns the plaintext JSON
    /// bytes ready to feed into `serde_json::from_slice` on a
    /// [`synthesis_pipeline::SynthesisObject`]. Returns `Ok(None)`
    /// if no row exists.
    ///
    /// # Errors
    ///
    /// * [`EvidenceError::Schema`] if the row is malformed (nonce
    ///   length wrong) — defensive against on-disk corruption.
    /// * [`EvidenceError::Crypto`] if the AEAD decrypt fails (e.g.
    ///   the ciphertext has been relocated to the wrong row or the
    ///   scope DEK has rotated).
    pub fn load_synthesis_object_version(
        &self,
        scope_id: ScopeId,
        window_id: uuid::Uuid,
        version: u32,
    ) -> Result<Option<Vec<u8>>> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT nonce, payload FROM synthesis_object_versions \
                 WHERE scope_id = ?1 AND window_id = ?2 AND version = ?3",
                params![
                    scope_id.as_uuid().as_bytes().as_slice(),
                    window_id.as_bytes().as_slice(),
                    i64::from(version),
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((nonce_bytes, ciphertext)) = row else {
            return Ok(None);
        };
        if nonce_bytes.len() != AEAD_NONCE_LEN {
            return Err(EvidenceError::Schema(
                "synthesis_object_versions row has malformed nonce length",
            ));
        }
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        nonce.copy_from_slice(&nonce_bytes);
        let key = self.scope_key(scope_id)?;
        let aad = synthesis_object_version_aad(scope_id, window_id, version);
        let plaintext = decrypt_aead(&key, &nonce, &ciphertext, &aad)?;
        Ok(Some(plaintext))
    }

    /// Enumerate metadata for every version row archived against
    /// `(scope_id, window_id)`, sorted by `version` ascending so
    /// the caller can present the history oldest-first or reverse
    /// at will. Each tuple is `(version, created_at_unix_seconds)`.
    ///
    /// This is a metadata-only scan — no AEAD decryption — so it is
    /// safe to call on the hot path of a host listing replay
    /// history for UI / debugging.
    pub fn list_synthesis_object_versions(
        &self,
        scope_id: ScopeId,
        window_id: uuid::Uuid,
    ) -> Result<Vec<(u32, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT version, created_at FROM synthesis_object_versions \
             WHERE scope_id = ?1 AND window_id = ?2 ORDER BY version ASC",
        )?;
        let rows = stmt.query_map(
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                window_id.as_bytes().as_slice(),
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (version_i64, created_at) = row?;
            let version = u32::try_from(version_i64).map_err(|_| {
                EvidenceError::Schema(
                    "synthesis_object_versions row has out-of-range version (>= 2^32)",
                )
            })?;
            out.push((version, created_at));
        }
        Ok(out)
    }

    /// Delete the oldest archived version for `(scope_id, window_id)`
    /// inside an already-open transaction. Used by
    /// `replay_synthesis` to enforce the
    /// `MAX_SYNTHESIS_VERSIONS_PER_WINDOW` cap atomically with the
    /// new-version insert. Returns the number of rows deleted
    /// (zero if the window had no archived versions yet, one
    /// otherwise).
    pub fn delete_oldest_synthesis_object_version_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        scope_id: ScopeId,
        window_id: uuid::Uuid,
    ) -> Result<usize> {
        let n = tx.execute(
            "DELETE FROM synthesis_object_versions \
             WHERE scope_id = ?1 AND window_id = ?2 \
               AND version = ( \
                   SELECT MIN(version) FROM synthesis_object_versions \
                   WHERE scope_id = ?1 AND window_id = ?2 \
               )",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                window_id.as_bytes().as_slice(),
            ],
        )?;
        Ok(n)
    }

    /// Delete every version row bound to `scope_id`. Called from
    /// the FFI layer's `forget_scope_state` after the scope DEK has
    /// already been destroyed, as a best-effort byte purge.
    /// Returns the count of rows deleted so the caller can log it.
    pub fn delete_synthesis_object_versions_for_scope(&self, scope_id: ScopeId) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM synthesis_object_versions WHERE scope_id = ?1",
            params![scope_id.as_uuid().as_bytes().as_slice()],
        )?;
        Ok(n)
    }

    /// Delete every version row bound to a specific
    /// `(scope_id, window_id)` pair. Used by the `open_store`
    /// orphan-sweep when the parent window has vanished from the
    /// live `SynthesisWindowManager`. Returns the row count.
    pub fn delete_synthesis_object_versions_for_window(
        &self,
        scope_id: ScopeId,
        window_id: uuid::Uuid,
    ) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM synthesis_object_versions \
             WHERE scope_id = ?1 AND window_id = ?2",
            params![
                scope_id.as_uuid().as_bytes().as_slice(),
                window_id.as_bytes().as_slice(),
            ],
        )?;
        Ok(n)
    }

    /// List every `(scope_id, window_id)` composite present in
    /// `synthesis_object_versions`. This is a cheap metadata-only
    /// scan (no AEAD decryption) used by the orphan-sweep at
    /// `open_store` time to detect version rows whose parent
    /// window has vanished from the rehydrated
    /// `SynthesisWindowManager` (e.g. a crash mid-`forget_scope`
    /// dropped the window but failed to delete the history).
    ///
    /// **Malformed-row policy:** mirrors the same skip-and-warn
    /// contract as
    /// [`Self::list_all_approved_document_payload_keys`]. A single
    /// non-UUID row must not block cleanup of every legitimate
    /// orphan; surface the corruption to operators via WARN and
    /// move on.
    pub fn list_all_synthesis_object_version_window_keys(
        &self,
    ) -> Result<Vec<(ScopeId, uuid::Uuid)>> {
        // SELECT DISTINCT so a window with N archived versions
        // contributes a single row to the diff set instead of N
        // identical entries.
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT scope_id, window_id FROM synthesis_object_versions")?;
        let rows = stmt.query_map([], |row| {
            let scope_bytes: Vec<u8> = row.get(0)?;
            let window_bytes: Vec<u8> = row.get(1)?;
            Ok((scope_bytes, window_bytes))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (scope_bytes, window_bytes) = row?;
            let Ok(scope_id) = uuid::Uuid::from_slice(&scope_bytes) else {
                tracing::warn!(
                    scope_bytes_len = scope_bytes.len(),
                    "list_all_synthesis_object_version_window_keys: skipping row with \
                     non-UUID scope_id; orphan sweep will leave this row untouched \
                     (manual purge required to recover)",
                );
                continue;
            };
            let Ok(window_id) = uuid::Uuid::from_slice(&window_bytes) else {
                tracing::warn!(scope = %scope_id,
                    window_bytes_len = window_bytes.len(),
                    "list_all_synthesis_object_version_window_keys: skipping row with \
                     non-UUID window_id; orphan sweep will leave this row untouched \
                     (manual purge required to recover)",
                );
                continue;
            };
            result.push((ScopeId::from_uuid(scope_id), window_id));
        }
        Ok(result)
    }

    /// Purge every secondary-index row that retains plaintext for
    /// `scope_id`.
    ///
    /// The `evidence` table itself is append-only (UPDATE / DELETE
    /// are rejected by triggers in `SCHEMA_SQL`), and the row body
    /// is keyed off the scope DEK that the FFI runtime already
    /// destroyed via [`crypto::forgetting::destroy_scope_dek`]. The
    /// remaining surface that survives DEK destruction is the
    /// secondary indexes:
    ///
    /// * `evidence_fts` — the FTS5 shadow tables retain tokenised
    ///   *plaintext* of every body, regardless of the row's AEAD
    ///   key. This is the gap pinned by
    ///   `crates/evidence_store/tests/forgetting_fts.rs`.
    /// * `evidence_fts_cjk` — the v14 trigram-tokenised companion
    ///   index for CJK / Thai content. Same property: the shadow
    ///   tables retain tokenised plaintext for any row whose body
    ///   contains a CJK Han / Hiragana / Katakana / Thai
    ///   codepoint, regardless of AEAD key.
    /// * `evidence_fts_bigram` — the v15 precomputed-bigram
    ///   recall lane. Same property as the trigram
    ///   shadow: the table's `content` column retains the
    ///   whitespace-separated overlapping 2-codepoint windows
    ///   derived from the plaintext body for any row whose body
    ///   contains a CJK / Thai codepoint, regardless of AEAD key.
    ///   Because the windows are themselves a direct transform of
    ///   the original plaintext (a windowed projection of the
    ///   CJK / Thai portion onto `chars()[i..i+2]` pairs) they
    ///   leak the same surface as the trigram shadow and so must
    ///   be purged in the same transaction.
    /// * `evidence_embeddings` — cached `f32` vectors derived from
    ///   the plaintext body via an on-device embedding model. They
    ///   are not strictly plaintext but are still
    ///   semantically-derivable evidence and so must go.
    ///
    /// This method runs a single transaction:
    ///
    /// 1. Look up every `evidence_id` belonging to `scope_id`.
    /// 2. `DELETE FROM evidence_fts WHERE evidence_id IN (...)`,
    ///    `DELETE FROM evidence_fts_cjk WHERE evidence_id IN (...)`,
    ///    *and* `DELETE FROM evidence_fts_bigram WHERE evidence_id
    ///    IN (...)` — FTS5 supports `DELETE` on virtual tables
    ///    (they do NOT have the append-only trigger that protects
    ///    `evidence`). All three FTS shadow tables are deleted in
    ///    the same transaction so they can never drift apart.
    /// 3. `DELETE FROM evidence_embeddings WHERE evidence_id IN (...)`.
    /// 4. If — and only if — step 2 actually removed at least one
    ///    FTS row across any of the three tables, issue
    ///    `INSERT INTO evidence_fts(evidence_fts) VALUES('rebuild')`,
    ///    `INSERT INTO evidence_fts_cjk(evidence_fts_cjk) VALUES('rebuild')`,
    ///    *and* `INSERT INTO evidence_fts_bigram(evidence_fts_bigram)
    ///    VALUES('rebuild')` to truncate the FTS5 shadow tables
    ///    and re-tokenise from the surviving content rows.
    ///    Skipping this when zero FTS rows were deleted is what
    ///    makes the function genuinely idempotent: re-purging an
    ///    already-purged scope on startup costs one `SELECT` plus
    ///    one zero-row `DELETE` per table, not a full
    ///    O(total_fts_rows)
    ///    rebuild.
    ///
    /// The `evidence` rows themselves are intentionally left in
    /// place — the append-only trigger forbids removing them, and
    /// without the scope DEK the encrypted bodies in `body_store`
    /// / inline `evidence.body` are unrecoverable anyway. Hosts
    /// that need to drop the physical bytes must perform a
    /// VACUUM-style rebuild at a higher layer.
    ///
    /// When replaying many tombstones on `open_store`, prefer
    /// [`Self::purge_fts_for_scopes`] instead. Both methods skip
    /// the `REBUILD` when zero FTS rows were actually deleted, so
    /// the steady-state replay (every scope already purged on a
    /// prior boot) is equivalent either way. The batch method's
    /// real advantage is the crash-recovery shape: when `K` of the
    /// `N` tombstones still have FTS rows because a crash landed
    /// between tombstone write and FTS purge, calling this
    /// single-scope method `N` times issues `K` separate rebuilds
    /// (one per scope that still has data); the batch method
    /// coalesces them into a single rebuild at the end of the
    /// transaction.
    pub fn purge_fts_for_scope(&mut self, scope_id: ScopeId) -> Result<()> {
        let tx = self.conn.transaction()?;
        let fts_rows_deleted = Self::purge_fts_for_scope_in_tx(&tx, scope_id)?;
        if fts_rows_deleted > 0 {
            Self::rebuild_evidence_fts_in_tx(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Batched equivalent of [`Self::purge_fts_for_scope`] for
    /// processing many tombstoned scopes in a single transaction.
    ///
    /// Runs the per-scope `DELETE` work for every entry in
    /// `scope_ids`, then issues at most one FTS5 `REBUILD` at the
    /// end — not one per scope. This is what the tombstone replay
    /// loop on `open_store` uses: a database that has forgotten
    /// `N` scopes pays O(total_fts_rows) for the rebuild instead
    /// of O(N × total_fts_rows).
    ///
    /// The rebuild is skipped entirely when zero FTS rows were
    /// removed across the whole batch (the steady-state case where
    /// every scope was already purged on a prior boot).
    pub fn purge_fts_for_scopes(&mut self, scope_ids: &[ScopeId]) -> Result<()> {
        if scope_ids.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        let mut total_fts_rows_deleted: usize = 0;
        for scope_id in scope_ids {
            total_fts_rows_deleted += Self::purge_fts_for_scope_in_tx(&tx, *scope_id)?;
        }
        if total_fts_rows_deleted > 0 {
            Self::rebuild_evidence_fts_in_tx(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every FTS / embedding row tied to `scope_id` inside
    /// the caller's transaction. Returns the number of FTS rows
    /// actually removed — callers use this to decide whether a
    /// `REBUILD` is needed at the end of the transaction.
    ///
    /// This is the shared core of [`Self::purge_fts_for_scope`]
    /// (single-scope) and [`Self::purge_fts_for_scopes`] (batch);
    /// keeping the delete logic in one place ensures the two
    /// entry points cannot drift on which tables they touch or
    /// how parameter batching is sized.
    fn purge_fts_for_scope_in_tx(
        tx: &rusqlite::Transaction<'_>,
        scope_id: ScopeId,
    ) -> Result<usize> {
        let evidence_ids: Vec<Vec<u8>> = {
            let mut stmt = tx.prepare("SELECT id FROM evidence WHERE scope_id = ?1")?;
            let rows = stmt
                .query_map(params![scope_id.as_uuid().as_bytes().as_slice()], |row| {
                    row.get::<_, Vec<u8>>(0)
                })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        // Issue the DELETEs in batches so we never build a single
        // `IN (?, ?, ...)` clause that exceeds SQLite's parameter
        // cap. `SQLITE_MAX_VARIABLE_NUMBER` is 999 on the default
        // build; we stay well under it.
        //
        // Per / schema v14, `evidence_fts_cjk` (trigram-
        // tokenised CJK / Thai index) is purged alongside the
        // primary `evidence_fts` (unicode61) in the same
        // transaction so the two indexes can never drift apart
        // under crash-recovery, and so a forgotten scope leaves
        // zero plaintext tokens in either FTS shadow table after
        // the subsequent `REBUILD`. / schema v15
        // extends the same invariant to `evidence_fts_bigram`
        // (precomputed-bigram recall lane) — the three FTS
        // shadow tables are purged together inside the same
        // transaction so a partial purge can never leave bigram
        // tokens behind that the unicode61 / trigram purge
        // cleared. The returned count is the sum across all
        // three tables — if any tokeniser still has rows for the
        // scope, the caller-side `if rows_deleted > 0` gate
        // still triggers a rebuild that fans out across the
        // three indexes via
        // [`Self::rebuild_evidence_fts_in_tx`].
        let mut fts_rows_deleted: usize = 0;
        for chunk in evidence_ids.chunks(DELETE_BATCH) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let fts_sql = format!("DELETE FROM evidence_fts WHERE evidence_id IN ({placeholders})");
            let fts_cjk_sql =
                format!("DELETE FROM evidence_fts_cjk WHERE evidence_id IN ({placeholders})");
            let fts_bigram_sql =
                format!("DELETE FROM evidence_fts_bigram WHERE evidence_id IN ({placeholders})");
            let emb_sql =
                format!("DELETE FROM evidence_embeddings WHERE evidence_id IN ({placeholders})");
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            fts_rows_deleted +=
                tx.execute(&fts_sql, rusqlite::params_from_iter(params.iter().copied()))?;
            fts_rows_deleted += tx.execute(
                &fts_cjk_sql,
                rusqlite::params_from_iter(params.iter().copied()),
            )?;
            fts_rows_deleted += tx.execute(
                &fts_bigram_sql,
                rusqlite::params_from_iter(params.iter().copied()),
            )?;
            tx.execute(&emb_sql, rusqlite::params_from_iter(params.iter().copied()))?;
        }
        Ok(fts_rows_deleted)
    }

    /// Issue the FTS5 `REBUILD` command on **all three** lexical
    /// indexes — `evidence_fts` (unicode61), `evidence_fts_cjk`
    /// (trigram, schema v14), and `evidence_fts_bigram`
    /// (precomputed-bigram, schema v15) — truncating their
    /// shadow tables (`%_data`, `%_idx`, `%_docsize`, …) and
    /// re-tokenising from the surviving content rows.
    ///
    /// `OPTIMIZE` only merges segments and can leave tokenised
    /// plaintext fragments behind in the `%_data` segment B-tree
    /// for rows that were `DELETE`'d in this same transaction.
    /// `REBUILD` re-tokenises from each table's stored `content`
    /// column — which now no longer references the purged scopes
    /// — so no residual plaintext tokens survive on disk for the
    /// forgotten scopes in any of the three tokenisers' shadow
    /// stores. All three rebuilds run inside the caller's
    /// transaction so the tables are committed atomically.
    ///
    /// For `evidence_fts_bigram` the stored `content` column
    /// already holds the precomputed bigram string (see
    /// [`crate::bigram::compute_cjk_bigrams`]), so the
    /// `unicode61` REBUILD re-derives the same bigram tokens
    /// without re-running the codepoint-filter pass — the only
    /// requirement is that any bigram strings for purged scopes
    /// have already been DELETEd by
    /// [`Self::purge_fts_for_scope_in_tx`] before this REBUILD
    /// fires, which the surrounding transaction enforces.
    ///
    /// This is the strongest in-engine guarantee SQLite FTS5
    /// exposes; the alternative would be a full `VACUUM` at a
    /// higher layer, which is owned by the host.
    fn rebuild_evidence_fts_in_tx(tx: &rusqlite::Transaction<'_>) -> Result<()> {
        tx.execute(
            "INSERT INTO evidence_fts(evidence_fts) VALUES('rebuild')",
            [],
        )?;
        tx.execute(
            "INSERT INTO evidence_fts_cjk(evidence_fts_cjk) VALUES('rebuild')",
            [],
        )?;
        tx.execute(
            "INSERT INTO evidence_fts_bigram(evidence_fts_bigram) VALUES('rebuild')",
            [],
        )?;
        Ok(())
    }

    /// Purge every `body_store_key_wraps` row for `scope_id`.
    ///
    /// After this call, the scope no longer has the CEK needed to
    /// decrypt any body-table row it referenced. If the purge leaves
    /// zero wraps for a content_hash, the body_store row is
    /// cryptographically unrecoverable — this method garbage-collects
    /// those orphaned body rows as well (clearing the ciphertext so
    /// the physical bytes do not linger on disk).
    pub fn purge_body_key_wraps_for_scope(&mut self, scope_id: ScopeId) -> Result<()> {
        let tx = self.conn.transaction()?;

        // Collect the content hashes that this scope wraps so we can
        // check for orphans after deletion.
        let hashes: Vec<Vec<u8>> = {
            let mut stmt =
                tx.prepare("SELECT content_hash FROM body_store_key_wraps WHERE scope_id = ?1")?;
            let rows = stmt
                .query_map(params![scope_id.as_uuid().as_bytes().as_slice()], |row| {
                    row.get::<_, Vec<u8>>(0)
                })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        // Delete all wraps for the forgotten scope.
        tx.execute(
            "DELETE FROM body_store_key_wraps WHERE scope_id = ?1",
            params![scope_id.as_uuid().as_bytes().as_slice()],
        )?;

        // Garbage-collect orphaned body_store rows whose last wrap
        // was just deleted. A body_store row with zero remaining
        // wraps is cryptographically unrecoverable.
        //
        // Single batched `DELETE … WHERE content_hash IN (?,?,…) AND
        // NOT EXISTS (…)` per chunk replaces the previous N+1 query
        // pattern (SELECT COUNT then DELETE per hash). `DELETE_BATCH`
        // stays well below SQLite's `SQLITE_MAX_VARIABLE_NUMBER` cap
        // so the statement compiles even on builds that lower it,
        // mirroring the FTS purge sibling above.
        // `slice::chunks` never yields an empty slice, so no explicit
        // empty-guard is needed; the loop body issues exactly one
        // `DELETE` per non-empty chunk.
        for chunk in hashes.chunks(DELETE_BATCH) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "DELETE FROM body_store \
                 WHERE content_hash IN ({placeholders}) \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM body_store_key_wraps w \
                     WHERE w.content_hash = body_store.content_hash \
                 )"
            );
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|h| h as &dyn rusqlite::ToSql).collect();
            tx.execute(
                sql.as_str(),
                rusqlite::params_from_iter(params.iter().copied()),
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Wire an [`EmbeddingModel`] into the store so subsequent
    /// [`Self::ingest`] calls populate the `evidence_embeddings`
    /// cache. `model_tag` is stamped on every persisted
    /// row so the cache can be invalidated when the model changes.
    ///
    /// Consumes `self` and returns the same store \u2014 callers can
    /// chain it directly onto [`Self::open`]:
    ///
    /// ```ignore
    /// let store = EvidenceStore::open(&path, &key, cfg)?
    ///     .with_embedding_model(my_model, "xlm-r-v1");
    /// ```
    pub fn with_embedding_model<M: EmbeddingModel + 'static>(
        mut self,
        model: M,
        model_tag: impl Into<String>,
    ) -> Self {
        let dim = model.dimension();
        self.embedding_model = Some(Box::new(model));
        self.embedding_model_tag = model_tag.into();
        // Register the wired-in (tag, dimension) pair so a same-tag /
        // different-dimension rotation violation is flagged in
        // `model_tag_dimension_violations_total` immediately on wire-
        // in, rather than only when the cache happens to be consulted.
        crate::vector_telemetry::record_observed_dimension(&self.embedding_model_tag, dim);
        self
    }

    /// Same as [`Self::with_embedding_model`] but takes `&mut self`
    /// for callers that already own a `&mut` handle to the store.
    pub fn set_embedding_model<M: EmbeddingModel + 'static>(
        &mut self,
        model: M,
        model_tag: impl Into<String>,
    ) {
        let dim = model.dimension();
        self.embedding_model = Some(Box::new(model));
        self.embedding_model_tag = model_tag.into();
        // Same wire-in observation as `with_embedding_model` — mirror
        // the rotation-violation flag for `&mut self` callers.
        crate::vector_telemetry::record_observed_dimension(&self.embedding_model_tag, dim);
    }

    /// `true` when an [`EmbeddingModel`] is wired in. Useful for tests
    /// and for the retriever's fallback logic.
    pub fn has_embedding_model(&self) -> bool {
        self.embedding_model.is_some()
    }

    /// Borrow the wired-in [`EmbeddingModel`], if any. Exposed so the
    /// hybrid retriever can share the store's model on the query side
    /// instead of asking callers to wire it in twice.
    pub fn embedding_model(&self) -> Option<&dyn EmbeddingModel> {
        self.embedding_model.as_deref()
    }

    /// Direct write into `evidence_embeddings`. Bypasses the ingest
    /// path \u2014 useful for tests, batch back-fill jobs, and callers
    /// that compute embeddings asynchronously after ingest.
    ///
    /// The embedding is serialised as little-endian raw `f32` bytes.
    pub fn store_embedding(
        &mut self,
        evidence_id: EvidenceId,
        embedding: &[f32],
        model_tag: &str,
    ) -> Result<()> {
        let bytes = embedding_to_bytes(embedding);
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO evidence_embeddings
                 (evidence_id, embedding, model_tag, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                evidence_id.as_uuid().as_bytes().as_slice(),
                bytes,
                model_tag,
                now,
            ],
        )?;
        Ok(())
    }

    /// Read a persisted embedding for `evidence_id` without filtering by
    /// `model_tag`. Intended for tests, batch back-fill jobs, and admin
    /// tooling that needs to inspect whatever vector is currently
    /// cached regardless of which model produced it.
    ///
    /// Production retrieval code MUST NOT use this — a stale row from a
    /// previous model that happens to share an output dimension with
    /// the active model would be returned and scored as if it had been
    /// produced by the active model, leading to semantically
    /// meaningless cosine similarities. The hybrid retriever uses
    /// [`Self::get_embedding_for_model`] instead, which enforces the
    /// same `model_tag` invariant the write side applies on dedup.
    ///
    /// Returns `None` when the row has no cached embedding yet (e.g.
    /// ingested before any model was wired in, or the model returned
    /// an error). Errors with [`EvidenceError::Schema`] when the stored
    /// BLOB has a length that is not a multiple of 4 (i.e. the row was
    /// corrupted or written by a future schema).
    ///
    /// Under the composite primary key (`evidence_id`, `model_tag`)
    /// multiple rows can exist for the same `evidence_id` (one per
    /// model the row has ever been embedded under). This method picks
    /// the most recently inserted such row (highest `created_at`) so
    /// the return value is deterministic; if more than one row shares
    /// the same `created_at` the SQLite query planner chooses the
    /// tiebreaker. Callers that need a specific tag must use
    /// [`Self::get_embedding_for_model`].
    pub fn get_embedding(&self, evidence_id: EvidenceId) -> Result<Option<Vec<f32>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT embedding FROM evidence_embeddings
                 WHERE evidence_id = ?1
                 ORDER BY created_at DESC
                 LIMIT 1",
                params![evidence_id.as_uuid().as_bytes().as_slice()],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        match bytes {
            None => Ok(None),
            Some(b) => bytes_to_embedding(&b).map(Some),
        }
    }

    /// Read a persisted embedding for `evidence_id` only when it was
    /// produced by `model_tag`. This is the production read path used
    /// by the hybrid retriever and mirrors the `model_tag`-aware write
    /// invariant in [`Self::index_embedding_or_copy_dedup`].
    ///
    /// Returns `None` when:
    ///   * The row has no cached embedding yet, OR
    ///   * The cached row's `model_tag` does not match the active model
    ///     (e.g. the model was swapped to a different version that
    ///     happens to share an output dimension — without this filter
    ///     the stale bytes would be returned and scored as if they had
    ///     been produced by the new model, which is silently incorrect).
    ///
    /// Errors with [`EvidenceError::Schema`] when the stored BLOB has a
    /// length that is not a multiple of 4 (i.e. the row was corrupted
    /// or written by a future schema).
    pub fn get_embedding_for_model(
        &self,
        evidence_id: EvidenceId,
        model_tag: &str,
    ) -> Result<Option<Vec<f32>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT embedding FROM evidence_embeddings
                 WHERE evidence_id = ?1 AND model_tag = ?2",
                params![evidence_id.as_uuid().as_bytes().as_slice(), model_tag],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        match bytes {
            None => Ok(None),
            Some(b) => bytes_to_embedding(&b).map(Some),
        }
    }
}

/// Serialise an `f32` slice as little-endian raw bytes.
fn embedding_to_bytes(emb: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(emb.len() * 4);
    for f in emb {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

/// Deserialise a little-endian raw-bytes embedding produced by
/// [`embedding_to_bytes`]. Errors if `bytes.len()` is not a multiple
/// of 4 (a corrupted row, since `f32` is 4 bytes wide).
fn bytes_to_embedding(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return Err(EvidenceError::Schema(
            "evidence_embeddings.embedding has length not a multiple of 4",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Ok(out)
}

fn random_nonce() -> AeadNonce {
    use rand::rngs::SysRng;
    use rand::TryRng;
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    // See SECURITY.md §"Random number generation" for why the
    // substrate uses the OS RNG (`SysRng`, not the userspace
    // `ThreadRng`) for every per-row AEAD nonce, even on the hot
    // path. Panicking on OS RNG failure is intentional — a
    // substrate that cannot draw entropy cannot encrypt safely.
    SysRng.try_fill_bytes(&mut nonce).expect("OS RNG failure");
    nonce
}

fn random_cek() -> AeadKey {
    // `SysRng` is the rand-0.10 OS RNG (renamed from rand 0.9's
    // `OsRng`) and impls the fallible `TryRng` trait (renamed from
    // rand 0.9's `TryRngCore`). Calling `try_fill_bytes(...).expect(...)`
    // panics on OS RNG failure — the correct posture, because a
    // substrate that cannot draw entropy cannot wrap content safely.
    // See SECURITY.md §"Random number generation" for the broader
    // policy.
    use rand::rngs::SysRng;
    use rand::TryRng;
    let mut key = [0u8; AEAD_KEY_LEN];
    SysRng.try_fill_bytes(&mut key).expect("OS RNG failure");
    key
}

/// Wrap (encrypt) a CEK under `wrapper_key` with a freshly drawn
/// nonce. AAD binds the content hash so a wrap cannot be re-labelled
/// across bodies.
fn wrap_cek(
    wrapper_key: &AeadKey,
    cek: &AeadKey,
    nonce: &AeadNonce,
    content_hash: &ContentHash,
) -> Result<Vec<u8>> {
    let aad = cek_wrap_aad(content_hash);
    Ok(encrypt_aead(wrapper_key, nonce, cek.as_slice(), &aad)?)
}

/// Unwrap a previously-wrapped CEK, recovering the 32-byte symmetric
/// key.
fn unwrap_cek(
    wrapper_key: &AeadKey,
    wrapped: &[u8],
    nonce_bytes: &[u8],
    content_hash: &ContentHash,
) -> Result<AeadKey> {
    if nonce_bytes.len() != AEAD_NONCE_LEN {
        return Err(EvidenceError::Schema(
            "body_store_key_wraps row has malformed nonce",
        ));
    }
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    nonce.copy_from_slice(nonce_bytes);
    let aad = cek_wrap_aad(content_hash);
    let pt = decrypt_aead(wrapper_key, &nonce, wrapped, &aad)?;
    if pt.len() != AEAD_KEY_LEN {
        return Err(EvidenceError::Schema("unwrapped CEK has wrong length"));
    }
    let mut key = [0u8; AEAD_KEY_LEN];
    key.copy_from_slice(&pt);
    Ok(key)
}

fn cek_wrap_aad(content_hash: &ContentHash) -> Vec<u8> {
    let mut aad = Vec::with_capacity(12 + 32);
    aad.extend_from_slice(b"cek_wrap:v1:");
    aad.extend_from_slice(content_hash);
    aad
}

fn ingest_aad(scope_id: ScopeId, evidence_id: EvidenceId, hash: &ContentHash) -> Vec<u8> {
    let mut aad = Vec::with_capacity(16 + 16 + 32);
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad.extend_from_slice(evidence_id.as_uuid().as_bytes());
    aad.extend_from_slice(hash);
    aad
}

fn body_table_aad(hash: &ContentHash) -> Vec<u8> {
    let mut aad = Vec::with_capacity(13 + 32);
    aad.extend_from_slice(b"body_store:v1");
    aad.extend_from_slice(hash);
    aad
}

fn ring_buffer_aad(scope_id: ScopeId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(14 + 16);
    aad.extend_from_slice(b"ring_buffer:v1");
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad
}

// AAD for v9 connector persistence. Each row's plaintext is bound to
// the `(scope_id, instance_id, kind_tag)` triple (instance rows) or
// `(scope_id, instance_id)` pair (token rows), so a ciphertext copied
// to a different row fails to decrypt with a structured error instead
// of silently aliasing onto a different identity. The leading magic
// prefix (`connector-instance:v1:` / `connector-token:v1:`) gives
// future schema evolutions a stable namespace to bump for an
// incompatible AAD format change.
fn connector_instance_aad(scope_id: ScopeId, instance_id: uuid::Uuid, kind_tag: &str) -> Vec<u8> {
    let prefix = b"connector-instance:v1:";
    let mut aad = Vec::with_capacity(prefix.len() + 16 + 16 + 1 + kind_tag.len());
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad.extend_from_slice(instance_id.as_bytes());
    // Delimiter-separate the kind tag. All preceding fields are
    // fixed-width (prefix 22 B + scope 16 B + instance 16 B), so
    // `kind_tag` starts at a deterministic offset and the colon is
    // strictly cosmetic — but cheap to keep for readability when
    // hex-dumping the AAD during debugging.
    aad.push(b':');
    aad.extend_from_slice(kind_tag.as_bytes());
    aad
}

fn connector_token_aad(scope_id: ScopeId, instance_id: uuid::Uuid) -> Vec<u8> {
    let prefix = b"connector-token:v1:";
    let mut aad = Vec::with_capacity(prefix.len() + 16 + 16);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad.extend_from_slice(instance_id.as_bytes());
    aad
}

fn synthesis_object_version_aad(scope_id: ScopeId, window_id: uuid::Uuid, version: u32) -> Vec<u8> {
    let prefix = b"synthesis-object-version:v1:";
    let mut aad = Vec::with_capacity(prefix.len() + 16 + 16 + 4);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad.extend_from_slice(window_id.as_bytes());
    aad.extend_from_slice(&version.to_be_bytes());
    aad
}

fn scope_dek_aad(scope_id: ScopeId) -> Vec<u8> {
    // b"scope-dek-wrap:v1" = 17 bytes + UUID = 16 bytes = 33 total.
    let mut aad = Vec::with_capacity(17 + 16);
    aad.extend_from_slice(b"scope-dek-wrap:v1");
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad
}

/// Clamp a Rust `usize` LIMIT/OFFSET argument into the signed range
/// SQLite expects on the wire. Saturating at `i64::MAX` is the
/// correct semantic for an out-of-range value: SQL `LIMIT i64::MAX`
/// is effectively "no limit", which is what "limit too large to
/// represent" means.
pub(crate) fn clamp_limit_to_sqlite(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Run a fanned-out multi-table FTS5 search across every lexical
/// index the schema exposes today (`evidence_fts` unicode61,
/// `evidence_fts_cjk` trigram, `evidence_fts_bigram`
/// precomputed-bigram) and return `(evidence_id, best_rank)` pairs
/// sorted by best rank ascending, truncated to `limit`.
///
/// See [`EvidenceStore::search_fts`] for the full design rationale.
/// Briefly:
///
/// * `evidence_fts` (unicode61, schema v1+) is the **source of
///   truth for query validity** — any error from its `MATCH` is
///   propagated. The per-branch `LIMIT` bounds the row count to
///   `limit`.
/// * `evidence_fts_cjk` (trigram, schema v14) is **purely
///   additive recall** for 3+ codepoint CJK / Thai substring
///   queries — any error from its `MATCH` (including the
///   documented short-term / `NEAR(…)` / column-filter / short
///   prefix-star rejections) is swallowed and the branch is
///   treated as the empty set, so a syntactically valid
///   `unicode61` query never breaks `search_fts` just because
///   `trigram` rejects the shape.
/// * `evidence_fts_bigram` (precomputed-bigram, schema v15) is
///   **purely additive recall** for 2+ codepoint CJK / Thai
///   substring queries — closes the FTS5 trigram tokeniser's
///   hard 3-codepoint minimum so queries like `天気` (Japanese
///   "weather", 2 codepoints) match real rows instead of
///   returning an empty result set. Like the trigram lane, any
///   error from this branch's `MATCH` is swallowed; additionally
///   the lane is skipped entirely when the query has fewer than
///   two CJK / Thai codepoints (because the bigram tokeniser
///   cannot produce any tokens for it — see
///   [`crate::bigram::compute_cjk_bigram_query`]).
///
/// The per-branch results are merged in a `HashMap<EvidenceId, f64>`
/// keeping `MIN(rank)` (FTS5 rank is negative-and-smaller-is-better),
/// then sorted ascending and truncated to `limit`. The merged set
/// is bounded by `3 * limit` before truncation, independent of
/// dataset size.
///
/// `pub(crate)` so [`crate::retrieval::HybridRetriever::search_fts`]
/// can reuse the same merge logic (both call sites need identical
/// dedupe + error-containment semantics; diverging implementations
/// would silently drift apart, which is
/// a latent failure mode).
///
/// The function is named `merged_fts_search` rather than
/// `dual_/triple_fts_search` so the public-crate-internal name
/// stays stable as the schema grows additional tokeniser branches
/// over future schema bumps.
pub(crate) fn merged_fts_search(
    conn: &Connection,
    scope_id: ScopeId,
    query: &str,
    limit: usize,
) -> Result<Vec<(EvidenceId, f64)>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let limit_sql = clamp_limit_to_sqlite(limit);
    let scope_uuid = scope_id.as_uuid();
    let scope_bytes = scope_uuid.as_bytes().as_slice();

    // schema v16: the trigram and bigram recall lanes
    // are queried against stopword-stripped indexed content (see
    // [`EvidenceStore::index_fts`]). The query side must apply the
    // same strip so that tokenisation is symmetric — without it a
    // query `今日のオリンピック` would window the bridging
    // trigram `日のオ` that no longer exists in the stripped body,
    // killing the recall this lane is responsible for. The
    // unicode61 baseline branch (Branch 1 below) deliberately does
    // NOT receive the strip — `evidence_fts.content` stores the
    // full plaintext and is the universal source of truth for
    // Latin / Cyrillic / Greek / Arabic / Hebrew / Devanagari /
    // Hangul terms embedded inside a CJK body. See
    // [`crate::fts_stopwords`] for the symmetric-stripping
    // rationale.
    // Counted variant feeds the query-time stopword
    // strip telemetry — `strip_count` is the number of stopword
    // instances replaced. See [`crate::fts_telemetry`] for the
    // counter semantics.
    let (stripped_query, strip_count) =
        crate::fts_stopwords::strip_recall_lane_stopwords_counted(query);
    crate::fts_telemetry::record_stopwords_stripped(
        crate::fts_telemetry::StripSite::QueryTime,
        strip_count,
    );

    // Capacity bound: 3 branches each contribute at most `limit`
    // unique evidence_ids, so `3 * limit` is the tight upper
    // bound on the pre-truncate set size. `saturating_mul`
    // handles the `usize::MAX / 3` overflow edge defensively.
    let mut best_rank: HashMap<EvidenceId, f64> = HashMap::with_capacity(limit.saturating_mul(3));

    // Branch 1: unicode61 (universal). Errors propagate.
    //
    // The SELECT uses the explicit `bm25(<table>, <col_w>...)`
    // form built by [`crate::fts_weights::bm25_select_fragment`]
    // rather than the bare `rank` alias. With the current single-
    // indexed-column shape the SQL is `bm25(evidence_fts, 1.0)`
    // which is numerically identical to `rank`, but the explicit
    // form is the integration point for the future multi-column
    // case (a separate `subject` / `title` column would extend
    // [`crate::fts_weights::EVIDENCE_FTS_COLUMN_WEIGHTS`] and
    // the same call site would render the new argument list).
    //
    // The post-fetch `* EVIDENCE_FTS_LANE_WEIGHT` multiply is the
    // *inter-lane* weight — orthogonal to the column-weight argument
    // list inside `bm25(...)` because the cross-lane comparison
    // happens between *different* SQL statements whose raw BM25
    // ranks are not directly comparable (different lanes
    // tokenise different prefixes of the body). For the unicode61
    // baseline the weight is `1.0` so this multiply is the
    // identity today — see [`crate::fts_weights`] for the
    // architectural rationale.
    {
        // `prepare_cached` reuses the compiled FTS5 statement across
        // every `merged_fts_search` call on the same connection — the
        // hot search path no longer re-parses + re-plans the SELECT
        // on every query. Combined with the `OnceLock`-cached SQL
        // string from [`unicode61_lane_sql`], the only per-call cost
        // is the bind / step / fetch loop. (rusqlite's default cache
        // size is 16; the three lane statements all fit comfortably.)
        let sql = unicode61_lane_sql();
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![query, scope_bytes, limit_sql], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut row_count: u64 = 0;
        for row in rows {
            let (id_bytes, rank) = row?;
            let id = EvidenceId(slice_to_uuid(&id_bytes)?);
            merge_min_rank(&mut best_rank, id, rank * EVIDENCE_FTS_LANE_WEIGHT);
            row_count += 1;
        }
        // Telemetry: record the lane invocation + raw row count
        // (pre-merge, pre-truncate, pre-rank-comparison). Counts
        // the rows the lane *contributed* to the merge, not the
        // rows that survived the final cross-lane sort+truncate.
        crate::fts_telemetry::record_lane_query(crate::fts_telemetry::Lane::Unicode61, row_count);
    }

    // Branch 2: trigram (additive recall). Errors swallowed.
    //
    // The closure captures every error path inside the trigram
    // branch — `prepare`, `query_map`, per-row column mapping,
    // AND the post-retrieval `slice_to_uuid` UUID parse — so any
    // failure from the trigram tokeniser OR any malformed
    // evidence-id payload is observed locally and treated as an
    // empty contribution. We only consume the `Ok` arm, so the
    // unicode61 branch remains the sole source of truth for
    // query validity even if `evidence_fts_cjk` ever returned a
    // corrupted UUID (e.g. external database tampering). The
    // architectural fix moved the
    // UUID parse inside the swallow-scope; that means the
    // doc-comment's "errors swallowed" contract holds without
    // any post-closure exception.
    //
    // The inner closure returns a `Vec<(EvidenceId, f64)>` so the
    // caller never has to re-parse a `Vec<u8>` — and so the only
    // post-closure code is the `MIN(rank)` merge, which is
    // infallible.
    // skip the trigram branch entirely when the
    // stopword-stripped query collapses to whitespace-only — for
    // example a query of pure particles like `のはがを` strips to
    // ` `. Feeding an all-whitespace MATCH operand to FTS5 is
    // undefined across SQLite versions (3.45 errors with
    // "fts5: syntax error near \"\"", 3.46+ returns zero rows
    // silently) and either outcome is pure waste — the trigram
    // tokeniser would emit zero tokens so no row could possibly
    // match. The check uses `trim().is_empty()` rather than a
    // codepoint scan because the strip output's whitespace is
    // exactly the ASCII spaces we inserted at strip sites.
    //
    // Skip-check architectural fix: the skip-check is
    // hoisted OUT of the closure so the skip-counter and
    // lane-query-counter branches are mutually exclusive by
    // construction — matches the bigram lane's `if let Some / else`
    // pattern below. Prior to this restructure the closure
    // returned `Ok(Vec::new())` on the skip path, which also
    // matched the `if let Ok(trigram_rows)` arm and bumped
    // `record_lane_query(CjkTrigram, 0)` for the very query that
    // had just bumped `record_lane_skip(CjkTrigramPureStopwordQuery)`.
    // That violated the `queries + skips = total_attempts`
    // invariant documented on [`crate::fts_telemetry`]. The
    // current shape makes that invariant a structural property
    // of the surrounding `if / else`, not a logical one buried
    // inside the closure.
    //
    // NOTE on Latin-only queries: the trigram lane intentionally
    // does NOT structurally skip Latin-only queries even though
    // the `evidence_fts_cjk` table is only populated for bodies
    // whose [`crate::script::contains_cjk_or_thai`] is true. The
    // FTS5 `trigram` tokeniser windows ALL 3-codepoint sequences
    // in the body — including any Latin substrings embedded in a
    // CJK body (e.g. `日本のiPhone発表` produces trigrams `iPh`,
    // `Pho`, `hon`, `one`). A Latin-only query like `iPhone`
    // therefore CAN match Latin substrings stored in the CJK-only
    // table, contributing additional weighted rank for the merge.
    // Operators reading `cjk_trigram_lane_rows_total /
    // cjk_trigram_lane_queries_total` will observe lower precision
    // on Latin-dominant workloads — that signal is intentional and
    // not a bug. An earlier commit (`4aaccba`) tried to
    // structurally skip Latin queries here and was reverted
    // once the trigram tokeniser's cross-script behaviour
    // was correctly identified.
    if stripped_query.as_ref().trim().is_empty() {
        // Telemetry: pure-stopword query collapsed to empty
        // after stripping — the trigram lane is declined
        // without invoking SQLite.
        crate::fts_telemetry::record_lane_skip(
            crate::fts_telemetry::SkipReason::CjkTrigramPureStopwordQuery,
        );
    } else {
        let trigram_attempt: rusqlite::Result<Vec<(EvidenceId, f64)>> = (|| {
            // `prepare_cached` — see Branch 1 comment for rationale.
            //
            // the bound query parameter is
            // `stripped_query.as_ref()` — NOT the raw `query` — so
            // the FTS5 trigram MATCH runs against the same
            // stopword-stripped content the index was written with
            // (schema v16 contract). Using the raw query here would
            // silently destroy recall: a `今日のオリンピック` query
            // would window `日のオ` which the stripped body no
            // longer contains.
            let sql = trigram_lane_sql();
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map(
                params![stripped_query.as_ref(), scope_bytes, limit_sql],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, f64>(1)?)),
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, rank) = row?;
                // A malformed UUID in `evidence_fts_cjk` can only come
                // from external corruption (the write path at
                // `index_fts` always writes a valid 16-byte
                // `Uuid::as_bytes`). Skip the offending row rather
                // than aborting the whole trigram branch so the rest
                // of the recall lane still merges into the unicode61
                // result set. This matches the broader contract that
                // the trigram lane is *purely additive*.
                match slice_to_uuid(&id_bytes) {
                    Ok(uuid) => out.push((EvidenceId(uuid), rank)),
                    Err(_) => continue,
                }
            }
            Ok(out)
        })();
        if let Ok(trigram_rows) = trigram_attempt {
            // apply the trigram lane's inter-lane weight
            // (`EVIDENCE_FTS_CJK_LANE_WEIGHT`, < 1.0) before merging
            // so the trigram lane's precision penalty propagates into
            // the cross-lane min-rank comparison. See
            // [`crate::fts_weights`] for the precision-vs-recall
            // rationale.
            let row_count = u64::try_from(trigram_rows.len()).unwrap_or(u64::MAX);
            for (id, rank) in trigram_rows {
                merge_min_rank(&mut best_rank, id, rank * EVIDENCE_FTS_CJK_LANE_WEIGHT);
            }
            // Telemetry: record the lane invocation + contributed
            // row count. The skip branch above is structurally
            // mutually exclusive with this arm (see the doc
            // comment on the outer `if / else`), so the
            // `queries + skips = total_attempts` contract holds
            // by construction. An `Err` outcome of the
            // swallow-scope is silently dropped (same
            // architectural rule as the doc-comment "errors
            // swallowed" contract); the telemetry counter is
            // bumped only when the lane contributed rows that
            // the merge actually consumed.
            crate::fts_telemetry::record_lane_query(
                crate::fts_telemetry::Lane::CjkTrigram,
                row_count,
            );
        }
    }

    // Branch 3: precomputed-bigram (additive recall). Errors
    // swallowed, same shape as the trigram lane.
    //
    // We only prepare the statement if the query bigram-
    // tokenisation produces at least one term — for a query
    // with fewer than two CJK / Thai codepoints the bigram lane
    // cannot contribute, and running a no-op MATCH would still
    // pay the prepare + statement cache cost. The branch-skip
    // gate is what makes Latin-only queries free of bigram
    // overhead.
    //
    // For queries that DO produce bigram terms we run a
    // synthesised MATCH clause (`"AB" AND "BC" AND "CD"`) against
    // `evidence_fts_bigram`. The `unicode61` tokeniser on that
    // table splits the precomputed bigram string on whitespace
    // into the individual bigram tokens, so the MATCH semantics
    // are "row's bigram string contains every requested bigram"
    // — the exact recall lane the v14 trigram tokeniser cannot
    // serve for 2-codepoint CJK queries because its built-in
    // tokeniser produces no trigrams for them.
    //
    // The same "every error path in the closure, post-retrieval
    // UUID parse inside the swallow scope" pattern from Branch
    // 2 applies here — see the trigram branch comment for the
    // architectural rationale.
    // feed `compute_cjk_bigram_query` the stopword-
    // stripped query (not the raw `query`) so the bigram windows
    // it generates are computed over the same character set that
    // the index-time write path windowed for storage. This is the
    // query half of the symmetric stripping contract — see
    // [`EvidenceStore::index_fts`] for the storage half. The
    // `compute_cjk_bigram_query` function additionally filters
    // each kept character down to the CJK / Thai routing
    // predicate, so the ASCII spaces produced by stripping are
    // dropped before windowing (the body went through the same
    // CJK-only filter via `compute_cjk_bigrams`, so the bridging
    // bigram is requested by the query whenever it would be
    // produced by the body).
    //
    // Skip-taxonomy architectural fix: the bigram lane's
    // skip taxonomy now matches the trigram lane's structural
    // shape — the pure-stopword check runs BEFORE
    // `compute_cjk_bigram_query` so a CJK pure-stopword query
    // like `の の の` records `BigramPureStopwordQuery` (correct
    // semantic) rather than `BigramNoCjkQuery` (technically
    // accurate but operationally misleading, since the query
    // DID start as CJK and only became no-CJK as a side effect
    // of stripping). The two bigram skip variants are therefore
    // mutually exclusive by construction: a pure-stopword query
    // exits at the first branch; a non-CJK / Latin-only query
    // proceeds past the empty-check and exits at the second.
    if stripped_query.as_ref().trim().is_empty() {
        // Telemetry: pure-stopword CJK query collapsed to empty
        // after stripping — the bigram lane is declined here
        // (NOT via `BigramNoCjkQuery`) so operators can tell a
        // genuinely non-CJK query path from a CJK-input-but-
        // pure-particles path. Parallels the trigram-lane
        // pure-stopword branch above.
        crate::fts_telemetry::record_lane_skip(
            crate::fts_telemetry::SkipReason::BigramPureStopwordQuery,
        );
    } else if let Some(bigram_match) =
        crate::bigram::compute_cjk_bigram_query(stripped_query.as_ref())
    {
        let bigram_attempt: rusqlite::Result<Vec<(EvidenceId, f64)>> = (|| {
            // Note on telemetry placement: the bigram lane query
            // counter is bumped post-swallow (after the `Ok` arm
            // is reached and `bigram_rows` is in scope) — same
            // architectural choice as the trigram lane above.
            // The bigram skip counters are bumped in the two
            // sibling `else` arms (pure-stopword above, no-CJK
            // below) — both mutually exclusive with this arm by
            // construction.
            //
            // `prepare_cached` — see Branch 1 comment for rationale.
            let sql = bigram_lane_sql();
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map(params![bigram_match, scope_bytes, limit_sql], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, f64>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, rank) = row?;
                match slice_to_uuid(&id_bytes) {
                    Ok(uuid) => out.push((EvidenceId(uuid), rank)),
                    Err(_) => continue,
                }
            }
            Ok(out)
        })();
        if let Ok(bigram_rows) = bigram_attempt {
            // apply the bigram lane's inter-lane weight
            // (`EVIDENCE_FTS_BIGRAM_LANE_WEIGHT`, < trigram's <
            // 1.0) before merging — the bigram lane is the
            // highest-recall and lowest-precision lane so its
            // weighted ranks pay the steepest cross-lane penalty.
            let row_count = u64::try_from(bigram_rows.len()).unwrap_or(u64::MAX);
            for (id, rank) in bigram_rows {
                merge_min_rank(&mut best_rank, id, rank * EVIDENCE_FTS_BIGRAM_LANE_WEIGHT);
            }
            crate::fts_telemetry::record_lane_query(crate::fts_telemetry::Lane::Bigram, row_count);
        }
    } else {
        // Telemetry: stripped query was non-empty but had no
        // adjacent-CJK codepoint pair (e.g. a Latin-only query
        // or a CJK query with all isolated codepoints separated
        // by non-CJK characters). This is the "expected" skip
        // path for the bigram lane on non-CJK traffic — it is
        // structurally distinct from `BigramPureStopwordQuery`
        // because operators inspecting the ratio
        // `bigram_skips_pure_stopword / bigram_skips_no_cjk`
        // need to be able to tell "Latin query, lane correctly
        // declined" from "CJK query annihilated by over-
        // aggressive stopword inventory".
        crate::fts_telemetry::record_lane_skip(crate::fts_telemetry::SkipReason::BigramNoCjkQuery);
    }

    // Sort by best (smallest) rank ascending; truncate to limit.
    //
    // Deterministic tiebreaker on `EvidenceId` (`Uuid::Ord`) for
    // rows whose FTS5 ranks compare as equal. Without it, the
    // upstream `HashMap` iteration order is hash-randomised, so
    // tied ranks would produce a different `Vec` ordering on every
    // call — a test-stability hazard for any caller asserting on
    // result order. Ties are rare in FTS5 rank (BM25 ties require
    // identical term frequencies on identical-length documents) but
    // are reproducible enough to flake CI: two rows in the
    // `evidence_fts_cjk` branch sharing a trigram-windowed body of
    // the same length is the canonical case. The tiebreaker is
    // O(1) per comparison and `Uuid::Ord` is byte-lexicographic, so
    // the resulting order is also stable across process restarts
    // (UUIDs are persisted, hash seeds are not).
    //
    // The pinned result order ensures downstream tests and
    // any caller that does NOT re-score (e.g. the raw `search_fts`
    // public surface) sees identical output across runs for the
    // same input.
    let mut sorted: Vec<(EvidenceId, f64)> = best_rank.into_iter().collect();
    sorted.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    sorted.truncate(limit);
    Ok(sorted)
}

/// Insert `(id, rank)` into `best_rank`, keeping the **smallest**
/// (most relevant) of any existing value vs the new one. FTS5 rank
/// is negative-and-smaller-is-better, so `min` is the correct dedupe
/// rule when the same `evidence_id` is returned by both the
/// `unicode61` and `trigram` branches.
///
/// Uses [`f64::min`] rather than a raw `<` comparison so the
/// implementation actually behaves like the doc-comment "MIN(rank)"
/// contract on the IEEE 754 NaN edge case: `f64::min` returns the
/// non-NaN argument when exactly one operand is NaN (IEEE 754-2008
/// `minNum` / libm `fmin` semantics), so a hypothetically corrupted
/// `NaN` rank from one branch never displaces a real finite rank
/// from the other. With a raw `<` (`if rank < *existing { … }`) a
/// `NaN` incoming value left a real existing value alone (correct
/// by accident), but a `NaN` *existing* value could never be
/// overwritten by a real incoming value (wrong — the slot would
/// stick at `NaN` forever and drag the row to the sort comparator's
/// `Equal` bucket). FTS5's BM25 rank is always a finite negative
/// `f64` so this codepath is unreachable in production today, but
/// pinning the merge to `f64::min` removes the trap entirely and
/// aligns with the defensive `partial_cmp().unwrap_or(Equal)` used
/// in the sort comparator at `merged_fts_search`
/// — both halves of the merge pipeline now treat NaN identically
/// instead of skewing in opposite directions.
///
/// This is the long-form fix.
fn merge_min_rank(best_rank: &mut HashMap<EvidenceId, f64>, id: EvidenceId, rank: f64) {
    best_rank
        .entry(id)
        .and_modify(|existing| {
            *existing = (*existing).min(rank);
        })
        .or_insert(rank);
}

/// cached SQL string for the unicode61 lane's MATCH
/// query in [`merged_fts_search`]. The `bm25(<table>, w...)`
/// fragment is built once at first use via
/// [`crate::fts_weights::bm25_select_fragment`] so a future
/// column addition becomes an [`crate::fts_weights::EVIDENCE_FTS_COLUMN_WEIGHTS`]
/// length bump rather than an in-place SQL edit here.
///
/// The cache uses [`std::sync::OnceLock`] so the format cost is
/// paid exactly once per process \u2014 the read-hot path
/// ([`merged_fts_search`]) then borrows the cached `&'static str`
/// for every subsequent query.
fn unicode61_lane_sql() -> &'static str {
    static SQL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SQL.get_or_init(|| {
        format!(
            // `ORDER BY rank` (FTS5's built-in pseudo-column),
            // NOT `ORDER BY weighted_rank` — the former triggers
            // FTS5's documented incremental-rank optimisation
            // which retrieves rows in best-to-worst order without
            // computing bm25() for every matching document, so the
            // per-query cost stays O(LIMIT) instead of O(matches).
            // With today's all-1.0 `EVIDENCE_FTS_COLUMN_WEIGHTS`
            // the built-in `rank` and `bm25(evidence_fts, 1.0)`
            // compute identical f64 values, so sorting by `rank`
            // and reading the SELECT-list column `weighted_rank`
            // are numerically equivalent. When a future schema
            // tunes column weights off `1.0`, the FTS5 rank
            // configuration (`INSERT INTO evidence_fts(// evidence_fts, rank) VALUES('rank', 'bm25(w1, w2)')`)
            // re-configures the built-in `rank` to use the
            // matching weights and preserves the optimisation —
            // see `EVIDENCE_FTS_*_COLUMN_WEIGHTS` doc-comments.
            "SELECT evidence_id, {bm25} AS weighted_rank FROM evidence_fts \
             WHERE evidence_fts MATCH ?1 AND scope_id = ?2 \
             ORDER BY rank LIMIT ?3",
            bm25 = bm25_select_fragment("evidence_fts", EVIDENCE_FTS_COLUMN_WEIGHTS),
        )
    })
}

/// cached SQL string for the trigram lane's MATCH
/// query. Same shape as [`unicode61_lane_sql`] but against
/// `evidence_fts_cjk` and with the trigram-lane column-weight
/// vector. See [`unicode61_lane_sql`] for the caching rationale.
fn trigram_lane_sql() -> &'static str {
    static SQL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SQL.get_or_init(|| {
        format!(
            // See [`unicode61_lane_sql`] for the rationale on using
            // `ORDER BY rank` (FTS5 built-in) rather than
            // `ORDER BY weighted_rank` (the SELECT-list alias) —
            // it preserves FTS5's incremental-rank optimisation.
            "SELECT evidence_id, {bm25} AS weighted_rank FROM evidence_fts_cjk \
             WHERE evidence_fts_cjk MATCH ?1 AND scope_id = ?2 \
             ORDER BY rank LIMIT ?3",
            bm25 = bm25_select_fragment("evidence_fts_cjk", EVIDENCE_FTS_CJK_COLUMN_WEIGHTS),
        )
    })
}

/// cached SQL string for the bigram lane's MATCH
/// query. Same shape as [`unicode61_lane_sql`] but against
/// `evidence_fts_bigram` and with the bigram-lane column-weight
/// vector. See [`unicode61_lane_sql`] for the caching rationale.
fn bigram_lane_sql() -> &'static str {
    static SQL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SQL.get_or_init(|| {
        format!(
            // See [`unicode61_lane_sql`] for the rationale on using
            // `ORDER BY rank` (FTS5 built-in) rather than
            // `ORDER BY weighted_rank` (the SELECT-list alias) —
            // it preserves FTS5's incremental-rank optimisation.
            "SELECT evidence_id, {bm25} AS weighted_rank FROM evidence_fts_bigram \
             WHERE evidence_fts_bigram MATCH ?1 AND scope_id = ?2 \
             ORDER BY rank LIMIT ?3",
            bm25 = bm25_select_fragment("evidence_fts_bigram", EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS),
        )
    })
}

/// Convert a `COUNT(*) / SUM(...)` result from SQLite into a Rust
/// `usize`. Both functions are non-negative by definition; the
/// `.max(0)` guard handles a negative value defensively in case
/// schema corruption or a non-substrate writer produced one. On a
/// 32-bit target a count > `usize::MAX` saturates rather than
/// truncating.
fn i64_count_to_usize(n: i64) -> usize {
    usize::try_from(n.max(0)).unwrap_or(usize::MAX)
}

fn slice_to_uuid(bytes: &[u8]) -> Result<Uuid> {
    if bytes.len() != 16 {
        return Err(EvidenceError::Schema("UUID column has wrong width"));
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(bytes);
    Ok(Uuid::from_bytes(arr))
}

/// Derive the SQLCipher page-encryption key from `master_key` and
/// return it hex-encoded, ready to splice into a `PRAGMA key`/`rekey`
/// `x'…'` literal. The returned [`Zeroizing<String>`] wipes the hex on
/// drop so the page key never lingers in freed heap memory. This is
/// the single source of truth for the page-key derivation, shared by
/// [`EvidenceStore::open`] and [`EvidenceStore::rotate_master_key`].
fn page_key_hex(master_key: &MasterKey) -> Result<Zeroizing<String>> {
    let mut page_key = derive_key(master_key, b"sqlcipher:store:v1")?;
    let hex = Zeroizing::new(hex_encode(&page_key));
    page_key.zeroize();
    Ok(hex)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Lower-case hex; SQLCipher accepts X'...' literals in either case.
        s.push(HEX_CHARS[(b >> 4) as usize] as char);
        s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
    }
    s
}

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

const _: () = {
    // Compile-time guard: the master-key length matches what the
    // crypto crate exposes.
    let _ = [(); MASTER_KEY_LEN - 32];
};

#[cfg(test)]
mod merge_min_rank_tests {
    //! earlier regression — pin the IEEE 754 NaN behaviour
    //! of [`merge_min_rank`]. Production FTS5 BM25 rank is always a
    //! finite negative `f64`, so this codepath is unreachable today;
    //! the tests exist to guard against a future contributor "simplifying"
    //! the `f64::min` call back to a raw `if rank < *existing` (which
    //! breaks the "MIN(rank)" contract for the `(existing = NaN,
    //! incoming = finite)` case).
    use super::{merge_min_rank, EvidenceId};
    use std::collections::HashMap;
    use uuid::Uuid;
    fn id_for(byte: u8) -> EvidenceId {
        EvidenceId(Uuid::from_bytes([byte; 16]))
    }
    #[test]
    fn min_of_two_finite_ranks_keeps_smallest() {
        let id = id_for(0xa1);
        let mut map: HashMap<EvidenceId, f64> = HashMap::new();
        merge_min_rank(&mut map, id, -3.0);
        merge_min_rank(&mut map, id, -7.5);
        merge_min_rank(&mut map, id, -1.0);
        assert_eq!(map.get(&id), Some(&-7.5));
    }
    #[test]
    fn nan_incoming_never_displaces_finite_existing() {
        // The pre-fix raw-`<` impl was correct-by-accident in this
        // direction (`NaN < anything` is false), so we pin it
        // explicitly so the new `f64::min` impl keeps the same
        // behaviour after the refactor.
        let id = id_for(0xa2);
        let mut map: HashMap<EvidenceId, f64> = HashMap::new();
        merge_min_rank(&mut map, id, -2.0);
        merge_min_rank(&mut map, id, f64::NAN);
        assert_eq!(map.get(&id), Some(&-2.0));
    }
    #[test]
    fn finite_incoming_displaces_nan_existing() {
        // This is the case the pre-fix raw-`<` impl got wrong:
        // `anything < NaN` is false, so a NaN-stuck slot could
        // never recover. `f64::min` returns the non-NaN argument,
        // so the slot heals on the next finite insert.
        let id = id_for(0xa3);
        let mut map: HashMap<EvidenceId, f64> = HashMap::new();
        merge_min_rank(&mut map, id, f64::NAN);
        merge_min_rank(&mut map, id, -4.5);
        assert_eq!(map.get(&id), Some(&-4.5));
    }
    #[test]
    fn nan_only_inserts_leave_nan_in_slot() {
        // Documented behaviour: with no finite operand ever supplied
        // the slot stays NaN. This is unreachable in production but
        // pinning it keeps the contract explicit.
        let id = id_for(0xa4);
        let mut map: HashMap<EvidenceId, f64> = HashMap::new();
        merge_min_rank(&mut map, id, f64::NAN);
        merge_min_rank(&mut map, id, f64::NAN);
        assert!(map.get(&id).copied().unwrap().is_nan());
    }
    #[test]
    fn distinct_ids_do_not_interfere() {
        let mut map: HashMap<EvidenceId, f64> = HashMap::new();
        merge_min_rank(&mut map, id_for(0xb1), -1.0);
        merge_min_rank(&mut map, id_for(0xb2), -2.0);
        merge_min_rank(&mut map, id_for(0xb1), -0.5);
        assert_eq!(map.get(&id_for(0xb1)), Some(&-1.0));
        assert_eq!(map.get(&id_for(0xb2)), Some(&-2.0));
    }
}

#[cfg(test)]
mod lane_sql_tests {
    //! pin the exact SQL emitted by
    //! [`super::unicode61_lane_sql`] / [`super::trigram_lane_sql`]
    //! / [`super::bigram_lane_sql`] so a refactor of either
    //! [`crate::fts_weights::bm25_select_fragment`] or the SELECT
    //! template here cannot drift the shape that
    //! [`super::merged_fts_search`] depends on.
    //!
    //! Each test asserts the cached SQL contains both the explicit
    //! `bm25(<table>, <weights>...)` form (proves the column-
    //! weight integration point fired) AND the lane-specific
    //! `MATCH` predicate (proves the SELECT routed to the right
    //! table).

    use super::{bigram_lane_sql, trigram_lane_sql, unicode61_lane_sql};

    #[test]
    fn unicode61_lane_sql_contains_explicit_bm25_call_and_match_clause() {
        let sql = unicode61_lane_sql();
        assert!(
            sql.contains("bm25(evidence_fts, 1.0)"),
            "unicode61 lane SQL must invoke bm25() with explicit \
             column weights — got: {sql}"
        );
        assert!(
            sql.contains("evidence_fts MATCH ?1"),
            "unicode61 lane SQL must MATCH against `evidence_fts` — got: {sql}"
        );
        // The SELECT-list alias is `weighted_rank` so a future
        // multi-column tune (where `bm25(t, w1, w2)` diverges
        // from FTS5's built-in `rank`) keeps the SELECT column
        // name unambiguously bound to the weighted score. The
        // `ORDER BY` clause uses FTS5's built-in `rank` pseudo-
        // column — NOT the alias — because only `ORDER BY rank`
        // triggers FTS5's incremental-rank optimisation that
        // avoids computing bm25() for every matching row. With
        // today's all-1.0 column weights the two are numerically
        // identical; when column weights diverge the FTS5 rank
        // configuration is updated in lockstep to keep the
        // optimisation valid (see the `EVIDENCE_FTS_*_COLUMN_WEIGHTS`
        // doc-comments for the forward-compat protocol).
        assert!(
            sql.contains("AS weighted_rank"),
            "unicode61 lane SQL must alias the bm25() expression as \
             `weighted_rank` so the SELECT column name remains \
             unambiguous when column weights diverge from 1.0 — \
             got: {sql}"
        );
        assert!(
            sql.contains("ORDER BY rank LIMIT"),
            "unicode61 lane SQL must `ORDER BY rank` (FTS5's \
             built-in pseudo-column) to keep the incremental-rank \
             optimisation — got: {sql}"
        );
        assert!(
            !sql.contains("ORDER BY weighted_rank"),
            "unicode61 lane SQL must not ORDER BY the alias — \
             that disables FTS5's incremental-rank optimisation — \
             got: {sql}"
        );
    }

    #[test]
    fn trigram_lane_sql_contains_explicit_bm25_call_and_match_clause() {
        let sql = trigram_lane_sql();
        assert!(
            sql.contains("bm25(evidence_fts_cjk, 1.0)"),
            "trigram lane SQL must invoke bm25() with explicit \
             column weights — got: {sql}"
        );
        assert!(
            sql.contains("evidence_fts_cjk MATCH ?1"),
            "trigram lane SQL must MATCH against `evidence_fts_cjk` — got: {sql}"
        );
        assert!(
            sql.contains("AS weighted_rank") && sql.contains("ORDER BY rank LIMIT"),
            "trigram lane SQL must alias the bm25() column as \
             `weighted_rank` AND sort on FTS5's built-in `rank` \
             pseudo-column (preserves the incremental-rank \
             optimisation) — got: {sql}"
        );
    }

    #[test]
    fn bigram_lane_sql_contains_explicit_bm25_call_and_match_clause() {
        let sql = bigram_lane_sql();
        assert!(
            sql.contains("bm25(evidence_fts_bigram, 1.0)"),
            "bigram lane SQL must invoke bm25() with explicit \
             column weights — got: {sql}"
        );
        assert!(
            sql.contains("evidence_fts_bigram MATCH ?1"),
            "bigram lane SQL must MATCH against `evidence_fts_bigram` — got: {sql}"
        );
        assert!(
            sql.contains("AS weighted_rank") && sql.contains("ORDER BY rank LIMIT"),
            "bigram lane SQL must alias the bm25() column as \
             `weighted_rank` AND sort on FTS5's built-in `rank` \
             pseudo-column (preserves the incremental-rank \
             optimisation) — got: {sql}"
        );
    }

    #[test]
    fn lane_sql_helpers_are_idempotent_across_calls() {
        // the OnceLock caching contract — every call
        // returns the same `&'static str` pointer (not just an
        // equal string), so a future contributor cannot
        // accidentally swap in a `format!()` that pays the
        // allocation cost on every query.
        let a = unicode61_lane_sql();
        let b = unicode61_lane_sql();
        assert!(
            std::ptr::eq(a, b),
            "unicode61_lane_sql() must return the cached pointer \
             on every call (OnceLock caching invariant)"
        );
        let a = trigram_lane_sql();
        let b = trigram_lane_sql();
        assert!(
            std::ptr::eq(a, b),
            "trigram_lane_sql() must return the cached pointer \
             on every call (OnceLock caching invariant)"
        );
        let a = bigram_lane_sql();
        let b = bigram_lane_sql();
        assert!(
            std::ptr::eq(a, b),
            "bigram_lane_sql() must return the cached pointer \
             on every call (OnceLock caching invariant)"
        );
    }
}
