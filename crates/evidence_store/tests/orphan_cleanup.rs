//! Integration tests for shared-ciphertext orphan detection and cleanup.
//!
//! These tests verify the tension between content-hash deduplication
//! and per-scope cryptographic forgetting:
//!
//! 1. When two scopes share the same body, forgetting one scope
//!    deletes its CEK wrap but leaves the body_store row for the
//!    other scope.
//! 2. When all scopes sharing a body are forgotten, the body_store
//!    row becomes an orphan (zero remaining wraps) and should be
//!    garbage-collected.
//! 3. `count_orphaned_bodies` and `purge_orphaned_bodies` provide
//!    standalone diagnostic and cleanup for orphaned ciphertext.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId,
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

/// Body must be large enough to route to body_store (not inline).
/// Default inline threshold is 4096 bytes.
const LARGE_BODY: &[u8] = b"This is a large body that exceeds the inline threshold to force body_table storage. \
We need enough text to push past 4096 bytes so the routing logic sends it to the deduplicated body_store \
table rather than keeping it inline in the evidence row. \
Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et \
dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip \
ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore \
eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia \
deserunt mollit anim id est laborum. Sed ut perspiciatis unde omnis iste natus error sit voluptatem \
accusantium doloremque laudantium, totam rem aperiam, eaque ipsa quae ab illo inventore veritatis et \
quasi architecto beatae vitae dicta sunt explicabo. Nemo enim ipsam voluptatem quia voluptas sit \
aspernatur aut odit aut fugit, sed quia consequuntur magni dolores eos qui ratione voluptatem sequi \
nesciunt. Neque porro quisquam est, qui dolorem ipsum quia dolor sit amet, consectetur, adipisci \
velit, sed quia non numquam eius modi tempora incidunt ut labore et dolore magnam aliquam quaerat \
voluptatem. Ut enim ad minima veniam, quis nostrum exercitationem ullam corporis suscipit \
laboriosam, nisi ut aliquid ex ea commodi consequatur. Quis autem vel eum iure reprehenderit qui \
in ea voluptate velit esse quam nihil molestiae consequatur, vel illum qui dolorem eum fugiat quo \
voluptas nulla pariatur. At vero eos et accusamus et iusto odio dignissimos ducimus qui blanditiis \
praesentium voluptatum deleniti atque corrupti quos dolores et quas molestias excepturi sint \
occaecati cupiditate non provident, similique sunt in culpa qui officia deserunt mollitia animi, id \
est laborum et dolorum fuga. Et harum quidem rerum facilis est et expedita distinctio. Nam libero \
tempore, cum soluta nobis est eligendi optio cumque nihil impedit quo minus id quod maxime placeat \
facere possimus, omnis voluptas assumenda est, omnis dolor repellendus. Temporibus autem quibusdam \
et aut officiis debitis aut rerum necessitatibus saepe eveniet ut et voluptates repudiandae sint \
et molestiae non recusandae. Itaque earum rerum hic tenetur a sapiente delectus, ut aut reiciendis \
voluptatibus maiores alias consequatur aut perferendis doloribus asperiores repellat.";

#[test]
fn no_orphans_on_fresh_store() {
    let (_dir, store) = fresh_store();
    assert_eq!(store.count_orphaned_bodies().unwrap(), 0);
}

#[test]
fn purge_orphaned_bodies_on_empty_store() {
    let (_dir, mut store) = fresh_store();
    let deleted = store.purge_orphaned_bodies().unwrap();
    assert_eq!(deleted, 0);
}

#[test]
fn shared_body_survives_one_scope_forgetting() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    // Ingest the same large body into both scopes.
    let r_a = store
        .ingest(scope_a, LARGE_BODY, Some("source:a"), ImportanceClass::Important)
        .unwrap();
    let r_b = store
        .ingest(scope_b, LARGE_BODY, Some("source:b"), ImportanceClass::Important)
        .unwrap();

    // Both should share the same content hash.
    assert_eq!(r_a.content_hash, r_b.content_hash);

    // No orphans yet.
    assert_eq!(store.count_orphaned_bodies().unwrap(), 0);

    // Forget scope_a — its CEK wrap is deleted, but scope_b's wrap
    // keeps the body alive.
    store.purge_body_key_wraps_for_scope(scope_a).unwrap();

    // Still no orphans — scope_b still wraps the body.
    assert_eq!(store.count_orphaned_bodies().unwrap(), 0);

    // scope_b can still read the body.
    let body = store.read_body(r_b.evidence_id).unwrap();
    assert_eq!(body, LARGE_BODY);
}

#[test]
fn orphaned_body_cleaned_up_when_all_scopes_forgotten() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    // Ingest the same large body into both scopes.
    store
        .ingest(scope_a, LARGE_BODY, Some("source:a"), ImportanceClass::Important)
        .unwrap();
    store
        .ingest(scope_b, LARGE_BODY, Some("source:b"), ImportanceClass::Important)
        .unwrap();

    // No orphans yet.
    assert_eq!(store.count_orphaned_bodies().unwrap(), 0);

    // Forget both scopes.
    store.purge_body_key_wraps_for_scope(scope_a).unwrap();
    store.purge_body_key_wraps_for_scope(scope_b).unwrap();

    // The body_store row should have been GCed by the second purge
    // (purge_body_key_wraps_for_scope deletes orphaned rows).
    assert_eq!(store.count_orphaned_bodies().unwrap(), 0);
}

#[test]
fn purge_orphaned_bodies_cleans_standalone_orphans() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest a large body.
    store
        .ingest(scope, LARGE_BODY, Some("source:a"), ImportanceClass::Important)
        .unwrap();

    assert_eq!(store.count_orphaned_bodies().unwrap(), 0);

    // Simulate a crash or direct wrap deletion by deleting the wrap
    // without going through purge_body_key_wraps_for_scope.
    store
        .raw_conn()
        .execute(
            "DELETE FROM body_store_key_wraps WHERE scope_id = ?1",
            rusqlite::params![scope.as_uuid().as_bytes().as_slice()],
        )
        .unwrap();

    // Now the body_store row is orphaned.
    assert_eq!(store.count_orphaned_bodies().unwrap(), 1);

    // Purge orphans.
    let deleted = store.purge_orphaned_bodies().unwrap();
    assert_eq!(deleted, 1);

    // No more orphans.
    assert_eq!(store.count_orphaned_bodies().unwrap(), 0);
}

#[test]
fn purge_orphaned_bodies_is_idempotent() {
    let (_dir, mut store) = fresh_store();

    // Purge on empty store.
    assert_eq!(store.purge_orphaned_bodies().unwrap(), 0);
    // Purge again — still nothing.
    assert_eq!(store.purge_orphaned_bodies().unwrap(), 0);
}
