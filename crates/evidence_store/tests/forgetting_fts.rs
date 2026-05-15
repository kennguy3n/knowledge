//! Integration test that surfaces the cryptographic-forgetting gap in
//! the FTS5 secondary index.
//!
//! Per the docs/DESIGN.md §3.1 / `docs/internal/PROGRESS.md` Phase 0 contract, the
//! substrate promises that "the scope id is the unit of cryptographic
//! forgetting." In practice the *body* of every evidence row is
//! encrypted under a scope-derived AEAD key (`scope:{uuid}:body:v1`),
//! and zeroizing that key plus the per-page SQLCipher key would
//! render the bodies unrecoverable. **However**, the `evidence_fts`
//! virtual table (SQLite FTS5) keeps the tokenized **plaintext** of
//! every ingested body as the index payload — that is how FTS5 works
//! — so it survives DEK destruction.
//!
//! This test pins that gap into CI so the team cannot accidentally
//! market the substrate as "cryptographically forgettable" without
//! also delivering one of the mitigations listed in the TODO at the
//! bottom of this file. The assertions are stated in the **positive**
//! form (FTS still finds the term after the in-memory DEK cache is
//! dropped) precisely so the test stays green while the gap exists
//! and so a future PR that closes the gap has to explicitly update
//! this file.

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
/// can find it, then document — via assertions on the raw
/// `evidence_fts` table — that destroying the scope DEK in memory
/// does **not** erase the plaintext tokens from the FTS5 index.
///
/// **Status:** intentional gap. See the TODO at the bottom of the
/// file for the three mitigation strategies that would actually
/// deliver cryptographic forgetting for the FTS surface.
#[test]
fn fts_index_retains_plaintext_after_scope_dek_destruction() {
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

        // Sanity: FTS5 actually indexed the phrase.
        let hits = store
            .search_fts(scope, FORGETTING_PHRASE, 10)
            .expect("search_fts");
        assert_eq!(hits, vec![evidence_id], "FTS5 must surface the phrase");
    } // `store` drops here, which zeroizes the master key + cached
      // scope AEAD keys. This is the closest analogue we currently
      // have to "destroying the scope DEK" — there is no public
      // `destroy_scope_dek(scope_id)` API.

    // Re-open the store with the SAME master key so we can probe the
    // FTS table directly. The master key (and the SQLCipher page
    // key it derives) is *not* the unit being destroyed in a
    // hypothetical per-scope forgetting flow, so leaving it intact
    // is the realistic worst case.
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open store");

    // The body AEAD is keyed off `scope:{uuid}:body:v1` which is
    // re-derived from the master key on demand, so to simulate "the
    // scope DEK is gone" we never call `read_body` here. Instead we
    // probe the FTS5 index by raw SQL — this is what an attacker
    // (or a confused operator) would see after deleting the scope
    // metadata but leaving the database file behind.
    let raw_term_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1 AND scope_id = ?2",
            rusqlite::params![FORGETTING_PHRASE, scope.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count fts rows");
    assert_eq!(
        raw_term_count, 1,
        "FTS5 index still contains the plaintext phrase after dropping \
         the in-memory scope DEK cache — this is the cryptographic-\
         forgetting gap the TODO below tracks."
    );

    // Public API mirrors the raw probe.
    let hits = store
        .search_fts(scope, FORGETTING_PHRASE, 10)
        .expect("search_fts after re-open");
    assert_eq!(
        hits,
        vec![evidence_id],
        "search_fts must still hit the phrase: the FTS5 index was not \
         re-keyed or rebuilt when the scope DEK was destroyed."
    );
}

// TODO(security/Phase 7 forgetting): close the FTS5 cryptographic-
// forgetting gap pinned by `fts_index_retains_plaintext_after_scope_dek_destruction`
// above. Three viable mitigations, any one of which would let us flip
// that test from "FTS5 still finds the phrase" to "FTS5 no longer
// finds the phrase":
//
//   1. **Rebuild the FTS table after key destruction.** When the
//      caller invokes a future `destroy_scope_dek(scope_id)` API,
//      `DELETE FROM evidence_fts WHERE scope_id = ?` and rebuild
//      the FTS5 index from the remaining (still-decryptable) rows.
//      Simple, but linear in the number of remaining rows.
//
//   2. **Encrypt FTS terms separately.** Use a per-scope token
//      encryption scheme (e.g. deterministic AES on token hashes
//      using the scope DEK) so that destroying the scope DEK makes
//      the FTS payload unsearchable. Trade-off: deterministic token
//      encryption leaks token frequency.
//
//   3. **Destroy the entire database key.** SQLCipher protects every
//      page (including the FTS5 shadow tables) with a single page
//      key derived from the master key. Zeroizing the master key
//      and rotating to a new SQLCipher database renders the *whole*
//      store — bodies, metadata, AND FTS index — unrecoverable.
//      Coarse-grained but bullet-proof.
//
// Until one of those lands, `docs/internal/PROGRESS.md` (Phase 0 forgetting line)
// and `docs/internal/MODULE_STATUS.md` ("Known security debt") must continue
// to call this gap out explicitly so consumers do not rely on
// per-scope cryptographic forgetting that does not yet exist.
