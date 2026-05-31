//! Schema-migration fixture tests for the [`EvidenceStore`].
//!
//! `crates/evidence_store/src/store.rs` runs the on-open migration
//! loop from `detected_version + 1 ..= SCHEMA_VERSION`, calling
//! `apply_migration` for each step. Every individual delta is
//! covered by inline unit tests in `store.rs`, but the *integration*
//! — open a database that says `user_version = N`, where N is an
//! older release — was not exercised end-to-end. That gap matters
//! because the substrate is shipped to long-lived devices, so a
//! `0.1.x` release must keep upgrading databases that were written
//! by `0.1.0`, `0.1.1`, …, regardless of how many `CREATE * IF NOT
//! EXISTS` statements got added to `SCHEMA_SQL` along the way.
//!
//! ## Test strategy
//!
//! Spinning up a *true* historical binary inside the test process
//! is impossible (we no longer have the v1 or v3 SQL files in the
//! tree). Instead, the fixture tests rely on two facts about the
//! current bootstrap:
//!
//! 1. `EvidenceStore::open` writes the bootstrap schema first
//!    (every statement is `CREATE * IF NOT EXISTS`, so it is
//!    safe to re-run), then walks the migration loop, then
//!    finally stamps `user_version = SCHEMA_VERSION`. Resetting
//!    `user_version` *backwards* on a fresh database produces a
//!    forward-compatible legacy fixture: the schema is the
//!    superset of every historical version, and the on-open
//!    migration loop will re-run the destructive deltas (v3) and
//!    the additive no-ops (v1, v2, v4..=v11) one more time. Each
//!    delta is required to be idempotent — that contract is
//!    documented at the top of `apply_migration` — so a clean
//!    re-run on the modern schema must finish with `user_version
//!    == SCHEMA_VERSION` and zero data loss.
//!
//! 2. `EvidenceStore::open` rejects a database whose
//!    `user_version` is strictly greater than the current
//!    `SCHEMA_VERSION`. Setting `user_version = SCHEMA_VERSION + 1`
//!    must therefore surface an `EvidenceError::Schema` rather
//!    than silently downgrading.
//!
//! Together, the two cases exercise the full preflight contract
//! at `store.rs:253-260`: legacy databases are upgraded
//! transparently, future-versioned databases are rejected with a
//! deterministic error.

use std::path::Path;

use crypto::derive_key;
use evidence_store::{
    schema::SCHEMA_VERSION, EvidenceError, EvidenceStore, EvidenceStoreConfig, ImportanceClass,
    ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use rusqlite::Connection;
use tempfile::tempdir;

/// Same key pattern as `store_integration.rs` so the fixture path
/// uses the SQLCipher decrypt the test suite expects.
const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// HKDF context used by `EvidenceStore::open` to derive the
/// SQLCipher page-encryption key from the master key. Mirror this
/// exactly or the test-side connection cannot decrypt the
/// database the production code just wrote.
const SQLCIPHER_KEY_CONTEXT: &[u8] = b"sqlcipher:store:v1";

/// Open a SQLCipher connection against an existing database file.
///
/// Mirrors the page-size / KDF-iter pragmas in
/// `EvidenceStore::open` so we can drop the version stamp on a
/// database the production code has just closed. The page key is
/// derived from `MASTER_KEY` via the substrate's HKDF wrap rather
/// than passed in raw — this matches the production code at
/// `store.rs:206` so the two paths see the same on-disk bytes.
fn open_sqlcipher(path: &Path) -> Connection {
    let page_key =
        derive_key(&MASTER_KEY, SQLCIPHER_KEY_CONTEXT).expect("derive sqlcipher page key");
    let key_pragma = format!("x'{}'", hex(&page_key));
    let conn = Connection::open(path).expect("open existing db");
    conn.pragma_update(None, "key", key_pragma.as_str())
        .expect("set sqlcipher key");
    conn.pragma_update(None, "cipher_page_size", 4096_i64)
        .expect("set cipher_page_size");
    conn.pragma_update(None, "kdf_iter", 256_000_i64)
        .expect("set kdf_iter");
    // Round-trip a trivial SELECT so any wrong-key failure surfaces
    // here rather than at the first PRAGMA write below.
    let _: i32 = conn
        .query_row("SELECT 1", [], |r| r.get(0))
        .expect("sqlcipher key did not unlock the test database");
    conn
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Stamp `user_version = target` on an existing SQLCipher database.
///
/// Used by the legacy-version fixtures to put a freshly-built
/// store back into a "this is actually a v1/v3 file" posture so
/// the next `open` re-walks the migration loop.
fn stamp_user_version(path: &Path, target: i32) {
    let conn = open_sqlcipher(path);
    conn.pragma_update(None, "user_version", target)
        .expect("stamp user_version");
    drop(conn);
}

/// Read `user_version` from an existing SQLCipher database.
fn read_user_version(path: &Path) -> i32 {
    let conn = open_sqlcipher(path);
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version")
}

/// Build a fresh database, insert a couple of evidence rows, then
/// stamp the supplied legacy `user_version`. Returns the temp dir
/// (kept alive so the file path stays valid) and the path to the
/// stamped database.
fn build_legacy_fixture(legacy_version: i32) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    // 1. Open the store at the current `SCHEMA_VERSION`. This
    //    produces the modern schema on disk.
    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store");

        // 2. Insert one inline-path row and one body-store-path row
        //    so the post-migration assertions have something to
        //    verify decryption / FTS behaviour against.
        let scope = ScopeId::new_v4();
        let inline_body = b"shipment ETA monday";
        assert!(inline_body.len() <= DEFAULT_INLINE_THRESHOLD_BYTES);
        store
            .ingest(
                scope,
                inline_body,
                Some("source:inline"),
                ImportanceClass::Important,
            )
            .expect("ingest inline row");

        let large_body = vec![b'Q'; DEFAULT_INLINE_THRESHOLD_BYTES * 4];
        store
            .ingest(
                scope,
                &large_body,
                Some("source:large"),
                ImportanceClass::Important,
            )
            .expect("ingest body-store row");
    }
    // Connection is dropped, SQLCipher writes the file out.

    // 3. Stamp the legacy version. From here on the file looks
    //    (to the next open) like a database written by the
    //    `legacy_version` release that was then schema-upgraded
    //    in place — which is exactly the upgrade contract we want
    //    to test.
    stamp_user_version(&path, legacy_version);
    assert_eq!(read_user_version(&path), legacy_version);

    (dir, path)
}

/// Walk the four required end-state assertions for a re-opened
/// legacy database.
///
/// * `user_version` is stamped forward to `SCHEMA_VERSION`.
/// * The two seeded rows are still decryptable.
/// * FTS5 search over their plaintext returns at least one hit.
/// * `body_store` still has the dedup'd large-row payload.
fn assert_post_migration_state(path: &Path, store: &EvidenceStore) {
    let stamped = read_user_version(path);
    assert_eq!(
        stamped, SCHEMA_VERSION,
        "post-migration user_version must equal SCHEMA_VERSION"
    );

    let evidence: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get(0))
        .expect("count evidence rows");
    assert_eq!(evidence, 2, "both seeded rows must survive the upgrade");

    let body_store_count = store
        .body_store_count()
        .expect("body_store count after upgrade");
    assert_eq!(
        body_store_count, 1,
        "body_store must still hold the large-body row after upgrade"
    );

    let fts_hits: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH 'shipment'",
            [],
            |r| r.get(0),
        )
        .expect("FTS query after upgrade");
    assert!(
        fts_hits >= 1,
        "FTS5 index over the inline plaintext must still match after upgrade"
    );
}

#[test]
fn opens_v1_database_and_upgrades_to_current() {
    // v1 was the very first shipped schema — pre-`evidence_embeddings`
    // composite PK, pre-`forgotten_scopes`, pre-`scope_deks`, pre-
    // everything. The on-open migration loop must walk v2 → v3 →
    // ... → SCHEMA_VERSION. v3 is the first destructive delta, so
    // re-running it on the modern schema exercises the "table is
    // already in the target shape" detect-and-skip branch in
    // `migrate_evidence_embeddings_to_composite_pk`.
    let (_dir, path) = build_legacy_fixture(1);

    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open legacy v1 db must succeed");
    assert_post_migration_state(&path, &store);
}

#[test]
fn opens_v3_database_and_upgrades_to_current() {
    // v3 is the first destructive migration (composite PK on
    // `evidence_embeddings`). A v3 → SCHEMA_VERSION upgrade re-walks
    // every additive delta (v4..=SCHEMA_VERSION) and skips the
    // already-applied v3 step via the idempotent detect-and-skip
    // branch noted above.
    let (_dir, path) = build_legacy_fixture(3);

    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open legacy v3 db must succeed");
    assert_post_migration_state(&path, &store);
}

#[test]
fn opens_v12_database_and_upgrades_to_current_with_language_tag_column() {
    // v12 -> v13 specifically exercises the
    // `ALTER TABLE evidence ADD COLUMN language_tag TEXT` step.
    // `build_legacy_fixture` re-uses the modern `SCHEMA_SQL`, which
    // already includes `language_tag` in the `CREATE TABLE` body,
    // so the v13 migration's idempotent skip branch would
    // short-circuit. To meaningfully exercise the ADD COLUMN arm
    // we reshape the table back to its pre-v13 layout via the
    // documented SQLite "12-step ALTER" pattern (rename + recreate
    // + copy + drop) — `ALTER TABLE ... DROP COLUMN` is rejected
    // by SQLCipher when the affected table participates in triggers
    // or covering indexes that the planner cannot prove are
    // column-independent, so the rename-recreate-copy pattern is
    // the portable way to get a true pre-v13 shape on disk.
    let (_dir, path) = build_legacy_fixture(12);
    {
        let conn = open_sqlcipher(&path);
        conn.execute_batch(
            "BEGIN;\n\
             DROP TRIGGER IF EXISTS evidence_no_update;\n\
             DROP TRIGGER IF EXISTS evidence_no_delete;\n\
             ALTER TABLE evidence RENAME TO evidence_v12_tmp;\n\
             CREATE TABLE evidence (\n\
                 id              BLOB    PRIMARY KEY,\n\
                 scope_id        BLOB    NOT NULL,\n\
                 content_hash    BLOB    NOT NULL,\n\
                 body            BLOB,\n\
                 body_ref        BLOB,\n\
                 nonce           BLOB,\n\
                 source_ref      TEXT,\n\
                 acl_pointer     TEXT,\n\
                 importance      INTEGER NOT NULL,\n\
                 storage_path    INTEGER NOT NULL,\n\
                 created_at      INTEGER NOT NULL\n\
             );\n\
             INSERT INTO evidence (id, scope_id, content_hash, body, body_ref,\n\
                                   nonce, source_ref, acl_pointer, importance,\n\
                                   storage_path, created_at)\n\
                 SELECT id, scope_id, content_hash, body, body_ref, nonce,\n\
                        source_ref, acl_pointer, importance, storage_path,\n\
                        created_at FROM evidence_v12_tmp;\n\
             DROP TABLE evidence_v12_tmp;\n\
             COMMIT;",
        )
        .expect("reshape evidence back to v12 layout");
        // Confirm the column really is gone before re-opening.
        let mut stmt = conn
            .prepare("PRAGMA table_info(evidence)")
            .expect("prepare table_info");
        let mut rows = stmt.query([]).expect("query table_info");
        let mut found = false;
        while let Some(row) = rows.next().expect("next row") {
            let name: String = row.get(1).expect("col name");
            if name == "language_tag" {
                found = true;
            }
        }
        assert!(
            !found,
            "test setup must leave the schema without language_tag before reopen"
        );
        conn.pragma_update(None, "user_version", 12_i64)
            .expect("re-stamp user_version=12 after reshape");
    }

    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open v12 db (post-reshape) must run v13 migration");
    assert_post_migration_state(&path, &store);

    // The migration must have re-added the language_tag column.
    let conn = open_sqlcipher(&path);
    let mut stmt = conn
        .prepare("PRAGMA table_info(evidence)")
        .expect("prepare table_info after upgrade");
    let mut rows = stmt.query([]).expect("query table_info after upgrade");
    let mut found = false;
    while let Some(row) = rows.next().expect("next row") {
        let name: String = row.get(1).expect("col name");
        if name == "language_tag" {
            found = true;
        }
    }
    assert!(
        found,
        "v12 -> v13 upgrade must restore the language_tag column"
    );
}

#[test]
fn rejects_database_written_by_a_newer_binary() {
    // The preflight guard at `store.rs:253-260` rejects any
    // `user_version > SCHEMA_VERSION` database so a downgrade
    // never silently rewrites a future schema. The
    // `EvidenceStoreError::Schema` arm carries a stable diagnostic
    // string; the test just verifies the open call fails rather
    // than pinning the message.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    {
        let _store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("seed a fresh store");
    }
    // Pretend the database was written by a hypothetical future
    // release with schema version `SCHEMA_VERSION + 1`.
    stamp_user_version(&path, SCHEMA_VERSION + 1);

    let result = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default());
    match result {
        Err(EvidenceError::Schema(_)) => {
            // expected — preflight guard fired.
        }
        Err(other) => panic!("expected EvidenceError::Schema for newer-version db, got {other:?}"),
        Ok(_) => {
            panic!("expected open to fail for newer-version db (user_version > SCHEMA_VERSION)")
        }
    }
}
