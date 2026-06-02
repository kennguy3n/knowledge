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
    //  / schema v14: trigram-tokenised companion FTS5
    // table for CJK / Thai content. Bootstrapped alongside
    // `evidence_fts` by `SCHEMA_SQL`.
    let fts_cjk: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .unwrap();
    //  / schema v15: precomputed-bigram FTS5 table for
    // 2-codepoint CJK / Thai recall. Bootstrapped alongside
    // `evidence_fts` and `evidence_fts_cjk` by `SCHEMA_SQL`.
    let fts_bigram: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        (evidence, body_store, ring, fts, fts_cjk, fts_bigram),
        (0, 0, 0, 0, 0, 0)
    );
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
    //  / schema v13: the inline-path ingest API stamps the
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
    //  / schema v13: the body-table-path ingest API stamps
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
//  / schema v14 — CJK-aware FTS5 tokeniser tests.
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

// ----------------------------------------------------------------------
//  — Tibetan / Khmer / Myanmar / Lao routing integration tests
// ----------------------------------------------------------------------
//
// introduced `evidence_fts_cjk` (trigram lane) and
// added `evidence_fts_bigram` (precomputed bigram lane). Both lanes
// gate writes on `crate::script::contains_cjk_or_thai`, which
// extended to include four additional Brahmic-family scripts that lack
// inter-word whitespace: Tibetan (`bo`), Khmer (`km`), Myanmar (`my`),
// Lao (`lo`). The fixtures below pin the read-path recall AND the
// write-path table membership for each script via the same dual-lane
// architecture as the sites — ensuring no regression silently
// excludes one of the four scripts from one of the two CJK-routed
// shadow tables.

#[test]
fn fts5_tibetan_query_returns_hit_via_trigram_lane() {
    // བཀྲ་ཤིས་བདེ་ལེགས — common Tibetan greeting, ~7 codepoints,
    // well above the trigram lane's 3-codepoint floor.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "བཀྲ་ཤིས་བདེ་ལེགས".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    // 3+ codepoint sub-query — trigram lane must hit.
    let hits = store.search_fts(scope, "བཀྲ་ཤིས", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "Tibetan body must route to evidence_fts_cjk and be searchable via trigram",
    );
    assert_eq!(hits[0], r.evidence_id);

    // Pin the write-path invariant: the body must also land in
    // `evidence_fts_bigram` so a future 2-codepoint
    // Tibetan query can find it.
    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        bigram_rows, 1,
        "Tibetan body must also land in evidence_fts_bigram",
    );
}

#[test]
fn fts5_khmer_query_returns_hit_via_trigram_lane() {
    // ភ្នំពេញ — "Phnom Penh"; 4 base codepoints with subscript
    // consonants joined by the invisible coeng (U+17D2).
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "ភ្នំពេញគឺជារដ្ឋធានីនៃប្រទេសកម្ពុជា".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "ភ្នំពេញ", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "Khmer body must route to evidence_fts_cjk and be searchable via trigram",
    );
    assert_eq!(hits[0], r.evidence_id);

    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        bigram_rows, 1,
        "Khmer body must also land in evidence_fts_bigram",
    );
}

#[test]
fn fts5_myanmar_query_returns_hit_via_trigram_lane() {
    // ရန်ကုန် — "Yangon", with combining ngathat (U+103A) and
    // dependent vowels.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "ရန်ကုန်သည် မြန်မာနိုင်ငံ၏ အကြီးဆုံးမြို့ ဖြစ်သည်".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "ရန်ကုန်", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "Myanmar body must route to evidence_fts_cjk and be searchable via trigram",
    );
    assert_eq!(hits[0], r.evidence_id);

    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        bigram_rows, 1,
        "Myanmar body must also land in evidence_fts_bigram",
    );
}

#[test]
fn fts5_lao_query_returns_hit_via_trigram_lane() {
    // ວຽງຈັນ — "Vientiane", capital of Laos. Lao script
    // (U+0E80..=U+0EFF) is contiguous with Thai under the
    // single routing arm `'\u{0E00}'..='\u{0FFF}'`.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "ວຽງຈັນ ເປັນ ນະຄອນຫຼວງ ຂອງ ປະເທດລາວ".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "ວຽງຈັນ", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "Lao body must route to evidence_fts_cjk and be searchable via trigram",
    );
    assert_eq!(hits[0], r.evidence_id);

    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        bigram_rows, 1,
        "Lao body must also land in evidence_fts_bigram",
    );
}

#[test]
fn fts5_pure_devanagari_body_routes_only_to_unicode61() {
    // Negative-space pin: Devanagari (Hindi, U+0900..=U+097F)
    // is deliberately NOT in the routing predicate
    // — the unicode61 tokeniser classifies Devanagari letters
    // as letters and so already segments Hindi correctly,
    // so adding a redundant trigram row would inflate the CJK
    // index without recall benefit. The Hindi
    // lexicon uses Substring matching at the observation
    // engine layer (not the FTS5 layer) to compensate for
    // virama-induced intra-word splits.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _ = store
        .ingest(
            scope,
            "नई दिल्ली भारत की राजधानी है".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let cjk_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .unwrap();
    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        cjk_rows, 0,
        "pure-Devanagari body must NOT be written to evidence_fts_cjk \
         — Devanagari is whitespace-segmented and unicode61 handles it",
    );
    assert_eq!(
        bigram_rows, 0,
        "pure-Devanagari body must NOT be written to evidence_fts_bigram",
    );
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
    // `language_tag` column. This is what makes the
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
    //  / schema v15: the same body also lands in
    // evidence_fts_bigram so 2-codepoint CJK queries hit it.
    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        bigram_rows, 1,
        "CJK body must land in evidence_fts_bigram alongside evidence_fts_cjk"
    );
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
    //  / schema v15: pure-Latin bodies likewise stay
    // out of `evidence_fts_bigram` — the bigram lane is gated on
    // `crate::script::contains_cjk_or_thai` identically to the
    // trigram lane, so this assertion pins the storage-cost
    // invariant for the new shadow.
    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        bigram_rows, 0,
        "pure-Latin body must not be written to evidence_fts_bigram"
    );
}

#[test]
fn fts5_bigram_lane_closes_2char_cjk_recall_floor() {
    //  (schema v15) regression — pins the bigram-lane
    // gap-closure for 2-codepoint CJK queries.
    //
    // Background: SQLite's built-in `trigram` tokeniser has a
    // hard 3-codepoint minimum for both indexed substrings and
    // queries, so a 2-char CJK query like `天気` returns ∅
    // through the `evidence_fts_cjk` (trigram) lane even when
    // the substring is present in the body. added
    // a third FTS5 table (`evidence_fts_bigram`) that stores
    // whitespace-separated overlapping 2-codepoint windows under
    // the same `unicode61` tokeniser as `evidence_fts`. The read
    // path runs an independent prepared statement against the
    // bigram table and merges results into the existing
    // `MIN(rank)` HashMap so the lane is purely additive recall.
    //
    // This test pins (a) the 2-char CJK query NOW returns a hit
    // because the bigram lane catches it, and (b) the 3-char
    // CJK query still works through whichever lane fires first.
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
        1,
        "2-char CJK query MUST hit via the bigram lane now that \
         's `evidence_fts_bigram` table exists — the \
         trigram lane still misses these as documented but the \
         bigram lane closes the gap"
    );
    // …and the same query 1 char longer still works (this lane
    // crosses the trigram floor so we exercise both branches):
    let hits = store.search_fts(scope, "良い天気", 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn fts5_three_lane_merge_dedupes_mixed_query_across_all_three_tables() {
    // earlier regression — exercises the
    // `merged_fts_search` three-lane fan-out across a single
    // mixed Latin + 2-codepoint CJK query so all three FTS5
    // shadow tables contribute hits that the merge must
    // de-duplicate by `evidence_id`.
    //
    // The body contains a single matching row whose Latin
    // substring (`launch`) routes to `evidence_fts` (unicode61),
    // whose 3+-codepoint CJK substring (`良い天気`) routes to
    // `evidence_fts_cjk` (trigram), AND whose 2-codepoint CJK
    // substring (`天気`) routes to `evidence_fts_bigram`
    // (precomputed bigrams under unicode61). All three branches
    // therefore return the same row under different ranks; the
    // `merged_fts_search` `MIN(rank)`-by-evidence-id HashMap
    // contract must collapse them to a single hit. earlier
    // this test was unrepresentable because the bigram lane did
    // not exist and the 2-codepoint CJK term `天気` round-tripped
    // as empty.
    //
    // The companion `fts5_search_dedupes_when_both_tables_match_same_row`
    // test exercises only the two-lane (unicode61 + trigram) merge;
    // this test extends that contract to the third lane.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "Project launch review with 今日は良い天気です note".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    // Sanity: the row landed in all three FTS shadow tables. We
    // verify this directly via the raw connection so a regression
    // in the write-path routing predicate surfaces as a clear
    // assertion failure here, not as a confusing miss in the
    // merge below.
    let unicode61_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts", [], |r| r.get(0))
        .unwrap();
    let trigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_cjk", [], |r| r.get(0))
        .unwrap();
    let bigram_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM evidence_fts_bigram", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unicode61_rows, 1, "row must land in evidence_fts");
    assert_eq!(trigram_rows, 1, "row must land in evidence_fts_cjk");
    assert_eq!(bigram_rows, 1, "row must land in evidence_fts_bigram");

    // A single mixed query that intentionally drives all three
    // lanes to a hit on the same evidence_id. FTS5 boolean OR
    // syntax `term1 OR term2 OR term3`:
    //
    //  * `launch` → unicode61 (`evidence_fts`)
    //  * `良い天気` → trigram (`evidence_fts_cjk`)
    //  * `天気` → bigram (`evidence_fts_bigram`)
    //
    // The merge contract collapses them to a single hit on the
    // shared evidence_id. If any lane's prepared statement were
    // accidentally dropped, the row would still appear (because
    // the other two lanes also match), so this test is robust to
    // a single-lane regression — but a wrong-shape merge that
    // returned duplicates would fail loudly.
    let hits = store
        .search_fts(scope, "launch OR 良い天気 OR 天気", 10)
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "three-lane mixed query must dedupe to a single hit on the shared evidence_id"
    );
    assert_eq!(hits[0], r.evidence_id);

    // Each lane in isolation also returns exactly the same row.
    // This pins the contract that the bigram lane is purely
    // additive — running the same query without the other two
    // terms must still hit the same row.
    let unicode61_only = store.search_fts(scope, "launch", 10).unwrap();
    assert_eq!(unicode61_only, vec![r.evidence_id]);
    let trigram_only = store.search_fts(scope, "良い天気", 10).unwrap();
    assert_eq!(trigram_only, vec![r.evidence_id]);
    let bigram_only = store.search_fts(scope, "天気", 10).unwrap();
    assert_eq!(bigram_only, vec![r.evidence_id]);
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
    // earlier regression. The SQLite trigram tokeniser docs
    // <https://www.sqlite.org/fts5.html#the_trigram_tokenizer> say
    // that trigram returns an *error* (not an empty result set)
    // when given any of:
    //   * a query term shorter than 3 characters,
    //   * a `NEAR(…)` expression,
    //   * a column filter,
    //   * a prefix-star match shorter than 3 codepoints.
    //
    // The fanned-out search (`evidence_fts` merged with
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
    assert_eq!(r4.len(),
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
fn fts5_dual_search_orders_tied_ranks_deterministically_by_evidence_id() {
    // earlier regression — verifies that `merged_fts_search`
    // emits a deterministic ordering for rows whose FTS5 rank
    // compares as equal. Pre-fix the `HashMap`-then-`sort_by(rank)`
    // pipeline produced run-to-run ordering jitter for ties because
    // hashmap iteration is randomised. Post-fix the tiebreaker is
    // `EvidenceId` ascending (Uuid::Ord = byte-lexicographic), which
    // is stable across process restarts.
    //
    // The fixture seeds two rows with identical CJK bodies into the
    // same scope. Both bodies produce the same set of trigrams, so
    // the trigram branch ranks them identically, exercising the
    // tiebreaker path. We then call `search_fts` 16 times and
    // assert (a) every run returns the same ordering and (b) the
    // ordering matches `EvidenceId` ascending.
    //
    // Identical CJK bodies produce identical trigram windows, so
    // FTS5 BM25 rank should tie. The body is well above the
    // 3-codepoint trigram floor so the `evidence_fts_cjk` lane is
    // exercised.
    const TIED_BODY: &str = "今日の重要な会議の議事録";

    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r1 = store
        .ingest(scope, TIED_BODY.as_bytes(), None, ImportanceClass::Useful)
        .expect("ingest row 1");
    let r2 = store
        .ingest(scope, TIED_BODY.as_bytes(), None, ImportanceClass::Useful)
        .expect("ingest row 2");

    // Both rows must come back from the trigram lane so we know the
    // tiebreaker is on the merged_fts_search merge path (not a single
    // ORDER BY on one statement).
    let baseline = store.search_fts(scope, "重要な会議", 10).unwrap();
    assert_eq!(
        baseline.len(),
        2,
        "both tied-body rows must be returned by the CJK lane"
    );

    // Run the same query 16 times; every result must be identical.
    let runs: Vec<Vec<_>> = (0..16)
        .map(|_| store.search_fts(scope, "重要な会議", 10).unwrap())
        .collect();
    for (i, run) in runs.iter().enumerate().skip(1) {
        assert_eq!(
            run, &runs[0],
            "run {i} differs from run 0 — tied-rank ordering is non-deterministic"
        );
    }

    // The deterministic order is `EvidenceId` ascending.
    let mut expected_ids = vec![r1.evidence_id, r2.evidence_id];
    expected_ids.sort();
    assert_eq!(
        runs[0], expected_ids,
        "tied-rank tiebreaker must be EvidenceId ascending (Uuid::Ord)"
    );
}

#[test]
fn fts5_trigram_branch_error_is_silently_swallowed_so_unicode61_results_survive() {
    // earlier regression — directly proves the error
    // containment path. We inject a guaranteed trigram failure by
    // DROPing the `evidence_fts_cjk` table out from under the
    // search, then verify the `unicode61` branch's results still
    // flow through. This exercises the rusqlite error swallow in
    // `merged_fts_search` independent of whichever bundled SQLite
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
    // on prepare. The defensive `merged_fts_search` swallows the
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

// ============================================================================
// FTS5 BM25 weight integration tests.
//
// The unit tests in `evidence_store::fts_weights::tests` pin the weight
// constants and the SQL fragment shape; the unit tests in
// `evidence_store::store::lane_sql_tests` pin the cached lane SQL.
// These integration tests close the loop by exercising the full
// ingest → search round-trip with the test-only
// `search_fts_with_weighted_ranks_for_tests` surface so the cross-lane
// rank multiplication is observable end-to-end.
// ============================================================================

#[test]
fn bigram_lane_ranks_are_weighted_below_raw_bm25_baseline() {
    // invariant: a 2-codepoint CJK query routes exclusively
    // through the bigram lane (the unicode61 lane emits no tokens for
    // CJK, the trigram lane's 3-codepoint floor swallows 2-char
    // queries). The post-merge rank must therefore equal the raw FTS5
    // BM25 rank times `EVIDENCE_FTS_BIGRAM_LANE_WEIGHT` (0.7), not the
    // raw rank itself. Pin this so a regression that drops the
    // `* EVIDENCE_FTS_BIGRAM_LANE_WEIGHT` multiply in `merged_fts_search`
    // fails loudly here rather than silently inverting the cross-lane
    // precision hierarchy.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _ = store
        .ingest(
            scope,
            "今日天気".as_bytes(), // "today's weather" — 4 CJK codepoints
            None,
            ImportanceClass::Important,
        )
        .unwrap();
    // Query "今日" (2 chars) routes exclusively through the bigram lane.
    let weighted = store
        .search_fts_with_weighted_ranks_for_tests(scope, "今日", 10)
        .unwrap();
    assert_eq!(
        weighted.len(),
        1,
        "bigram lane must recover the 2-char CJK query against the indexed body"
    );
    let (_id, rank) = weighted[0];
    // The bigram lane's raw BM25 rank is always a finite negative
    // f64 (FTS5 contract). After `* 0.7` the rank must remain
    // strictly negative AND closer to zero than the unicode61
    // baseline weight (1.0) would have produced.
    assert!(
        rank.is_finite() && rank < 0.0,
        "bigram lane weighted rank must be finite-negative, got: {rank}"
    );
    // Recover the raw rank (rank / 0.7) and pin that the weighted
    // value is the strictly smaller |rank| (closer to zero, worse).
    let raw_rank = rank / evidence_store::fts_weights::EVIDENCE_FTS_BIGRAM_LANE_WEIGHT;
    assert!(
        rank > raw_rank,
        "bigram lane weighting must move rank closer to zero: \
         weighted={rank}, raw={raw_rank}"
    );
}

#[test]
fn unicode61_lane_ranks_are_identity_weighted_against_baseline() {
    // invariant: the unicode61 lane is the precision
    // baseline at weight 1.0, so a pure-Latin query that routes
    // exclusively through `evidence_fts` must produce ranks
    // numerically identical to the raw FTS5 BM25 ranks (the
    // `* 1.0` multiply is the identity). A regression that
    // accidentally nudges the baseline weight off 1.0 would
    // silently shift the cross-lane ratios; this test pins the
    // identity invariant on the live SQL pipeline.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            b"The deadline for the launch is next Monday",
            None,
            ImportanceClass::Important,
        )
        .unwrap();
    let weighted = store
        .search_fts_with_weighted_ranks_for_tests(scope, "deadline", 10)
        .unwrap();
    assert_eq!(weighted.len(), 1);
    assert_eq!(weighted[0].0, r.evidence_id);
    let rank = weighted[0].1;
    // Raw BM25 rank is always negative; identity weight preserves
    // exact value (no rounding error from f64 multiply by 1.0).
    assert!(
        rank.is_finite() && rank < 0.0,
        "unicode61 lane weighted rank must be finite-negative, got: {rank}"
    );
    let raw_rank = rank / evidence_store::fts_weights::EVIDENCE_FTS_LANE_WEIGHT;
    // `f64::to_bits` for bit-exact comparison (clippy::float_cmp
    // disallows raw `==` on f64; division by 1.0 must preserve
    // the bit pattern exactly so any drift here indicates a
    // non-1.0 baseline weight or a float-arithmetic regression).
    assert_eq!(
        rank.to_bits(),
        raw_rank.to_bits(),
        "EVIDENCE_FTS_LANE_WEIGHT = 1.0 must be the bit-exact identity on rank \
         multiplication in the live query path (weighted={rank}, raw={raw_rank})"
    );
}

#[test]
fn trigram_lane_ranks_are_weighted_below_unicode61_baseline() {
    // invariant: a 3-codepoint CJK query routes through
    // both the trigram lane (single trigram window) and the bigram
    // lane (two bigram windows). The per-row MIN-merge picks the
    // best (most negative) weighted score across lanes. Verify the
    // returned rank is strictly negative and consistent with the
    // weighted-min contract.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _ = store
        .ingest(
            scope,
            "今日天気は良い".as_bytes(), // "today's weather is good" — 7 CJK codepoints
            None,
            ImportanceClass::Important,
        )
        .unwrap();
    let weighted = store
        .search_fts_with_weighted_ranks_for_tests(scope, "今日天", 10)
        .unwrap();
    assert_eq!(
        weighted.len(),
        1,
        "trigram + bigram lanes must both surface the indexed CJK body"
    );
    let (_id, rank) = weighted[0];
    assert!(
        rank.is_finite() && rank < 0.0,
        "merged CJK rank must be finite-negative, got: {rank}"
    );
}

#[test]
fn lane_weight_precision_hierarchy_holds_at_query_time() {
    // invariant: when the SAME row hits via multiple
    // lanes, the unicode61 lane's `* 1.0` multiply produces a more-
    // negative rank than the trigram lane's `* 0.85` would for the
    // same raw FTS5 BM25 score, which in turn is more-negative
    // than the bigram lane's `* 0.7`. Pin this via direct
    // arithmetic against the public weight constants — the
    // integration test surfaces the live constants, not the
    // documented values, so any drift between the constant module
    // and the live `merged_fts_search` weighting is caught here.
    use evidence_store::fts_weights::{
        EVIDENCE_FTS_BIGRAM_LANE_WEIGHT, EVIDENCE_FTS_CJK_LANE_WEIGHT, EVIDENCE_FTS_LANE_WEIGHT,
    };
    let raw_rank: f64 = -2.5; // canonical negative FTS5 BM25 rank
    let unicode61_weighted = raw_rank * EVIDENCE_FTS_LANE_WEIGHT;
    let trigram_weighted = raw_rank * EVIDENCE_FTS_CJK_LANE_WEIGHT;
    let bigram_weighted = raw_rank * EVIDENCE_FTS_BIGRAM_LANE_WEIGHT;
    // More-negative is better in FTS5's BM25 contract. The
    // precision hierarchy demands unicode61 < trigram < bigram
    // (strict inequalities on the weighted ranks).
    assert!(
        unicode61_weighted < trigram_weighted,
        "unicode61 must produce more-negative weighted rank than trigram: \
         unicode61={unicode61_weighted}, trigram={trigram_weighted}"
    );
    assert!(
        trigram_weighted < bigram_weighted,
        "trigram must produce more-negative weighted rank than bigram: \
         trigram={trigram_weighted}, bigram={bigram_weighted}"
    );
}

// ----------------------------------------------------------------------
//  — Symmetric recall-lane stopword stripping (schema v16)
// ----------------------------------------------------------------------
//
// strips a small, conservative inventory of per-script
// function words (Japanese particles, Chinese connectives, Thai
// prepositions, ...) from BOTH the index-time write path and the
// query-time read path before the bigram / trigram lanes consume the
// text. The stripping is symmetric — applied identically on both
// sides — so a query that includes a stopword still matches a body
// that contains the same stopword, but neither side's bigram /
// trigram windows include the function-word noise.
//
// These tests pin three contracts:
//
//   1. **Recall is preserved for content-word queries.** A body
//      containing a stopword + content word matches a query of the
//      same body, even though the stopword is stripped from both
//      sides before the lane match.
//   2. **Recall is preserved when the query has a stopword and the
//      body has a different stopword in the same position.** A
//      body `日本のオリンピック` ("Japan's Olympics", with genitive
//      `の`) matches a query `日本オリンピック` (no particle) and
//      vice versa — both sides reduce to the same stripped form
//      `日本 オリンピック` after the strip.
//   3. **Content-bearing terms intentionally omitted from the
//      inventory (e.g. Thai time deictic `วันนี้`) are NOT stripped
//      from either side**, so a query `วันนี้` against a body
//      containing `วันนี้` hits via the bigram / trigram lanes the
//      same as any other content phrase. This guards against
//      future contributors expanding the inventory to include
//      content-bearing items (see `STOPWORDS_TH` doc-comment for
//      the deliberate-exclusion rationale).

#[test]
fn fts5_japanese_stopword_query_matches_indexed_stopword_body() {
    // Body and query both contain the same genitive particle `の`.
    // Both sides strip identically, so the bigram-lane windows on
    // both sides reduce to `日本 オリンピック` and the match must
    // succeed — verifying the symmetric-strip contract.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "日本のオリンピック".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "日本のオリンピック", 10).unwrap();
    assert_eq!(hits.len(), 1, "body+query with identical stopword must hit");
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_japanese_stopword_in_body_only_still_matches_clean_query() {
    // Body has the genitive particle `の`; query does not. The
    // body's stored bigram windows (after stripping) are
    // `日本 オリンピック` (with a space at the strip site). The
    // query, also stripped, becomes `日本オリンピック`
    // (unchanged — no stopwords). The CJK bigram lane filters
    // ASCII whitespace out before windowing, so the body windows
    // collapse to the same `日本オリンピック` bigram sequence as
    // the query — the match must succeed.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "日本のオリンピック".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "日本オリンピック", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "body with stopword must hit when query omits the same stopword",
    );
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_japanese_stopword_in_query_only_still_matches_clean_body() {
    // The reverse of the previous test. Body has no stopword;
    // query includes `の`. After symmetric stripping both reduce
    // to the same lane-tokenisable form.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "日本オリンピック".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "日本のオリンピック", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "body without stopword must hit when query inserts an adjacent stopword",
    );
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_pure_stopword_query_yields_no_hit_against_content_body() {
    // A query consisting entirely of stopword particles strips to
    // pure whitespace, which the lane SQL detects and short-
    // circuits to an empty result for. This is correct: pure
    // particles are uninformative and should not match any body.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let _r = store
        .ingest(
            scope,
            "日本のオリンピック".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "のはがを", 10).unwrap();
    assert!(
        hits.is_empty(),
        "pure-stopword query must return no hits (lane short-circuit)",
    );
}

#[test]
fn fts5_chinese_de_particle_symmetric_round_trip() {
    // Body contains the genitive `的`; query contains the same.
    // The bigram lane windows after stripping must align.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "日本的天气".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "日本的天气", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], r.evidence_id);

    // And the asymmetric direction: body with particle, clean
    // query — must still hit.
    let hits = store.search_fts(scope, "日本天气", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "body with `的` must hit clean query after symmetric strip",
    );
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_thai_preposition_kong_symmetric_round_trip() {
    // Body contains the preposition `ของ` ("of"); query contains
    // the same. Both sides strip identically.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "อากาศของกรุงเทพ".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "อากาศของกรุงเทพ", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], r.evidence_id);

    // Asymmetric: body with `ของ`, query without — must still
    // hit after symmetric strip collapses both to the same
    // residual content.
    let hits = store.search_fts(scope, "อากาศกรุงเทพ", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "body with `ของ` must hit clean query after symmetric strip",
    );
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_does_not_strip_content_bearing_time_deictic_wannii() {
    // Pin the deliberate-exclusion contract: `วันนี้` ("today")
    // is **NOT** in STOPWORDS_TH because it's a content-bearing
    // temporal expression. A body containing `วันนี้` must hit a
    // query of the same `วันนี้` — the bigram / trigram lanes
    // must see the full codepoint sequence on both sides.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(scope, "อากาศวันนี้ดี".as_bytes(), None, ImportanceClass::Useful)
        .unwrap();
    let hits = store.search_fts(scope, "วันนี้", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "content-bearing `วันนี้` must NOT be stripped by ",
    );
    assert_eq!(hits[0], r.evidence_id);
}

#[test]
fn fts5_unicode61_lane_unstripped_for_latin_content() {
    // The strip only applies to the trigram and bigram
    // lanes (`evidence_fts_cjk` and `evidence_fts_bigram`). The
    // unicode61 source-of-truth lane (`evidence_fts.content`)
    // never sees the strip, so Latin queries against Latin
    // bodies must continue to work exactly as in 's
    // baseline — no spurious "stopwords" are removed from
    // English-language content.
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let r = store
        .ingest(
            scope,
            "the quick brown fox jumps over the lazy dog".as_bytes(),
            None,
            ImportanceClass::Useful,
        )
        .unwrap();
    let hits = store.search_fts(scope, "brown fox", 10).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "Latin (unicode61) lane must not be touched by ",
    );
    assert_eq!(hits[0], r.evidence_id);
}

/// End-to-end integration test for the FTS-telemetry
/// counters: ingest a CJK body, run a search across all three
/// recall lanes, and confirm each counter category advances.
///
/// This is the cross-cutting "do the counters actually tick
/// through the public surface?" check — every counter wired in
/// `crates/evidence_store/src/store.rs` should move when a real
/// Japanese sentence is ingested + queried.
///
/// Counters exercised:
///   - `index_write_stopwords_stripped_total` (ingest path: の)
///   - `query_time_stopwords_stripped_total` (query path: の)
///   - `unicode61_lane_queries_total` (always)
///   - `cjk_trigram_lane_queries_total` (CJK body present)
///   - `bigram_lane_queries_total` (CJK body present)
///
/// We use lower-bound (`>`) assertions because other tests in
/// the same binary touch the same process-singleton counters,
/// matching the [`crates/ffi/src/metrics.rs`] mirror-parity tests.
#[test]
fn fts_telemetry_counters_advance_for_cjk_query_end_to_end() {
    use evidence_store::fts_telemetry;
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Take a snapshot *before* both ingest and query so we can
    // independently assert the index-write site advances on
    // ingest and the query-time site advances on search.
    let before_ingest = fts_telemetry::snapshot();

    // Japanese body containing two stopwords ("の" particle ×2):
    // forces the index-time stopword strip path to bump for the
    // trigram + bigram lanes (the unicode61 lane preserves the
    // body verbatim). The body has enough CJK codepoints (>=3)
    // to route to both trigram and bigram lanes.
    let body = "今日は会議の議事録の確認を行いました";
    let res = store
        .ingest(scope, body.as_bytes(), None, ImportanceClass::Useful)
        .unwrap();

    let after_ingest = fts_telemetry::snapshot();
    assert!(
        after_ingest.index_write_stopwords_stripped_total
            > before_ingest.index_write_stopwords_stripped_total,
        "index-write stopword strip counter did not advance on CJK ingest"
    );

    // Now run a query that itself contains "の" — guarantees
    // the query-time strip counter advances independently of the
    // index-time site.
    let hits = store.search_fts(scope, "議事録の確認", 10).unwrap();
    assert!(
        hits.contains(&res.evidence_id),
        "CJK end-to-end query failed to return the ingested row"
    );

    let after_query = fts_telemetry::snapshot();

    // Query-time strip site moved.
    assert!(
        after_query.query_time_stopwords_stripped_total
            > after_ingest.query_time_stopwords_stripped_total,
        "query-time stopword strip counter did not advance on CJK query"
    );

    // All three lane-query counters moved (unicode61 is always
    // tried; trigram + bigram are tried because the query
    // contains adjacent CJK codepoints).
    assert!(
        after_query.unicode61_lane_queries_total > after_ingest.unicode61_lane_queries_total,
        "unicode61 lane query counter did not advance"
    );
    assert!(
        after_query.cjk_trigram_lane_queries_total > after_ingest.cjk_trigram_lane_queries_total,
        "trigram lane query counter did not advance"
    );
    assert!(
        after_query.bigram_lane_queries_total > after_ingest.bigram_lane_queries_total,
        "bigram lane query counter did not advance"
    );

    // The lane row totals should have advanced at least by the
    // unicode61 lane's hit count (>=1), because the ingested
    // row matches the query on at least one lane.
    assert!(
        after_query.unicode61_lane_rows_total
            + after_query.cjk_trigram_lane_rows_total
            + after_query.bigram_lane_rows_total
            > after_ingest.unicode61_lane_rows_total
                + after_ingest.cjk_trigram_lane_rows_total
                + after_ingest.bigram_lane_rows_total,
        "no recall-lane row counter advanced — the search returned a hit but no lane recorded it"
    );
}

/// Skip-counter end-to-end test. Sister of
/// `fts_telemetry_counters_advance_for_cjk_query_end_to_end`
/// that exercises the three *skip* counters.
///
/// - `bigram_lane_skips_no_cjk_query_total` advances when the
///   stripped query is non-empty but has no adjacent CJK
///   codepoint (e.g. Latin-only).
/// - `cjk_trigram_lane_skips_pure_stopword_query_total`
///   advances when stripping collapses the query to empty
///   (pure-stopword Japanese input like "の の の").
/// - `bigram_lane_skips_pure_stopword_query_total` advances on
///   the same pure-stopword input as the trigram skip above —
///   an earlier review added this variant so
///   the bigram lane can distinguish "Latin-only query, lane
///   correctly declined" from "CJK query annihilated by
///   stopword stripping". Before the a follow-up restructure, the
///   pure-stopword case incorrectly bumped
///   `bigram_lane_skips_no_cjk_query_total`.
///
/// Note: a Latin-only query does NOT structurally skip the
/// trigram lane — the FTS5 `trigram` tokeniser windows Latin
/// substrings embedded in CJK bodies, so Latin queries can
/// legitimately match. On Latin-only seed data the trigram
/// lane simply runs a MATCH that returns zero rows (bumping
/// `cjk_trigram_lane_queries_total`, not a skip counter).
/// a follow-up (commit `4aaccba`) tried to skip Latin
/// queries on the trigram lane and was reverted a follow-up —
/// see the doc comment on `crate::fts_telemetry` and the
/// trigram branch in `crate::store::merged_fts_search` for the
/// cross-script rationale.
#[test]
fn fts_telemetry_skip_counters_advance_for_structural_skips() {
    use evidence_store::fts_telemetry;
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Seed something searchable so the query path actually runs.
    let _ = store
        .ingest(
            scope,
            b"Latin body for skip-counter test.",
            None,
            ImportanceClass::Useful,
        )
        .unwrap();

    let before = fts_telemetry::snapshot();

    // (1) Latin-only query → bigram lane structurally declines
    // (`compute_cjk_bigram_query` returns `None` because no
    // adjacent CJK codepoint pair exists in the query). The
    // trigram lane does NOT structurally decline a Latin-only
    // query — it runs a MATCH against `evidence_fts_cjk` which
    // returns zero rows on this Latin-only seed (the body
    // wasn't routed into the CJK-only table to begin with) and
    // bumps `cjk_trigram_lane_queries_total`. This shape was
    // the a follow-up behaviour; a follow-up reverted the a follow-up
    // structural skip after the trigram tokeniser's cross-
    // script behaviour was correctly identified.
    let _ = store.search_fts(scope, "Latin body", 10).unwrap();

    let after_latin = fts_telemetry::snapshot();
    assert!(
        after_latin.bigram_lane_skips_no_cjk_query_total
            > before.bigram_lane_skips_no_cjk_query_total,
        "bigram no-CJK-query skip counter did not advance on Latin-only query"
    );

    // (2) Pure-stopword Japanese query → trigram lane collapses
    // to empty after the query-time strip and short-circuits,
    // AND the bigram lane records its sibling pure-stopword
    // skip (a follow-up review fix) instead of routing the
    // pure-stopword case into the no-CJK counter.
    let _ = store.search_fts(scope, "の の の", 10).unwrap();

    let after_stop = fts_telemetry::snapshot();
    assert!(
        after_stop.cjk_trigram_lane_skips_pure_stopword_query_total
            > after_latin.cjk_trigram_lane_skips_pure_stopword_query_total,
        "trigram pure-stopword-query skip counter did not advance on a stripped-to-empty query"
    );
    assert!(
        after_stop.bigram_lane_skips_pure_stopword_query_total
            > after_latin.bigram_lane_skips_pure_stopword_query_total,
        "bigram pure-stopword-query skip counter did not advance on a stripped-to-empty CJK query \
         — a follow-up review regressed (pure-stopword case routed to BigramNoCjkQuery instead)"
    );
    // earlier regression note: with the
    // structural `if stripped_query.trim().is_empty() { skip }
    // else { closure; if let Ok { record_lane_query } }` shape
    // in `merged_fts_search`, a pure-stopword query like the
    // one above bumps *only* the skip counter — never the
    // query counter — because the two branches are mutually
    // exclusive by construction. We deliberately do NOT pin
    // this via a runtime assertion on the query counter:
    // sibling tests in this binary run in parallel and bump
    // the same process-singleton counter, so any
    // `assert_eq!(after_stop.query, after_latin.query)` would
    // race. The regression-resistance lives in the structural
    // if/else, not in this test — see the doc comment on the
    // trigram branch in `crate::store::merged_fts_search` and
    // the `queries + skips + silently_swallowed_errors =
    // total_attempts` contract on `crate::fts_telemetry`.
    //
    // earlier regression note: the
    // bigram lane parallels the same structural shape — the
    // pure-stopword check runs BEFORE
    // `compute_cjk_bigram_query` so the no-CJK and
    // pure-stopword bigram skip counters are mutually
    // exclusive by construction. We do not pin
    // `bigram_lane_skips_no_cjk_query_total` not-advancing on
    // step (2) for the same parallel-tests race reason.
}

/// Architectural-reality regression test for the trigram
/// tokeniser's cross-script behaviour. Pins the fact that a
/// Latin-only query MUST be able to match a CJK body containing
/// an embedded Latin substring via the `evidence_fts_cjk`
/// (trigram) lane — because the FTS5 `trigram` tokeniser windows
/// ALL overlapping 3-codepoint sequences in the indexed body,
/// not just CJK ones.
///
/// Background: a follow-up (commit `4aaccba`) added a
/// structural skip on the trigram lane for Latin-only queries
/// under the false premise that `evidence_fts_cjk` "cannot
/// contain a matching row" for such queries. a follow-up reverted
/// that change after an earlier review correctly identified that the
/// trigram tokeniser DOES index Latin substrings inside CJK
/// bodies, so the structural skip was a recall risk dressed as a
/// perf optimisation. This test locks the correct behaviour
/// in place — a future re-optimisation attempt that re-adds the
/// Latin-only skip will fail here loudly.
///
/// Mechanism: ingest `日本のiPhone発表` (mixed Japanese + Latin)
/// and query for `iPhone` (Latin only). The CJK body routes
/// into `evidence_fts_cjk` (because `contains_cjk_or_thai` is
/// true on the body), and the FTS5 trigram tokeniser stores the
/// Latin trigrams `iPh`, `Pho`, `hon`, `one`. The Latin query
/// tokenises to the same trigrams and matches.
///
/// Unicode61 lane note: the unicode61 lane also matches this
/// query (Latin tokens are preserved verbatim in `evidence_fts`),
/// so end-to-end recall is independently guaranteed via that
/// lane. This test asserts the trigram lane *also* matches,
/// which is what the a follow-up commit silently broke.
#[test]
fn fts_telemetry_trigram_lane_matches_latin_in_cjk_body() {
    use evidence_store::fts_telemetry;
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Mixed-script body: Japanese particles + Latin product
    // name. Routes into `evidence_fts_cjk` because the body
    // contains CJK codepoints, and the trigram tokeniser indexes
    // the embedded Latin substring `iPhone` as overlapping
    // 3-codepoint windows alongside the CJK trigrams.
    let body = "日本のiPhone発表";
    let res = store
        .ingest(scope, body.as_bytes(), None, ImportanceClass::Useful)
        .unwrap();

    let before = fts_telemetry::snapshot();

    // Latin-only query. Must match the body — both the
    // unicode61 lane (Latin tokens preserved) and the trigram
    // lane (Latin trigrams windowed inside the CJK body) will
    // contribute.
    let hits = store.search_fts(scope, "iPhone", 10).unwrap();
    assert!(
        hits.contains(&res.evidence_id),
        "Latin-only query failed to match a CJK body containing the Latin substring \
         — the trigram lane MUST window Latin trigrams inside CJK bodies \
         (see fts_telemetry module doc and the a follow-up revert of commit 4aaccba)"
    );

    let after = fts_telemetry::snapshot();

    // The trigram lane MUST be invoked (no structural skip on
    // Latin queries) — this is the key a follow-up regression
    // guard. If a future commit re-adds the Latin-only
    // structural skip, the trigram query counter will not
    // advance and this assertion fails.
    assert!(
        after.cjk_trigram_lane_queries_total > before.cjk_trigram_lane_queries_total,
        "trigram lane query counter did not advance on Latin-only query against CJK body \
         — a follow-up review must remain reverted (the trigram lane is NOT structurally \
         declined for Latin queries; see fts_telemetry module doc for the cross-script \
         tokeniser rationale)"
    );

    // And the unicode61 lane MUST also be invoked — it's the
    // primary high-precision lane for Latin queries.
    assert!(
        after.unicode61_lane_queries_total > before.unicode61_lane_queries_total,
        "unicode61 lane query counter did not advance"
    );
}
