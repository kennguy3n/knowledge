//! End-to-end ingest → FTS query → cryptographic-forgetting test.
//!
//! Walks the substrate's evidence plane through the full lifecycle a
//! production caller exercises:
//!
//! 1. Open a fresh SQLCipher-backed [`EvidenceStore`].
//! 2. Ingest ten messages, five each across two distinct scopes.
//!    Bodies are sized above [`DEFAULT_INLINE_THRESHOLD_BYTES`] so
//!    they land in the deduplicated body table — that's the only
//!    routing path where the substrate's "destroy the CEK wrap to
//!    forget" guarantee actually holds.
//! 3. FTS5-search both scopes and assert each side sees exactly its
//!    own rows.
//! 4. Forget scope A through the canonical sequence:
//!    `purge_body_key_wraps_for_scope` (destroys the CEK wrap) →
//!    `purge_fts_for_scope` (removes tokens + rebuilds the FTS
//!    shadow tables) → `record_forgotten_scope` (durable
//!    tombstone) → `delete_scope_dek` (clears the in-memory key
//!    cache and the wrapped-key row).
//! 5. Verify:
//!    * Scope A's FTS queries now return no hits.
//!    * Scope A's `read_body` for the surviving evidence rows fails.
//!    * Scope B's queries and body reads are unaffected.

use evidence_store::{
    EvidenceId, EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId,
    DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::TempDir;

const MASTER_KEY: [u8; 32] = [0xA5; 32];
/// Bodies must exceed the inline threshold so they take the
/// body-table path — that path is the one cryptographic forgetting
/// actually shreds.
const BODY_SIZE: usize = DEFAULT_INLINE_THRESHOLD_BYTES * 4;

fn open_store(path: &std::path::Path) -> EvidenceStore {
    EvidenceStore::open(path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open evidence store")
}

fn body_for(scope_tag: &str, idx: usize) -> Vec<u8> {
    // Two-token shape so search_fts has both a per-row unique token
    // and a per-scope shared token to query against.
    let prefix = format!("scope-{scope_tag} unique-token-{idx} migration deadline channel-recap ");
    let mut body = prefix.into_bytes();
    body.resize(BODY_SIZE, b'.');
    body
}

#[test]
fn ingest_query_forget_one_scope_preserves_the_other() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    let mut store = open_store(&path);

    // 1. Ingest 5 rows per scope (10 total).
    let mut ids_a = Vec::new();
    let mut ids_b = Vec::new();
    for i in 0..5 {
        let ra = store
            .ingest(
                scope_a,
                &body_for("A", i),
                Some("integration:ingest"),
                ImportanceClass::Useful,
            )
            .expect("ingest A");
        let rb = store
            .ingest(
                scope_b,
                &body_for("B", i),
                Some("integration:ingest"),
                ImportanceClass::Useful,
            )
            .expect("ingest B");
        ids_a.push(ra.evidence_id);
        ids_b.push(rb.evidence_id);
    }

    // 2. Both scopes are queryable + readable.
    assert_eq!(
        store
            .search_fts(scope_a, "migration", 100)
            .expect("search A pre-forget")
            .len(),
        5,
        "scope A's FTS hits should be its own five rows"
    );
    assert_eq!(
        store
            .search_fts(scope_b, "migration", 100)
            .expect("search B pre-forget")
            .len(),
        5,
        "scope B's FTS hits should be its own five rows"
    );
    // Per-row body reads succeed across both scopes.
    for &id in &ids_a {
        assert_eq!(store.read_body(id).expect("read body A").len(), BODY_SIZE);
    }
    for &id in &ids_b {
        assert_eq!(store.read_body(id).expect("read body B").len(), BODY_SIZE);
    }

    // 3. Forget scope A via the canonical sequence.
    store
        .purge_body_key_wraps_for_scope(scope_a)
        .expect("purge wraps A");
    store.purge_fts_for_scope(scope_a).expect("purge fts A");
    store
        .record_forgotten_scope(scope_a)
        .expect("record forgotten A");
    store.delete_scope_dek(scope_a).expect("delete scope DEK A");

    // 4a. Scope A's FTS queries return nothing.
    let post_forget_a: Vec<EvidenceId> = store
        .search_fts(scope_a, "migration", 100)
        .expect("search A post-forget");
    assert!(
        post_forget_a.is_empty(),
        "scope A must have no FTS hits after forget, got {} hits",
        post_forget_a.len()
    );

    // 4b. Scope A's body reads fail — the CEK wrap is gone.
    for &id in &ids_a {
        let err = store.read_body(id);
        assert!(
            err.is_err(),
            "scope A read_body must fail after forget, got {err:?}"
        );
    }

    // 5. Scope B is unaffected.
    let post_forget_b: Vec<EvidenceId> = store
        .search_fts(scope_b, "migration", 100)
        .expect("search B post-forget");
    assert_eq!(
        post_forget_b.len(),
        5,
        "scope B's FTS hits must survive scope A's forget"
    );
    for &id in &ids_b {
        assert_eq!(
            store.read_body(id).expect("read body B post-forget").len(),
            BODY_SIZE
        );
    }

    // 6. The forgotten-scope tombstone is durable across reopen.
    drop(store);
    let store = open_store(&path);
    let tombstones = store.load_forgotten_scopes().expect("load forgotten");
    assert!(
        tombstones.contains(&scope_a),
        "scope A's tombstone must survive reopen, got {tombstones:?}"
    );
    assert!(
        !tombstones.contains(&scope_b),
        "scope B must NOT be tombstoned"
    );
}
