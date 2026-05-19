//! Integration test that pins the cryptographic-forgetting contract
//! for the FTS5 secondary index.
//!
//! Per `docs/DESIGN.md` §3.1, the
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
