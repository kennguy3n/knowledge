//! [`PersistentSyncEngine`] — SQLCipher-backed persistence wrapper
//! over the in-memory [`SyncEngine`].
//!
//! The in-memory [`SyncEngine`] remains the authoritative query
//! surface; this wrapper mirrors every appended `SyncOp` to a
//! SQLCipher database, following the same `concept_graph` pattern
//! used elsewhere in the substrate (see
//! `crates/concept_graph/src/persist.rs`):
//!
//! * Page-encryption key derived via HKDF context
//!   `b"sqlcipher:sync_engine:v1"`.
//! * Per-scope payload-encryption key derived via HKDF context
//!   `b"scope:{scope_uuid}:sync_op:v1"`, used to AEAD-seal the
//!   serialised [`SyncOp`] under XChaCha20-Poly1305 with the row's
//!   `(scope_id, replica_id, seq)` triple bound into the AAD.
//! * Schema versioning gated on `PRAGMA user_version`.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE sync_ops (
//!     scope_id BLOB NOT NULL,
//!     replica_id BLOB NOT NULL,
//!     seq INTEGER NOT NULL,
//!     created_at INTEGER NOT NULL,
//!     op_kind TEXT NOT NULL,
//!     nonce BLOB NOT NULL,
//!     payload BLOB NOT NULL,
//!     PRIMARY KEY (scope_id, replica_id, seq)
//! );
//! CREATE INDEX sync_ops_scope_idx ON sync_ops(scope_id);
//!
//! CREATE TABLE sync_meta (
//!     scope_id BLOB PRIMARY KEY,
//!     replica_id BLOB NOT NULL,
//!     clock INTEGER NOT NULL,
//!     compaction_epoch INTEGER NOT NULL
//! );
//! ```
//!
//! `payload` is the AEAD ciphertext of the JSON-serialised
//! [`SyncOp`]. The plaintext `op_kind` column carries the discriminant
//! (`"add"` / `"remove"` / `"supersede"`) for scope-filtered queries
//! and admin tooling; the discriminant is also part of the on-disk
//! taxonomy documented in `docs/DESIGN.md` §3.2, so this does not
//! leak more than the schema already does.

use std::hash::Hash;
use std::path::Path;

// `TryRng` is the fallible RNG trait in rand 0.10 (which renamed
// `TryRngCore` to `TryRng` and `OsRng` to `SysRng`). We invoke
// `try_fill_bytes(...).expect("OS RNG failure")` so a transient
// kernel-RNG failure surfaces as a panic — the correct posture for
// a substrate that cannot continue safely without entropy.
use rand::TryRng;
use rusqlite::{params, Connection};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crypto::{decrypt_aead, derive_key, encrypt_aead, AeadKey, MasterKey, AEAD_NONCE_LEN};

use crate::error::{Result, SyncError};
use crate::op_log::{OpLog, SyncOp, SyncOpKind};
use crate::{SyncEngine, SyncScopeId};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS sync_ops (scope_id BLOB NOT NULL,
    replica_id BLOB NOT NULL,
    seq INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    op_kind TEXT NOT NULL,
    nonce BLOB NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY (scope_id, replica_id, seq)
);
CREATE INDEX IF NOT EXISTS sync_ops_scope_idx ON sync_ops(scope_id);

CREATE TABLE IF NOT EXISTS sync_meta (scope_id BLOB PRIMARY KEY,
    replica_id BLOB NOT NULL,
    clock INTEGER NOT NULL,
    compaction_epoch INTEGER NOT NULL
);
";

const SCHEMA_VERSION: i32 = 1;

/// Encrypt-on-write, decrypt-on-read SQLCipher persistence wrapper
/// over [`SyncEngine`], scoped to a single [`SyncScopeId`].
///
/// Multiple `PersistentSyncEngine` instances can share a single
/// database file by opening it under different scope ids; the
/// `scope_id` column on every row keeps the scopes isolated and the
/// per-scope AEAD key derivation keeps them cryptographically
/// independent (a per-scope key compromise reveals only that
/// scope's ciphertexts).
///
/// `Drop` zeroises the cached AEAD scope key so it does not
/// linger in freed heap memory after the wrapper is dropped. The
/// master key passed to [`Self::open`] is **not** retained: both
/// the SQLCipher page-encryption key and the per-scope AEAD key
/// are derived from it at open time and the master key is then
/// allowed to drop out of scope, minimising the window of time
/// the substrate's root key material lives in this wrapper's
/// memory.
///
/// `Debug` is intentionally redacted — the wrapper holds key
/// material whose serialised form must never reach a panic message
/// or a log line.
///
/// The AEAD ciphertext column carries the `serde_json` UTF-8
/// encoding of [`SyncOp<T>`] verbatim — the persistence layer is
/// byte-clean across every Unicode script (ASCII / CJK / RTL /
/// Indic / etc.) and does not normalise, fold, or otherwise
/// inspect the contents of `T`. The "Multilingual contract"
/// section in the crate-level docs
/// (`crates/sync_engine/src/lib.rs`) details what this means for
/// callers that pick text-bearing `T`.
pub struct PersistentSyncEngine<T = Uuid>
where
    T: Eq + Hash + Clone,
{
    engine: SyncEngine<T>,
    conn: Connection,
    scope: SyncScopeId,
    scope_key: AeadKey,
    /// Number of ops we have already persisted from the engine's
    /// log. Always `<= self.engine.op_log().ops.len()`.
    persisted_len: usize,
}

impl<T> std::fmt::Debug for PersistentSyncEngine<T>
where
    T: Eq + Hash + Clone + std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentSyncEngine")
            .field("scope", &self.scope.as_uuid())
            .field("engine_len", &self.engine.op_log().ops.len())
            .field("persisted_len", &self.persisted_len)
            .field("scope_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl<T> Drop for PersistentSyncEngine<T>
where
    T: Eq + Hash + Clone,
{
    fn drop(&mut self) {
        self.scope_key.zeroize();
    }
}

impl<T> PersistentSyncEngine<T>
where
    T: Eq + Hash + Clone + Serialize + DeserializeOwned,
{
    /// Open or create a SQLCipher-backed sync engine at `path` for
    /// the given `scope` and `replica`.
    ///
    /// On open the wrapper rehydrates the in-memory engine from
    /// disk: every persisted [`SyncOp`] for this scope is decrypted
    /// (under the per-scope AEAD key), deserialised, and absorbed
    /// into a fresh [`OpLog`] keyed by `replica`. The
    /// `compaction_epoch` and `clock` values are restored from
    /// `sync_meta`.
    ///
    /// `master_key` is the user's substrate master key; the
    /// SQLCipher page-encryption key is derived from it via HKDF
    /// context `b"sqlcipher:sync_engine:v1"`. The per-scope AEAD key
    /// is derived via context `b"scope:{scope_uuid}:sync_op:v1"`.
    pub fn open<P: AsRef<Path>>(path: P,
        scope: SyncScopeId,
        replica: Uuid,
        master_key: &MasterKey,
    ) -> Result<Self> {
        let conn = Connection::open(path).map_err(SyncError::Sqlite)?;

        let mut page_key = derive_key(master_key, b"sqlcipher:sync_engine:v1")?;
        // `Zeroizing<String>` zeroes the heap-allocated hex bytes
        // when dropped — without this wrapper the SQLCipher page
        // key would linger in freed heap memory after `String`'s
        // default `Drop`. The same wrap is applied to the
        // `format!("x'…'")` SQL pragma value below.
        let key_hex: Zeroizing<String> = Zeroizing::new(hex_encode(&page_key));
        page_key.zeroize();

        let key_pragma: Zeroizing<String> = Zeroizing::new(format!("x'{}'", &*key_hex));
        conn.pragma_update(None, "key", key_pragma.as_str())?;
        conn.pragma_update(None, "cipher_page_size", 4096_i64)?;
        conn.pragma_update(None, "kdf_iter", 256_000_i64)?;

        // Verify the key works.
        let _: i32 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|_| SyncError::Persistence("SQLCipher key did not unlock the database"))?;

        // Read the existing schema version *before* applying the
        // schema or stamping a new one. `0` means "fresh database";
        // any other non-matching value means a future / incompatible
        // schema is in place and we refuse to open.
        let existing_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);
        if existing_version != 0 && existing_version != SCHEMA_VERSION {
            return Err(SyncError::Persistence("schema version mismatch — refusing to open",
            ));
        }

        conn.execute_batch(SCHEMA_SQL)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        let scope_label = format!("scope:{}:sync_op:v1", scope.as_uuid());
        let scope_key = derive_key(master_key, scope_label.as_bytes())?;

        // We do not retain `master_key` past this point — both
        // derived keys (SQLCipher page key + per-scope AEAD key)
        // are now in place, so the substrate's root key material
        // can drop out of scope at the caller.
        let mut wrapper = Self {
            engine: SyncEngine::from_log(replica, OpLog::<T>::new(replica)),
            conn,
            scope,
            scope_key,
            persisted_len: 0,
        };
        wrapper.load()?;
        Ok(wrapper)
    }

    /// Borrow the in-memory engine for read-only inspection.
    pub fn engine(&self) -> &SyncEngine<T> {
        &self.engine
    }

    /// Borrow the in-memory engine mutably.
    ///
    /// Mutations done through this borrow are **not** mirrored to
    /// disk. Prefer the typed wrapper methods ([`Self::add`],
    /// [`Self::remove`], [`Self::supersede`], [`Self::merge`],
    /// [`Self::compact`]) for mutations that must survive a
    /// restart.
    pub fn engine_mut(&mut self) -> &mut SyncEngine<T> {
        &mut self.engine
    }

    /// Scope this wrapper is bound to.
    pub fn scope(&self) -> SyncScopeId {
        self.scope
    }

    /// Record an `Add` op on the engine and mirror it to disk.
    pub fn add(&mut self, value: T) -> Result<Uuid> {
        let tag = self.engine.add(value);
        self.flush_appended()?;
        Ok(tag)
    }

    /// Record a `Remove` op on the engine and mirror it to disk.
    ///
    /// If [`SyncEngine::remove`] short-circuits (no live tags to
    /// tombstone, i.e. the value has never been observed on this
    /// replica), no op is appended to the log and no SQLite
    /// transaction is opened — defensive `remove` of an unknown
    /// value is a true no-op end-to-end.
    pub fn remove(&mut self, value: T) -> Result<()> {
        let before = self.engine.op_log().ops.len();
        self.engine.remove(value);
        if self.engine.op_log().ops.len() == before {
            return Ok(());
        }
        self.flush_appended()
    }

    /// Record a `Supersede` op on the engine and mirror it to disk.
    ///
    /// If [`SyncEngine::supersede`] does not append a new op (the
    /// engine may elide it in future revisions; today it always
    /// appends one because a supersession carries the
    /// `(value, successor)` edge regardless of observed tags), no
    /// SQLite transaction is opened. The check is symmetrical to
    /// [`Self::remove`].
    pub fn supersede(&mut self, value: T, successor: T) -> Result<()> {
        let before = self.engine.op_log().ops.len();
        self.engine.supersede(value, successor);
        if self.engine.op_log().ops.len() == before {
            return Ok(());
        }
        self.flush_appended()
    }

    /// Merge another sync engine into this one. Any newly absorbed
    /// ops are mirrored to disk, and `sync_meta` is updated.
    pub fn merge(&mut self, other: &SyncEngine<T>) -> Result<()> {
        self.engine.merge(other);
        self.flush_appended()
    }

    /// Compact the local op log (see [`SyncEngine::compact`]). The
    /// on-disk row set is rewritten to match the new minimal op
    /// log inside a single SQLite transaction, so a restart at any
    /// point yields a consistent on-disk state.
    pub fn compact(&mut self) -> Result<usize> {
        let removed = self.engine.compact()?;
        self.rewrite_all()?;
        Ok(removed)
    }

    /// Force-flush the entire engine op log + metadata to disk,
    /// overwriting any previously-persisted rows for this scope.
    /// Useful when the caller has mutated the engine via
    /// [`Self::engine_mut`] and wants to checkpoint.
    pub fn save(&mut self) -> Result<()> {
        self.rewrite_all()
    }

    /// Number of ops currently persisted on disk for this scope.
    pub fn persisted_len(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM sync_ops WHERE scope_id = ?1",
            params![self.scope.as_uuid().as_bytes().to_vec()],
            |row| row.get(0),
        )?;
        // SQLite COUNT(*) is always non-negative and bounded by the
        // row count of a single table, so this fits in `usize` on
        // 64-bit targets. The explicit fallible conversion guards
        // 32-bit targets and any future driver oddity.
        usize::try_from(n).map_err(|_| {
            SyncError::Serialisation("sync_ops row count exceeds usize::MAX on this target")
        })
    }

    /// Append every op in `[self.persisted_len .. engine.log.len())`
    /// to disk inside a single transaction, then update
    /// `self.persisted_len` and `sync_meta`.
    ///
    /// `sync_ops` inserts **and** the matching `sync_meta` upsert are
    /// committed atomically: a crash between them would otherwise
    /// leave the on-disk `compaction_epoch` lagging the
    /// just-persisted op set, breaking the delta-sync epoch guard
    /// that prevents accepting stale deltas after a `compact()`.
    fn flush_appended(&mut self) -> Result<()> {
        let target_len = self.engine.op_log().ops.len();
        if target_len < self.persisted_len {
            // The engine's log shrunk under us — only possible via
            // an out-of-band mutation through `engine_mut()` (e.g.
            // `compact()` rewriting the log). Fall back to a full
            // rewrite so disk and memory line up.
            return self.rewrite_all();
        }
        // Even when target_len == persisted_len we still issue a
        // `BEGIN IMMEDIATE` / `COMMIT` round so the meta upsert is
        // atomic with any concurrent reader: a sibling connection
        // never observes a partial update to `sync_meta`.

        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result: Result<()> = (|| {
            for op in &self.engine.op_log().ops[self.persisted_len..target_len] {
                self.insert_op_row(op)?;
            }
            self.upsert_meta_inner()?;
            Ok(())
        })();

        match result {
            Ok(()) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => {
                    self.persisted_len = target_len;
                    Ok(())
                }
                Err(e) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(SyncError::Sqlite(e))
                }
            },
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Truncate-and-reinsert the whole on-disk op set for this scope,
    /// with the matching `sync_meta` upsert in the **same**
    /// transaction. Used by [`Self::compact`] and [`Self::save`].
    fn rewrite_all(&mut self) -> Result<()> {
        let scope_bytes = self.scope.as_uuid().as_bytes().to_vec();
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result: Result<()> = (|| {
            self.conn.execute("DELETE FROM sync_ops WHERE scope_id = ?1",
                params![scope_bytes.clone()],
            )?;
            for op in &self.engine.op_log().ops {
                self.insert_op_row(op)?;
            }
            self.upsert_meta_inner()?;
            Ok(())
        })();
        match result {
            Ok(()) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => {
                    self.persisted_len = self.engine.op_log().ops.len();
                    Ok(())
                }
                Err(e) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(SyncError::Sqlite(e))
                }
            },
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn insert_op_row(&self, op: &SyncOp<T>) -> Result<()> {
        let payload = serde_json::to_vec(op)
            .map_err(|_| SyncError::Serialisation("could not serialise sync op for persistence"))?;
        let nonce = random_nonce();
        let aad = op_aad(self.scope, op.replica_id, op.seq);
        let ct = encrypt_aead(&self.scope_key, &nonce, &payload, &aad)?;

        self.conn.execute("INSERT INTO sync_ops
              (scope_id, replica_id, seq, created_at, op_kind, nonce, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(scope_id, replica_id, seq) DO UPDATE SET
               created_at = excluded.created_at,
               op_kind = excluded.op_kind,
               nonce = excluded.nonce,
               payload = excluded.payload",
            params![
                self.scope.as_uuid().as_bytes().to_vec(),
                op.replica_id.as_bytes().to_vec(),
                i64::try_from(op.seq).map_err(|_| {
                    SyncError::Serialisation("op seq exceeds SQLite signed 64-bit range")
                })?,
                op.created_at.timestamp(),
                op_kind_tag(&op.op),
                nonce.to_vec(),
                ct,
            ],
        )?;
        Ok(())
    }

    /// Upsert the `sync_meta` row for this scope.
    ///
    /// The `_inner` suffix signals that this **does not** open or
    /// commit its own transaction: callers must invoke it within an
    /// already-open `BEGIN IMMEDIATE` block so the meta write commits
    /// atomically with the matching `sync_ops` changes. Otherwise a
    /// crash between the ops commit and the meta write would silently
    /// roll `compaction_epoch` back and let the delta-sync epoch
    /// guard accept stale deltas.
    fn upsert_meta_inner(&self) -> Result<()> {
        self.conn.execute("INSERT INTO sync_meta (scope_id, replica_id, clock, compaction_epoch)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_id) DO UPDATE SET
               replica_id = excluded.replica_id,
               clock = excluded.clock,
               compaction_epoch = excluded.compaction_epoch",
            params![
                self.scope.as_uuid().as_bytes().to_vec(),
                self.engine.replica_id().as_bytes().to_vec(),
                i64::try_from(self.engine.op_log().clock).map_err(|_| {
                    SyncError::Serialisation("sync clock exceeds SQLite signed 64-bit range")
                })?,
                i64::try_from(self.engine.op_log().compaction_epoch).map_err(|_| {
                    SyncError::Serialisation("compaction_epoch exceeds SQLite signed 64-bit range")
                })?,
            ],
        )?;
        Ok(())
    }

    /// Rehydrate the in-memory engine from disk.
    fn load(&mut self) -> Result<()> {
        let scope_bytes = self.scope.as_uuid().as_bytes().to_vec();

        // First materialise raw rows, then decrypt / deserialise
        // outside the prepared-statement borrow so we don't
        // pin `self.conn`.
        let mut raw_rows: Vec<([u8; AEAD_NONCE_LEN], Vec<u8>, Uuid, u64)> = Vec::new();
        {
            let mut stmt = self.conn.prepare("SELECT replica_id, seq, nonce, payload FROM sync_ops
                 WHERE scope_id = ?1
                 ORDER BY replica_id ASC, seq ASC",
            )?;
            let mut rows = stmt.query(params![scope_bytes.clone()])?;
            while let Some(row) = rows.next()? {
                let replica_bytes: Vec<u8> = row.get(0)?;
                let seq: i64 = row.get(1)?;
                let nonce_bytes: Vec<u8> = row.get(2)?;
                let ct: Vec<u8> = row.get(3)?;
                let replica_id = slice_to_uuid(&replica_bytes)?;
                let nonce = slice_to_nonce(&nonce_bytes)?;
                // SQLite stores `seq` as signed 64-bit; ops with a
                // negative seq would indicate either schema
                // corruption or a writer that bypassed the
                // `i64::try_from(op.seq)` check above. Surface as
                // a deserialisation error rather than wrap.
                let seq_u64 = u64::try_from(seq)
                    .map_err(|_| SyncError::Serialisation("persisted sync op had negative seq"))?;
                raw_rows.push((nonce, ct, replica_id, seq_u64));
            }
        }

        let mut log = OpLog::<T>::new(self.engine.replica_id());
        for (nonce, ct, replica_id, seq) in raw_rows {
            let aad = op_aad(self.scope, replica_id, seq);
            let pt = decrypt_aead(&self.scope_key, &nonce, &ct, &aad)?;
            let op: SyncOp<T> = serde_json::from_slice(&pt)
                .map_err(|_| SyncError::Serialisation("persisted sync op did not deserialise"))?;
            // `merge_single` dedupes by (replica_id, seq) and
            // tracks the local clock if `op` is from this replica.
            log.merge_single(op);
        }

        // Restore meta (clock / compaction_epoch) if a row exists.
        if let Ok((clock, epoch)) = self.conn.query_row("SELECT clock, compaction_epoch FROM sync_meta WHERE scope_id = ?1",
            params![scope_bytes],
            |row| {
                let clock: i64 = row.get(0)?;
                let epoch: i64 = row.get(1)?;
                // SQLite stores both fields as signed; a negative
                // value would indicate schema corruption from a
                // non-substrate writer. Treat as zero for the
                // `.max()` reconciliation below — that's the
                // safe-conservative behaviour.
                let clock_u64 = u64::try_from(clock).unwrap_or(0);
                let epoch_u64 = u64::try_from(epoch).unwrap_or(0);
                Ok((clock_u64, epoch_u64))
            },
        ) {
            // Symmetric `.max()` semantics for both meta fields: the
            // persisted values are authoritative for a fresh load, but
            // the same recovery code path will also be reused if
            // `merge_single` ever bumps `compaction_epoch` in the
            // future (today it only bumps `clock`). Keeping both
            // restores defensive avoids a silent regression if that
            // changes.
            log.clock = log.clock.max(clock);
            log.compaction_epoch = log.compaction_epoch.max(epoch);
        }

        self.engine = SyncEngine::from_log(self.engine.replica_id(), log);
        self.persisted_len = self.engine.op_log().ops.len();
        Ok(())
    }
}

fn random_nonce() -> [u8; AEAD_NONCE_LEN] {
    let mut n = [0u8; AEAD_NONCE_LEN];
    // See the import-site comment for why `SysRng` + the fallible
    // `TryRng::try_fill_bytes(...).expect(...)` is the correct
    // shape under rand 0.10 (panic on OS RNG failure rather than
    // silently producing weak nonces).
    rand::rngs::SysRng
        .try_fill_bytes(&mut n)
        .expect("OS RNG failure");
    n
}

fn op_aad(scope: SyncScopeId, replica: Uuid, seq: u64) -> Vec<u8> {
    let mut aad = b"sync_op:v1:".to_vec();
    aad.extend_from_slice(scope.as_uuid().as_bytes());
    aad.extend_from_slice(replica.as_bytes());
    aad.extend_from_slice(&seq.to_le_bytes());
    aad
}

fn op_kind_tag<T>(op: &SyncOpKind<T>) -> &'static str
where
    T: Eq + std::hash::Hash + Clone,
{
    match op {
        SyncOpKind::Add { .. } => "add",
        SyncOpKind::Remove { .. } => "remove",
        SyncOpKind::Supersede { .. } => "supersede",
    }
}

fn slice_to_uuid(b: &[u8]) -> Result<Uuid> {
    if b.len() != 16 {
        return Err(SyncError::Persistence("uuid column has wrong width"));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(b);
    Ok(Uuid::from_bytes(bytes))
}

fn slice_to_nonce(b: &[u8]) -> Result<[u8; AEAD_NONCE_LEN]> {
    if b.len() != AEAD_NONCE_LEN {
        return Err(SyncError::Persistence("nonce column has wrong width"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_master_key() -> MasterKey {
        // Deterministic test key — fine for unit tests; production
        // callers derive `MasterKey` from the user's hybrid-KEM
        // unwrap path. `MasterKey` is the type alias
        // `[u8; MASTER_KEY_LEN]`.
        let mut k: MasterKey = [0u8; crypto::MASTER_KEY_LEN];
        for (i, slot) in k.iter_mut().enumerate() {
            // `i` is bounded by `MASTER_KEY_LEN` (32) so masking
            // to a byte never truncates the meaningful bits.
            #[allow(clippy::cast_possible_truncation,
                reason = "deterministic test key seed; i < MASTER_KEY_LEN < 256"
            )]
            let byte = (i & 0xFF) as u8;
            *slot = byte.wrapping_mul(7).wrapping_add(13);
        }
        k
    }

    #[test]
    fn write_close_reopen_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sync.sqlite");
        let scope = SyncScopeId::new_v4();
        let replica = Uuid::new_v4();
        let mk = test_master_key();

        {
            let mut p = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
            p.add("a".into()).unwrap();
            p.add("b".into()).unwrap();
            p.remove("a".into()).unwrap();
            assert_eq!(p.persisted_len().unwrap(), 3);
        }

        let p2 = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
        let (set, _) = p2.engine().state().unwrap();
        assert!(!set.contains(&"a".to_string()));
        assert!(set.contains(&"b".to_string()));
    }

    #[test]
    fn wrong_master_key_refuses_to_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sync.sqlite");
        let scope = SyncScopeId::new_v4();
        let replica = Uuid::new_v4();
        let mk1 = test_master_key();
        {
            let mut p = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk1).unwrap();
            p.add("alpha".into()).unwrap();
        }

        let mut mk2: MasterKey = [0u8; crypto::MASTER_KEY_LEN];
        mk2[0] = 0xab;
        let err = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk2).unwrap_err();
        // SQLCipher surfaces the wrong-key failure from whichever pragma
        // or query first touches the file pages; on this build that is
        // `pragma_update("cipher_page_size", ...)` rather than the
        // `SELECT 1` verification. Either error path is acceptable as
        // long as the open *fails*.
        assert!(matches!(err, SyncError::Persistence(_) | SyncError::Sqlite(_)),
            "expected open failure with wrong key, got {err:?}",
        );
    }

    #[test]
    fn compact_then_reopen_preserves_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sync.sqlite");
        let scope = SyncScopeId::new_v4();
        let replica = Uuid::new_v4();
        let mk = test_master_key();

        let original_epoch;
        let preserved_len;
        {
            let mut p = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
            for i in 0..50 {
                p.add(format!("v{}", i)).unwrap();
            }
            for i in 0..25 {
                p.remove(format!("v{}", i)).unwrap();
            }
            assert_eq!(p.persisted_len().unwrap(), 75);
            let removed = p.compact().unwrap();
            assert!(removed > 0);
            original_epoch = p.engine().compaction_epoch();
            preserved_len = p.persisted_len().unwrap();
            assert_eq!(preserved_len, p.engine().op_log().ops.len());
        }

        let p2 = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
        assert_eq!(p2.engine().compaction_epoch(), original_epoch);
        assert_eq!(p2.persisted_len().unwrap(), preserved_len);
        let (set, _) = p2.engine().state().unwrap();
        for i in 25..50 {
            assert!(set.contains(&format!("v{}", i)));
        }
        for i in 0..25 {
            assert!(!set.contains(&format!("v{}", i)));
        }
    }
}
