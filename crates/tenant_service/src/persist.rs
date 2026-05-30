//! [`PersistentTenantRegistry`] — SQLCipher-backed persistence
//! wrapper over the in-memory [`crate::TenantRegistry`].
//!
//! The in-memory registry remains the primary query surface for
//! tenant lookups, member listings, and lifecycle checks. Every
//! mutating call ([`Self::create`], [`Self::add_member`],
//! [`Self::remove_member`], [`Self::update_role`],
//! [`Self::suspend`], [`Self::activate`], [`Self::delete`])
//! mirrors the change to a SQLCipher database whose page-encryption
//! key is derived from the per-user master key under HKDF context
//! `b"sqlcipher:tenants:v1"`.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE tenants (
//!     id BLOB PRIMARY KEY,
//!     name TEXT NOT NULL,
//!     status TEXT NOT NULL,
//!     created_at INTEGER NOT NULL,
//!     updated_at INTEGER NOT NULL,
//!     deleted_at INTEGER,
//!     root_key_ref BLOB,
//!     nonce BLOB NOT NULL,
//!     payload BLOB NOT NULL
//! );
//!
//! CREATE TABLE tenant_members (
//!     tenant_id BLOB NOT NULL,
//!     user_id BLOB NOT NULL,
//!     role TEXT NOT NULL,
//!     status TEXT NOT NULL,
//!     created_at INTEGER NOT NULL,
//!     updated_at INTEGER NOT NULL,
//!     PRIMARY KEY (tenant_id, user_id)
//! );
//!
//! CREATE TABLE tenant_configs (
//!     tenant_id BLOB PRIMARY KEY,
//!     encryption_key_ref BLOB,
//!     storage_config TEXT,
//!     synthesis_config TEXT
//! );
//! ```
//!
//! For the `tenants` table the structured columns let SQL queries
//! filter by status / lifecycle without decrypting; the encrypted
//! `payload` carries the canonical [`crate::Tenant`] struct under a
//! per-store AEAD key. `tenant_members` is plaintext because the
//! member taxonomy (user id, role, status) is already exposed in
//! the permission graph by design (`docs/DESIGN.md` §7.1). The
//! tenant **config** table is plaintext JSON because the substrate's
//! threat model treats config (storage caps, synthesis cadence,
//! managed-endpoint URLs) as non-secret — the actual secret is the
//! root key, and that is held by the `crypto` crate, not here.

use std::path::Path;

use rand::RngCore;
use rusqlite::{params, Connection};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crypto::{decrypt_aead, derive_key, encrypt_aead, AeadKey, MasterKey, AEAD_NONCE_LEN};
use permission_service::Relation;

use crate::config::TenantConfig;
use crate::error::{Result, TenantError};
use crate::lifecycle::TenantStatus;
use crate::member::{TenantMember, TenantMemberStatus};
use crate::tenant::{Tenant, TenantId, TenantRegistry};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS tenants (
    id BLOB PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    root_key_ref BLOB,
    nonce BLOB NOT NULL,
    payload BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS tenants_status_idx ON tenants(status);

CREATE TABLE IF NOT EXISTS tenant_members (
    tenant_id BLOB NOT NULL,
    user_id BLOB NOT NULL,
    role TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, user_id)
);

CREATE TABLE IF NOT EXISTS tenant_configs (
    tenant_id BLOB PRIMARY KEY,
    encryption_key_ref BLOB,
    storage_config TEXT,
    synthesis_config TEXT
);
";

const SCHEMA_VERSION: i32 = 1;

/// SQLCipher-backed persistence wrapper over [`TenantRegistry`].
///
/// Holds the registry by value and intercepts every mutating call
/// so the on-disk view stays in lockstep. Read calls
/// ([`Self::get`], [`Self::list_members`], [`Self::get_member`])
/// delegate straight to the in-memory registry.
///
/// `Drop` zeroises the master key and the payload AEAD key.
///
/// `Debug` is intentionally redacted — the wrapper holds key
/// material whose serialised form must never reach a panic message
/// or a log line.
pub struct PersistentTenantRegistry {
    registry: TenantRegistry,
    conn: Connection,
    payload_key: AeadKey,
    master_key: MasterKey,
}

impl std::fmt::Debug for PersistentTenantRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentTenantRegistry")
            .field("tenants", &self.registry.len())
            .field("payload_key", &"<redacted>")
            .field("master_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Drop for PersistentTenantRegistry {
    fn drop(&mut self) {
        self.master_key.zeroize();
        self.payload_key.zeroize();
    }
}

impl PersistentTenantRegistry {
    /// Open or create the SQLCipher tenant database at `path` and
    /// rehydrate the in-memory registry from disk.
    pub fn open<P: AsRef<Path>>(path: P, master_key: &MasterKey) -> Result<Self> {
        let conn = Connection::open(path).map_err(TenantError::Sqlite)?;

        let mut page_key = derive_key(master_key, b"sqlcipher:tenants:v1")?;
        // `Zeroizing<String>` zeroes the heap-allocated hex bytes
        // when dropped — without this wrapper the SQLCipher page
        // key would linger in freed heap memory after `String`'s
        // default `Drop`. The same wrap is applied to the
        // `format!("x'…'")` SQL pragma value below.
        let key_hex: Zeroizing<String> = Zeroizing::new(hex_encode(&page_key));
        page_key.zeroize();

        let key_pragma: Zeroizing<String> = Zeroizing::new(format!("x'{}'", &*key_hex));
        conn.pragma_update(None, "key", key_pragma.as_str())
            .map_err(TenantError::Sqlite)?;
        conn.pragma_update(None, "cipher_page_size", 4096_i64)
            .map_err(TenantError::Sqlite)?;
        conn.pragma_update(None, "kdf_iter", 256_000_i64)
            .map_err(TenantError::Sqlite)?;

        let _: i32 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|_| TenantError::Persistence("SQLCipher key did not unlock the database"))?;

        let existing_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);
        if existing_version != 0 && existing_version != SCHEMA_VERSION {
            return Err(TenantError::Persistence(
                "schema version mismatch — refusing to open",
            ));
        }

        conn.execute_batch(SCHEMA_SQL)
            .map_err(TenantError::Sqlite)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(TenantError::Sqlite)?;

        let payload_key = derive_key(master_key, b"tenant_row:v1")?;

        let mut this = Self {
            registry: TenantRegistry::new(),
            conn,
            payload_key,
            master_key: *master_key,
        };
        this.load_all()?;
        Ok(this)
    }

    /// Borrow the in-memory registry for read-only queries.
    pub fn registry(&self) -> &TenantRegistry {
        &self.registry
    }

    /// Create a tenant in-memory and mirror it to disk along with
    /// its config row. Returns the freshly-assigned [`TenantId`].
    pub fn create(&mut self, name: impl Into<String>, config: TenantConfig) -> Result<TenantId> {
        let id = self.registry.create(name, config)?;
        let tenant = self.registry.get(id)?.clone();
        // We cannot undo the in-memory `create` cleanly because
        // the registry has no public `delete-from-memory-only`
        // API; surface the error and let the caller decide.
        // The in-memory state is still consistent (the tenant
        // exists in memory and the disk row is missing), so the
        // next `load_all` / process restart would reconverge.
        self.persist_tenant(&tenant)?;
        self.persist_config(id, &tenant.config)?;
        Ok(id)
    }

    /// Suspend `id`. Mirrors the status change to disk.
    pub fn suspend(&mut self, id: TenantId) -> Result<()> {
        self.registry.suspend(id)?;
        let tenant = self.registry.get(id)?.clone();
        self.persist_tenant(&tenant)
    }

    /// Activate `id`. Mirrors the status change to disk.
    pub fn activate(&mut self, id: TenantId) -> Result<()> {
        self.registry.activate(id)?;
        let tenant = self.registry.get(id)?.clone();
        self.persist_tenant(&tenant)
    }

    /// Delete `id` (cryptographic forgetting + lifecycle terminal
    /// transition). Mirrors the resulting state to disk atomically:
    /// the encrypted `tenants` payload, the denormalised
    /// `tenant_configs` row, and the `deleted_at` timestamp all
    /// commit together inside a single transaction so a crash or a
    /// SQL failure mid-delete cannot leave the on-disk store
    /// half-updated (e.g. payload says `Deleted` but `deleted_at`
    /// is still NULL).
    pub fn delete(&mut self, id: TenantId) -> Result<()> {
        self.registry.delete(id)?;
        let tenant = self.registry.get(id)?.clone();
        let now = chrono::Utc::now().timestamp_millis();
        let tx = self.conn.transaction().map_err(TenantError::Sqlite)?;
        Self::persist_tenant_in(&tx, &self.payload_key, &tenant)?;
        Self::persist_config_in(&tx, id, &tenant.config)?;
        tx.execute(
            "UPDATE tenants SET deleted_at = ?1 WHERE id = ?2",
            params![now, id.as_uuid().as_bytes().to_vec()],
        )
        .map_err(TenantError::Sqlite)?;
        tx.commit().map_err(TenantError::Sqlite)?;
        Ok(())
    }

    /// Provision a member for `tenant_id`. Mirrors the membership
    /// row to disk.
    pub fn add_member(
        &mut self,
        tenant_id: TenantId,
        user_id: Uuid,
        role: Relation,
    ) -> Result<TenantMember> {
        let member = self.registry.add_member(tenant_id, user_id, role)?;
        self.persist_member(&member)?;
        Ok(member)
    }

    /// Remove a member from `tenant_id`. Mirrors the status flip
    /// to disk (the row stays around with `status = Removed` so the
    /// audit trail keeps a single removal timestamp).
    pub fn remove_member(&mut self, tenant_id: TenantId, user_id: Uuid) -> Result<()> {
        self.registry.remove_member(tenant_id, user_id)?;
        let member = self.registry.get_member(tenant_id, user_id)?.clone();
        self.persist_member(&member)
    }

    /// Update a member's role. Mirrors the change to disk.
    pub fn update_role(
        &mut self,
        tenant_id: TenantId,
        user_id: Uuid,
        role: Relation,
    ) -> Result<()> {
        self.registry.update_role(tenant_id, user_id, role)?;
        let member = self.registry.get_member(tenant_id, user_id)?.clone();
        self.persist_member(&member)
    }

    /// Count of tenants on disk. Useful for tests that want to
    /// verify the mirror is in sync.
    pub fn persisted_tenant_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tenants", [], |row| row.get(0))
            .map_err(TenantError::Sqlite)?;
        usize::try_from(n)
            .map_err(|_| TenantError::Persistence("persisted tenant count exceeds usize::MAX"))
    }

    /// Count of membership rows on disk.
    pub fn persisted_member_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tenant_members", [], |row| row.get(0))
            .map_err(TenantError::Sqlite)?;
        usize::try_from(n)
            .map_err(|_| TenantError::Persistence("persisted member count exceeds usize::MAX"))
    }

    /// Reload every tenant + member row from disk into the
    /// in-memory registry. Used by [`Self::open`].
    pub fn load_all(&mut self) -> Result<()> {
        let tenants = self.load_tenants()?;
        let members = self.load_members()?;
        self.registry = TenantRegistry::new();
        for tenant in tenants {
            self.registry.insert_persisted(tenant)?;
        }
        for member in members {
            self.registry.insert_persisted_member(member)?;
        }
        Ok(())
    }

    fn load_tenants(&self) -> Result<Vec<Tenant>> {
        let mut rows: Vec<(Uuid, [u8; AEAD_NONCE_LEN], Vec<u8>)> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, nonce, payload FROM tenants ORDER BY created_at ASC")
                .map_err(TenantError::Sqlite)?;
            let mut iter = stmt.query([]).map_err(TenantError::Sqlite)?;
            while let Some(row) = iter.next().map_err(TenantError::Sqlite)? {
                let id_bytes: Vec<u8> = row.get(0).map_err(TenantError::Sqlite)?;
                let nonce_bytes: Vec<u8> = row.get(1).map_err(TenantError::Sqlite)?;
                let ct: Vec<u8> = row.get(2).map_err(TenantError::Sqlite)?;
                rows.push((slice_to_uuid(&id_bytes)?, slice_to_nonce(&nonce_bytes)?, ct));
            }
        }
        let mut tenants = Vec::with_capacity(rows.len());
        for (id, nonce, ct) in rows {
            let aad = tenant_aad(id);
            let pt = decrypt_aead(&self.payload_key, &nonce, &ct, &aad)?;
            let tenant: Tenant = serde_json::from_slice(&pt)
                .map_err(|_| TenantError::Persistence("tenant payload is not valid JSON"))?;
            if tenant.id.as_uuid() != id {
                return Err(TenantError::Persistence("tenant id does not match its row"));
            }
            tenants.push(tenant);
        }
        Ok(tenants)
    }

    fn load_members(&self) -> Result<Vec<TenantMember>> {
        let mut out = Vec::new();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT tenant_id, user_id, role, status, created_at, updated_at
                 FROM tenant_members ORDER BY tenant_id ASC, created_at ASC",
            )
            .map_err(TenantError::Sqlite)?;
        let mut iter = stmt.query([]).map_err(TenantError::Sqlite)?;
        while let Some(row) = iter.next().map_err(TenantError::Sqlite)? {
            let tenant_bytes: Vec<u8> = row.get(0).map_err(TenantError::Sqlite)?;
            let user_bytes: Vec<u8> = row.get(1).map_err(TenantError::Sqlite)?;
            let role_tag: String = row.get(2).map_err(TenantError::Sqlite)?;
            let status_tag: String = row.get(3).map_err(TenantError::Sqlite)?;
            let created_at_ms: i64 = row.get(4).map_err(TenantError::Sqlite)?;
            let updated_at_ms: i64 = row.get(5).map_err(TenantError::Sqlite)?;
            let tenant_id = slice_to_uuid(&tenant_bytes)?;
            let user_id = slice_to_uuid(&user_bytes)?;
            let role = parse_relation(&role_tag)?;
            let status = parse_member_status(&status_tag)?;
            let created_at = ms_to_dt(created_at_ms)?;
            let updated_at = ms_to_dt(updated_at_ms)?;
            out.push(TenantMember {
                tenant_id,
                user_id,
                role,
                status,
                provisioned_at: created_at,
                updated_at,
            });
        }
        Ok(out)
    }

    fn persist_tenant(&mut self, tenant: &Tenant) -> Result<()> {
        Self::persist_tenant_in(&self.conn, &self.payload_key, tenant)
    }

    fn persist_tenant_in(conn: &Connection, payload_key: &AeadKey, tenant: &Tenant) -> Result<()> {
        let payload = serde_json::to_vec(tenant)
            .map_err(|_| TenantError::Persistence("tenant payload could not be serialised"))?;
        let nonce = random_nonce();
        let id = tenant.id.as_uuid();
        let aad = tenant_aad(id);
        let ct = encrypt_aead(payload_key, &nonce, &payload, &aad)?;
        let root_key_bytes = tenant.config.root_key.handle.as_bytes().to_vec();
        conn.execute(
            "INSERT INTO tenants
                (id, name, status, created_at, updated_at, deleted_at,
                 root_key_ref, nonce, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                status = excluded.status,
                updated_at = excluded.updated_at,
                root_key_ref = excluded.root_key_ref,
                nonce = excluded.nonce,
                payload = excluded.payload",
            params![
                id.as_bytes().to_vec(),
                &tenant.name,
                tenant.status.as_str(),
                tenant.created_at.timestamp_millis(),
                tenant.updated_at.timestamp_millis(),
                // deleted_at is stamped by `delete()` after
                // the upsert so we don't accidentally clear
                // it on a routine status flip.
                Option::<i64>::None,
                root_key_bytes,
                nonce.to_vec(),
                ct,
            ],
        )
        .map_err(TenantError::Sqlite)?;
        Ok(())
    }

    fn persist_config(&mut self, id: TenantId, config: &TenantConfig) -> Result<()> {
        Self::persist_config_in(&self.conn, id, config)
    }

    fn persist_config_in(conn: &Connection, id: TenantId, config: &TenantConfig) -> Result<()> {
        let storage_json = serde_json::to_string(&config.storage)
            .map_err(|_| TenantError::Persistence("storage config could not be serialised"))?;
        let synthesis_json = serde_json::to_string(&config.synthesis)
            .map_err(|_| TenantError::Persistence("synthesis config could not be serialised"))?;
        let key_ref_bytes = config.root_key.handle.as_bytes().to_vec();
        conn.execute(
            "INSERT INTO tenant_configs
                (tenant_id, encryption_key_ref, storage_config, synthesis_config)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(tenant_id) DO UPDATE SET
                encryption_key_ref = excluded.encryption_key_ref,
                storage_config = excluded.storage_config,
                synthesis_config = excluded.synthesis_config",
            params![
                id.as_uuid().as_bytes().to_vec(),
                key_ref_bytes,
                storage_json,
                synthesis_json,
            ],
        )
        .map_err(TenantError::Sqlite)?;
        Ok(())
    }

    fn persist_member(&mut self, member: &TenantMember) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO tenant_members
                    (tenant_id, user_id, role, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(tenant_id, user_id) DO UPDATE SET
                    role = excluded.role,
                    status = excluded.status,
                    updated_at = excluded.updated_at",
                params![
                    member.tenant_id.as_bytes().to_vec(),
                    member.user_id.as_bytes().to_vec(),
                    member.role.as_str(),
                    member.status.as_str(),
                    member.provisioned_at.timestamp_millis(),
                    member.updated_at.timestamp_millis(),
                ],
            )
            .map_err(TenantError::Sqlite)?;
        Ok(())
    }
}

fn tenant_aad(id: Uuid) -> Vec<u8> {
    let mut aad = b"tenant_row:v1:".to_vec();
    aad.extend_from_slice(id.as_bytes());
    aad
}

fn random_nonce() -> [u8; AEAD_NONCE_LEN] {
    let mut n = [0u8; AEAD_NONCE_LEN];
    // `rand::thread_rng()` was renamed to `rand::rng()` in rand 0.9.
    rand::rng().fill_bytes(&mut n);
    n
}

fn slice_to_uuid(b: &[u8]) -> Result<Uuid> {
    if b.len() != 16 {
        return Err(TenantError::Persistence("uuid column has wrong width"));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(b);
    Ok(Uuid::from_bytes(bytes))
}

fn slice_to_nonce(b: &[u8]) -> Result<[u8; AEAD_NONCE_LEN]> {
    if b.len() != AEAD_NONCE_LEN {
        return Err(TenantError::Persistence("nonce column has wrong width"));
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

fn ms_to_dt(ms: i64) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).ok_or(TenantError::Persistence(
        "timestamp column out of range for DateTime<Utc>",
    ))
}

fn parse_relation(tag: &str) -> Result<Relation> {
    match tag {
        "owner" => Ok(Relation::Owner),
        "admin" => Ok(Relation::Admin),
        "editor" => Ok(Relation::Editor),
        "member" => Ok(Relation::Member),
        "viewer" => Ok(Relation::Viewer),
        "synthesizer" => Ok(Relation::Synthesizer),
        "proposer" => Ok(Relation::Proposer),
        _ => Err(TenantError::Persistence("unknown relation tag on disk")),
    }
}

fn parse_member_status(tag: &str) -> Result<TenantMemberStatus> {
    match tag {
        "active" => Ok(TenantMemberStatus::Active),
        "suspended" => Ok(TenantMemberStatus::Suspended),
        "removed" => Ok(TenantMemberStatus::Removed),
        _ => Err(TenantError::Persistence(
            "unknown member-status tag on disk",
        )),
    }
}

// Silence unused-import warnings for re-exports used only by the
// public methods above (kept as a single insertion point so future
// schema upgrades don't have to chase warnings).
#[allow(dead_code)]
fn _unused_imports(_s: TenantStatus) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::MASTER_KEY_LEN;
    use tempfile::NamedTempFile;

    fn fixture_key() -> MasterKey {
        let mut k: MasterKey = [0u8; MASTER_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "deterministic test key seed; i < MASTER_KEY_LEN < 256"
            )]
            let byte = (i & 0xFF) as u8;
            *b = byte.wrapping_mul(11);
        }
        k
    }

    #[test]
    fn round_trip_persists_and_rehydrates() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let id_a;
        let id_b;
        let user_a = Uuid::new_v4();
        let user_b = Uuid::new_v4();
        {
            let mut reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
            id_a = reg.create("alpha", TenantConfig::new()).unwrap();
            id_b = reg.create("beta", TenantConfig::new()).unwrap();
            reg.add_member(id_a, user_a, Relation::Owner).unwrap();
            reg.add_member(id_a, user_b, Relation::Editor).unwrap();
            reg.add_member(id_b, user_a, Relation::Member).unwrap();
            assert_eq!(reg.registry().len(), 2);
            assert_eq!(reg.persisted_tenant_count().unwrap(), 2);
            assert_eq!(reg.persisted_member_count().unwrap(), 3);
        }

        let reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
        assert_eq!(reg.registry().len(), 2);
        let t_a = reg.registry().get(id_a).unwrap();
        let t_b = reg.registry().get(id_b).unwrap();
        assert_eq!(t_a.name, "alpha");
        assert_eq!(t_b.name, "beta");
        assert_eq!(reg.registry().list_members(id_a).len(), 2);
        assert_eq!(reg.registry().list_members(id_b).len(), 1);
        let m_a_user_a = reg.registry().get_member(id_a, user_a).unwrap();
        assert_eq!(m_a_user_a.role, Relation::Owner);
    }

    #[test]
    fn lifecycle_mirrors_to_disk() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let id = {
            let mut reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
            let id = reg.create("alpha", TenantConfig::new()).unwrap();
            reg.suspend(id).unwrap();
            id
        };

        {
            let reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
            assert_eq!(
                reg.registry().get(id).unwrap().status,
                TenantStatus::Suspended
            );
        }

        {
            let mut reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
            reg.activate(id).unwrap();
            reg.delete(id).unwrap();
        }

        let reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
        let tenant = reg.registry().get(id).unwrap();
        assert_eq!(tenant.status, TenantStatus::Deleted);
        assert!(tenant.config.root_key.destroyed);

        // The delete path runs the payload upsert, the config
        // upsert, and the `deleted_at` stamp inside a single
        // SQLite transaction. Read every on-disk artefact back to
        // prove all three commits actually landed (the transaction
        // semantics make the three writes atomic; this assertion
        // makes sure none of them was silently dropped).
        let deleted_at: Option<i64> = reg
            .conn
            .query_row(
                "SELECT deleted_at FROM tenants WHERE id = ?1",
                params![id.as_uuid().as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(deleted_at.is_some(), "delete must stamp deleted_at");
        let status_on_disk: String = reg
            .conn
            .query_row(
                "SELECT status FROM tenants WHERE id = ?1",
                params![id.as_uuid().as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status_on_disk, TenantStatus::Deleted.as_str());
        let config_rows: i64 = reg
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tenant_configs WHERE tenant_id = ?1",
                params![id.as_uuid().as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(config_rows, 1, "config row must exist post-delete");
    }

    #[test]
    fn member_role_update_is_persisted() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();
        let user = Uuid::new_v4();

        let id = {
            let mut reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
            let id = reg.create("alpha", TenantConfig::new()).unwrap();
            reg.add_member(id, user, Relation::Viewer).unwrap();
            reg.update_role(id, user, Relation::Editor).unwrap();
            id
        };

        let reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
        let m = reg.registry().get_member(id, user).unwrap();
        assert_eq!(m.role, Relation::Editor);
    }

    #[test]
    fn member_removal_keeps_audit_row() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();
        let user = Uuid::new_v4();

        let id = {
            let mut reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
            let id = reg.create("alpha", TenantConfig::new()).unwrap();
            reg.add_member(id, user, Relation::Member).unwrap();
            reg.remove_member(id, user).unwrap();
            id
        };

        let reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
        let m = reg.registry().get_member(id, user).unwrap();
        assert_eq!(m.status, TenantMemberStatus::Removed);
    }

    #[test]
    fn wrong_master_key_fails_to_open() {
        let tmp = NamedTempFile::new().unwrap();
        let key_a = fixture_key();
        let mut key_b: MasterKey = [0u8; MASTER_KEY_LEN];
        for (i, b) in key_b.iter_mut().enumerate() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "deterministic test key seed; i < MASTER_KEY_LEN < 256"
            )]
            let byte = (i & 0xFF) as u8;
            *b = byte.wrapping_add(73);
        }

        {
            let mut reg = PersistentTenantRegistry::open(tmp.path(), &key_a).unwrap();
            reg.create("alpha", TenantConfig::new()).unwrap();
        }

        let err = PersistentTenantRegistry::open(tmp.path(), &key_b).unwrap_err();
        assert!(
            matches!(err, TenantError::Persistence(_) | TenantError::Sqlite(_)),
            "expected an open failure for the wrong key, got {err:?}",
        );
    }

    #[test]
    fn crash_recovery_keeps_committed_state() {
        let tmp = NamedTempFile::new().unwrap();
        let key = fixture_key();

        let id_a = {
            let mut reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
            let id_a = reg.create("alpha", TenantConfig::new()).unwrap();
            // Drop simulates a crash before any further mutation;
            // SQLite commits per `execute`, so `alpha` survives.
            drop(reg);
            id_a
        };

        let mut reg = PersistentTenantRegistry::open(tmp.path(), &key).unwrap();
        assert!(reg.registry().get(id_a).is_ok());
        let id_b = reg.create("beta", TenantConfig::new()).unwrap();
        assert!(reg.registry().get(id_b).is_ok());
        assert_eq!(reg.persisted_tenant_count().unwrap(), 2);
    }
}
