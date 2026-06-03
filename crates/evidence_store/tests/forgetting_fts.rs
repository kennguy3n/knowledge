//! Integration test that pins the cryptographic-forgetting contract
//! for the FTS5 secondary index.
//!
//! Per `docs/technical/design.md` §3.1, the
//! substrate promises that "the scope id is the unit of cryptographic
//! forgetting." The **bodies** of every evidence row are encrypted
//! under a scope-derived AEAD key (`scope:{uuid}:body:v1`), so
//! destroying that key in [`crypto::forgetting::DekRegistry`] renders
//! the ciphertexts unrecoverable in this process.
//!
//! The remaining gap that this test pins is the SQLite **FTS5
//! secondary index**: the `evidence_fts` virtual table keeps the
//! tokenised **plaintext** of every ingested body, regardless of the
//! AEAD key, so the index would survive DEK destruction unless the
//! runtime explicitly purges it.
//!
//! The durable-tombstone work closes that gap. The FFI `forget()` path now
//! calls [`EvidenceStore::purge_fts_for_scope`] after destroying the
//! scope DEK, deleting every FTS5 row for the scope (and every
//! `evidence_embeddings` row, which is plaintext-derived in the same
//! way). The `evidence` table itself stays append-only — its rows
//! remain on disk but their bodies are now uniquely unrecoverable
//! through both lanes (ciphertext + plaintext-derived index).
//!
//! This test exercises the contract end-to-end against
//! `EvidenceStore` directly, without going through the FFI runtime,
//! so the assertions remain valid even if the FFI surface is
//! refactored.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xA5; 32];
/// Distinctive single token (no punctuation) so the FTS5 default
/// `unicode61` tokenizer indexes it verbatim and `MATCH` does not
/// need any phrase-quoting gymnastics. We still call it a "phrase"
/// in test names and comments because the spec uses that wording.
const FORGETTING_PHRASE: &str = "xyzzyforgettingtestphrase";

/// Ingest a message containing [`FORGETTING_PHRASE`], confirm FTS5
/// can find it, call [`EvidenceStore::purge_fts_for_scope`], and
/// verify that both the raw FTS5 table and the public
/// [`EvidenceStore::search_fts`] surface no longer return any hits.
///
/// Re-opens the store with the same master key between the purge
/// and the verification step so the test also documents that the
/// purge is durable across `open_store` / `close_store` cycles
/// (the on-disk FTS5 row is gone, not merely flushed from a cache).
#[test]
fn fts_index_is_purged_after_purge_fts_for_scope() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope = ScopeId::new_v4();
    let body = format!(
        "Reminder: the {FORGETTING_PHRASE} must be redacted before any \
         end-of-quarter audit ships."
    );
    assert!(body.len() <= DEFAULT_INLINE_THRESHOLD_BYTES);

    let evidence_id;
    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open store");

        let res = store
            .ingest(
                scope,
                body.as_bytes(),
                Some("source:forgetting-test"),
                ImportanceClass::Important,
            )
            .expect("ingest");
        evidence_id = res.evidence_id;

        // Sanity: FTS5 actually indexed the phrase before the purge.
        let hits = store
            .search_fts(scope, FORGETTING_PHRASE, 10)
            .expect("search_fts pre-purge");
        assert_eq!(
            hits,
            vec![evidence_id],
            "FTS5 must surface the phrase before purge_fts_for_scope runs"
        );

        // The unit of forgetting: zero out the FTS5 / embeddings
        // payload for the scope. The encrypted body in `evidence`
        // is untouched — without the scope DEK it is already
        // unrecoverable.
        store
            .purge_fts_for_scope(scope)
            .expect("purge_fts_for_scope");
    }

    // Re-open the store with the SAME master key so we can probe the
    // FTS table directly after the purge. If purge_fts_for_scope
    // had only flushed an in-memory cache, the on-disk FTS5 shadow
    // tables would still match the term and the assertion below
    // would fail.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open store");

    let raw_term_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1 AND scope_id = ?2",
            rusqlite::params![FORGETTING_PHRASE, scope.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count fts rows");
    assert_eq!(
        raw_term_count, 0,
        "FTS5 index must not contain any rows for the forgotten scope after purge_fts_for_scope"
    );

    // Public API mirrors the raw probe.
    let hits = store
        .search_fts(scope, FORGETTING_PHRASE, 10)
        .expect("search_fts post-purge");
    assert!(
        hits.is_empty(),
        "search_fts must return no rows for a forgotten scope after purge_fts_for_scope: {hits:?}"
    );

    // The `evidence` row itself is still on disk — `evidence` is
    // append-only and the body is encrypted under a scope DEK that
    // no longer exists in memory, so the row's persistence is
    // harmless. We only want to verify that the row was not
    // accidentally deleted along with the FTS rows.
    let row = store.get(evidence_id).expect("get evidence row");
    assert!(
        row.is_some(),
        "purge_fts_for_scope must not delete from the append-only evidence table"
    );
}

/// Ingest two evidence rows in two *different* scopes, then call
/// `purge_fts_for_scope` for one of them. The FTS5 row for the
/// untouched scope must remain searchable — the purge is strictly
/// per-scope and must not accidentally drop rows for sibling scopes.
#[test]
fn purge_fts_for_scope_only_purges_target_scope() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body_a = format!("scope-a body with {FORGETTING_PHRASE}");
    let body_b = format!("scope-b body with {FORGETTING_PHRASE}");

    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");

    let res_a = store
        .ingest(
            scope_a,
            body_a.as_bytes(),
            Some("a"),
            ImportanceClass::Important,
        )
        .expect("ingest a");
    let res_b = store
        .ingest(
            scope_b,
            body_b.as_bytes(),
            Some("b"),
            ImportanceClass::Important,
        )
        .expect("ingest b");

    store
        .purge_fts_for_scope(scope_a)
        .expect("purge scope a only");

    let hits_a = store
        .search_fts(scope_a, FORGETTING_PHRASE, 10)
        .expect("search a");
    assert!(
        hits_a.is_empty(),
        "purged scope must have no FTS hits: {hits_a:?}"
    );

    let hits_b = store
        .search_fts(scope_b, FORGETTING_PHRASE, 10)
        .expect("search b");
    assert_eq!(
        hits_b,
        vec![res_b.evidence_id],
        "untouched scope must still be searchable after a sibling scope is purged"
    );

    // Sanity: the evidence rows themselves are append-only and
    // remain on disk in both scopes.
    assert!(store.get(res_a.evidence_id).expect("get a").is_some());
    assert!(store.get(res_b.evidence_id).expect("get b").is_some());
}

/// After `purge_fts_for_scope`, the FTS5 REBUILD command must have
/// truncated and re-built the shadow tables so that tokenised
/// plaintext fragments no longer linger in the `%_data` segment
/// B-tree. We verify this by querying the raw `evidence_fts` virtual
/// table — after REBUILD, the segment structure is reconstructed
/// from the surviving content rows only and contains no entries for
/// the purged scope.
#[test]
fn fts5_rebuild_purges_shadow_tables_after_purge() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope = ScopeId::new_v4();
    let body = format!(
        "This message contains {FORGETTING_PHRASE} which must be fully \
         purged from shadow tables after purge and REBUILD."
    );

    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open store");

        store
            .ingest(
                scope,
                body.as_bytes(),
                Some("source:rebuild-test"),
                ImportanceClass::Important,
            )
            .expect("ingest");

        // Pre-purge: FTS5 indexes the phrase.
        let hits = store
            .search_fts(scope, FORGETTING_PHRASE, 10)
            .expect("search pre-purge");
        assert_eq!(hits.len(), 1, "FTS5 must find the phrase before purge");

        // Purge runs DELETE + REBUILD inside a single transaction.
        // The REBUILD step truncates the shadow tables and
        // re-tokenises from the surviving `content` column, so no
        // plaintext fragments from the purged scope survive on disk.
        store
            .purge_fts_for_scope(scope)
            .expect("purge_fts_for_scope");
    }

    // Re-open so we're reading only the on-disk state.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open store");

    // After REBUILD, querying the FTS table must return nothing.
    let hits = store
        .search_fts(scope, FORGETTING_PHRASE, 10)
        .expect("search post-rebuild");
    assert!(
        hits.is_empty(),
        "FTS5 must return no results after purge + REBUILD"
    );

    // Also verify via the raw FTS5 MATCH — the shadow table's
    // segment B-tree must contain no matching rows for the phrase.
    let raw_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1",
            rusqlite::params![FORGETTING_PHRASE],
            |row| row.get(0),
        )
        .expect("raw fts count");
    assert_eq!(
        raw_count, 0,
        "Raw FTS5 shadow tables must contain zero matching rows after REBUILD"
    );
}

/// Re-purging an already-purged scope must be a cheap no-op: zero
/// FTS rows are deleted, so the function must skip the
/// O(total_fts_rows) `REBUILD` entirely. We can't directly observe
/// "did we issue REBUILD?", but we can pin the externally visible
/// contract: the FTS table's `dbstat`-style row count for the
/// segment B-tree must not change across an already-purged
/// re-invocation, and unrelated scopes' FTS hits must continue to
/// match. (REBUILD is observable as a *reset* of FTS5's internal
/// segment structure — a no-op skip leaves it untouched.)
#[test]
fn purge_fts_for_scope_is_idempotent_no_op_after_first_purge() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let purged_scope = ScopeId::new_v4();
    let surviving_scope = ScopeId::new_v4();
    let surviving_phrase = "kept_scope_survivor_marker_phrase";

    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");

    // Ingest into both scopes.
    store
        .ingest(
            purged_scope,
            format!("body with {FORGETTING_PHRASE} for purge").as_bytes(),
            Some("source:idempotent-purge"),
            ImportanceClass::Important,
        )
        .expect("ingest purged scope");
    store
        .ingest(
            surviving_scope,
            format!("body with {surviving_phrase} for survival").as_bytes(),
            Some("source:idempotent-purge"),
            ImportanceClass::Important,
        )
        .expect("ingest surviving scope");

    // First purge: deletes one FTS row and triggers a REBUILD.
    store
        .purge_fts_for_scope(purged_scope)
        .expect("first purge");
    let hits = store
        .search_fts(purged_scope, FORGETTING_PHRASE, 10)
        .expect("search after first purge");
    assert!(hits.is_empty(), "phrase must be gone after first purge");

    // Snapshot the FTS5 segment id sequence after the first purge.
    // `evidence_fts_data` is the `%_data` shadow table; its rowids
    // are reset by REBUILD. After a no-op repurge they must be
    // unchanged.
    let segments_after_first_purge: Vec<i64> = store
        .raw_conn()
        .prepare("SELECT id FROM evidence_fts_data ORDER BY id")
        .expect("prepare segments")
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("query segments")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect segments");

    // Re-purge the same already-purged scope. This must be a true
    // no-op: zero FTS rows are deleted, so the shadow tables must
    // not be reset by a redundant REBUILD.
    store
        .purge_fts_for_scope(purged_scope)
        .expect("second (idempotent) purge");

    let segments_after_second_purge: Vec<i64> = store
        .raw_conn()
        .prepare("SELECT id FROM evidence_fts_data ORDER BY id")
        .expect("prepare segments")
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("query segments")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect segments");

    assert_eq!(
        segments_after_first_purge, segments_after_second_purge,
        "purge_fts_for_scope must skip REBUILD when zero FTS rows were deleted"
    );

    // The surviving scope's phrase remains discoverable across the
    // idempotent re-purge.
    let surviving_hits = store
        .search_fts(surviving_scope, surviving_phrase, 10)
        .expect("search surviving scope");
    assert_eq!(
        surviving_hits.len(),
        1,
        "Surviving scope's FTS row must remain after idempotent re-purge"
    );
}

/// The batch entry point `purge_fts_for_scopes` must produce the
/// same end state as calling `purge_fts_for_scope` once per scope,
/// while issuing at most one FTS5 REBUILD for the whole batch. We
/// verify the end state directly (every purged scope's phrase is
/// gone, surviving scopes still match), which is the externally
/// visible contract callers depend on.
#[test]
fn purge_fts_for_scopes_batch_matches_per_scope_purge() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let scope_c = ScopeId::new_v4();
    let scope_survivor = ScopeId::new_v4();

    let phrase_a = "batchpurge_marker_alpha_token";
    let phrase_b = "batchpurge_marker_bravo_token";
    let phrase_c = "batchpurge_marker_charlie_token";
    let phrase_survivor = "batchpurge_marker_survivor_token";

    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");

    for (scope, phrase) in [
        (scope_a, phrase_a),
        (scope_b, phrase_b),
        (scope_c, phrase_c),
        (scope_survivor, phrase_survivor),
    ] {
        store
            .ingest(
                scope,
                format!("body containing {phrase} for batch purge test").as_bytes(),
                Some("source:batch-purge"),
                ImportanceClass::Important,
            )
            .expect("ingest");
    }

    // Pre-purge: every phrase resolves.
    for (scope, phrase) in [
        (scope_a, phrase_a),
        (scope_b, phrase_b),
        (scope_c, phrase_c),
        (scope_survivor, phrase_survivor),
    ] {
        let hits = store
            .search_fts(scope, phrase, 10)
            .expect("search pre-purge");
        assert_eq!(hits.len(), 1, "phrase {phrase} must match before purge");
    }

    // One batch purge for three scopes; the fourth survives.
    store
        .purge_fts_for_scopes(&[scope_a, scope_b, scope_c])
        .expect("batch purge");

    for (scope, phrase) in [
        (scope_a, phrase_a),
        (scope_b, phrase_b),
        (scope_c, phrase_c),
    ] {
        let hits = store
            .search_fts(scope, phrase, 10)
            .expect("search purged scope post-batch");
        assert!(
            hits.is_empty(),
            "phrase {phrase} must be gone after batch purge"
        );

        // Raw FTS MATCH must also return zero hits across the
        // entire table for the purged phrase (REBUILD truncated
        // the shadow tables and re-tokenised from surviving rows
        // only).
        let raw_count: i64 = store
            .raw_conn()
            .query_row(
                "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1",
                rusqlite::params![phrase],
                |row| row.get(0),
            )
            .expect("raw fts count");
        assert_eq!(
            raw_count, 0,
            "Raw FTS5 must contain zero rows for purged phrase {phrase} after batch purge"
        );
    }

    let survivor_hits = store
        .search_fts(scope_survivor, phrase_survivor, 10)
        .expect("search surviving scope post-batch");
    assert_eq!(
        survivor_hits.len(),
        1,
        "Surviving scope's FTS row must remain after batch purge"
    );

    // Calling the batch entry point a second time on the same
    // already-purged scopes must be a no-op: zero rows deleted, no
    // REBUILD issued, surviving scope still matches.
    store
        .purge_fts_for_scopes(&[scope_a, scope_b, scope_c])
        .expect("idempotent batch repurge");
    let survivor_hits = store
        .search_fts(scope_survivor, phrase_survivor, 10)
        .expect("search surviving scope after idempotent batch repurge");
    assert_eq!(
        survivor_hits.len(),
        1,
        "Surviving scope's FTS row must remain after idempotent batch repurge"
    );

    // Empty-slice batch purge is a no-op without touching the
    // database at all.
    store.purge_fts_for_scopes(&[]).expect("empty batch purge");
}

/// schema v14: the v14 `evidence_fts_cjk` companion
/// table (trigram-tokenised) is plaintext-derived in the same way
/// the v0..v13 `evidence_fts` (unicode61) table is, so the
/// cryptographic-forgetting contract requires both tables to be
/// purged together. This test pins that contract by ingesting a
/// pure-CJK body, verifying the CJK trigram index sees it, calling
/// `purge_fts_for_scope`, and asserting both `evidence_fts` and
/// `evidence_fts_cjk` are empty for the scope on a fresh re-open.
#[test]
fn fts_cjk_companion_table_is_purged_alongside_evidence_fts() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();

    // Distinctive ≥3-codepoint CJK substring so the trigram index
    // can serve the query — pre-purge it must hit, post-purge it
    // must not.
    let body = "今日の重要な会議の議事録です";
    let query = "重要な会議";
    let evidence_id;
    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open store");
        let res = store
            .ingest(
                scope,
                body.as_bytes(),
                Some("source:cjk-forgetting-test"),
                ImportanceClass::Important,
            )
            .expect("ingest");
        evidence_id = res.evidence_id;

        // Sanity: trigram index sees the query before purge.
        let hits = store
            .search_fts(scope, query, 10)
            .expect("search_fts pre-purge");
        assert_eq!(
            hits,
            vec![evidence_id],
            "evidence_fts_cjk must surface the CJK substring before purge"
        );

        // …and a raw probe confirms the row lives in
        // evidence_fts_cjk specifically, not just evidence_fts.
        let cjk_rows: i64 = store
            .raw_conn()
            .query_row(
                "SELECT COUNT(*) FROM evidence_fts_cjk WHERE scope_id = ?1",
                rusqlite::params![scope.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("count cjk rows pre-purge");
        assert_eq!(
            cjk_rows, 1,
            "ingest of CJK body must populate evidence_fts_cjk"
        );

        store
            .purge_fts_for_scope(scope)
            .expect("purge_fts_for_scope");
    }

    // Re-open the store and probe both FTS tables directly.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open store");

    let primary_rows: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts WHERE scope_id = ?1",
            rusqlite::params![scope.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count primary fts rows post-purge");
    assert_eq!(
        primary_rows, 0,
        "evidence_fts must contain no rows for the forgotten scope after purge"
    );

    let cjk_rows: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts_cjk WHERE scope_id = ?1",
            rusqlite::params![scope.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count cjk fts rows post-purge");
    assert_eq!(
        cjk_rows, 0,
        "evidence_fts_cjk must contain no rows for the forgotten scope after purge"
    );

    // Public API mirrors the raw probe.
    let hits = store
        .search_fts(scope, query, 10)
        .expect("search_fts post-purge");
    assert!(
        hits.is_empty(),
        "search_fts must return no rows for a forgotten scope after purge: {hits:?}"
    );
}
