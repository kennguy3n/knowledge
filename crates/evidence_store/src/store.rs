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
            StoragePath::Inline => self.ingest_inline(scope_id, body, source_ref, importance, hash),
            StoragePath::BodyTable => {
                self.ingest_body_table(scope_id, body, source_ref, importance, hash)
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
              source_ref, acl_pointer, importance, storage_path, created_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, ?7, ?8, ?9)",
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
              source_ref, acl_pointer, importance, storage_path, created_at)
             VALUES (?1, ?2, ?3, NULL, ?4, NULL, ?5, NULL, ?6, ?7, ?8)",
            params![
                evidence_id.as_uuid().as_bytes().as_slice(),
                scope_id.as_uuid().as_bytes().as_slice(),
                hash.as_slice(),
                hash.as_slice(),
                source_ref,
                importance.as_tag(),
                path_tag,
                now,
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
        if let Ok(text) = std::str::from_utf8(body) {
            tx.execute(
                "INSERT INTO evidence_fts (content, evidence_id, scope_id) VALUES (?1, ?2, ?3)",
                params![
                    text,
                    evidence_id.as_uuid().as_bytes().as_slice(),
                    scope_id.as_uuid().as_bytes().as_slice(),
                ],
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
                        storage_path, created_at
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
                    ))
                },
            )
            .optional()?;

        let Some((scope_bytes, hash_bytes, source_ref, acl_pointer, imp_tag, path_tag, created)) =
            row
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
        }))
    }

    /// Run an FTS5 search scoped to `scope_id`.
    ///
    /// The query is passed straight through to FTS5; callers should
    /// pre-process per FTS5's syntax (e.g. quote phrases). The result
    /// is the matching evidence ids ordered by FTS5 rank.
    pub fn search_fts(
        &self,
        scope_id: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceId>> {
        let mut stmt = self.conn.prepare(
            "SELECT evidence_id FROM evidence_fts
             WHERE evidence_fts MATCH ?1 AND scope_id = ?2
             ORDER BY rank
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                query,
                scope_id.as_uuid().as_bytes().as_slice(),
                limit as i64,
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
        let payload_size = (ciphertext.len() + nonce.len()) as i64;
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

        // Evict oldest until we fit within the cap.
        let cap = self.config.ring_buffer_max_bytes as i64;
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
        Ok(total.max(0) as usize)
    }

    /// Return the number of ring-buffer entries (across all scopes).
    pub fn ring_buffer_len(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM ring_buffer", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
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
        Ok(n.max(0) as usize)
    }

    /// Number of distinct body-table rows. Useful in tests of dedup.
    pub fn body_store_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM body_store", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
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
        self.conn.execute(
            "INSERT OR IGNORE INTO forgotten_scopes (scope_id, forgotten_at) VALUES (?1, ?2)",
            params![scope_id.as_uuid().as_bytes().as_slice(), now],
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

    /// Return a snapshot of the in-memory scope-key cache. Used by
    /// `open_store` to populate the `DekRegistry` without a second
    /// DB round-trip.
    pub fn cached_scope_keys(&self) -> std::collections::HashMap<ScopeId, AeadKey> {
        self.scope_keys.read().unwrap().clone()
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
        use rand::RngCore;
        let mut dek = [0u8; AEAD_KEY_LEN];
        rand::rngs::OsRng.fill_bytes(&mut dek);
        self.store_scope_dek(scope_id, &dek)?;
        Ok(dek)
    }

    // ─────────────── Memory-object persistence (C10) ───────────────

    /// Persist a serializable memory object (user or channel) for
    /// `scope_id`. The `kind` tag discriminates between different
    /// memory types ("user_memory" / "channel_memory"). The object
    /// is JSON-serialized and AEAD-encrypted under the scope key.
    ///
    /// Upserts: calling this with the same `(scope_id, kind)` pair
    /// overwrites the previous blob.
    pub fn save_memory_blob(
        &self,
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
        self.conn.execute(
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
    /// * `evidence_embeddings` — cached `f32` vectors derived from
    ///   the plaintext body via an on-device embedding model. They
    ///   are not strictly plaintext but are still
    ///   semantically-derivable evidence and so must go.
    ///
    /// This method runs a single transaction:
    ///
    /// 1. Look up every `evidence_id` belonging to `scope_id`.
    /// 2. `DELETE FROM evidence_fts WHERE evidence_id IN (...)` —
    ///    FTS5 supports `DELETE` on the virtual table (it does NOT
    ///    have the append-only trigger that protects `evidence`).
    /// 3. `DELETE FROM evidence_embeddings WHERE evidence_id IN (...)`.
    ///
    /// The `evidence` rows themselves are intentionally left in
    /// place — the append-only trigger forbids removing them, and
    /// without the scope DEK the encrypted bodies in `body_store`
    /// / inline `evidence.body` are unrecoverable anyway. Hosts
    /// that need to drop the physical bytes must perform a
    /// VACUUM-style rebuild at a higher layer.
    pub fn purge_fts_for_scope(&mut self, scope_id: ScopeId) -> Result<()> {
        let tx = self.conn.transaction()?;
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
        const BATCH: usize = 256;
        for chunk in evidence_ids.chunks(BATCH) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let fts_sql = format!("DELETE FROM evidence_fts WHERE evidence_id IN ({placeholders})");
            let emb_sql =
                format!("DELETE FROM evidence_embeddings WHERE evidence_id IN ({placeholders})");
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            tx.execute(&fts_sql, rusqlite::params_from_iter(params.iter().copied()))?;
            tx.execute(&emb_sql, rusqlite::params_from_iter(params.iter().copied()))?;
        }

        // Force FTS5 to rebuild its shadow tables from the surviving
        // content rows. `OPTIMIZE` only merges segments and can leave
        // tokenised plaintext fragments behind in the `%_data`
        // segment B-tree for rows that were `DELETE`'d in this same
        // transaction. `REBUILD` truncates the shadow tables
        // (`%_data`, `%_idx`, `%_docsize`, …) and re-tokenises from
        // the FTS table's stored `content` column, which now no
        // longer references the purged scope — so no residual
        // plaintext tokens survive on disk for the forgotten scope.
        //
        // This is the strongest in-engine guarantee SQLite FTS5
        // exposes; the alternative would be a full `VACUUM` at a
        // higher layer, which is owned by the host.
        tx.execute(
            "INSERT INTO evidence_fts(evidence_fts) VALUES('rebuild')",
            [],
        )?;

        tx.commit()?;
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
        const BATCH: usize = 256;
        for chunk in hashes.chunks(BATCH) {
            for h in chunk {
                let remaining: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM body_store_key_wraps WHERE content_hash = ?1",
                        params![h.as_slice()],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if remaining == 0 {
                    // No scope can decrypt this body any more — drop it.
                    tx.execute(
                        "DELETE FROM body_store WHERE content_hash = ?1",
                        params![h.as_slice()],
                    )?;
                }
            }
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
        _ => Err(EvidenceError::Schema(
            "no migration registered for the requested schema version",
        )),
    }
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
    use rand::RngCore;
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

fn random_cek() -> AeadKey {
    use rand::rngs::OsRng;
    use rand::RngCore;
    let mut key = [0u8; AEAD_KEY_LEN];
    OsRng.fill_bytes(&mut key);
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

fn scope_dek_aad(scope_id: ScopeId) -> Vec<u8> {
    // b"scope-dek-wrap:v1" = 17 bytes + UUID = 16 bytes = 33 total.
    let mut aad = Vec::with_capacity(17 + 16);
    aad.extend_from_slice(b"scope-dek-wrap:v1");
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad
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
