//! Integration tests for the SQLCipher-backed [`EvidenceStore`].
//!
//! These exercise the full ingest → encrypt → store → decrypt → read
//! roundtrip, the size-threshold routing, content-hash deduplication,
//! ring-buffer FIFO eviction, FTS5 search, and the append-only
//! invariant on the `evidence` table.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ImportanceClassifier, LexiconClassifier,
    ScopeId, StoragePath, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xA5; 32];

fn fresh_store() -> (tempfile::TempDir, EvidenceStore) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");
    (dir, store)
}

#[test]
fn schema_creates_required_tables() {
    let (_dir, store) = fresh_store();
    // Basic smoke: SELECT COUNT(*) on every expected table.
    let conn = store.raw_conn();
    let evidence: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get(0))
        .unwrap();
    let body_store: i64 = conn
        .query_row("SELECT COUNT(*) FROM body_store", [], |r| r.get(0))
        .unwrap();
    let ring: i64 = conn
        .query_row("SELECT COUNT(*) FROM ring_buffer", [], |r| r.get(0))
        .unwrap();
    let fts: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence_fts", [], |r| r.get(0))
        .unwrap();
    // Phase 1.2 / schema v14: trigram-tokenised companion FTS5
    // table for CJK / Thai content. Bootstrapped alongside
    // `evidence_fts` by `SCHEMA_SQL`.
    let fts_cjk: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .unwrap();
    assert_eq!((evidence, body_store, ring, fts, fts_cjk), (0, 0, 0, 0, 0));
}

#[test]
fn ingest_inline_path_for_small_useful_message() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let body = b"Friday is the deadline for the migration.";
    assert!(body.len() <= DEFAULT_INLINE_THRESHOLD_BYTES);

    let res = store
        .ingest(
            scope,
            body,
            Some("source:msg-1"),
            ImportanceClass::Important,
        )
        .unwrap();
    assert_eq!(res.storage_path, StoragePath::Inline);

    let pt = store.read_body(res.evidence_id).unwrap();
    assert_eq!(pt, body);

    let row = store.get(res.evidence_id).unwrap().expect("row");
    assert_eq!(row.scope_id, scope);
    assert_eq!(row.importance, ImportanceClass::Important);
    assert_eq!(row.storage_path, StoragePath::Inline);
    assert_eq!(row.source_ref.as_deref(), Some("source:msg-1"));
    assert_eq!(row.content_hash, res.content_hash);

    // Inline path leaves body_store empty.
    assert_eq!(store.body_store_count().unwrap(), 0);
}

#[test]
fn ingest_with_language_tag_round_trips_inline() {
    // Phase 1.3 / schema v13: the inline-path ingest API stamps the
    // optional BCP-47 primary subtag onto the row's `language_tag`
    // column and `EvidenceStore::get` round-trips it back through
    // `EvidenceRow::language_tag`.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let body = b"Friday is the deadline for the migration.";

    let res = store
        .ingest_with_language(
            scope,
            body,
            Some("source:msg-jp"),
            ImportanceClass::Important,
            Some("ja"),
        )
        .unwrap();
    assert_eq!(res.storage_path, StoragePath::Inline);
    let row = store.get(res.evidence_id).unwrap().expect("row");
    assert_eq!(row.language_tag.as_deref(), Some("ja"));
}

#[test]
fn ingest_with_language_tag_round_trips_body_table() {
    // Phase 1.3 / schema v13: the body-table-path ingest API stamps
    // the BCP-47 subtag onto the row's `language_tag` column, even
    // when the same body content is dedup-shared across scopes
    // (the language stamp lives on the per-scope `evidence` row, not
    // on the deduplicated `body_store` row).
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body = vec![b'Z'; DEFAULT_INLINE_THRESHOLD_BYTES + 1];

    let res_a = store
        .ingest_with_language(scope_a, &body, None, ImportanceClass::Useful, Some("en"))
        .unwrap();
    let res_b = store
        .ingest_with_language(scope_b, &body, None, ImportanceClass::Useful, Some("ja"))
        .unwrap();
    assert_eq!(res_a.storage_path, StoragePath::BodyTable);
    assert_eq!(res_b.storage_path, StoragePath::BodyTable);

    let row_a = store.get(res_a.evidence_id).unwrap().expect("row a");
    let row_b = store.get(res_b.evidence_id).unwrap().expect("row b");
    // The two rows share the body_store row but carry independent
    // language tags on their `evidence` rows.
    assert_eq!(row_a.content_hash, row_b.content_hash);
    assert_eq!(row_a.language_tag.as_deref(), Some("en"));
    assert_eq!(row_b.language_tag.as_deref(), Some("ja"));
}

#[test]
fn legacy_ingest_leaves_language_tag_null() {
    // Backwards-compatibility check: the legacy `ingest` shim does
    // not require callers to plumb a language tag through. Rows it
    // produces carry `language_tag = NULL`, which downstream
    // consumers MUST treat as "unknown".
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let body = b"Friday is the deadline for the migration.";

    let res = store
        .ingest(scope, body, None, ImportanceClass::Important)
        .unwrap();
    let row = store.get(res.evidence_id).unwrap().expect("row");
    assert_eq!(row.language_tag, None);
}

#[test]
fn ingest_body_table_path_for_large_body() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let body = vec![b'A'; DEFAULT_INLINE_THRESHOLD_BYTES * 4];

    let res = store
        .ingest(scope, &body, None, ImportanceClass::Useful)
        .unwrap();
    assert_eq!(res.storage_path, StoragePath::BodyTable);

    let pt = store.read_body(res.evidence_id).unwrap();
    assert_eq!(pt, body);

    assert_eq!(store.body_store_count().unwrap(), 1);
    assert_eq!(store.body_ref_count(&res.content_hash).unwrap(), Some(1));
}

#[test]
fn content_hash_dedup_shares_one_body_row() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body = vec![b'B'; DEFAULT_INLINE_THRESHOLD_BYTES + 1];

    let res1 = store
        .ingest(scope_a, &body, None, ImportanceClass::Useful)
        .unwrap();
    let res2 = store
        .ingest(scope_b, &body, None, ImportanceClass::Useful)
        .unwrap();
    let res3 = store
        .ingest(scope_a, &body, None, ImportanceClass::Useful)
        .unwrap();

    // Same plaintext → same content hash.
    assert_eq!(res1.content_hash, res2.content_hash);
    assert_eq!(res2.content_hash, res3.content_hash);

    // Three evidence rows, but only one body_store row, ref_count == 3.
    assert_eq!(store.evidence_count().unwrap(), 3);
    assert_eq!(store.body_store_count().unwrap(), 1);
    assert_eq!(store.body_ref_count(&res1.content_hash).unwrap(), Some(3));

    // Per-scope CEK wrapping: reading must succeed from evidence rows
    // in either scope via the scope's CEK wrap.
    assert_eq!(store.read_body(res1.evidence_id).unwrap(), body);
    assert_eq!(store.read_body(res2.evidence_id).unwrap(), body);
    assert_eq!(store.read_body(res3.evidence_id).unwrap(), body);

    // Verify per-scope CEK wraps exist (two scopes ⇒ two wraps).
    let wrap_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM body_store_key_wraps WHERE content_hash = ?1",
            rusqlite::params![res1.content_hash.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(wrap_count, 2, "two scopes must produce two CEK wraps");
}

#[test]
fn cross_scope_body_dedup_reads_succeed_from_both_scopes() {
    // Task 1: ingest the same large (>512 byte) body from two
    // different scopes, then verify both scopes' evidence ids can
    // decrypt the plaintext. This is the regression test for the
    // cross-scope body dedup decryption bug.
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body = vec![b'Z'; DEFAULT_INLINE_THRESHOLD_BYTES * 2 + 7];

    let res_a = store
        .ingest(scope_a, &body, None, ImportanceClass::Useful)
        .unwrap();
    let res_b = store
        .ingest(scope_b, &body, None, ImportanceClass::Useful)
        .unwrap();

    // Both evidence rows share a single body-store row.
    assert_eq!(res_a.content_hash, res_b.content_hash);
    assert_eq!(store.body_store_count().unwrap(), 1);
    assert_eq!(store.body_ref_count(&res_a.content_hash).unwrap(), Some(2));

    let pt_a = store.read_body(res_a.evidence_id).unwrap();
    let pt_b = store.read_body(res_b.evidence_id).unwrap();
    assert_eq!(pt_a, body);
    assert_eq!(pt_b, body);
    assert_eq!(pt_a, pt_b);
}

#[test]
fn ring_buffer_created_at_is_unix_epoch_seconds() {
    // Task 2: `RingBufferEntry.created_at` must be Unix epoch seconds,
    // matching the documented type and the `evidence` table's
    // `created_at` column.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    store.ring_buffer_insert(scope, b"a noise body").unwrap();
    let entries = store.ring_buffer_read_window(scope).unwrap();
    assert_eq!(entries.len(), 1);
    let ts = entries[0].created_at;
    // 1_700_000_000 ≈ 2023-11-14; 2_000_000_000 ≈ 2033-05-18. A
    // microsecond timestamp from the same wall-clock instant would be
    // ~1e15, well outside this band, so this assertion catches the
    // unit mismatch.
    assert!(
        (1_700_000_000..=2_000_000_000).contains(&ts),
        "ring buffer created_at {ts} is not a Unix epoch second"
    );
}

#[test]
fn noise_class_routes_to_ring_buffer_and_skips_evidence_table() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let res = store
        .ingest(scope, b"hi", None, ImportanceClass::Noise)
        .unwrap();
    assert_eq!(res.storage_path, StoragePath::RingBuffer);
    // No evidence row was created.
    assert_eq!(store.evidence_count().unwrap(), 0);

    let entries = store.ring_buffer_read_window(scope).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, b"hi");
}

#[test]
fn ring_buffer_fifo_eviction_under_cap() {
    let (_dir, mut store) = {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evidence.db");
        let cfg = EvidenceStoreConfig {
            ring_buffer_max_bytes: 256,
            ..Default::default()
        };
        let s = EvidenceStore::open(&path, &MASTER_KEY, cfg).unwrap();
        (dir, s)
    };
    let scope = ScopeId::new_v4();
    // Each entry is roughly 32 (body) + 24 (nonce) + 16 (tag) = 72 bytes payload
    // plus we count nonce in payload_size (24).
    for i in 0..10u8 {
        let body = vec![i; 32];
        store.ring_buffer_insert(scope, &body).unwrap();
    }
    let total = store.ring_buffer_current_size().unwrap();
    assert!(total <= 256, "ring buffer total {total} exceeds cap");
    let len = store.ring_buffer_len().unwrap();
    assert!(len < 10, "expected FIFO eviction, got {len} entries");

    // Surviving entries should be the most recently inserted ones.
    let entries = store.ring_buffer_read_window(scope).unwrap();
    let last = entries.last().unwrap();
    assert_eq!(last.body, vec![9u8; 32]);
}

#[test]
fn ring_buffer_clear_drops_everything() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    store.ring_buffer_insert(scope, b"payload").unwrap();
    assert_eq!(store.ring_buffer_len().unwrap(), 1);
    store.ring_buffer_clear().unwrap();
    assert_eq!(store.ring_buffer_len().unwrap(), 0);
    assert!(store.ring_buffer_read_window(scope).unwrap().is_empty());
}

#[test]
fn fts5_search_finds_ingested_text() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r1 = store
        .ingest(
            scope,
            b"The launch deadline for the export pipeline is May",
            None,
            ImportanceClass::Important,
        )
        .unwrap();
    let _ = store
        .ingest(
            scope,
            b"This is unrelated content about lunch and ducks",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let hits = store.search_fts(scope, "deadline", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], r1.evidence_id);

    // Substrate-canonical tokenizer is unicode61 with diacritic
    // folding, so we search lower-case and case shouldn't matter.
    let hits_case = store.search_fts(scope, "DEADLINE", 10).unwrap();
    assert_eq!(hits_case.len(), 1);
}

#[test]
fn append_only_constraint_rejects_update_and_delete() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let res = store
        .ingest(scope, b"a non-noise message", None, ImportanceClass::Useful)
        .unwrap();
    let id_bytes = res.evidence_id.as_uuid().as_bytes().to_vec();

    let update_err = store.raw_conn().execute(
        "UPDATE evidence SET source_ref = 'tampered' WHERE id = ?1",
        rusqlite::params![id_bytes.as_slice()],
    );
    assert!(update_err.is_err(), "UPDATE on evidence must be rejected");

    let delete_err = store.raw_conn().execute(
        "DELETE FROM evidence WHERE id = ?1",
        rusqlite::params![id_bytes.as_slice()],
    );
    assert!(delete_err.is_err(), "DELETE on evidence must be rejected");

    // Row is still readable.
    assert!(store.read_body(res.evidence_id).is_ok());
}

#[test]
fn classifier_drives_routing_end_to_end() {
    // The full pipeline: classifier picks the importance class, the
    // store routes accordingly.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let classifier = LexiconClassifier::english_default();

    let cases = [
        ("hi", StoragePath::RingBuffer),
        (
            "Friday is the deadline for the migration.",
            StoragePath::Inline,
        ),
        (
            // Long non-noise body.
            std::str::from_utf8(&[b'X'; 1024]).unwrap(),
            StoragePath::BodyTable,
        ),
    ];
    for (text, expected_path) in cases {
        let class = classifier.classify(text);
        let res = store.ingest(scope, text.as_bytes(), None, class).unwrap();
        assert_eq!(
            res.storage_path, expected_path,
            "text {text:?} routed unexpectedly: {:?}",
            res.storage_path
        );
    }
}

#[test]
fn data_persists_across_reopen() {
    // Open, write, drop. Re-open with the same master key — body must
    // decrypt cleanly. (This is the SQLCipher key-derivation contract:
    // same master key + same context → same page key.)
    let dir = tempdir().unwrap();
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let body = b"a persistent note".to_vec();

    let evidence_id = {
        let mut store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
        let res = store
            .ingest(scope, &body, None, ImportanceClass::Useful)
            .unwrap();
        res.evidence_id
    };

    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
    assert_eq!(store.read_body(evidence_id).unwrap(), body);
}

#[test]
fn wrong_master_key_fails_to_open() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("evidence.db");
    {
        let _store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
    }
    // Re-open with a different master key — SQLCipher should refuse
    // to unlock and `open` should bubble an error.
    let bad = [0xFF; 32];
    let result = EvidenceStore::open(&path, &bad, EvidenceStoreConfig::default());
    assert!(result.is_err(), "wrong master key must refuse to open");
}

// ---------------------------------------------------------------------
// Durable cryptographic-forgetting tombstones.
//
// `record_forgotten_scope` writes a row into `forgotten_scopes`, and
// `load_forgotten_scopes` returns the full set. The substrate uses these
// two methods to make the FFI runtime's per-process `DekRegistry` survive
// a process restart: every persisted tombstone is replayed into a fresh
// in-memory registry on `open_store`.
// ---------------------------------------------------------------------

#[test]
fn forgotten_scopes_persist_across_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    {
        let mut store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
        // No tombstones on a fresh DB.
        assert!(
            store.load_forgotten_scopes().unwrap().is_empty(),
            "fresh store must have no forgotten scopes"
        );

        store.record_forgotten_scope(scope_a).unwrap();
        store.record_forgotten_scope(scope_b).unwrap();

        let mut loaded = store.load_forgotten_scopes().unwrap();
        loaded.sort_by_key(|s| *s.as_uuid().as_bytes());
        let mut expected = vec![scope_a, scope_b];
        expected.sort_by_key(|s| *s.as_uuid().as_bytes());
        assert_eq!(loaded, expected);
    }

    // Re-open with the same master key — the tombstones must still be
    // there. This is the durability contract Gap 4 introduces.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
    let mut loaded = store.load_forgotten_scopes().unwrap();
    loaded.sort_by_key(|s| *s.as_uuid().as_bytes());
    let mut expected = vec![scope_a, scope_b];
    expected.sort_by_key(|s| *s.as_uuid().as_bytes());
    assert_eq!(
        loaded, expected,
        "forgotten scopes must survive a process restart"
    );
}

#[test]
fn record_forgotten_scope_is_idempotent() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    store.record_forgotten_scope(scope).unwrap();
    // INSERT OR IGNORE — re-recording the same scope must succeed and
    // not produce a duplicate row.
    store.record_forgotten_scope(scope).unwrap();
    store.record_forgotten_scope(scope).unwrap();

    let loaded = store.load_forgotten_scopes().unwrap();
    assert_eq!(loaded, vec![scope]);
}

// ---------------------------------------------------------------------
// WS1 — per-scope CEK wrapping regression tests.
//
// The body-store deduplication design shares a single encrypted body
// row across scopes. Each scope holds a per-scope "CEK wrap" — a
// Content Encryption Key wrapped under the scope's AEAD key. On
// `forget()` the wraps for the forgotten scope are purged. When no
// wraps remain for a content hash the body is cryptographically
// unrecoverable.
// ---------------------------------------------------------------------

#[test]
fn cek_wrap_forget_scope_a_leaves_scope_b_readable() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body = vec![b'C'; DEFAULT_INLINE_THRESHOLD_BYTES * 4];

    let (res_a, res_b) = {
        let mut store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
        let ra = store
            .ingest(scope_a, &body, None, ImportanceClass::Useful)
            .unwrap();
        let rb = store
            .ingest(scope_b, &body, None, ImportanceClass::Useful)
            .unwrap();
        assert_eq!(ra.content_hash, rb.content_hash);
        assert_eq!(store.body_store_count().unwrap(), 1);

        // Forget scope A — purge its CEK wraps.
        store.purge_body_key_wraps_for_scope(scope_a).unwrap();

        // Scope B must still be able to read the body.
        let pt = store.read_body(rb.evidence_id).unwrap();
        assert_eq!(pt, body, "scope B must still decrypt after scope A forgot");

        (ra, rb)
    };

    // Re-open with the same master key — scope B must survive.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
    let pt = store.read_body(res_b.evidence_id).unwrap();
    assert_eq!(pt, body, "scope B must survive a process restart");

    // Scope A's evidence row still exists (append-only table), but
    // attempting to read the body must fail because its CEK wrap is
    // gone.
    let err = store.read_body(res_a.evidence_id);
    assert!(
        err.is_err(),
        "scope A's body must be unrecoverable after forget"
    );
}

#[test]
fn cek_wrap_forget_both_scopes_makes_body_unrecoverable() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let content_hash;

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body = vec![b'D'; DEFAULT_INLINE_THRESHOLD_BYTES + 100];

    {
        let mut store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
        let ra = store
            .ingest(scope_a, &body, None, ImportanceClass::Useful)
            .unwrap();
        let _rb = store
            .ingest(scope_b, &body, None, ImportanceClass::Useful)
            .unwrap();
        content_hash = ra.content_hash;

        // Forget both scopes.
        store.purge_body_key_wraps_for_scope(scope_a).unwrap();
        store.purge_body_key_wraps_for_scope(scope_b).unwrap();

        // The body_store row should have been garbage-collected.
        assert_eq!(
            store.body_store_count().unwrap(),
            0,
            "orphaned body_store row must be garbage-collected"
        );
    }

    // Re-open and verify the body is gone at the storage level.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();

    // No CEK wraps remain.
    let wrap_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM body_store_key_wraps WHERE content_hash = ?1",
            rusqlite::params![content_hash.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(wrap_count, 0, "all CEK wraps must be purged");

    // Body store row must be gone too.
    assert_eq!(
        store.body_store_count().unwrap(),
        0,
        "body_store must be empty after both scopes forgot"
    );
}

#[test]
fn cek_wrap_same_scope_reingest_is_idempotent() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let body = vec![b'E'; DEFAULT_INLINE_THRESHOLD_BYTES + 1];

    let r1 = store
        .ingest(scope, &body, None, ImportanceClass::Useful)
        .unwrap();
    let r2 = store
        .ingest(scope, &body, None, ImportanceClass::Useful)
        .unwrap();
    assert_eq!(r1.content_hash, r2.content_hash);

    // Only one CEK wrap for the single scope.
    let wrap_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM body_store_key_wraps WHERE content_hash = ?1",
            rusqlite::params![r1.content_hash.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        wrap_count, 1,
        "same-scope re-ingest must not duplicate wraps"
    );

    // Both evidence rows must read the same body.
    assert_eq!(store.read_body(r1.evidence_id).unwrap(), body);
    assert_eq!(store.read_body(r2.evidence_id).unwrap(), body);
}

// ─────────────── with_transaction rollback / commit ───────────────

#[test]
fn with_transaction_commits_on_ok() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();

    store
        .with_transaction(|tx| {
            store.save_memory_blob_in_tx(tx, scope, "kind_a", b"{\"a\":1}")?;
            store.save_memory_blob_in_tx(tx, scope, "kind_b", b"{\"b\":2}")?;
            Ok(())
        })
        .expect("tx commit");

    // Both rows must be readable after commit.
    let a = store.load_memory_blob(scope, "kind_a").unwrap();
    let b = store.load_memory_blob(scope, "kind_b").unwrap();
    assert!(a.is_some(), "kind_a must survive commit");
    assert!(b.is_some(), "kind_b must survive commit");
}

#[test]
fn with_transaction_rolls_back_on_err() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Write one blob successfully, then inject an error before the
    // second write can land. The entire transaction must roll back.
    let result: evidence_store::Result<()> = store.with_transaction(|tx| {
        store.save_memory_blob_in_tx(tx, scope, "kind_ok", b"{\"ok\":true}")?;
        // Simulate a mid-sequence failure — the specific error
        // variant is irrelevant; only the rollback semantics matter.
        Err(evidence_store::EvidenceError::Schema(
            "injected failure for rollback test",
        ))
    });
    assert!(result.is_err(), "tx must propagate the injected error");

    // Neither the successful first write NOR the failed second write
    // may be readable — the entire transaction rolled back.
    let blob = store.load_memory_blob(scope, "kind_ok").unwrap();
    assert!(
        blob.is_none(),
        "rolled-back write must not be visible after tx abort"
    );
}

// ============================================================
// Phase 1.2 / schema v14 — CJK-aware FTS5 tokeniser tests.
//
// Pre-v14 the substrate's only lexical index used the FTS5
// `unicode61 remove_diacritics 2` tokeniser, which classifies CJK
// Han / Hiragana / Katakana / Thai codepoints as non-letter
// separators and emits zero tokens for those scripts. The new
// `evidence_fts_cjk` table indexes the same bodies with the
// built-in `trigram` tokeniser (overlapping 3-codepoint windows),
// so queries of ≥3 CJK / Thai characters can now hit. These tests
// pin the read / write / forget / rebuild contract of the
// dual-index design.
// ============================================================

#[test]
fn fts5_cjk_japanese_query_returns_hit() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "今日は良い天気です".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "良い天気", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "trigram index must find 4-char CJK substring"
    );
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_cjk_chinese_query_returns_hit() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "今天天气很好".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "今天天气", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_thai_query_returns_hit() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, "อากาศวันนี้ดี".as_bytes(), None, ImportanceClass::Useful)
        .unwrap();
    let hits = store.search_fts(scope, "วันนี้", 10).unwrap();
    assert_eq!(hits.len(), 1, "trigram must segment Thai");
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_pre_v14_latin_path_still_works_unchanged() {
    // Regression pin: the unicode61 lexical path that the v0..v13
    // substrate has always exposed must remain bit-identical
    // — same hit set, same exact-id, same ASCII case-folding.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            b"The launch deadline for the export pipeline is May",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "deadline", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], r.evidence_id);
    let hits_case = store.search_fts(scope, "DEADLINE", 10).unwrap();
    assert_eq!(hits_case.len(), 1);
}

#[test]
fn fts5_mixed_script_doc_searchable_by_both_scripts() {
    // A row whose body mixes Latin and CJK should be findable
    // via either tokeniser: the Latin term goes through
    // `evidence_fts` (unicode61) and the CJK substring through
    // `evidence_fts_cjk` (trigram). UNION dedupe on evidence_id
    // ensures the same row is returned exactly once.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "Project 計画書 review on Friday".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let hits_latin = store.search_fts(scope, "Project", 10).unwrap();
    assert_eq!(hits_latin.len(), 1);
    assert_eq!(hits_latin[0], r.evidence_id);

    let hits_cjk = store.search_fts(scope, "計画書", 10).unwrap();
    assert_eq!(hits_cjk.len(), 1);
    assert_eq!(hits_cjk[0], r.evidence_id);
}

#[test]
fn fts5_cjk_routing_is_body_derived_not_language_tag_derived() {
    // A row ingested without any language tag still gets routed
    // into the CJK FTS table iff its body contains CJK / Thai
    // codepoints — the write path keys off body content, not the
    // (Phase 1.3) `language_tag` column. This is what makes the
    // CJK index robust to a NULL or mis-detected language tag.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "今天天气很好".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    // Sanity: the row was inserted without a language tag.
    let language_tag: Option<String> = store
        .raw_conn()
        .query_row(
            "SELECT language_tag FROM evidence WHERE id = ?1",
            rusqlite::params![r.evidence_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        language_tag.is_none(),
        "ingest() must not stamp a language tag"
    );

    // Despite the NULL tag, the body landed in evidence_fts_cjk:
    let cjk_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cjk_rows, 1);
}

#[test]
fn fts5_pure_latin_does_not_consume_cjk_table_storage() {
    // A pure-Latin body must NOT be written to evidence_fts_cjk —
    // unicode61 already handles whitespace-segmented scripts and
    // adding a redundant trigram row would inflate the CJK index
    // size without recall benefit.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _ = store
        .ingest(
            scope,
            b"The launch deadline for the export pipeline is May",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let cjk_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        cjk_rows, 0,
        "pure-Latin body must not be written to evidence_fts_cjk"
    );
}

#[test]
fn fts5_trigram_2char_cjk_query_is_documented_floor() {
    // SQLite's built-in `trigram` tokeniser has a hard 3-codepoint
    // minimum for both indexed substrings and queries. A 2-char
    // CJK query like `天気` returns ∅ even when the substring is
    // present in the body. This is the known limitation flagged
    // in the schema v14 doc-comment; a future phase can register
    // a custom Rust-side bigram tokeniser via the `fts5_api` FFI
    // to close the gap. This test pins the current floor so any
    // future bigram-tokeniser work has a regression signal.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _ = store
        .ingest(
            scope,
            "今日は良い天気です".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "天気", 10).unwrap();
    assert_eq!(
        hits.len(),
        0,
        "2-char CJK query is below the trigram floor — \
         change this assertion only when a bigram-tokeniser \
         lands"
    );
    // …and the same query 1 char longer crosses the floor:
    let hits = store.search_fts(scope, "良い天気", 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn fts5_search_dedupes_when_both_tables_match_same_row() {
    // A mixed-script body where the Latin substring matches
    // unicode61 AND a CJK trigram substring matches trigram is
    // returned exactly once by `search_fts` — the UNION ALL +
    // GROUP BY contract in `EvidenceStore::search_fts` dedupes
    // on evidence_id.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "Project 計画書 launch review meeting".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    // A query that pure-unicode61 would match (`launch`)…
    let hits = store.search_fts(scope, "launch", 10).unwrap();
    assert_eq!(hits.len(), 1, "unicode61 branch returns single hit");
    assert_eq!(hits[0], r.evidence_id);

    // …and a multi-term query that hits BOTH branches
    // (`launch` against unicode61, `計画書` against trigram) must
    // still return the row exactly once.
    //
    // FTS5 boolean OR syntax: `term1 OR term2`.
    let hits = store.search_fts(scope, "launch OR 計画書", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "row matched by both branches must dedupe to single hit"
    );
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_unicode61_query_succeeds_even_when_trigram_branch_rejects_shape() {
    // Sweep-2 BUG-0001 regression. The SQLite trigram tokeniser docs
    // <https://www.sqlite.org/fts5.html#the_trigram_tokenizer> say
    // that trigram returns an *error* (not an empty result set)
    // when given any of:
    //   * a query term shorter than 3 characters,
    //   * a `NEAR(…)` expression,
    //   * a column filter,
    //   * a prefix-star match shorter than 3 codepoints.
    //
    // The dual-table search (`evidence_fts` UNION-style merged with
    // `evidence_fts_cjk`) splits the query across both branches.
    // The architectural invariant the post-bug-0001 fix pins is:
    // **a syntactically valid `unicode61` query never breaks
    // `search_fts` just because the same query happens to be a
    // shape that `trigram` rejects per the docs.** The trigram
    // branch is purely additive recall; any rusqlite error on it
    // is swallowed and the branch is treated as the empty set.
    //
    // We assert this by reaching past `.unwrap()` for every
    // documented-as-error shape — if the error propagated, every
    // case would panic. Empirically the bundled SQLite version
    // happens to be lenient and silently returns empty (or even
    // contributes hits) for some of these shapes rather than
    // erroring; the defensive containment still applies and
    // future-proofs against a bundled-SQLite upgrade that tightens
    // to the documented behaviour.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "Project 計画書 launch review meeting".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    // Case 1: 2-codepoint Latin term — explicitly below the trigram
    // floor per the docs. Reach past `.unwrap()`.
    let _r1 = store.search_fts(scope, "to", 10).unwrap();

    // Case 2: 1-codepoint query — below the floor.
    let _r2 = store.search_fts(scope, "a", 10).unwrap();

    // Case 3: `NEAR(...)` expression — trigram rejects NEAR per docs.
    // unicode61 supports it; both `launch` and `review` are in the body.
    let r3 = store
        .search_fts(scope, "NEAR(launch review, 5)", 10)
        .unwrap();
    assert_eq!(
        r3.len(),
        1,
        "NEAR matches in unicode61 MUST return the row even when trigram rejects NEAR"
    );
    assert_eq!(r3[0], r.evidence_id);

    // Case 4: column filter — trigram rejects column filters per docs.
    let r4 = store.search_fts(scope, "{content} : launch", 10).unwrap();
    assert_eq!(
        r4.len(),
        1,
        "column-filter matches in unicode61 MUST return the row even when trigram rejects column filters"
    );
    assert_eq!(r4[0], r.evidence_id);

    // Case 5: 2-codepoint CJK prefix-star match — below the trigram
    // prefix-floor per docs. Reach past `.unwrap()`. The hit count
    // depends on the bundled SQLite's leniency.
    let _r5 = store.search_fts(scope, "計画*", 10).unwrap();

    // Case 6: well-formed long-prefix query — sanity check that the
    // refactor still returns hits when both branches accept the
    // query.
    let r6 = store.search_fts(scope, "launch*", 10).unwrap();
    assert_eq!(r6.len(), 1, "long-prefix query still returns hit");
    assert_eq!(r6[0], r.evidence_id);
}

#[test]
fn fts5_trigram_branch_error_is_silently_swallowed_so_unicode61_results_survive() {
    // Sweep-2 BUG-0001 regression — directly proves the error
    // containment path. We inject a guaranteed trigram failure by
    // DROPing the `evidence_fts_cjk` table out from under the
    // search, then verify the `unicode61` branch's results still
    // flow through. This exercises the rusqlite error swallow in
    // `dual_fts_search` independent of whichever bundled SQLite
    // version's tolerance for short trigram terms.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "Project launch review meeting".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    // Sanity baseline before drop: unicode61 returns the hit.
    let baseline = store.search_fts(scope, "launch", 10).unwrap();
    assert_eq!(baseline.len(), 1);
    assert_eq!(baseline[0], r.evidence_id);

    // Force the trigram branch to fail by dropping the underlying
    // virtual table. Any subsequent `evidence_fts_cjk MATCH …`
    // prepare returns `Err(SqliteFailure(...))`.
    store
        .raw_conn()
        .execute("DROP TABLE evidence_fts_cjk", [])
        .expect("drop evidence_fts_cjk for test");

    // The unicode61 branch is unchanged; the trigram branch errors
    // on prepare. The defensive `dual_fts_search` swallows the
    // trigram error and surfaces only the unicode61 hit — the
    // public `search_fts` API does NOT propagate the trigram
    // failure.
    let hits = store
        .search_fts(scope, "launch", 10)
        .expect("trigram failure MUST NOT propagate; unicode61 result MUST survive");
    assert_eq!(
        hits.len(),
        1,
        "trigram-branch failure must be swallowed; unicode61 hit must still surface"
    );
    assert_eq!(hits[0], r.evidence_id);
}
