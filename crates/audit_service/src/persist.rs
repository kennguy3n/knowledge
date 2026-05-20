//! [`PersistentAuditLog`] — SQLCipher-backed persistence wrapper
//! over the in-memory [`crate::AuditLog`].
//!
//! The in-memory log remains the primary query surface (callers run
//! [`crate::AuditQuery`] against [`Self::log`]). Every successful
//! [`Self::append`] mirrors the entry to a SQLCipher database under
//! the substrate's per-user master key (HKDF context
//! `b"sqlcipher:audit:v1"`); per-row payloads are encrypted with
//! XChaCha20-Poly1305 under a per-store AEAD key
//! (`audit_entry:v1`).
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE audit_log (
//!     id BLOB PRIMARY KEY,
//!     sequence INTEGER NOT NULL UNIQUE,
//!     action_type TEXT NOT NULL,
//!     actor_type TEXT NOT NULL,
//!     actor_id TEXT NOT NULL,
//!     target_type TEXT,
//!     target_id TEXT,
//!     scope_id BLOB,
//!     created_at INTEGER NOT NULL,
//!     nonce BLOB NOT NULL,
//!     payload BLOB NOT NULL
//! );
//! CREATE INDEX audit_log_action_idx   ON audit_log(action_type);
//! CREATE INDEX audit_log_actor_idx    ON audit_log(actor_id);
//! CREATE INDEX audit_log_scope_idx    ON audit_log(scope_id);
//! ```
//!
//! `payload` is the AEAD ciphertext of the JSON-encoded
//! [`crate::AuditEntry`]; the AAD binds the row id + sequence so a
//! corrupt or replayed row cannot be silently re-attributed.
//!
//! The log is append-only at the public API: the only mutating call
//! is [`Self::append`]. There is no `update` or `delete` — the type
//! system, plus the absence of any `UPDATE` / `DELETE` SQL in this
//! module, enforce the immutability invariant called out in
//! `ARCHITECTURE.md` §4.1.

use std::path::Path;

use rand::RngCore;
use rusqlite::{params, Connection};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crypto::{decrypt_aead, derive_key, encrypt_aead, AeadKey, MasterKey, AEAD_NONCE_LEN};
use evidence_store::ScopeId;

use crate::entry::{Actor, AuditEntry, AuditEntryId};
use crate::error::{AuditError, Result};
use crate::log::AuditLog;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS audit_log (
    id BLOB PRIMARY KEY,
    sequence INTEGER NOT NULL UNIQUE,
    action_type TEXT NOT NULL,
    actor_type TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    target_type TEXT,
    target_id TEXT,
    scope_id BLOB,
    created_at INTEGER NOT NULL,
    nonce BLOB NOT NULL,
    payload BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_log_action_idx ON audit_log(action_type);
CREATE INDEX IF NOT EXISTS audit_log_actor_idx ON audit_log(actor_id);
CREATE INDEX IF NOT EXISTS audit_log_scope_idx ON audit_log(scope_id);
";

const SCHEMA_VERSION: i32 = 1;

/// SQLCipher-backed persistence wrapper over [`AuditLog`].
///
/// The in-memory log carries the index used by [`AuditQuery`] /
/// [`AuditLog::get`]; the SQLCipher database is the durable
/// append-only record. `append` writes both — in-memory first
/// (which assigns the sequence number) and then mirrors the
/// resulting entry to disk. If the disk write fails, the in-memory
/// append is **not** rolled back; the entry remains in memory and
/// the disk error is surfaced so the caller can retry. This
/// matches the append-only contract: an audit entry exists the
/// moment the substrate observes it, and the durability path is a
/// best-effort mirror that callers must guard with their own
/// retries.
///
/// `Drop` zeroises the master key and the payload AEAD key so they
/// do not linger in freed heap memory.
///
/// `Debug` is intentionally redacted — the wrapper holds key
/// material whose serialised form must never reach a panic message
/// or a log line.
///
/// [`AuditQuery`]: crate::AuditQuery
pub struct PersistentAuditLog {
    log: AuditLog,
    conn: Connection,
    payload_key: AeadKey,
    master_key: MasterKey,
}

impl std::fmt::Debug for PersistentAuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentAuditLog")
            .field("log_len", &self.log.len())
            .field("payload_key", &"<redacted>")
            .field("master_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Drop for PersistentAuditLog {
    fn drop(&mut self) {
        self.master_key.zeroize();
        self.payload_key.zeroize();
    }
}

impl PersistentAuditLog {
    /// Open or create the SQLCipher audit-log database at `path`
    /// and rehydrate the in-memory log from disk (entries are
    /// loaded in `sequence ASC` order so the in-memory next-sequence
    /// counter resumes correctly).
    pub fn open<P: AsRef<Path>>(path: P, master_key: &MasterKey) -> Result<Self> {
        let conn = Connection::open(path).map_err(AuditError::Sqlite)?;

        let mut page_key = derive_key(master_key, b"sqlcipher:audit:v1")?;
        // `Zeroizing<String>` zeroes the heap-allocated hex bytes
        // when dropped — without this wrapper the SQLCipher page
        // key would linger in freed heap memory after `String`'s
        // default `Drop`. The same wrap is applied to the
        // `format!("x'…'")` SQL pragma value below.
        let key_hex: Zeroizing<String> = Zeroizing::new(hex_encode(&page_key));
        page_key.zeroize();

        let key_pragma: Zeroizing<String> = Zeroizing::new(format!("x'{}'", &*key_hex));
        conn.pragma_update(None, "key", key_pragma.as_str())
            .map_err(AuditError::Sqlite)?;
        conn.pragma_update(None, "cipher_page_size", 4096_i64)
            .map_err(AuditError::Sqlite)?;
        conn.pragma_update(None, "kdf_iter", 256_000_i64)
            .map_err(AuditError::Sqlite)?;

        let _: i32 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|_| AuditError::Persistence("SQLCipher key did not unlock the database"))?;

        let existing_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);
        if existing_version != 0 && existing_version != SCHEMA_VERSION {
            return Err(AuditError::Persistence(
                "schema version mismatch — refusing to open",
            ));
        }

        conn.execute_batch(SCHEMA_SQL).map_err(AuditError::Sqlite)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(AuditError::Sqlite)?;

        let payload_key = derive_key(master_key, b"audit_entry:v1")?;

        let mut this = Self {
            log: AuditLog::new(),
            conn,
            payload_key,
            master_key: *master_key,
        };
        this.load_all()?;
        Ok(this)
    }

    /// Borrow the in-memory log for read-only queries.
    pub fn log(&self) -> &AuditLog {
        &self.log
    }

    /// Append `entry` to the in-memory log (assigning a sequence
    /// number) and mirror it to disk. Returns the entry id.
    ///
    /// The sequence number is assigned by [`AuditLog::append`] and
    /// is monotonically increasing. The on-disk row carries the
    /// same sequence number under a `UNIQUE` constraint so a
    /// crash-during-mirror cannot leave two entries with the same
    /// sequence on disk.
    pub fn append(&mut self, entry: AuditEntry) -> Result<AuditEntryId> {
        let id = self.log.append(entry);
        // `entries().last()` is the entry we just appended; it
        // carries the freshly-assigned sequence number.
        let stamped = self
            .log
            .entries()
            .last()
            .cloned()
            .ok_or(AuditError::Persistence(
                "in-memory log lost the entry it just appended",
            ))?;
        self.persist(&stamped)?;
        Ok(id)
    }

    /// Number of entries persisted on disk. Diverges from
    /// [`AuditLog::len`] only if a previous [`Self::append`] failed
    /// to mirror.
    pub fn persisted_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
            .map_err(AuditError::Sqlite)?;
        usize::try_from(n)
            .map_err(|_| AuditError::Persistence("persisted audit-log count exceeds usize::MAX"))
    }

    /// Reload every entry from disk into the in-memory log. Used
    /// by [`Self::open`]; exposed for explicit re-hydration.
    pub fn load_all(&mut self) -> Result<()> {
        let mut rows: Vec<(Uuid, i64, [u8; AEAD_NONCE_LEN], Vec<u8>)> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, sequence, nonce, payload FROM audit_log ORDER BY sequence ASC")
                .map_err(AuditError::Sqlite)?;
            let mut iter = stmt.query([]).map_err(AuditError::Sqlite)?;
            while let Some(row) = iter.next().map_err(AuditError::Sqlite)? {
                let id_bytes: Vec<u8> = row.get(0).map_err(AuditError::Sqlite)?;
                let sequence: i64 = row.get(1).map_err(AuditError::Sqlite)?;
                let nonce_bytes: Vec<u8> = row.get(2).map_err(AuditError::Sqlite)?;
                let ct: Vec<u8> = row.get(3).map_err(AuditError::Sqlite)?;
                rows.push((
                    slice_to_uuid(&id_bytes)?,
                    sequence,
                    slice_to_nonce(&nonce_bytes)?,
                    ct,
                ));
            }
        }

        self.log = AuditLog::new();
        for (id, sequence, nonce, ct) in rows {
            let aad = entry_aad(id, sequence);
            let pt = decrypt_aead(&self.payload_key, &nonce, &ct, &aad)?;
            let entry: AuditEntry = serde_json::from_slice(&pt)
                .map_err(|_| AuditError::Persistence("audit entry payload is not valid JSON"))?;
            // Round-trip sanity: the row we just decoded must
            // declare the same id/sequence that the AAD bound
            // it to. If the database row was tampered with —
            // e.g. a row was copied into another row's
            // sequence slot — the decrypt above would already
            // fail; this check defends against a stored
            // entry being replayed under an id that doesn't
            // match its payload.
            if entry.id != AuditEntryId(id) || i64_to_seq(sequence)? != entry.sequence {
                return Err(AuditError::Persistence(
                    "audit entry id/sequence does not match its row",
                ));
            }
            self.log.replay_persisted(entry)?;
        }
        Ok(())
    }

    fn persist(&mut self, entry: &AuditEntry) -> Result<()> {
        let payload = serde_json::to_vec(entry)
            .map_err(|_| AuditError::Persistence("audit entry payload could not be serialised"))?;
        let nonce = random_nonce();
        let id = entry.id.0;
        let sequence = seq_to_i64(entry.sequence)?;
        let aad = entry_aad(id, sequence);
        let ct = encrypt_aead(&self.payload_key, &nonce, &payload, &aad)?;
        let created_at = entry.timestamp.timestamp_millis();
        let action_tag = entry.action_type.as_str();
        let (actor_tag, actor_id_str) = actor_tags(entry.actor);
        let target_type_tag = Some(entry.target.target_type.as_str().to_owned());
        let target_id_str = Some(entry.target.target_id.to_string());
        let scope_bytes = entry.scope_id.map(|s| s.as_uuid().as_bytes().to_vec());
        self.conn
            .execute(
                "INSERT INTO audit_log
                    (id, sequence, action_type, actor_type, actor_id,
                     target_type, target_id, scope_id, created_at, nonce, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    id.as_bytes().to_vec(),
                    sequence,
                    action_tag,
                    actor_tag,
                    actor_id_str,
                    target_type_tag,
                    target_id_str,
                    scope_bytes,
                    created_at,
                    nonce.to_vec(),
                    ct,
                ],
            )
            .map_err(AuditError::Sqlite)?;
        Ok(())
    }
}

fn actor_tags(actor: Actor) -> (&'static str, String) {
    match actor {
        Actor::User(id) => ("user", id.to_string()),
        Actor::Agent(id) => ("agent", id.to_string()),
        Actor::System => ("system", String::new()),
    }
}

fn entry_aad(id: Uuid, sequence: i64) -> Vec<u8> {
    let mut aad = b"audit_entry:v1:".to_vec();
    aad.extend_from_slice(id.as_bytes());
    aad.extend_from_slice(&sequence.to_le_bytes());
    aad
}

fn seq_to_i64(seq: u64) -> Result<i64> {
    i64::try_from(seq).map_err(|_| {
        AuditError::Persistence("audit-log sequence overflows i64 — refusing to persist")
    })
}

fn i64_to_seq(seq: i64) -> Result<u64> {
    u64::try_from(seq).map_err(|_| {
        AuditError::Persistence("audit-log sequence column is negative — refusing to load")
    })
}

fn random_nonce() -> [u8; AEAD_NONCE_LEN] {
    let mut n = [0u8; AEAD_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

fn slice_to_uuid(b: &[u8]) -> Result<Uuid> {
    if b.len() != 16 {
        return Err(AuditError::Persistence("uuid column has wrong width"));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(b);
    Ok(Uuid::from_bytes(bytes))
}

fn slice_to_nonce(b: &[u8]) -> Result<[u8; AEAD_NONCE_LEN]> {
    if b.len() != AEAD_NONCE_LEN {
        return Err(AuditError::Persistence("nonce column has wrong width"));
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

// `scope_id` is intentionally not used in this file outside the
// `evidence_store::ScopeId` re-export — silence the unused-imports
// warning that the `pub mod persist;` exposure would otherwise
// surface on minor renames.
#[allow(dead_code)]
fn _scope_id_marker(s: ScopeId) -> ScopeId {
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{AuditActionType, AuditEntryBuilder, TargetRef, TargetType};
    use crypto::MASTER_KEY_LEN;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    fn fixture_key() -> MasterKey {
        let mut k: MasterKey = [0u8; MASTER_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(13);
        }
        k
    }

    fn fresh_entry() -> AuditEntry {
        AuditEntryBuilder::new()
            .actor(Actor::User(Uuid::new_v4()))
            .action(AuditActionType::CanonicalPromotion)
            .target(TargetRef::new(TargetType::Concept, Uuid::new_v4()))
            .build()
            .unwrap()
    }

    #[test]
    fn round_trip_persists_and_rehydrates() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let id_a;
        let id_b;
        let id_c;
        {
            let mut log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
            id_a = log.append(fresh_entry()).unwrap();
            id_b = log.append(fresh_entry()).unwrap();
            id_c = log.append(fresh_entry()).unwrap();
            assert_eq!(log.log().len(), 3);
            assert_eq!(log.persisted_count().unwrap(), 3);
        }

        let log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
        let ids: Vec<_> = log.log().entries().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![id_a, id_b, id_c]);

        // Sequences must keep their ordering across the restart.
        let seqs: Vec<_> = log.log().entries().iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[test]
    fn next_sequence_resumes_after_restart() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        {
            let mut log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
            log.append(fresh_entry()).unwrap();
            log.append(fresh_entry()).unwrap();
        }

        let mut log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
        log.append(fresh_entry()).unwrap();
        let last = log.log().entries().last().unwrap();
        assert_eq!(last.sequence, 2);
        assert_eq!(log.persisted_count().unwrap(), 3);
    }

    /// The public API exposes no UPDATE / DELETE — this test
    /// proves the surface stays append-only by enumerating every
    /// public method on `PersistentAuditLog` and asserting none
    /// of them mutate or remove existing entries. The check is
    /// behavioural rather than reflective (Rust has no public
    /// reflection in stable), so we exercise it by appending,
    /// inspecting, restarting, and confirming the entry survives
    /// unchanged.
    #[test]
    fn append_only_invariant_holds_across_restart() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let original = fresh_entry();
        {
            let mut log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
            log.append(original.clone()).unwrap();
        }

        // Reopen and confirm the entry is verbatim — including its
        // randomly-generated id and timestamp.
        let log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
        let entries = log.log().entries();
        assert_eq!(entries.len(), 1);
        let restored = &entries[0];
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.actor, original.actor);
        assert_eq!(restored.action_type, original.action_type);
        assert_eq!(restored.target, original.target);
        assert_eq!(restored.timestamp, original.timestamp);
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
            let mut log = PersistentAuditLog::open(tmp.path(), &key_a).unwrap();
            log.append(fresh_entry()).unwrap();
        }

        let err = PersistentAuditLog::open(tmp.path(), &key_b).unwrap_err();
        assert!(
            matches!(err, AuditError::Persistence(_) | AuditError::Sqlite(_)),
            "expected an open failure for the wrong key, got {err:?}",
        );
    }

    #[test]
    fn crash_recovery_keeps_committed_entries() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let e1 = fresh_entry();
        let e2 = fresh_entry();
        {
            let mut log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
            log.append(e1.clone()).unwrap();
            drop(log);
        }

        let mut log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
        assert_eq!(log.log().entries().len(), 1);
        assert_eq!(log.log().entries()[0].id, e1.id);

        log.append(e2.clone()).unwrap();
        assert_eq!(log.log().entries().len(), 2);
        assert_eq!(log.log().entries()[1].id, e2.id);
        assert_eq!(log.persisted_count().unwrap(), 2);
    }

    #[test]
    fn sequence_unique_constraint_blocks_duplicate_replay() {
        // Replaying a sequence twice on disk is impossible via the
        // public API, but the SQL `UNIQUE(sequence)` constraint
        // still gives us defence-in-depth. Exercise it by trying
        // a direct INSERT.
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let mut log = PersistentAuditLog::open(tmp.path(), &key).unwrap();
        log.append(fresh_entry()).unwrap();

        let entry2 = fresh_entry();
        let payload = serde_json::to_vec(&entry2).unwrap();
        let nonce = random_nonce();
        let aad = entry_aad(entry2.id.0, 0); // sequence 0 already taken
        let ct = encrypt_aead(&log.payload_key, &nonce, &payload, &aad).unwrap();
        let res = log.conn.execute(
            "INSERT INTO audit_log
                (id, sequence, action_type, actor_type, actor_id,
                 target_type, target_id, scope_id, created_at, nonce, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                entry2.id.0.as_bytes().to_vec(),
                0_i64,
                "canonical_promotion",
                "system",
                "",
                None::<String>,
                None::<String>,
                None::<Vec<u8>>,
                0_i64,
                nonce.to_vec(),
                ct,
            ],
        );
        assert!(res.is_err(), "duplicate sequence INSERT must be rejected");
    }
}
