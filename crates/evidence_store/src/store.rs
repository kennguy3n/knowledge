//! Top-level [`EvidenceStore`] type — opens the SQLCipher database,
//! runs the schema, and exposes the append-only ingestion + read API.

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
use crate::ids::{EvidenceId, ScopeId};
use crate::importance::ImportanceClass;
use crate::routing::{route_storage_with_threshold, StoragePath, DEFAULT_INLINE_THRESHOLD_BYTES};
use crate::schema::{SCHEMA_SQL, SCHEMA_VERSION};

/// Default ring-buffer size cap (`docs/DESIGN.md` §3.1, `ARCHITECTURE.md`
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
    /// body at ingest time (schema v13, Phase 1.3). `None` when
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
    /// Per `ARCHITECTURE.md` §2.2, in a real deployment the master key
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
        // ARCHITECTURE.md §2.2.
        let mut page_key = derive_key(master_key, b"sqlcipher:store:v1")?;
        // `Zeroizing<String>` zeroes the heap-allocated bytes when
        // dropped — without this wrapper the hex-encoded SQLCipher
        // page key would linger in freed heap memory after `String`'s
        // default `Drop`. The same wrap is applied to the
        // `format!("x'…'")` SQL pragma value below.
        let key_hex: Zeroizing<String> = Zeroizing::new(hex_encode(&page_key));
        page_key.zeroize();

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

        // Schema migration. Read the existing `user_version` BEFORE
        // running the bootstrap SQL so we can detect three states:
        //
        //   * `user_version == 0`  → fresh database, run the full
        //                            bootstrap and stamp the version.
        //   * `user_version <  SCHEMA_VERSION` → legacy database; the
        //                            additive `CREATE * IF NOT EXISTS`
        //                            statements in [`SCHEMA_SQL`]
        //                            forward-port the schema, and any
        //                            version-specific deltas are
        //                            applied by [`apply_migration`].
        //   * `user_version >  SCHEMA_VERSION` → database written by a
        //                            newer build; refuse to open
        //                            rather than corrupt it.
        //
        // The previous implementation always wrote `SCHEMA_VERSION`
        // before `preflight()` read it back, which made the "refuse
        // to open against a future version" check tautological. This
        // structure puts the rejection ahead of any writes.
        let detected_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);
        if detected_version > SCHEMA_VERSION {
            return Err(EvidenceError::Schema(
                "evidence_store database was written by a newer schema version",
            ));
        }

        // Run the schema bootstrap. Every statement is
        // `CREATE * IF NOT EXISTS`, which makes it safe to re-run
        // against an already-initialised database — that is exactly
        // what the v1 → v2 upgrade relies on (adding
        // `evidence_embeddings` to an existing v1 store).
        conn.execute_batch(SCHEMA_SQL)?;

        // Per-version migration deltas. Additive bumps (v1, v2) are
        // already handled by the idempotent SCHEMA_SQL above and the
        // corresponding `apply_migration` arms are no-ops. Destructive
        // bumps (v3: widen `evidence_embeddings` PK from single to
        // composite) cannot be expressed with `CREATE * IF NOT EXISTS`
        // and must rewrite an existing table; they live in
        // `apply_migration`. Each migration delta is idempotent and
        // detects "already in target shape".
        //
        // For a fresh database (`detected_version == 0`) the
        // SCHEMA_SQL bootstrap above has already produced the current
        // schema directly, so every per-version delta would either be
        // an explicit no-op (v1, v2) or detect-and-skip (v3). Running
        // the loop in that case is harmless but pure overhead —
        // including an unnecessary `PRAGMA table_info` round-trip on
        // every open. Skip the loop entirely on the fresh-DB path and
        // only iterate when migrating an existing on-disk database
        // forward from an older `user_version`.
        //
        // The loop still exists so the `preflight()` invariant below
        // ("version is current") has teeth instead of being satisfied
        // by an unconditional write — every legacy database is walked
        // through every required delta.
        if detected_version > 0 {
            for v in (detected_version + 1)..=SCHEMA_VERSION {
                apply_migration(&conn, v)?;
            }
        }

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

        // v6 (C2): hydrate the in-memory scope-key cache from the
        // durable `scope_deks` table. Scopes registered after v6
        // have their DEKs stored wrapped here; loading them on open
        // means `scope_key()` finds the independently-generated key
        // in cache rather than falling back to HKDF derivation.
        {
            let deks = store.load_scope_deks()?;
            let mut cache = store.scope_keys.write().unwrap();
            for (scope, key) in deks {
                cache.insert(scope, key);
            }
        }

        // v4→v5 backfill: re-encrypt any pre-existing body-table rows
        // that were encrypted under the old scope-independent
        // body_store_key but have no per-scope CEK wraps yet.
        if detected_version > 0 && detected_version < 5 {
            store.backfill_legacy_body_wraps()?;
        }

        // v11→v12 (Phase 10 Item 6) — move every existing inline
        // approved-document payload ciphertext into the deduplicated
        // `body_store` table, then drop the legacy `nonce` + `payload`
        // columns from `approved_document_payloads`. This is
        // self-detecting and idempotent: on a v12-or-newer database
        // the legacy columns are already gone and the function is a
        // no-op. We trigger it whenever the detected version is
        // below 12 so a fresh v11 database that just stamped its
        // `user_version = 12` via the bootstrap path still goes
        // through the data-shape detection (defense-in-depth against
        // a future schema regression that recreates the legacy
        // columns).
        if detected_version > 0 && detected_version < 12 {
            store.migrate_approved_doc_payloads_to_body_store()?;
        }

        Ok(store)
    }

    /// Post-bootstrap sanity check: after [`Self::open`] runs the
    /// migration sequence the on-disk schema version must equal
    /// [`SCHEMA_VERSION`]. A mismatch here is a bug in the migration
    /// logic, not a user-recoverable condition.
    fn preflight(&mut self) -> Result<()> {
        let version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(EvidenceError::Schema(
                "post-migration user_version does not match SCHEMA_VERSION",
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
        let label = format!("scope:{}:body:v1", scope_id.as_uuid());
        let key = derive_key(&self.master_key, label.as_bytes())?;
        self.scope_keys.write().unwrap().insert(scope_id, key);
        Ok(key)
    }

    /// Append-only ingest a fresh evidence row.
    ///
    /// Per `docs/DESIGN.md` §3.1 / §4.3:
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
    /// the row's `language_tag` column (schema v13, Phase 1.3) with
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
        // Phase 1.2 / schema v14: rows whose body contains any CJK
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
            tx.execute(
                "INSERT INTO evidence_fts_cjk (content, evidence_id, scope_id) \
                 VALUES (?1, ?2, ?3)",
                params![text, evidence_id_bytes, scope_id_bytes],
            )?;
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
        // A model failure should not block ingestion — the row is
        // still recoverable via FTS and the retriever's re-embedding
        // fallback.
        let Ok(vec) = model.embed(text) else {
            return;
        };
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
            tracing::debug!(
                evidence_id = %evidence_id.as_uuid(),
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
                tracing::debug!(
                    evidence_id = %evidence_id.as_uuid(),
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
    /// Per Phase 1.2 / schema v14 the search fans out across **both**
    /// lexical indexes — `evidence_fts` (unicode61) for whitespace-
    /// segmented scripts and `evidence_fts_cjk` (trigram) for CJK
    /// Han / Hiragana / Katakana / Thai content — and de-duplicates
    /// on `evidence_id`, taking the best (smallest, since FTS5 rank
    /// is negative-and-smaller-is-better) of the two ranks.
    ///
    /// **Query-syntax compatibility, not equivalence.** Both branches
    /// accept the same FTS5 query *grammar* (the bareword / `"phrase"`
    /// / `term1 OR term2` / `NEAR(…)` / column-filter / prefix-star
    /// syntax described in <https://sqlite.org/fts5.html#full_text_query_syntax>).
    /// They differ in what terms each tokeniser is able to match:
    ///
    /// * `unicode61` (universal table) splits on Unicode whitespace
    ///   and punctuation and is happy with single-codepoint terms.
    ///   A query like `"to OR deadline"` is well-formed and may
    ///   match real rows.
    /// * `trigram` (CJK table) only stores overlapping 3-codepoint
    ///   windows of `content`, so any **individual** query term that
    ///   is fewer than 3 codepoints will simply never match a row in
    ///   that branch — it is silently and validly empty. This is
    ///   the documented Phase 1.2 floor and is what enables the
    ///   `天気` (2-codepoint) test case to round-trip as
    ///   `Ok(vec![])` instead of erroring (a custom FFI bigram
    ///   tokeniser is the future-phase fix). Compound queries
    ///   with at least one term ≥ 3 codepoints (e.g.
    ///   `"to OR 良い天気"`) match in `trigram` on the long term
    ///   and in `unicode61` on the short term; the UNION then
    ///   surfaces the row from whichever index found it.
    ///
    /// Either branch returning zero rows for queries it cannot serve
    /// (a < 3-codepoint term against trigram, or a pure-CJK term
    /// against unicode61 with no Latin context) is the expected
    /// non-error path, not a tokeniser mismatch.
    pub fn search_fts(
        &self,
        scope_id: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceId>> {
        let mut stmt = self.conn.prepare(
            "SELECT evidence_id, MIN(rank) AS best_rank FROM (
                 SELECT evidence_id, rank FROM evidence_fts
                  WHERE evidence_fts MATCH ?1 AND scope_id = ?2
                 UNION ALL
                 SELECT evidence_id, rank FROM evidence_fts_cjk
                  WHERE evidence_fts_cjk MATCH ?1 AND scope_id = ?2
             ) merged
             GROUP BY evidence_id
             ORDER BY best_rank
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                query,
                scope_id.as_uuid().as_bytes().as_slice(),
                clamp_limit_to_sqlite(limit),
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = row?;
            out.push(EvidenceId(slice_to_uuid(&bytes)?));
        }
        Ok(out)
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
            let rows_changed = tx.execute(
                "DELETE FROM ring_buffer WHERE id = (
                     SELECT id FROM ring_buffer ORDER BY created_at ASC, id ASC LIMIT 1
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
    /// 4. Fresh random DEK via `OsRng` — only for genuinely new
    ///    scopes with no prior evidence.
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
        // rand 0.9 made `OsRng` fallible-only (impls `TryRngCore`,
        // not `RngCore`). `TryRngCore::unwrap_err` produces
        // `UnwrapErr<OsRng>` which impls infallible `RngCore` by
        // panicking on OS RNG failure — the correct behavior for
        // DEK generation: a substrate that cannot draw entropy
        // cannot safely create new encrypted scopes, so panicking
        // surfaces the breakage rather than silently producing weak
        // keys. Called via UFCS to avoid a mid-function `use` that
        // clippy's `items-after-statements` lint would flag.
        rand::RngCore::fill_bytes(
            &mut rand::TryRngCore::unwrap_err(rand::rngs::OsRng),
            &mut dek,
        );
        self.store_scope_dek(scope_id, &dek)?;
        Ok(dek)
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

    /// Test-only helper that surgically reshapes
    /// `approved_document_payloads` back to its pre-v12 (Phase 8 /
    /// v10) inline layout and writes a single legacy-shape row.
    ///
    /// The post-bootstrap migration
    /// [`Self::migrate_approved_doc_payloads_to_body_store`] runs
    /// from [`Self::open`] on every reopen and is self-detecting via
    /// `PRAGMA table_info`, so this helper lets the
    /// `migration_v11_to_v12_round_trips_legacy_payloads` regression
    /// test plant a real pre-v12 row, close + reopen the store, and
    /// verify the migration moves the bytes through the body-store
    /// pipeline before dropping the legacy columns.
    ///
    /// Encrypts under the same scope DEK + AAD discipline the
    /// pre-v12 `save_approved_document_payload_in_tx` used, so the
    /// migration's `decrypt_aead` call sees an authentic ciphertext.
    ///
    /// Only available with the `test-support` feature (or in unit
    /// tests of this crate). Do not call from production code paths
    /// — the legacy table shape is unsupported under v12 and the
    /// next [`Self::open`] will silently re-migrate it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn write_legacy_approved_doc_payload_for_tests(
        &self,
        scope_id: ScopeId,
        document_id: uuid::Uuid,
        plaintext: &[u8],
        content_hash: &ContentHash,
    ) -> Result<()> {
        let scope_key = self.scope_key(scope_id)?;
        let aad = approved_doc_payload_aad(scope_id, document_id);
        let nonce = random_nonce();
        let ciphertext = encrypt_aead(&scope_key, &nonce, plaintext, &aad)?;

        // Reshape the table back to its pre-v12 layout. ALTER TABLE
        // ADD COLUMN tolerates re-adding a dropped column on a v12
        // database, and SQLCipher's transactional DDL keeps the
        // reshape atomic. Default expressions keep any existing v12
        // metadata-only rows valid for the duration of the test.
        self.conn.execute_batch(
            "ALTER TABLE approved_document_payloads \
                 ADD COLUMN nonce BLOB NOT NULL DEFAULT x'';\n\
             ALTER TABLE approved_document_payloads \
                 ADD COLUMN payload BLOB NOT NULL DEFAULT x'';",
        )?;

        let size_bytes = i64::try_from(plaintext.len()).unwrap_or(i64::MAX);
        let updated_at = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO approved_document_payloads \
             (scope_id, document_id, content_hash, size_bytes, updated_at, nonce, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                scope_id.as_uuid().as_bytes().as_slice(),
                document_id.as_bytes().as_slice(),
                content_hash.as_slice(),
                size_bytes,
                updated_at,
                nonce.as_slice(),
                ciphertext.as_slice(),
            ],
        )?;

        // Rewind `user_version` so the next `Self::open` sees a
        // pre-v12 database and runs the migration. Without this
        // step the migration is gated out (detected_version >= 12
        // means "already migrated") and the legacy row stays
        // unmoved.
        self.conn.pragma_update(None, "user_version", 11_i32)?;
        Ok(())
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
    /// previous blob (used by `sync_connector` Phase 3 to advance the
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
                    tracing::warn!(
                        instance_bytes_len = instance_bytes.len(),
                        error = %e,
                        "connector_instances row has malformed instance_id; skipping",
                    );
                    continue;
                }
            };
            let scope_id = match slice_to_uuid(&scope_bytes) {
                Ok(id) => ScopeId::from_uuid(id),
                Err(e) => {
                    tracing::warn!(
                        instance = %instance_id,
                        scope_bytes_len = scope_bytes.len(),
                        error = %e,
                        "connector_instances row has malformed scope_id; skipping",
                    );
                    continue;
                }
            };
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                tracing::warn!(
                    instance = %instance_id,
                    "connector_instances row has malformed nonce; skipping",
                );
                continue;
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&nonce_bytes);
            let key = match self.scope_key(scope_id) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(
                        instance = %instance_id,
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
                    tracing::warn!(
                        instance = %instance_id,
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
                    tracing::warn!(
                        instance_bytes_len = instance_bytes.len(),
                        error = %e,
                        "connector_tokens row has malformed instance_id; skipping",
                    );
                    continue;
                }
            };
            let scope_id = match slice_to_uuid(&scope_bytes) {
                Ok(id) => ScopeId::from_uuid(id),
                Err(e) => {
                    tracing::warn!(
                        instance = %instance_id,
                        scope_bytes_len = scope_bytes.len(),
                        error = %e,
                        "connector_tokens row has malformed scope_id; skipping",
                    );
                    continue;
                }
            };
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                tracing::warn!(
                    instance = %instance_id,
                    "connector_tokens row has malformed nonce; skipping",
                );
                continue;
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&nonce_bytes);
            let key = match self.scope_key(scope_id) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(
                        instance = %instance_id,
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
                    tracing::warn!(
                        instance = %instance_id,
                        scope = %scope_id.as_uuid(),
                        error = %e,
                        "connector_tokens row failed to decrypt; skipping",
                    );
                }
            }
        }
        Ok(out)
    }

    // ───────────── Approved-document payloads (v10 / Phase 8;
    //               v12 / Phase 10 Item 6: body-store dedup) ──────────
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
    // **Pre-v12 layout (legacy).** Rows used to carry inline
    // `nonce` + `payload` columns AEAD-encrypted directly under the
    // per-scope DEK with AAD binding (scope_id, document_id). That
    // layout still appears in v11 databases on disk; the v11 -> v12
    // migration (`migrate_approved_doc_payloads_to_body_store` in
    // this file, run as a post-bootstrap step from `open`) decrypts
    // every legacy row under
    // [`approved_doc_payload_aad`] and admits the plaintext through
    // the v12 body-store pipeline before dropping the inline
    // columns. The legacy AAD helper is retained for that migration
    // path only.
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
    /// As of v12 (Phase 10 Item 6) the plaintext is stored in the
    /// content-hash-deduplicated `body_store` table — admitting the
    /// same content into N tenant scopes costs one body row + N
    /// per-scope CEK wraps in `body_store_key_wraps` instead of N
    /// inline ciphertexts. The `approved_document_payloads` row
    /// itself is now metadata-only (content_hash + size_bytes +
    /// updated_at); it joins to the body via `content_hash`. See
    /// [`Self::admit_approved_doc_body_in_tx`] for the body-store
    /// admission logic and the schema-history note for v12 on
    /// [`crate::schema::SCHEMA_VERSION`] for the rationale.
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
    /// As of v12 (Phase 10 Item 6) the read path joins
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
                tracing::warn!(
                    scope_bytes_len = scope_bytes.len(),
                    "list_all_approved_document_payload_keys: skipping row with non-UUID scope_id; \
                     orphan sweep will leave this row untouched (manual purge required to recover)",
                );
                continue;
            };
            let Ok(doc_id) = uuid::Uuid::from_slice(&doc_bytes) else {
                tracing::warn!(
                    scope = %scope_id,
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

    // ────────── synthesis_object_versions (Phase 10 Item 4) ──────────
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
                tracing::warn!(
                    scope = %scope_id,
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
    /// * `evidence_embeddings` — cached `f32` vectors derived from
    ///   the plaintext body via an on-device embedding model. They
    ///   are not strictly plaintext but are still
    ///   semantically-derivable evidence and so must go.
    ///
    /// This method runs a single transaction:
    ///
    /// 1. Look up every `evidence_id` belonging to `scope_id`.
    /// 2. `DELETE FROM evidence_fts WHERE evidence_id IN (...)`
    ///    *and* `DELETE FROM evidence_fts_cjk WHERE evidence_id
    ///    IN (...)` — FTS5 supports `DELETE` on virtual tables
    ///    (they do NOT have the append-only trigger that protects
    ///    `evidence`). Both tables are deleted in the same
    ///    transaction so they can never drift apart.
    /// 3. `DELETE FROM evidence_embeddings WHERE evidence_id IN (...)`.
    /// 4. If — and only if — step 2 actually removed at least one
    ///    FTS row across either table, issue `INSERT INTO
    ///    evidence_fts(evidence_fts) VALUES('rebuild')` and
    ///    `INSERT INTO evidence_fts_cjk(evidence_fts_cjk)
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
        // Per Phase 1.2 / schema v14, `evidence_fts_cjk` (trigram-
        // tokenised CJK / Thai index) is purged alongside the
        // primary `evidence_fts` (unicode61) in the same
        // transaction so the two indexes can never drift apart
        // under crash-recovery, and so a forgotten scope leaves
        // zero plaintext tokens in either FTS shadow table after
        // the subsequent `REBUILD`. The returned count is the sum
        // across both tables — if either tokeniser still has rows
        // for the scope, the caller-side `if rows_deleted > 0`
        // gate still triggers a rebuild.
        let mut fts_rows_deleted: usize = 0;
        for chunk in evidence_ids.chunks(DELETE_BATCH) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let fts_sql = format!("DELETE FROM evidence_fts WHERE evidence_id IN ({placeholders})");
            let fts_cjk_sql =
                format!("DELETE FROM evidence_fts_cjk WHERE evidence_id IN ({placeholders})");
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
            tx.execute(&emb_sql, rusqlite::params_from_iter(params.iter().copied()))?;
        }
        Ok(fts_rows_deleted)
    }

    /// Issue the FTS5 `REBUILD` command on **both** lexical
    /// indexes — `evidence_fts` (unicode61) and `evidence_fts_cjk`
    /// (trigram, schema v14) — truncating their shadow tables
    /// (`%_data`, `%_idx`, `%_docsize`, …) and re-tokenising from
    /// the surviving content rows.
    ///
    /// `OPTIMIZE` only merges segments and can leave tokenised
    /// plaintext fragments behind in the `%_data` segment B-tree
    /// for rows that were `DELETE`'d in this same transaction.
    /// `REBUILD` re-tokenises from each table's stored `content`
    /// column — which now no longer references the purged scopes
    /// — so no residual plaintext tokens survive on disk for the
    /// forgotten scopes in either tokeniser's shadow store. Both
    /// rebuilds run inside the caller's transaction so the two
    /// tables are committed atomically.
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

    /// v4→v5 backfill: re-encrypt body-table rows that were written
    /// under the old scope-independent body_store_key.  For each
    /// orphaned content_hash (in body_store but not in
    /// body_store_key_wraps), derive the legacy key, decrypt, generate
    /// a fresh CEK, re-encrypt, update the row, then create a wrap
    /// for every scope that references the body in the evidence table.
    fn backfill_legacy_body_wraps(&mut self) -> Result<()> {
        let legacy_key = derive_key(&self.master_key, b"body_store:v1")?;

        // Find body_store rows that have zero wraps.
        let orphans: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = {
            let mut stmt = self.conn.prepare(
                "SELECT bs.content_hash, bs.body, bs.nonce \
                 FROM body_store bs \
                 WHERE NOT EXISTS ( \
                     SELECT 1 FROM body_store_key_wraps w \
                     WHERE w.content_hash = bs.content_hash \
                 )",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        if orphans.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        for (hash_bytes, ct, nonce_bytes) in &orphans {
            if nonce_bytes.len() != AEAD_NONCE_LEN {
                continue;
            }
            let mut body_nonce = [0u8; AEAD_NONCE_LEN];
            body_nonce.copy_from_slice(nonce_bytes);

            let mut content_hash = [0u8; 32];
            if hash_bytes.len() != 32 {
                continue;
            }
            content_hash.copy_from_slice(hash_bytes);

            // Decrypt under the legacy key.
            let aad = body_table_aad(&content_hash);
            let Ok(pt) = decrypt_aead(&legacy_key, &body_nonce, ct, &aad) else {
                continue; // already re-encrypted or corrupt
            };

            // Re-encrypt under a fresh CEK.
            let cek = random_cek();
            let new_nonce = random_nonce();
            let new_ct = encrypt_aead(&cek, &new_nonce, &pt, &aad)?;
            tx.execute(
                "UPDATE body_store SET body = ?1, nonce = ?2 WHERE content_hash = ?3",
                params![new_ct, new_nonce.as_slice(), hash_bytes.as_slice()],
            )?;

            // Create a CEK wrap for every scope that references this hash.
            let scope_ids: Vec<Vec<u8>> = {
                let mut s = tx.prepare(
                    "SELECT DISTINCT scope_id FROM evidence \
                     WHERE content_hash = ?1 AND storage_path = ?2",
                )?;
                let rows = s.query_map(
                    params![hash_bytes.as_slice(), StoragePath::BodyTable as i64],
                    |r| r.get::<_, Vec<u8>>(0),
                )?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                out
            };

            for scope_bytes in &scope_ids {
                let scope = ScopeId::from_uuid(match slice_to_uuid(scope_bytes) {
                    Ok(u) => u,
                    Err(_) => continue,
                });
                let scope_key = {
                    let label = format!("scope:{}:body:v1", scope.as_uuid());
                    derive_key(&self.master_key, label.as_bytes())?
                };
                let wrap_nonce = random_nonce();
                let wrapped = wrap_cek(&scope_key, &cek, &wrap_nonce, &content_hash)?;
                tx.execute(
                    "INSERT OR IGNORE INTO body_store_key_wraps \
                     (content_hash, scope_id, wrapped_cek, nonce) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        hash_bytes.as_slice(),
                        scope_bytes.as_slice(),
                        wrapped,
                        wrap_nonce.as_slice(),
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// v11→v12 migration (Phase 10 Item 6) — move every existing
    /// inline approved-document payload ciphertext into the
    /// deduplicated `body_store` table and drop the legacy `nonce` +
    /// `payload` columns from `approved_document_payloads`.
    ///
    /// Self-detecting + idempotent: inspects the live table shape
    /// via `PRAGMA table_info` and returns `Ok(())` immediately if
    /// the legacy columns are already gone (e.g. on a v12 fresh
    /// database). When the legacy columns exist:
    ///   1. Read every row's `(scope_id, document_id, nonce,
    ///      payload, content_hash)` tuple.
    ///   2. Decrypt the payload under the per-scope DEK with AAD
    ///      via [`approved_doc_payload_aad`].
    ///   3. Verify the decrypted plaintext hashes to the stored
    ///      `content_hash` (defensive; a corrupted row is logged
    ///      and skipped rather than aborting the whole migration —
    ///      the row will surface as an orphan at the next
    ///      `open_store` once the legacy columns are gone and the
    ///      metadata row points at nothing in `body_store`).
    ///   4. Admit the plaintext into `body_store` via
    ///      [`Self::admit_approved_doc_body_in_tx`] so the dedup
    ///      pipeline naturally collapses identical content across
    ///      scopes into one body row + N wraps.
    ///   5. `ALTER TABLE ... DROP COLUMN nonce / payload` to retire
    ///      the legacy columns.
    ///
    /// The whole thing runs inside one SQLCipher transaction so a
    /// crash mid-migration rolls everything back; the next
    /// `open_store` retries from the same legacy shape.
    fn migrate_approved_doc_payloads_to_body_store(&mut self) -> Result<()> {
        // Detect legacy shape via `PRAGMA table_info`. If the
        // `payload` column is missing, the table is already in v12
        // shape and there is nothing to do.
        let has_payload_column = {
            let mut stmt = self.conn.prepare(
                "SELECT 1 FROM pragma_table_info('approved_document_payloads') \
                 WHERE name = 'payload'",
            )?;
            stmt.query_row([], |_| Ok(())).optional()?.is_some()
        };
        if !has_payload_column {
            return Ok(());
        }

        // Read every legacy row up front so the migration tx does
        // not hold a long-lived statement open. `Vec<Vec<u8>>` is
        // intentional — the rows are about to be re-encrypted into
        // a new shape so we own the bytes from here on.
        let legacy_rows: Vec<LegacyRow> = {
            let mut stmt = self.conn.prepare(
                "SELECT scope_id, document_id, nonce, payload, content_hash \
                 FROM approved_document_payloads",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (scope_id_bytes, doc_id_bytes, nonce_bytes, ciphertext, content_hash_bytes) =
                    row?;
                let scope_id = ScopeId::from_uuid(slice_to_uuid(&scope_id_bytes)?);
                let document_id = slice_to_uuid(&doc_id_bytes)?;
                out.push(LegacyRow {
                    scope_id,
                    document_id,
                    nonce_bytes,
                    ciphertext,
                    content_hash_bytes,
                });
            }
            out
        };

        let tx = self.conn.unchecked_transaction()?;
        for row in &legacy_rows {
            if row.nonce_bytes.len() != AEAD_NONCE_LEN {
                tracing::warn!(
                    scope = %row.scope_id.as_uuid(),
                    document_id = %row.document_id,
                    "v11→v12 migration: approved_document_payloads row has malformed nonce; \
                     skipping (row will be visible as orphan metadata at next open_store)",
                );
                continue;
            }
            if row.content_hash_bytes.len() != crypto::CONTENT_HASH_LEN {
                tracing::warn!(
                    scope = %row.scope_id.as_uuid(),
                    document_id = %row.document_id,
                    "v11→v12 migration: approved_document_payloads row has malformed \
                     content_hash; skipping (row will be visible as orphan metadata at \
                     next open_store)",
                );
                continue;
            }
            let mut nonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&row.nonce_bytes);
            let mut stored_hash = [0u8; crypto::CONTENT_HASH_LEN];
            stored_hash.copy_from_slice(&row.content_hash_bytes);

            // Decrypt under the legacy per-scope DEK + AAD.
            let scope_key = self.scope_key(row.scope_id)?;
            let aad = approved_doc_payload_aad(row.scope_id, row.document_id);
            let plaintext = match decrypt_aead(&scope_key, &nonce, &row.ciphertext, &aad) {
                Ok(pt) => pt,
                Err(e) => {
                    tracing::warn!(
                        scope = %row.scope_id.as_uuid(),
                        document_id = %row.document_id,
                        error = %e,
                        "v11→v12 migration: approved_document_payloads row failed to \
                         decrypt; skipping (row will be visible as orphan metadata at \
                         next open_store)",
                    );
                    continue;
                }
            };

            // Defensive content_hash recheck: a row whose stored
            // content_hash does not match its decrypted plaintext
            // would silently corrupt the body_store dedup index.
            // Recompute and verify before admitting.
            let computed = content_hash(&plaintext);
            if computed != stored_hash {
                tracing::warn!(
                    scope = %row.scope_id.as_uuid(),
                    document_id = %row.document_id,
                    "v11→v12 migration: approved_document_payloads row has stored content_hash \
                     that does not match the decrypted plaintext; skipping (row will be \
                     visible as orphan metadata at next open_store)",
                );
                continue;
            }

            // Admit through the v12 body-store pipeline. Dedup is
            // automatic: identical content across scopes collapses
            // to one body row + per-scope wraps.
            self.admit_approved_doc_body_in_tx(&tx, row.scope_id, &plaintext, &stored_hash)?;
        }

        // Retire the legacy inline columns. `ALTER TABLE ... DROP
        // COLUMN` is supported on SQLite 3.35.0+ and SQLCipher
        // builds against modern SQLite; both are required by the
        // substrate so a downlevel SQLCipher would fail open_store
        // earlier on a SCHEMA mismatch.
        tx.execute(
            "ALTER TABLE approved_document_payloads DROP COLUMN payload",
            [],
        )?;
        tx.execute(
            "ALTER TABLE approved_document_payloads DROP COLUMN nonce",
            [],
        )?;
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
        self.embedding_model = Some(Box::new(model));
        self.embedding_model_tag = model_tag.into();
        self
    }

    /// Same as [`Self::with_embedding_model`] but takes `&mut self`
    /// for callers that already own a `&mut` handle to the store.
    pub fn set_embedding_model<M: EmbeddingModel + 'static>(
        &mut self,
        model: M,
        model_tag: impl Into<String>,
    ) {
        self.embedding_model = Some(Box::new(model));
        self.embedding_model_tag = model_tag.into();
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
    /// Under the v3 composite primary key (`evidence_id`, `model_tag`)
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

/// Apply the version-`target`-specific migration delta against `conn`.
///
/// `SCHEMA_SQL` carries every additive change (tables, indexes,
/// triggers, FTS virtual tables) and is re-run idempotently on every
/// open, so additive bumps need no work here. This function exists for
/// migrations that cannot be expressed as `CREATE * IF NOT EXISTS` —
/// e.g. dropping or renaming a column, changing a column's storage
/// type, or back-filling derived data from existing rows.
///
/// Each delta MUST be idempotent: when an in-loop migration runs over
/// a database whose `SCHEMA_SQL` bootstrap already produced the
/// target shape (the fresh-DB case), it must detect "already there"
/// and return `Ok(())` without doing destructive work.
fn apply_migration(conn: &Connection, target: i32) -> Result<()> {
    // Each schema version gets its own arm even when several share an
    // `Ok(())` body. The per-version comment documents *why* the
    // migration is a no-op (purely additive `CREATE TABLE IF NOT
    // EXISTS` handled by `SCHEMA_SQL`, or genuinely empty). Collapsing
    // the no-op arms into a single `_ => Ok(())` would lose that
    // per-version provenance, which matters when auditing migrations
    // across releases.
    #[allow(clippy::match_same_arms)]
    match target {
        // v1: initial schema; nothing to do (handled by SCHEMA_SQL).
        // v2: add `evidence_embeddings`; handled by SCHEMA_SQL.
        1 | 2 => Ok(()),
        // v3: widen `evidence_embeddings` PK from single column
        // (`evidence_id`) to composite (`evidence_id`, `model_tag`).
        // See `migrate_evidence_embeddings_to_composite_pk` for the
        // shape-detection + table-swap logic.
        3 => migrate_evidence_embeddings_to_composite_pk(conn),
        // v4: add `forgotten_scopes`. Purely
        // additive; the idempotent `CREATE TABLE IF NOT EXISTS` in
        // `SCHEMA_SQL` handles both the fresh-DB and forward-port
        // paths, so this arm is a no-op.
        4 => Ok(()),
        // v5 (WS1): add `body_store_key_wraps`. Purely additive;
        // handled by SCHEMA_SQL's `CREATE TABLE IF NOT EXISTS`.
        5 => Ok(()),
        // v6 (C2): add `scope_deks`. Purely additive; handled by
        // SCHEMA_SQL's `CREATE TABLE IF NOT EXISTS`.
        6 => Ok(()),
        // v7 (C10): add `memory_objects`. Purely additive; handled
        // by SCHEMA_SQL's `CREATE TABLE IF NOT EXISTS`.
        7 => Ok(()),
        // v8: add `epoch_tombstones`. Purely additive; handled
        // by SCHEMA_SQL's `CREATE TABLE IF NOT EXISTS`.
        8 => Ok(()),
        // v9 (Phase 3): add `connector_instances` + `connector_tokens`.
        // Purely additive; handled by SCHEMA_SQL's
        // `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS`
        // (the unique index on `(scope_id, kind)` is part of that
        // bootstrap, so a fresh-DB open and a v8→v9 upgrade both end
        // up with the same shape).
        9 => Ok(()),
        // v10 (Phase 8): add `approved_document_payloads`. Purely
        // additive; handled by SCHEMA_SQL's
        // `CREATE TABLE IF NOT EXISTS`. No separate covering index
        // is created: the composite PK `(scope_id, document_id)`
        // already serves the `WHERE scope_id = ?` listing query via
        // SQLite's PK index, so an additional index would be pure
        // write/disk overhead with no read-side benefit.
        // Pre-v10 databases simply do not have any approved-document
        // payload rows yet, which matches the "tenant memory carries
        // refs but the substrate never persisted payloads" state
        // that Phase 7 shipped.
        10 => Ok(()),
        // v11 (Phase 10 Item 4): add `synthesis_object_versions`
        // and the supplemental `idx_synthesis_object_versions_scope`
        // index. Purely additive; both are handled by SCHEMA_SQL's
        // `CREATE TABLE / INDEX IF NOT EXISTS` so a v10 -> v11
        // upgrade and a fresh-DB open end up with the same shape.
        // Pre-v11 databases have no replay history rows yet, which
        // matches the pre-Item-4 contract where every synthesis
        // output overwrote the prior one with no recoverable trail.
        11 => Ok(()),
        // v12 (Phase 10 Item 6): destructive shape change to
        // `approved_document_payloads` — drop the inline `nonce` +
        // `payload` columns and route the bytes through the
        // deduplicated `body_store` table. The actual data move +
        // ALTER TABLE DROP COLUMN run in a post-bootstrap step
        // (`migrate_approved_doc_payloads_to_body_store` in
        // `store.rs`) called from `Self::open` once the scope-DEK
        // cache has been hydrated; that step is self-detecting and
        // idempotent. This arm is intentionally a no-op so the
        // bootstrap loop simply walks past v12 — the destructive
        // work lives where the scope keys are available.
        //
        // Pre-v12 databases on a fresh `open_store` still have the
        // legacy `nonce` + `payload` columns because
        // `CREATE TABLE IF NOT EXISTS` cannot retract them; the
        // post-bootstrap step is what actually moves them off.
        // A v12 fresh database (one whose `user_version` was set
        // to 12 by the bootstrap path before any data was written)
        // skips the post-bootstrap step because the legacy columns
        // never exist.
        12 => Ok(()),
        // v13 (Phase 1.3 — multilingual ingestion): add the
        // optional `language_tag` column to the `evidence` table
        // so the BCP-47 primary subtag detected on the row's
        // plaintext body at ingest time can flow through to the
        // multilingual lexicon registry and per-locale FTS5
        // tokenizer without re-running detection on the read
        // side. The column is nullable and has no NOT NULL or
        // DEFAULT constraint, so the `ALTER TABLE ADD COLUMN`
        // is non-destructive for existing rows (they retroactively
        // read as `NULL`) and SQLite executes it without
        // rewriting the table. A fresh v13 database picks the
        // column up from `SCHEMA_SQL`'s `CREATE TABLE IF NOT
        // EXISTS`; only the v12 -> v13 upgrade path needs the
        // explicit ALTER.
        13 => migrate_v13_add_evidence_language_tag(conn),
        // v14 (Phase 1.2 — CJK-aware FTS5 tokeniser): add the
        // `evidence_fts_cjk` virtual table and backfill it from the
        // pre-existing `evidence_fts.content` rows whose plaintext
        // body contains any CJK Han / Hiragana / Katakana / Thai
        // codepoint. The `CREATE VIRTUAL TABLE IF NOT EXISTS` lives
        // in SCHEMA_SQL so a fresh v14 database picks the table up
        // directly; a v13 -> v14 upgrade hits the same statement
        // (no-op) and then walks the backfill below.
        //
        // Backfill is gated on `evidence_fts_cjk` being empty so
        // re-running the migration on an already-populated v14
        // database is a no-op rather than producing duplicate rows.
        14 => migrate_v14_backfill_evidence_fts_cjk(conn),
        _ => Err(EvidenceError::Schema(
            "no migration registered for the requested schema version",
        )),
    }
}

/// v12 -> v13 additive migration: add the `language_tag` column to
/// the `evidence` table.
///
/// Idempotent: pre-checks `PRAGMA table_info(evidence)` so a
/// re-applied migration (e.g. on a fresh v13 database whose schema
/// already includes the column via `SCHEMA_SQL`) is a no-op rather
/// than a `duplicate column name` error.
fn migrate_v13_add_evidence_language_tag(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(evidence)")?;
    let mut rows = stmt.query([])?;
    let mut has_column = false;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == "language_tag" {
            has_column = true;
            break;
        }
    }
    drop(rows);
    drop(stmt);
    if !has_column {
        conn.execute("ALTER TABLE evidence ADD COLUMN language_tag TEXT", [])?;
    }
    Ok(())
}

/// v13 -> v14 additive migration: backfill `evidence_fts_cjk` from
/// pre-existing `evidence_fts.content` rows whose body contains any
/// CJK Han / Hiragana / Katakana / Thai codepoint.
///
/// The `evidence_fts_cjk` virtual table itself is created by
/// `SCHEMA_SQL`'s `CREATE VIRTUAL TABLE IF NOT EXISTS` so this
/// function does not need to issue the DDL — `Self::open` runs
/// `SCHEMA_SQL` before walking the migration ladder, so by the time
/// we are called the table exists (possibly empty for a v13 -> v14
/// upgrade, possibly already populated for a fresh v14 database).
///
/// Idempotency: the function first checks whether
/// `evidence_fts_cjk` already has any rows. If it does we return
/// without doing any work — the table is either freshly populated
/// (fresh v14 open) or the migration has already run successfully
/// against this database (re-applied v13 -> v14 upgrade after a
/// crash before the `user_version` write hit disk). The check is
/// O(1) at the SQLite level because FTS5 maintains row-count
/// metadata in `evidence_fts_cjk_docsize`.
///
/// Per-row routing matches the write path
/// ([`EvidenceStore::index_fts`]): a body is backfilled into
/// `evidence_fts_cjk` iff `script::contains_cjk_or_thai` returns
/// true for its plaintext content. The pre-existing
/// `evidence_fts` rows themselves are untouched.
///
/// Crash-safety: the backfill runs inside an explicit
/// `unchecked_transaction` so partial progress is rolled back on
/// crash. The `user_version` write that records "migration v14
/// applied" lives in [`EvidenceStore::open`] *after* the
/// `apply_migration` loop completes, so a crash mid-backfill leaves
/// the database at `user_version = 13` and the next open re-walks
/// this function from scratch over an empty `evidence_fts_cjk`. The
/// idempotency check ("already have rows in evidence_fts_cjk?") is
/// what makes a successful re-walk on an already-migrated database
/// — for example after a crash *between* the backfill commit and
/// the `user_version` write — a single O(1) `COUNT(*)` rather than
/// a duplicate-row producer.
///
/// We use `unchecked_transaction` because the migration entry
/// point [`apply_migration`] receives `&Connection` (not `&mut`),
/// matching the contract of the sibling migrations
/// ([`migrate_v13_add_evidence_language_tag`],
/// [`migrate_evidence_embeddings_to_composite_pk`]).
fn migrate_v14_backfill_evidence_fts_cjk(conn: &Connection) -> Result<()> {
    let existing_cjk_rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |row| {
            row.get(0)
        })?;
    if existing_cjk_rows > 0 {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
        let mut stmt = tx.prepare("SELECT content, evidence_id, scope_id FROM evidence_fts")?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        mapped
    };
    for (content, evidence_id, scope_id) in rows {
        if !crate::script::contains_cjk_or_thai(&content) {
            continue;
        }
        tx.execute(
            "INSERT INTO evidence_fts_cjk (content, evidence_id, scope_id) \
             VALUES (?1, ?2, ?3)",
            params![content, evidence_id, scope_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// v2 -> v3 destructive migration for `evidence_embeddings`.
///
/// SQLite cannot change a table's primary key in place — the only
/// supported recipe (`https://sqlite.org/lang_altertable.html`
/// §7.2) is to create a new table with the desired shape, copy rows
/// into it, drop the old table, and rename the new one. This function
/// implements that recipe wrapped in an `unchecked_transaction` so the
/// whole rewrite is atomic (a crash mid-migration leaves the old v2
/// table intact and the next open retries from `detected_version=2`).
///
/// Idempotency: the function first inspects the live table's primary
/// key via `PRAGMA table_info`. When it already has the v3 composite
/// shape (two columns with `pk > 0`) the function returns `Ok(())`
/// without doing any work. This is what makes the migration safe to
/// re-run over a fresh database whose `SCHEMA_SQL` bootstrap already
/// produced the v3 shape directly.
fn migrate_evidence_embeddings_to_composite_pk(conn: &Connection) -> Result<()> {
    // `PRAGMA table_info(name)` returns one row per column. The `pk`
    // column is 0 for non-PK columns and 1..=N for PK columns in
    // declaration order. Counting non-zero `pk` values gives the PK
    // arity — 1 means the legacy single-PK shape, 2 means the v3
    // composite shape, 0 means the table is missing entirely (which
    // should be impossible after SCHEMA_SQL has run but we handle it
    // defensively).
    let mut stmt = conn.prepare("PRAGMA table_info(evidence_embeddings)")?;
    let mut pk_arity: i32 = 0;
    {
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            // Column 5 of `PRAGMA table_info` is `pk`.
            let pk: i32 = row.get(5)?;
            if pk > 0 {
                pk_arity += 1;
            }
        }
    }
    drop(stmt);

    match pk_arity {
        2 => {
            // Already v3 shape (fresh DB whose SCHEMA_SQL produced
            // the composite PK directly). Nothing to do.
            Ok(())
        }
        0 => Err(EvidenceError::Schema(
            "v3 migration: evidence_embeddings table is missing after schema bootstrap",
        )),
        1 => {
            // Legacy single-PK v2 shape. Rewrite the table atomically:
            //   1. Create `evidence_embeddings_v3` with the composite
            //      PK directly (no `IF NOT EXISTS` — the table must
            //      not exist before this point; if it does we have a
            //      half-applied migration from a previous crash and
            //      bailing out is safer than blindly overwriting it).
            //   2. Copy every row across. With the old PK every
            //      `evidence_id` appears at most once, so the copy
            //      cannot violate the new composite PK uniqueness
            //      constraint.
            //   3. Drop the old table and rename the new one in place.
            //
            // All inside `unchecked_transaction` so a crash anywhere
            // in the sequence rolls back to the v2 shape.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE evidence_embeddings_v3 (
                     evidence_id     BLOB    NOT NULL,
                     embedding       BLOB    NOT NULL,
                     model_tag       TEXT    NOT NULL,
                     created_at      INTEGER NOT NULL,
                     PRIMARY KEY (evidence_id, model_tag)
                 );
                 INSERT INTO evidence_embeddings_v3
                     (evidence_id, embedding, model_tag, created_at)
                 SELECT evidence_id, embedding, model_tag, created_at
                 FROM evidence_embeddings;
                 DROP TABLE evidence_embeddings;
                 ALTER TABLE evidence_embeddings_v3
                     RENAME TO evidence_embeddings;",
            )?;
            tx.commit()?;
            Ok(())
        }
        _ => Err(EvidenceError::Schema(
            "v3 migration: evidence_embeddings has an unexpected primary key arity",
        )),
    }
}

fn random_nonce() -> AeadNonce {
    use rand::rngs::OsRng;
    use rand::{RngCore, TryRngCore};
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    // See SECURITY.md §"Random number generation" for why the
    // substrate uses `OsRng` (not `ThreadRng`) for every per-row
    // AEAD nonce. Panicking on OS RNG failure is intentional — a
    // substrate that cannot draw entropy cannot encrypt safely.
    OsRng.unwrap_err().fill_bytes(&mut nonce);
    nonce
}

fn random_cek() -> AeadKey {
    use rand::rngs::OsRng;
    // `TryRngCore` is needed because rand 0.9 made `OsRng` fallible.
    // The `.unwrap_err()` adapter restores the infallible `RngCore`
    // surface that this CEK generator depends on (panics on OS RNG
    // failure, which is the correct behavior — a substrate that
    // cannot draw entropy cannot wrap content safely).
    use rand::{RngCore, TryRngCore};
    let mut key = [0u8; AEAD_KEY_LEN];
    OsRng.unwrap_err().fill_bytes(&mut key);
    key
}

/// Wrap (encrypt) a CEK under `wrapper_key` with a freshly drawn
/// nonce.  AAD binds the content hash so a wrap cannot be re-labelled
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

fn approved_doc_payload_aad(scope_id: ScopeId, document_id: uuid::Uuid) -> Vec<u8> {
    let prefix = b"approved-doc-payload:v1:";
    let mut aad = Vec::with_capacity(prefix.len() + 16 + 16);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad.extend_from_slice(document_id.as_bytes());
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

/// Convert a `COUNT(*) / SUM(...)` result from SQLite into a Rust
/// `usize`. Both functions are non-negative by definition; the
/// `.max(0)` guard handles a negative value defensively in case
/// schema corruption or a non-substrate writer produced one. On a
/// 32-bit target a count > `usize::MAX` saturates rather than
/// truncating.
fn i64_count_to_usize(n: i64) -> usize {
    usize::try_from(n.max(0)).unwrap_or(usize::MAX)
}

/// Row shape read from a pre-v12 `approved_document_payloads`
/// table during the v11→v12 migration. Lives at module scope so
/// `migrate_approved_doc_payloads_to_body_store` can keep its body
/// flat without tripping clippy's `items_after_statements` lint.
struct LegacyRow {
    scope_id: ScopeId,
    document_id: Uuid,
    nonce_bytes: Vec<u8>,
    ciphertext: Vec<u8>,
    content_hash_bytes: Vec<u8>,
}

fn slice_to_uuid(bytes: &[u8]) -> Result<Uuid> {
    if bytes.len() != 16 {
        return Err(EvidenceError::Schema("UUID column has wrong width"));
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(bytes);
    Ok(Uuid::from_bytes(arr))
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
