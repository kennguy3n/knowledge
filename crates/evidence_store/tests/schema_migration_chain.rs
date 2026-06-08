//! Schema migration chain test.
//!
//! Verifies that the migration path from v1 → current schema version
//! completes without error and preserves data integrity at each step.
//!
//! The test creates a minimal v1-shape database by hand (bypassing
//! `EvidenceStore::open`), then opens it with the real store to
//! exercise the full migration chain.

use evidence_store::EvidenceStoreConfig;
use rusqlite::Connection;
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xA5; 32];

fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Create a minimal v1 database with the core tables that existed
/// at schema version 1. This is the baseline from which all
/// migrations run.
fn create_v1_database(path: &std::path::Path) {
    let page_key = crypto::derive_key(&MASTER_KEY, b"sqlcipher:store:v1").expect("derive key");
    let hex_key = bytes_to_hex(&page_key);
    let conn = Connection::open(path).expect("open raw");
    conn.pragma_update(None, "key", format!("x'{hex_key}'"))
        .expect("set key");

    // v1 schema: evidence, body_store, ring_buffer, evidence_fts.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS evidence (
            id         BLOB PRIMARY KEY,
            scope_id   BLOB NOT NULL,
            content_hash BLOB NOT NULL,
            source_ref TEXT,
            acl_pointer TEXT,
            importance INTEGER NOT NULL DEFAULT 2,
            storage_path INTEGER NOT NULL DEFAULT 0,
            body       BLOB,
            nonce      BLOB,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS body_store (
            content_hash BLOB PRIMARY KEY,
            body         BLOB NOT NULL,
            nonce        BLOB NOT NULL,
            ref_count    INTEGER NOT NULL DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS ring_buffer (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            scope_id     BLOB NOT NULL,
            body         BLOB NOT NULL,
            nonce        BLOB NOT NULL,
            payload_size INTEGER NOT NULL,
            created_at   INTEGER NOT NULL
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS evidence_fts USING fts5(
            content,
            evidence_id UNINDEXED,
            scope_id    UNINDEXED,
            tokenize    = 'unicode61 remove_diacritics 2'
        );
        ",
    )
    .expect("create v1 schema");

    // Stamp as version 1.
    conn.pragma_update(None, "user_version", 1)
        .expect("stamp v1");

    // Insert a sentinel evidence row so we can verify data survives
    // the migration chain.
    let scope_bytes = uuid::Uuid::new_v4().as_bytes().to_vec();
    let id_bytes = uuid::Uuid::new_v4().as_bytes().to_vec();
    let hash_bytes = vec![0xABu8; 32];
    conn.execute(
        "INSERT INTO evidence (id, scope_id, content_hash, importance, storage_path, created_at)
         VALUES (?1, ?2, ?3, 2, 0, 1000)",
        rusqlite::params![id_bytes, scope_bytes, hash_bytes],
    )
    .expect("insert sentinel");

    // Verify the sentinel.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 1, "sentinel row must exist in v1 DB");
}

#[test]
fn v1_database_migrates_to_current_version() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("v1_migration.db");

    // Create a v1-shape database.
    create_v1_database(&db_path);

    // Open with the real store — this triggers the full migration
    // chain from v1 → SCHEMA_VERSION.
    let store =
        evidence_store::EvidenceStore::open(&db_path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open v1 DB with migrations");

    // Verify the sentinel row survived the migration chain.
    let conn = store.raw_conn();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get(0))
        .expect("count after migration");
    assert_eq!(
        count, 1,
        "sentinel row must survive the full migration chain"
    );

    // Verify key tables introduced by later migrations exist.
    // v4: forgotten_scopes
    let _: i64 = conn
        .query_row("SELECT COUNT(*) FROM forgotten_scopes", [], |r| r.get(0))
        .expect("forgotten_scopes table must exist after migration");

    // v5: body_store_key_wraps
    let _: i64 = conn
        .query_row("SELECT COUNT(*) FROM body_store_key_wraps", [], |r| {
            r.get(0)
        })
        .expect("body_store_key_wraps table must exist");

    // v6: scope_deks
    let _: i64 = conn
        .query_row("SELECT COUNT(*) FROM scope_deks", [], |r| r.get(0))
        .expect("scope_deks table must exist");

    // v7: memory_objects
    let _: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_objects", [], |r| r.get(0))
        .expect("memory_objects table must exist");

    // v8: epoch_tombstones
    let _: i64 = conn
        .query_row("SELECT COUNT(*) FROM epoch_tombstones", [], |r| r.get(0))
        .expect("epoch_tombstones table must exist");

    // Verify schema version is current.
    let version: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("read user_version");
    assert_eq!(
        version,
        evidence_store::schema::SCHEMA_VERSION,
        "schema version must be current after migration"
    );
}
