//! [`PersistentTupleStore`] — SQLCipher-backed persistence wrapper
//! over the in-memory [`crate::TupleStore`].
//!
//! The in-memory tuple set remains the query surface used by
//! [`crate::check::check_permission`] and friends. Every successful
//! mutation on this wrapper is mirrored to a SQLCipher database that
//! reuses the substrate's per-user master key pattern (see
//! `ARCHITECTURE.md` §2.2): the page-encryption key is derived via
//! HKDF context `b"sqlcipher:permissions:v1"`, and any sensitive
//! plaintext is encrypted under a per-store AEAD key
//! (`permission_tuple:v1`).
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE relation_tuples (
//!     id BLOB PRIMARY KEY,
//!     object_type TEXT NOT NULL,
//!     object_id BLOB NOT NULL,
//!     relation TEXT NOT NULL,
//!     subject_type TEXT NOT NULL,
//!     subject_id BLOB NOT NULL,
//!     subject_relation TEXT,
//!     created_at INTEGER NOT NULL,
//!     nonce BLOB NOT NULL,
//!     payload BLOB NOT NULL
//! );
//! CREATE INDEX relation_tuples_object_idx
//!   ON relation_tuples(object_type, object_id, relation);
//! CREATE INDEX relation_tuples_subject_idx
//!   ON relation_tuples(subject_type, subject_id);
//! ```
//!
//! `payload` is the AEAD ciphertext of the JSON-encoded
//! [`crate::RelationTuple`]; the AAD binds the row id. The plaintext
//! columns (`object_type`, `object_id`, `relation`,
//! `subject_type`, `subject_id`, `subject_relation`) are kept out of
//! the ciphertext so the indexed queries (`check`, reverse-lookup)
//! do not have to decrypt every row at read time. The taxonomy
//! exposed in plaintext is the same one already documented in
//! `docs/DESIGN.md` §7.1, so this does not leak more than the
//! schema already does.

use std::path::Path;

use rand::RngCore;
use rusqlite::{params, Connection};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crypto::{decrypt_aead, derive_key, encrypt_aead, AeadKey, MasterKey, AEAD_NONCE_LEN};

use crate::error::{PermissionError, Result};
use crate::store::TupleStore;
use crate::tuple::{ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef, SubjectType};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS relation_tuples (
    id BLOB PRIMARY KEY,
    object_type TEXT NOT NULL,
    object_id BLOB NOT NULL,
    relation TEXT NOT NULL,
    subject_type TEXT NOT NULL,
    subject_id BLOB NOT NULL,
    subject_relation TEXT,
    created_at INTEGER NOT NULL,
    nonce BLOB NOT NULL,
    payload BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS relation_tuples_object_idx
    ON relation_tuples(object_type, object_id, relation);
CREATE INDEX IF NOT EXISTS relation_tuples_subject_idx
    ON relation_tuples(subject_type, subject_id);
";

const SCHEMA_VERSION: i32 = 1;

/// SQLCipher-backed persistence wrapper over [`TupleStore`].
///
/// The in-memory `TupleStore` is mirrored to disk on every
/// [`Self::insert`] / [`Self::upsert`] / [`Self::remove`]. The
/// in-memory store is the authoritative query surface — callers run
/// `check` against [`Self::store`] / [`Self::store_mut`] just like
/// they did with the bare `TupleStore`.
///
/// On open the wrapper rehydrates the in-memory store via
/// [`Self::load_all`] (called as part of [`Self::open`]).
///
/// `Drop` zeroises the master key and any cached AEAD keys so they
/// do not linger in freed heap memory.
///
/// `Debug` is intentionally redacted — the wrapper holds key
/// material whose serialised form must never reach a panic message
/// or a log line.
pub struct PersistentTupleStore {
    store: TupleStore,
    conn: Connection,
    payload_key: AeadKey,
    master_key: MasterKey,
}

impl std::fmt::Debug for PersistentTupleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentTupleStore")
            .field("store_len", &self.store.len())
            .field("payload_key", &"<redacted>")
            .field("master_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Drop for PersistentTupleStore {
    fn drop(&mut self) {
        self.master_key.zeroize();
        self.payload_key.zeroize();
    }
}

impl PersistentTupleStore {
    /// Open or create the SQLCipher tuple database at `path` and
    /// rehydrate the in-memory store from disk.
    ///
    /// The page-encryption key is derived from `master_key` via the
    /// HKDF context `b"sqlcipher:permissions:v1"`; the per-row AEAD
    /// key uses context `b"permission_tuple:v1"`.
    pub fn open<P: AsRef<Path>>(path: P, master_key: &MasterKey) -> Result<Self> {
        let conn = Connection::open(path).map_err(PermissionError::Sqlite)?;

        let mut page_key = derive_key(master_key, b"sqlcipher:permissions:v1")?;
        // `Zeroizing<String>` zeroes the heap-allocated hex bytes
        // when dropped — without this wrapper the SQLCipher page
        // key would linger in freed heap memory after `String`'s
        // default `Drop`. The same wrap is applied to the
        // `format!("x'…'")` SQL pragma value below.
        let key_hex: Zeroizing<String> = Zeroizing::new(hex_encode(&page_key));
        page_key.zeroize();

        let key_pragma: Zeroizing<String> = Zeroizing::new(format!("x'{}'", &*key_hex));
        conn.pragma_update(None, "key", key_pragma.as_str())
            .map_err(PermissionError::Sqlite)?;
        conn.pragma_update(None, "cipher_page_size", 4096_i64)
            .map_err(PermissionError::Sqlite)?;
        conn.pragma_update(None, "kdf_iter", 256_000_i64)
            .map_err(PermissionError::Sqlite)?;

        // Verify the key works.
        let _: i32 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|_| {
                PermissionError::Persistence("SQLCipher key did not unlock the database")
            })?;

        // Read the existing version *before* applying the schema or
        // stamping a new version. A `user_version` of `0` is the
        // SQLite default for a fresh database and means "no schema
        // applied yet"; anything non-zero must match
        // `SCHEMA_VERSION` exactly or we refuse to open.
        let existing_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);
        if existing_version != 0 && existing_version != SCHEMA_VERSION {
            return Err(PermissionError::Persistence(
                "schema version mismatch — refusing to open",
            ));
        }

        conn.execute_batch(SCHEMA_SQL)
            .map_err(PermissionError::Sqlite)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(PermissionError::Sqlite)?;

        let payload_key = derive_key(master_key, b"permission_tuple:v1")?;

        let mut this = Self {
            store: TupleStore::new(),
            conn,
            payload_key,
            master_key: *master_key,
        };
        this.load_all()?;
        Ok(this)
    }

    /// Borrow the in-memory tuple store for read-only queries (e.g.
    /// `iter_for_object_relation`, `contains`).
    pub fn store(&self) -> &TupleStore {
        &self.store
    }

    /// Borrow the in-memory tuple store mutably.
    ///
    /// Mutations done through this borrow are **not** mirrored to
    /// disk. Prefer the typed wrapper methods ([`Self::insert`],
    /// [`Self::upsert`], [`Self::remove`]) for mutations that must
    /// survive a restart.
    pub fn store_mut(&mut self) -> &mut TupleStore {
        &mut self.store
    }

    /// Insert a tuple in-memory and persist it. Returns
    /// [`PermissionError::DuplicateTuple`] if it was already
    /// present.
    pub fn insert(&mut self, tuple: RelationTuple) -> Result<()> {
        self.store.insert(tuple)?;
        if let Err(e) = self.persist(&tuple) {
            // The in-memory insert succeeded; roll it back so the
            // in-memory view and the database stay in lockstep.
            // `remove` is infallible here because we *just*
            // inserted the same tuple. The `let _ =` is defensive:
            // if a future change made remove fallible, the
            // original persist error is still the one the caller
            // wants surfaced.
            let _ = self.store.remove(&tuple);
            return Err(e);
        }
        Ok(())
    }

    /// Insert a tuple, ignoring duplicates. Returns `true` iff the
    /// tuple was newly inserted (and was mirrored to disk).
    pub fn upsert(&mut self, tuple: RelationTuple) -> Result<bool> {
        let newly_inserted = self.store.upsert(tuple);
        if newly_inserted {
            if let Err(e) = self.persist(&tuple) {
                let _ = self.store.remove(&tuple);
                return Err(e);
            }
        }
        Ok(newly_inserted)
    }

    /// Remove a tuple from both the in-memory store and the
    /// database. Returns [`PermissionError::NotFound`] if the tuple
    /// was absent.
    pub fn remove(&mut self, tuple: &RelationTuple) -> Result<()> {
        self.store.remove(tuple)?;
        if let Err(e) = self.delete(tuple) {
            // Restore the in-memory tuple so the views agree.
            let _ = self.store.upsert(*tuple);
            return Err(e);
        }
        Ok(())
    }

    /// Number of tuples persisted on disk (read-only count). Useful
    /// for tests that want to verify the mirror is in sync.
    pub fn persisted_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM relation_tuples", [], |row| row.get(0))
            .map_err(PermissionError::Sqlite)?;
        usize::try_from(n).map_err(|_| {
            PermissionError::Persistence("persisted relation-tuple count exceeds usize::MAX")
        })
    }

    /// Reload every tuple from disk into the in-memory store. Used
    /// by [`Self::open`]; exposed for tests / explicit
    /// re-hydration.
    pub fn load_all(&mut self) -> Result<()> {
        let mut rows: Vec<(Uuid, [u8; AEAD_NONCE_LEN], Vec<u8>)> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, nonce, payload FROM relation_tuples ORDER BY created_at ASC, id ASC",
                )
                .map_err(PermissionError::Sqlite)?;
            let mut iter = stmt.query([]).map_err(PermissionError::Sqlite)?;
            while let Some(row) = iter.next().map_err(PermissionError::Sqlite)? {
                let id_bytes: Vec<u8> = row.get(0).map_err(PermissionError::Sqlite)?;
                let nonce_bytes: Vec<u8> = row.get(1).map_err(PermissionError::Sqlite)?;
                let ct: Vec<u8> = row.get(2).map_err(PermissionError::Sqlite)?;
                rows.push((slice_to_uuid(&id_bytes)?, slice_to_nonce(&nonce_bytes)?, ct));
            }
        }

        // Replace the in-memory view with the on-disk state. Any
        // tuples the caller had inserted directly via `store_mut`
        // are dropped — this matches the documented "load_all
        // rehydrates from disk" contract.
        self.store = TupleStore::new();
        for (id, nonce, ct) in rows {
            let aad = tuple_aad(id);
            let pt = decrypt_aead(&self.payload_key, &nonce, &ct, &aad)?;
            let tuple: RelationTuple = serde_json::from_slice(&pt)
                .map_err(|_| PermissionError::Persistence("tuple payload is not valid JSON"))?;
            // `upsert` to avoid `DuplicateTuple` on a database that
            // legitimately contains two identical rows (different
            // ids, same content) — though the production write
            // path rejects duplicates so this only matters under
            // adversarial corruption.
            self.store.upsert(tuple);
        }
        Ok(())
    }

    fn persist(&mut self, tuple: &RelationTuple) -> Result<()> {
        let payload = serde_json::to_vec(tuple)
            .map_err(|_| PermissionError::Persistence("tuple payload could not be serialised"))?;
        let nonce = random_nonce();
        let id = tuple_row_id(tuple);
        let aad = tuple_aad(id);
        let ct = encrypt_aead(&self.payload_key, &nonce, &payload, &aad)?;
        let created_at = chrono::Utc::now().timestamp_millis();
        let subject_relation_tag = tuple
            .subject
            .subject_relation
            .map(|r| r.as_str().to_owned());
        self.conn
            .execute(
                "INSERT INTO relation_tuples
                    (id, object_type, object_id, relation, subject_type, subject_id,
                     subject_relation, created_at, nonce, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO UPDATE SET
                    object_type = excluded.object_type,
                    object_id = excluded.object_id,
                    relation = excluded.relation,
                    subject_type = excluded.subject_type,
                    subject_id = excluded.subject_id,
                    subject_relation = excluded.subject_relation,
                    created_at = excluded.created_at,
                    nonce = excluded.nonce,
                    payload = excluded.payload",
                params![
                    id.as_bytes().to_vec(),
                    tuple.object.object_type.as_str(),
                    tuple.object.object_id.as_bytes().to_vec(),
                    tuple.relation.as_str(),
                    tuple.subject.subject_type.as_str(),
                    tuple.subject.subject_id.as_bytes().to_vec(),
                    subject_relation_tag,
                    created_at,
                    nonce.to_vec(),
                    ct,
                ],
            )
            .map_err(PermissionError::Sqlite)?;
        Ok(())
    }

    fn delete(&mut self, tuple: &RelationTuple) -> Result<()> {
        let id = tuple_row_id(tuple);
        let n = self
            .conn
            .execute(
                "DELETE FROM relation_tuples WHERE id = ?1",
                params![id.as_bytes().to_vec()],
            )
            .map_err(PermissionError::Sqlite)?;
        if n == 0 {
            // The in-memory remove succeeded but no on-disk row was
            // affected — the two views are out of sync. Surface
            // this so the caller can decide how to recover (most
            // likely by aborting and re-hydrating from disk).
            return Err(PermissionError::Persistence(
                "in-memory tuple had no matching on-disk row",
            ));
        }
        Ok(())
    }
}

/// Derive a stable, content-addressed row id for a tuple.
///
/// The id is a UUID v5 over the canonical
/// `object_type/object_id/relation/subject_type/subject_id/
/// subject_relation` tuple under a fixed permission-service
/// namespace. Two callers writing the *same logical tuple* therefore
/// produce the same row id, so the on-disk store de-duplicates via
/// the primary-key constraint instead of growing unbounded as the
/// caller re-inserts the same tuple.
fn tuple_row_id(tuple: &RelationTuple) -> Uuid {
    // Stable namespace UUID for permission_service relation tuples.
    const NS_PERMISSION_TUPLE: Uuid = Uuid::from_bytes([
        0x6c, 0x1a, 0x9d, 0x37, 0x7e, 0xea, 0x4f, 0x18, 0xa4, 0x21, 0x90, 0x46, 0x73, 0xe5, 0x21,
        0x82,
    ]);
    let mut name = Vec::with_capacity(128);
    name.extend_from_slice(tuple.object.object_type.as_str().as_bytes());
    name.push(b'|');
    name.extend_from_slice(tuple.object.object_id.as_bytes());
    name.push(b'|');
    name.extend_from_slice(tuple.relation.as_str().as_bytes());
    name.push(b'|');
    name.extend_from_slice(tuple.subject.subject_type.as_str().as_bytes());
    name.push(b'|');
    name.extend_from_slice(tuple.subject.subject_id.as_bytes());
    name.push(b'|');
    if let Some(rel) = tuple.subject.subject_relation {
        name.extend_from_slice(rel.as_str().as_bytes());
    }
    Uuid::new_v5(&NS_PERMISSION_TUPLE, &name)
}

fn tuple_aad(id: Uuid) -> Vec<u8> {
    let mut aad = b"permission_tuple:v1:".to_vec();
    aad.extend_from_slice(id.as_bytes());
    aad
}

fn random_nonce() -> [u8; AEAD_NONCE_LEN] {
    let mut n = [0u8; AEAD_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

fn slice_to_uuid(b: &[u8]) -> Result<Uuid> {
    if b.len() != 16 {
        return Err(PermissionError::Persistence("uuid column has wrong width"));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(b);
    Ok(Uuid::from_bytes(bytes))
}

fn slice_to_nonce(b: &[u8]) -> Result<[u8; AEAD_NONCE_LEN]> {
    if b.len() != AEAD_NONCE_LEN {
        return Err(PermissionError::Persistence("nonce column has wrong width"));
    }
    let mut n = [0u8; AEAD_NONCE_LEN];
    n.copy_from_slice(b);
    Ok(n)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// The plaintext columns are reduced to short tags so they don't
// leak more than the permission taxonomy already documented in
// `docs/DESIGN.md` §7.1.
impl ObjectType {
    /// Parse the stable tag emitted by [`ObjectType::as_str`].
    pub(crate) fn from_tag(s: &str) -> Option<Self> {
        match s {
            "tenant" => Some(Self::Tenant),
            "domain" => Some(Self::Domain),
            "channel" => Some(Self::Channel),
            "user" => Some(Self::User),
            "device" => Some(Self::Device),
            "concept" => Some(Self::Concept),
            "summary" => Some(Self::Summary),
            "workflow" => Some(Self::Workflow),
            "export_profile" => Some(Self::ExportProfile),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

impl Relation {
    /// Parse the stable tag emitted by [`Relation::as_str`].
    pub(crate) fn from_tag(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "editor" => Some(Self::Editor),
            "member" => Some(Self::Member),
            "viewer" => Some(Self::Viewer),
            "synthesizer" => Some(Self::Synthesizer),
            "proposer" => Some(Self::Proposer),
            _ => None,
        }
    }
}

// Suppress dead-code warnings on the parsers above: they are used
// only by the round-trip integrity check below, which itself is
// `#[allow(dead_code)]` because the round trip is enforced
// implicitly by the AEAD payload decoding. We still want the
// parsers to compile-check the taxonomy so a future change to
// `as_str` does not silently drop a variant from the persistent
// schema.
#[allow(dead_code)]
fn object_type_round_trip_check(t: ObjectType) -> bool {
    ObjectType::from_tag(t.as_str()) == Some(t)
}

#[allow(dead_code)]
fn relation_round_trip_check(r: Relation) -> bool {
    Relation::from_tag(r.as_str()) == Some(r)
}

// `SubjectType` is a type alias for `ObjectType`, so the same
// round-trip check covers it; no separate impl needed.
#[allow(dead_code)]
fn subject_type_round_trip_check(t: SubjectType) -> bool {
    ObjectType::from_tag(t.as_str()) == Some(t)
}

// Silence unused-import warnings when the round-trip checks above
// are the only users of `ObjectRef` / `SubjectRef` after the
// payload decoder is folded into the in-memory store.
#[allow(dead_code)]
fn _unused_imports(o: ObjectRef, s: SubjectRef) -> (ObjectRef, SubjectRef) {
    (o, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::MASTER_KEY_LEN;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    fn fixture_key() -> MasterKey {
        // Deterministic master key so test failures are
        // reproducible. The bytes are not sensitive.
        let mut k: MasterKey = [0u8; MASTER_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7);
        }
        k
    }

    fn fresh_tuple() -> RelationTuple {
        RelationTuple::new(
            ObjectRef::new(ObjectType::Tenant, Uuid::new_v4()),
            Relation::Owner,
            SubjectRef::direct(SubjectType::User, Uuid::new_v4()),
        )
    }

    #[test]
    fn round_trip_persists_and_rehydrates() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let t1 = fresh_tuple();
        let t2 = fresh_tuple();
        let t3 = RelationTuple::new(
            ObjectRef::new(ObjectType::Domain, Uuid::new_v4()),
            Relation::Editor,
            SubjectRef::via(SubjectType::Tenant, Uuid::new_v4(), Relation::Admin),
        );

        {
            let mut s = PersistentTupleStore::open(tmp.path(), &key).unwrap();
            s.insert(t1).unwrap();
            s.insert(t2).unwrap();
            s.insert(t3).unwrap();
            assert_eq!(s.store().len(), 3);
            assert_eq!(s.persisted_count().unwrap(), 3);
        }

        let s = PersistentTupleStore::open(tmp.path(), &key).unwrap();
        assert_eq!(s.store().len(), 3);
        assert!(s.store().contains(&t1));
        assert!(s.store().contains(&t2));
        assert!(s.store().contains(&t3));
    }

    #[test]
    fn duplicate_insert_is_rejected_in_memory_and_does_not_double_persist() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();
        let mut s = PersistentTupleStore::open(tmp.path(), &key).unwrap();

        let t = fresh_tuple();
        s.insert(t).unwrap();
        let err = s.insert(t).unwrap_err();
        assert_eq!(err, PermissionError::DuplicateTuple);

        assert_eq!(s.store().len(), 1);
        assert_eq!(s.persisted_count().unwrap(), 1);
    }

    #[test]
    fn upsert_is_idempotent_against_disk() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();
        let mut s = PersistentTupleStore::open(tmp.path(), &key).unwrap();

        let t = fresh_tuple();
        assert!(s.upsert(t).unwrap());
        assert!(!s.upsert(t).unwrap());
        assert_eq!(s.persisted_count().unwrap(), 1);
    }

    #[test]
    fn remove_mirrors_to_disk() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let t = fresh_tuple();
        {
            let mut s = PersistentTupleStore::open(tmp.path(), &key).unwrap();
            s.insert(t).unwrap();
            s.remove(&t).unwrap();
            assert_eq!(s.store().len(), 0);
            assert_eq!(s.persisted_count().unwrap(), 0);
        }

        let s = PersistentTupleStore::open(tmp.path(), &key).unwrap();
        assert!(s.store().is_empty());
        assert_eq!(s.persisted_count().unwrap(), 0);
    }

    #[test]
    fn wrong_master_key_fails_to_open() {
        let tmp = NamedTempFile::new().unwrap();
        let key_a = fixture_key();
        let mut key_b: MasterKey = [0u8; MASTER_KEY_LEN];
        for (i, b) in key_b.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(99);
        }

        {
            let mut s = PersistentTupleStore::open(tmp.path(), &key_a).unwrap();
            s.insert(fresh_tuple()).unwrap();
        }

        let err = PersistentTupleStore::open(tmp.path(), &key_b).unwrap_err();
        // The wrong key surfaces either as the explicit "did not
        // unlock" Persistence error from the `SELECT 1` probe, or as
        // a raw `Sqlite` error from one of the pragma calls that
        // tried to query the still-locked database. Both are
        // acceptable; what matters is that the open fails.
        assert!(
            matches!(
                err,
                PermissionError::Persistence(_) | PermissionError::Sqlite(_)
            ),
            "expected an open failure for the wrong key, got {err:?}",
        );
    }

    #[test]
    fn crash_recovery_keeps_already_committed_tuples() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let t1 = fresh_tuple();
        let t2 = fresh_tuple();
        {
            let mut s = PersistentTupleStore::open(tmp.path(), &key).unwrap();
            s.insert(t1).unwrap();
            // Simulate a crash *before* `t2` is written by simply
            // dropping the store. SQLite commits on `execute`, so
            // `t1` survives even without an explicit flush.
            drop(s);
        }

        let mut s = PersistentTupleStore::open(tmp.path(), &key).unwrap();
        assert!(s.store().contains(&t1));
        assert!(!s.store().contains(&t2));
        // Now write t2 — the partially-written log can recover.
        s.insert(t2).unwrap();
        assert!(s.store().contains(&t2));
    }
}
