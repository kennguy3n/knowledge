//! Top-level [`EvidenceStore`] type — opens the SQLCipher database,
//! runs the schema, and exposes the append-only ingestion + read API.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;
use zeroize::Zeroize;

use crypto::{
    content_hash, decrypt_aead, derive_key, encrypt_aead, AeadKey, AeadNonce, ContentHash,
    MasterKey, AEAD_NONCE_LEN, MASTER_KEY_LEN,
};

use crate::error::{EvidenceError, Result};
use crate::ids::{EvidenceId, ScopeId};
use crate::importance::ImportanceClass;
use crate::routing::{route_storage_with_threshold, StoragePath, DEFAULT_INLINE_THRESHOLD_BYTES};
use crate::schema::{SCHEMA_SQL, SCHEMA_VERSION};

/// Default ring-buffer size cap (`PROPOSAL.md` §3.1, `ARCHITECTURE.md`
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
    /// master key + scope context label.
    scope_keys: std::collections::HashMap<ScopeId, AeadKey>,
    /// Master key — wiped on drop.
    master_key: MasterKey,
}

impl Drop for EvidenceStore {
    fn drop(&mut self) {
        self.master_key.zeroize();
        for (_id, key) in self.scope_keys.iter_mut() {
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
        let key_hex = hex_encode(&page_key);
        page_key.zeroize();

        // Apply SQLCipher PRAGMAs. `cipher_page_size = 4096` and
        // `kdf_iter = 256000` are the SQLCipher 4.x defaults; we set
        // them explicitly so the schema is portable across versions.
        conn.pragma_update(None, "key", format!("x'{key_hex}'"))?;
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

        // Run the schema bootstrap.
        conn.execute_batch(SCHEMA_SQL)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        let mut store = Self {
            conn,
            config,
            scope_keys: std::collections::HashMap::new(),
            master_key: *master_key,
        };
        // No-op for now, but keeps the borrow checker happy if we add
        // post-open prepared statements.
        store.preflight()?;
        Ok(store)
    }

    fn preflight(&mut self) -> Result<()> {
        let version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(SCHEMA_VERSION);
        if version != SCHEMA_VERSION {
            // Future migrations would run here. For Phase 0 the schema
            // is fresh; refuse to open against a future version.
            return Err(EvidenceError::Schema(
                "schema version mismatch — refusing to open",
            ));
        }
        Ok(())
    }

    /// Get-or-derive the AEAD key for the given scope.
    fn scope_key(&mut self, scope_id: ScopeId) -> Result<AeadKey> {
        if let Some(k) = self.scope_keys.get(&scope_id) {
            return Ok(*k);
        }
        let label = format!("scope:{}:body:v1", scope_id.as_uuid());
        let key = derive_key(&self.master_key, label.as_bytes())?;
        self.scope_keys.insert(scope_id, key);
        Ok(key)
    }

    /// Append-only ingest a fresh evidence row.
    ///
    /// Per `PROPOSAL.md` §3.1 / §4.3:
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
        let key = self.scope_key(scope_id)?;

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
        } else {
            let nonce = random_nonce();
            // AAD for body-table rows binds the content hash itself —
            // an attacker cannot relabel a body across scopes without
            // rewriting the cipher.
            let aad = body_table_aad(&hash);
            let ciphertext = encrypt_aead(&key, &nonce, body, &aad)?;
            tx.execute(
                "INSERT INTO body_store (content_hash, body, nonce, ref_count)
                 VALUES (?1, ?2, ?3, 1)",
                params![hash.as_slice(), ciphertext, nonce.as_slice()],
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
        // handles those in later phases.
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

    /// Read the plaintext body of an evidence row.
    pub fn read_body(&mut self, evidence_id: EvidenceId) -> Result<Vec<u8>> {
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
        let key = self.scope_key(scope_id)?;

        match path_tag {
            t if t == StoragePath::Inline as i64 => {
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
                let (ct, nonce_bytes): (Vec<u8>, Vec<u8>) = self
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
                if nonce_bytes.len() != AEAD_NONCE_LEN {
                    return Err(EvidenceError::Schema("body_store row has malformed nonce"));
                }
                let mut nonce = [0u8; AEAD_NONCE_LEN];
                nonce.copy_from_slice(&nonce_bytes);
                let aad = body_table_aad(&content_hash_arr);
                let pt = decrypt_aead(&key, &nonce, &ct, &aad)?;
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
        let now = Utc::now().timestamp_micros();

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
}

fn random_nonce() -> AeadNonce {
    use rand::RngCore;
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
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
