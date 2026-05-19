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
    assert_eq!((evidence, body_store, ring, fts), (0, 0, 0, 0));
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
// Phase A.5 (Gap 4) — durable cryptographic-forgetting tombstones.
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
    let store =
        EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
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
    let store =
        EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();

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
    assert_eq!(wrap_count, 1, "same-scope re-ingest must not duplicate wraps");

    // Both evidence rows must read the same body.
    assert_eq!(store.read_body(r1.evidence_id).unwrap(), body);
    assert_eq!(store.read_body(r2.evidence_id).unwrap(), body);
}
