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

#[test]
fn opens_v13_database_and_upgrades_to_current_with_evidence_fts_cjk_backfilled() {
    // v13 -> v14 specifically exercises the
    // `evidence_fts_cjk` (trigram) virtual table + the
    // `migrate_v14_backfill_evidence_fts_cjk` backfill that
    // walks every row of `evidence_fts.content` and re-inserts
    // any row whose plaintext contains a CJK Han / Hiragana /
    // Katakana / Thai codepoint.
    //
    // `build_legacy_fixture` re-uses the modern `SCHEMA_SQL`,
    // which already includes the `CREATE VIRTUAL TABLE IF NOT
    // EXISTS evidence_fts_cjk` statement, so a freshly ingested
    // CJK body would already be present in `evidence_fts_cjk`
    // and the v14 migration's idempotent skip branch would
    // short-circuit (`existing_cjk_rows > 0`). To meaningfully
    // exercise the backfill arm we ingest some CJK content
    // under the modern schema (so it lives in `evidence_fts`
    // *and* `evidence_fts_cjk`), then explicitly DROP the v14
    // table to put the database in a true pre-v14 shape on
    // disk before re-stamping `user_version = 13` and re-opening.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let cjk_body = "今日の重要な会議の議事録です";
    let latin_body = b"shipment ETA monday morning sharp";

    // Seed the database under modern schema, then reshape.
    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store at SCHEMA_VERSION");
        store
            .ingest(
                scope,
                cjk_body.as_bytes(),
                Some("source:cjk"),
                ImportanceClass::Important,
            )
            .expect("ingest cjk row");
        store
            .ingest(
                scope,
                latin_body,
                Some("source:latin"),
                ImportanceClass::Important,
            )
            .expect("ingest latin row");
    }

    // Drop the v14 companion table so the database is back to a
    // true pre-v14 shape on disk (only `evidence_fts` exists).
    // The v13 store wouldn't have had this table at all, so the
    // backfill arm of `migrate_v14_backfill_evidence_fts_cjk`
    // must re-discover every CJK row from `evidence_fts.content`.
    {
        let conn = open_sqlcipher(&path);
        conn.execute_batch("DROP TABLE evidence_fts_cjk;")
            .expect("drop evidence_fts_cjk to reach v13 shape");
        // Confirm the table really is gone before re-opening.
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'evidence_fts_cjk'",
                [],
                |r| r.get(0),
            )
            .expect("query sqlite_master for evidence_fts_cjk");
        assert_eq!(
            exists, 0,
            "test setup must leave the schema without evidence_fts_cjk before reopen"
        );
        conn.pragma_update(None, "user_version", 13_i64)
            .expect("re-stamp user_version=13 after reshape");
    }

    // Re-open: SCHEMA_SQL bootstraps the empty evidence_fts_cjk,
    // then apply_migration(14) walks evidence_fts.content and
    // re-inserts the CJK row.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open v13 db (post-reshape) must run v14 migration");

    // user_version stamped forward, original rows still readable,
    // FTS index over the Latin row still works.
    assert_eq!(
        read_user_version(&path),
        SCHEMA_VERSION,
        "post-migration user_version must equal SCHEMA_VERSION"
    );
    let evidence: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get(0))
        .expect("count evidence rows");
    assert_eq!(
        evidence, 2,
        "both seeded rows must survive the v13 -> v14 upgrade"
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
        "FTS5 unicode61 index over the Latin row must still match after v14 upgrade"
    );

    // The migration must have created `evidence_fts_cjk` and
    // back-filled it with the CJK row from `evidence_fts.content`.
    let cjk_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .expect("count cjk rows after upgrade");
    assert_eq!(
        cjk_rows, 1,
        "v13 -> v14 backfill must re-insert the CJK row into evidence_fts_cjk"
    );

    // And the pure-Latin row must NOT have been backfilled
    // (it has no CJK / Thai codepoints, so it would only
    // inflate the trigram index for no recall benefit).
    let latin_in_cjk: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts_cjk \
             WHERE evidence_fts_cjk MATCH 'shipment'",
            [],
            |r| r.get(0),
        )
        .expect("query cjk table for latin term");
    assert_eq!(
        latin_in_cjk, 0,
        "v13 -> v14 backfill must skip pure-Latin rows"
    );

    // End-to-end: the public search_fts API now returns the
    // CJK row for a CJK substring query that pre-v14 returned
    // nothing.
    let hits = store
        .search_fts(scope, "重要な会議", 10)
        .expect("search_fts post-upgrade");
    assert_eq!(
        hits.len(),
        1,
        "search_fts must hit the back-filled CJK row after v14 upgrade"
    );
}

#[test]
fn v14_migration_is_idempotent_on_already_populated_database() {
    // The v14 migration's idempotency guard
    // (`existing_cjk_rows > 0`) is what makes re-running the
    // migration safe on a database where SCHEMA_SQL already
    // bootstrapped `evidence_fts_cjk` directly (fresh-DB path
    // or a previously-completed v14 upgrade). This test pins
    // the contract: re-stamping `user_version = 13` on a
    // v14-shaped database with rows in `evidence_fts_cjk`
    // re-runs the migration on next open *without* producing
    // duplicate rows.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let cjk_body = "今日の重要な会議の議事録です";

    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store");
        store
            .ingest(
                scope,
                cjk_body.as_bytes(),
                Some("source:cjk-idem"),
                ImportanceClass::Important,
            )
            .expect("ingest cjk row");
        // Sanity: row is in evidence_fts_cjk.
        let cjk_before: i64 = store
            .raw_conn()
            .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
            .expect("count cjk rows before downgrade");
        assert_eq!(cjk_before, 1);
    }

    // Re-stamp user_version=13 so the next open re-runs the v14
    // migration despite `evidence_fts_cjk` already being populated.
    stamp_user_version(&path, 13);
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open with user_version=13 must re-run v14 migration");

    let cjk_after: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .expect("count cjk rows after re-migration");
    assert_eq!(
        cjk_after, 1,
        "idempotent v14 re-migration must not duplicate evidence_fts_cjk rows"
    );
    assert_eq!(read_user_version(&path), SCHEMA_VERSION);
}

#[test]
fn v14_migration_streams_backfill_across_multiple_chunks_without_data_loss() {
    // Regression test for a follow-up
    // (`migrate_v14_backfill_evidence_fts_cjk` previously loaded
    // the entire `evidence_fts` table into a single `Vec`). The
    // fix paginates the read in chunks of `MIGRATION_CHUNK_SIZE`
    // (1_000 rows). This test seeds `MIGRATION_CHUNK_SIZE + 500`
    // (1_500) distinct CJK rows directly into `evidence_fts` to
    // force the backfill loop to traverse at least two chunk
    // boundaries, then verifies:
    //
    //   * every seeded CJK row appears in `evidence_fts_cjk`
    //     after the migration (no rows dropped at the chunk
    //     boundary),
    //   * no duplicate rows are emitted (rowid cursor advances
    //     strictly forward across chunks),
    //   * the public `search_fts` API still works against the
    //     migrated index for a CJK query.
    //
    // The seeded rows are written directly via the raw SQLCipher
    // connection rather than the full `EvidenceStore::ingest`
    // pipeline because the migration only reads
    // `evidence_fts.content` / `evidence_fts.evidence_id` /
    // `evidence_fts.scope_id` and never joins back to the
    // `evidence` table — so an FTS-table-only seed is a faithful
    // proxy for a pre-v14 corpus and runs in well under a second
    // even at 1_500 rows.
    use uuid::Uuid;

    // The v14 backfill streams in chunks of 1_000 rows; 1_500
    // rows guarantees the loop straddles at least one chunk
    // boundary. Tracked as an `i64` from the start so we can
    // compare to `SELECT COUNT(*)` results without a usize→i64
    // cast (which clippy correctly flags as a potential wrap
    // hazard on 32-bit targets).
    const SEEDED_ROWS: i64 = 1_500;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let scope_bytes = scope.as_uuid().as_bytes().to_vec();

    // Bring the database to its modern shape, then strip the v14
    // companion table so the migration must actually run on next
    // open. Mirrors the setup used by
    // `opens_v13_database_and_upgrades_to_current_with_evidence_fts_cjk_backfilled`.
    {
        let _store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store at SCHEMA_VERSION");
    }
    {
        let conn = open_sqlcipher(&path);
        conn.execute_batch("DROP TABLE evidence_fts_cjk;")
            .expect("drop evidence_fts_cjk to reach v13 shape");
        // Direct-INSERT a CJK body per row. Each body shares the
        // common substring `重要な会議` so a single search query
        // can sweep all of them; the suffix `#{n}` makes every
        // body unique so a duplicate-emit bug in the migration
        // would inflate `evidence_fts_cjk` past the SEEDED_ROWS
        // count.
        let tx = conn
            .unchecked_transaction()
            .expect("begin tx for bulk seed");
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO evidence_fts (content, evidence_id, scope_id) \
                     VALUES (?1, ?2, ?3)",
                )
                .expect("prepare bulk-seed insert");
            for n in 0..SEEDED_ROWS {
                let eid = Uuid::new_v4().as_bytes().to_vec();
                let body = format!("重要な会議の議事録 #{n}");
                insert
                    .execute(rusqlite::params![body, eid, scope_bytes])
                    .expect("seed evidence_fts row");
            }
        }
        tx.commit().expect("commit bulk seed");

        // Re-stamp user_version=13 so the next open re-walks the
        // v14 migration over the seeded rows.
        conn.pragma_update(None, "user_version", 13_i64)
            .expect("re-stamp user_version=13 after bulk seed");

        // Sanity: confirm we actually wrote SEEDED_ROWS into
        // evidence_fts and that the companion table is gone.
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM evidence_fts", [], |r| r.get(0))
            .expect("count evidence_fts after seed");
        assert_eq!(
            fts_count, SEEDED_ROWS,
            "bulk seed must populate evidence_fts with SEEDED_ROWS rows"
        );
        let companion_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'evidence_fts_cjk'",
                [],
                |r| r.get(0),
            )
            .expect("inspect sqlite_master for evidence_fts_cjk");
        assert_eq!(
            companion_exists, 0,
            "evidence_fts_cjk must be absent before re-open so the migration backfill arm runs"
        );
    }

    // Re-open. SCHEMA_SQL re-creates `evidence_fts_cjk` empty;
    // `migrate_v14_backfill_evidence_fts_cjk` walks
    // `evidence_fts` in chunks and inserts every CJK row.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open v13-shaped db with SEEDED_ROWS rows must run v14 migration");

    // Every seeded row must have been backfilled. Strict
    // equality (not >=) is what catches a chunk-boundary drop or
    // a duplicate-emit bug.
    let cjk_total: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .expect("count evidence_fts_cjk after multi-chunk backfill");
    assert_eq!(
        cjk_total, SEEDED_ROWS,
        "multi-chunk v14 backfill must produce exactly one evidence_fts_cjk row per seeded \
         evidence_fts row (rowid cursor advanced past a chunk boundary, no rows dropped, no rows \
         duplicated)"
    );

    // Search via the public API on the shared substring — every
    // seeded row should be a hit.
    let seeded_rows_usize = usize::try_from(SEEDED_ROWS).expect("SEEDED_ROWS fits in usize");
    let hits = store
        .search_fts(scope, "重要な会議", seeded_rows_usize)
        .expect("search_fts post-multi-chunk migration");
    assert_eq!(
        hits.len(),
        seeded_rows_usize,
        "public search_fts must surface every back-filled CJK row after multi-chunk migration"
    );
}

#[test]
fn opens_v14_database_and_upgrades_to_current_with_evidence_fts_bigram_backfilled() {
    // v14 -> v15 specifically exercises the `evidence_fts_bigram`
    // virtual table + the `migrate_v15_backfill_evidence_fts_bigram`
    // backfill that walks every row of `evidence_fts.content` and
    // re-inserts any row whose plaintext contains a CJK Han /
    // Hiragana / Katakana / Thai codepoint, transforming the body
    // into its precomputed-bigram form on the way in.
    //
    // Same setup shape as the v13 -> v14 test: ingest under the
    // modern schema, then DROP the v15 companion table and
    // re-stamp `user_version = 14` to put the database in a true
    // pre-v15 shape on disk before re-opening.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let cjk_body = "今日の重要な会議の議事録です";
    let latin_body = b"shipment ETA monday morning sharp";

    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store at SCHEMA_VERSION");
        store
            .ingest(
                scope,
                cjk_body.as_bytes(),
                Some("source:cjk"),
                ImportanceClass::Important,
            )
            .expect("ingest cjk row");
        store
            .ingest(
                scope,
                latin_body,
                Some("source:latin"),
                ImportanceClass::Important,
            )
            .expect("ingest latin row");
    }

    // Drop the v15 companion table so the database is back to a
    // true pre-v15 shape on disk. The v14 store wouldn't have had
    // this table at all, so the backfill arm of
    // `migrate_v15_backfill_evidence_fts_bigram` must re-discover
    // every CJK row from `evidence_fts.content`.
    {
        let conn = open_sqlcipher(&path);
        conn.execute_batch("DROP TABLE evidence_fts_bigram;")
            .expect("drop evidence_fts_bigram to reach v14 shape");
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'evidence_fts_bigram'",
                [],
                |r| r.get(0),
            )
            .expect("query sqlite_master for evidence_fts_bigram");
        assert_eq!(
            exists, 0,
            "test setup must leave the schema without evidence_fts_bigram before reopen"
        );
        conn.pragma_update(None, "user_version", 14_i64)
            .expect("re-stamp user_version=14 after reshape");
    }

    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open v14 db (post-reshape) must run v15 migration");

    assert_eq!(
        read_user_version(&path),
        SCHEMA_VERSION,
        "post-migration user_version must equal SCHEMA_VERSION"
    );

    // The migration must have created `evidence_fts_bigram` and
    // back-filled it with the CJK row from `evidence_fts.content`.
    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .expect("count bigram rows after upgrade");
    assert_eq!(
        bigram_rows, 1,
        "v14 -> v15 backfill must re-insert the CJK row into evidence_fts_bigram"
    );

    // Defensive content-column inspection (closes a follow-up
    // finding #6: prior assertion only verified row count). The
    // backfilled `content` column must hold the precomputed-bigram
    // string emitted by `crate::bigram::compute_cjk_bigrams` over
    // the original CJK body — not the raw body, and not an empty
    // string. Pinning a sample of expected bigrams catches a
    // future regression where the migration accidentally writes
    // the trigram-shaped string (or the raw body) into the bigram
    // table while still satisfying the COUNT-based idempotency
    // guard.
    let stored_bigram_content: String = store
        .raw_conn()
        .query_row("SELECT content FROM evidence_fts_bigram", [], |row| {
            row.get(0)
        })
        .expect("read evidence_fts_bigram.content after upgrade");
    assert!(
        !stored_bigram_content.is_empty(),
        "v14 -> v15 backfill must populate evidence_fts_bigram.content"
    );
    assert_ne!(
        stored_bigram_content, cjk_body,
        "v14 -> v15 backfill must transform the body into bigram form, not store raw content"
    );
    for expected_bigram in ["会議", "議事", "今日", "重要"] {
        assert!(
            stored_bigram_content.contains(expected_bigram),
            "backfilled bigram content must contain '{expected_bigram}' \
             from `{cjk_body}` (was: `{stored_bigram_content}`)"
        );
    }

    // End-to-end: the 2-codepoint CJK query that the trigram lane
    // cannot serve (because of the 3-codepoint floor) must now
    // return the back-filled row through the bigram lane. 「会議」
    // is exactly 2 codepoints and appears verbatim in the seeded
    // body, so this is the canonical 2-char CJK gap-closure probe.
    let hits = store
        .search_fts(scope, "会議", 10)
        .expect("search_fts post-upgrade for 2-char query");
    assert_eq!(
        hits.len(),
        1,
        "v14 -> v15 backfill must make 2-char CJK queries hit via the bigram lane"
    );

    // Latin row sanity: a body that contains zero CJK / Thai
    // codepoints must NOT be back-filled into `evidence_fts_bigram`
    // (the routing predicate gates writes identically on the
    // migration path and the steady-state ingest path). Cross-
    // checks the storage-cost invariant so the migration cannot
    // silently bloat the bigram lane with Latin rows.
    let total_evidence_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts", [], |r| r.get(0))
        .expect("count evidence_fts rows after upgrade");
    assert!(
        total_evidence_rows >= 2,
        "expected at least the cjk + latin rows in evidence_fts"
    );
    let latin_bytes_len = latin_body.len();
    assert!(
        latin_bytes_len > 0,
        "latin_body fixture must be non-empty to make the next assertion meaningful"
    );
    let bigram_count_after: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .expect("recount bigram rows for latin guard");
    assert_eq!(
        bigram_count_after, 1,
        "v14 -> v15 backfill must NOT insert the pure-Latin row into evidence_fts_bigram"
    );
}

#[test]
fn v15_migration_is_idempotent_on_already_populated_database() {
    // Idempotency guard for the v15 migration mirrors the v14
    // contract: re-stamping `user_version = 14` on a v15-shaped
    // database with rows in `evidence_fts_bigram` re-runs the
    // migration on next open *without* producing duplicate rows.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let cjk_body = "今日の重要な会議の議事録です";

    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store");
        store
            .ingest(
                scope,
                cjk_body.as_bytes(),
                Some("source:cjk-idem-bigram"),
                ImportanceClass::Important,
            )
            .expect("ingest cjk row");
        let bigram_before: i64 = store
            .raw_conn()
            .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
            .expect("count bigram rows before downgrade");
        assert_eq!(bigram_before, 1);
    }

    stamp_user_version(&path, 14);
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open with user_version=14 must re-run v15 migration");

    let bigram_after: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .expect("count bigram rows after re-migration");
    assert_eq!(
        bigram_after, 1,
        "idempotent v15 re-migration must not duplicate evidence_fts_bigram rows"
    );
    assert_eq!(read_user_version(&path), SCHEMA_VERSION);
}

#[test]
fn v15_migration_streams_backfill_across_multiple_chunks_without_data_loss() {
    // Same chunked-streaming regression as the v14 sibling test,
    // pinning the v15 backfill's per-chunk forward-progress and
    // no-duplicate-emit invariants.
    use uuid::Uuid;

    // The v15 backfill streams in chunks of 1_000 rows; 1_500
    // rows guarantees the loop straddles at least one chunk
    // boundary.
    const SEEDED_ROWS: i64 = 1_500;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let scope_bytes = scope.as_uuid().as_bytes().to_vec();

    {
        let _store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store at SCHEMA_VERSION");
    }
    {
        let conn = open_sqlcipher(&path);
        conn.execute_batch("DROP TABLE evidence_fts_bigram;")
            .expect("drop evidence_fts_bigram to reach v14 shape");
        let tx = conn
            .unchecked_transaction()
            .expect("begin tx for bulk seed");
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO evidence_fts (content, evidence_id, scope_id) \
                     VALUES (?1, ?2, ?3)",
                )
                .expect("prepare bulk-seed insert");
            for n in 0..SEEDED_ROWS {
                let eid = Uuid::new_v4().as_bytes().to_vec();
                let body = format!("重要な会議の議事録 #{n}");
                insert
                    .execute(rusqlite::params![body, eid, scope_bytes])
                    .expect("seed evidence_fts row");
            }
        }
        tx.commit().expect("commit bulk seed");

        conn.pragma_update(None, "user_version", 14_i64)
            .expect("re-stamp user_version=14 after bulk seed");

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM evidence_fts", [], |r| r.get(0))
            .expect("count evidence_fts after seed");
        assert_eq!(
            fts_count, SEEDED_ROWS,
            "bulk seed must populate evidence_fts with SEEDED_ROWS rows"
        );
        let companion_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'evidence_fts_bigram'",
                [],
                |r| r.get(0),
            )
            .expect("inspect sqlite_master for evidence_fts_bigram");
        assert_eq!(
            companion_exists, 0,
            "evidence_fts_bigram must be absent before re-open so the migration backfill arm runs"
        );
    }

    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open v14-shaped db with SEEDED_ROWS rows must run v15 migration");

    let bigram_total: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .expect("count evidence_fts_bigram after multi-chunk backfill");
    assert_eq!(
        bigram_total, SEEDED_ROWS,
        "multi-chunk v15 backfill must produce exactly one evidence_fts_bigram row per seeded \
         evidence_fts row (rowid cursor advanced past a chunk boundary, no rows dropped, no rows \
         duplicated)"
    );

    let seeded_rows_usize = usize::try_from(SEEDED_ROWS).expect("SEEDED_ROWS fits in usize");
    let hits = store
        .search_fts(scope, "会議", seeded_rows_usize)
        .expect("search_fts post-multi-chunk migration for 2-char query");
    assert_eq!(
        hits.len(),
        seeded_rows_usize,
        "public search_fts must surface every back-filled CJK row via the bigram lane (2-char \
         query that the trigram floor blocks) after multi-chunk migration"
    );
}

// ----------------------------------------------------------------------
//  / schema v16 — Symmetric stopword stripping migration
// ----------------------------------------------------------------------
//
// The v16 migration deletes every row from `evidence_fts_cjk` and
// `evidence_fts_bigram` (the two recall-lane shadow tables) and
// rewrites them from `evidence_fts.content` with the
// stopword strip applied. Re-running the migration twice produces
// the same final state (idempotency-by-reconstruction).

#[test]
fn opens_v15_database_and_upgrades_to_current_with_recall_lanes_re_stripped() {
    // Setup: ingest a CJK row under the modern schema (so
    // `evidence_fts.content` carries the original plaintext
    // verbatim), then re-stamp `user_version = 15` to put the
    // database in a pre-v16 shape on disk. The v16 migration
    // must re-tokenise the bigram + trigram lanes with the
    //  stopword strip applied.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    // Body contains the genitive particle `の` — a
    // stopword — so the post-migration bigram content must not
    // contain `日本` adjacent to `の` (the stopword is replaced
    // with a single ASCII space and the lane filters whitespace
    // out before windowing).
    let cjk_body = "日本のオリンピック";

    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store at SCHEMA_VERSION");
        store
            .ingest(
                scope,
                cjk_body.as_bytes(),
                Some("source:cjk-v16-strip"),
                ImportanceClass::Important,
            )
            .expect("ingest cjk row");
    }

    // Reshape: re-stamp `user_version = 15` AND seed the v15
    // shape of the recall lanes (unstripped, so we can prove the
    // v16 migration replaces them). We do this by inserting a
    // sentinel that contains the `の` codepoint into the bigram
    // lane — if the v16 migration is broken, the sentinel will
    // survive into the post-migration state.
    {
        let conn = open_sqlcipher(&path);
        // Overwrite the bigram lane with the pre-v16 (unstripped)
        // shape: the body's bigrams INCLUDING the `の` particle.
        // The migration must DELETE this row and rewrite from
        // `evidence_fts.content` with the strip applied.
        conn.execute_batch("DELETE FROM evidence_fts_bigram;")
            .expect("delete pre-v16 bigram seeds");
        // Re-insert with the earlier (unstripped) bigram
        // form so we can prove the migration overwrites it. The
        // pre-v16 form contained the `の` particle in the
        // computed bigrams (e.g. `日本`, `本の`, `のオ`, ...).
        // We use a sentinel bigram `本の` that would never appear
        // in the post-v16 stripped form.
        conn.execute_batch(
            "INSERT INTO evidence_fts_bigram(content, evidence_id, scope_id) \
             SELECT '日本 本の のオ オリ リン ンピ ピッ ック', evidence_id, scope_id \
             FROM evidence_fts;",
        )
        .expect("seed pre-v16 unstripped bigram row");
        conn.pragma_update(None, "user_version", 15_i64)
            .expect("re-stamp user_version=15 after reshape");
    }

    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open v15 db (post-reshape) must run v16 migration");

    assert_eq!(
        read_user_version(&path),
        SCHEMA_VERSION,
        "post-migration user_version must equal SCHEMA_VERSION"
    );

    // The migration must have rewritten the bigram lane: the
    // sentinel `本の` must be gone, replaced by the stripped
    // form (where `の` was replaced with whitespace before
    // bigram computation).
    let stored_bigram_content: String = store
        .raw_conn()
        .query_row("SELECT content FROM evidence_fts_bigram", [], |row| {
            row.get(0)
        })
        .expect("read evidence_fts_bigram.content after v16 upgrade");
    assert!(
        !stored_bigram_content.contains("本の"),
        "v16 migration must rewrite bigram lane WITHOUT the pre-strip `本の` window \
         (was: `{stored_bigram_content}`)"
    );
    assert!(
        !stored_bigram_content.contains("のオ"),
        "v16 migration must rewrite bigram lane WITHOUT the pre-strip `のオ` window \
         (was: `{stored_bigram_content}`)"
    );
    // The post-strip content-side bigrams must still be present
    // — proving the migration didn't simply purge the row.
    for expected_bigram in ["オリ", "リン", "ンピ", "ピッ", "ック"] {
        assert!(
            stored_bigram_content.contains(expected_bigram),
            "post-v16 stripped bigram content must contain `{expected_bigram}` \
             (was: `{stored_bigram_content}`)"
        );
    }

    // End-to-end: the symmetric-strip read path must find the
    // body via the bigram lane on a query that omits the
    // particle.
    let hits = store
        .search_fts(scope, "日本オリンピック", 10)
        .expect("post-v16 read path must find body via stripped bigram windows");
    assert_eq!(
        hits.len(),
        1,
        "post-v16 symmetric strip must let particle-free query match particle-containing body",
    );
}

#[test]
fn v16_migration_is_idempotent_on_already_stripped_database() {
    // The v16 migration deletes-and-rewrites the recall lanes
    // every time, so re-running it must produce the same final
    // state. This is the "idempotency-by-reconstruction"
    // contract: there is no "already populated" guard on the
    // v16 path — instead, the migration is defined such that
    // running it twice from any starting shape produces the
    // same final shape.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let cjk_body = "日本のオリンピック";

    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open fresh store");
        store
            .ingest(
                scope,
                cjk_body.as_bytes(),
                Some("source:cjk-v16-idem"),
                ImportanceClass::Important,
            )
            .expect("ingest cjk row");
        let bigram_before: i64 = store
            .raw_conn()
            .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
            .expect("count bigram rows before downgrade");
        assert_eq!(bigram_before, 1);
    }

    // Re-stamp user_version=15 to trigger re-running the v16
    // migration on next open. The post-state must be identical
    // — exactly one bigram row, exactly the stripped content.
    stamp_user_version(&path, 15);
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open with user_version=15 must re-run v16 migration");

    let bigram_after: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .expect("count bigram rows after re-migration");
    assert_eq!(
        bigram_after, 1,
        "idempotent v16 re-migration must not duplicate evidence_fts_bigram rows"
    );
    let trigram_after: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .expect("count trigram rows after re-migration");
    assert_eq!(
        trigram_after, 1,
        "idempotent v16 re-migration must not duplicate evidence_fts_cjk rows"
    );
    assert_eq!(read_user_version(&path), SCHEMA_VERSION);

    // The post-strip bigram content must NOT contain the
    // pre-strip `本の` window even after re-running.
    let stored_bigram_content: String = store
        .raw_conn()
        .query_row("SELECT content FROM evidence_fts_bigram", [], |row| {
            row.get(0)
        })
        .expect("read evidence_fts_bigram.content after idempotent re-migration");
    assert!(
        !stored_bigram_content.contains("本の"),
        "idempotent v16 re-migration must keep the strip applied (was: `{stored_bigram_content}`)"
    );
}
