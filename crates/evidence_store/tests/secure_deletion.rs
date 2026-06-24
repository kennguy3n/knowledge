//! Integration tests for secure deletion automation (VACUUM + TRIM).

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, SecureDeletionReport,
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
const LARGE_BODY: &[u8] = b"This is a large body that exceeds the inline threshold to force body_table storage. \
We need enough text to push past 4096 bytes so the routing logic sends it to the deduplicated body_store \
table rather than keeping it inline in the evidence row. \
Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et \
dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip \
ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore \
eu fugiat nulla pariatur. Excepteur sint occaecat cupiditate non proident, sunt in culpa qui officia \
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

fn db_file_size(dir: &tempfile::TempDir) -> u64 {
    let db_path = dir.path().join("evidence.db");
    std::fs::metadata(&db_path).map_or(0, |m| m.len())
}

#[test]
fn secure_vacuum_on_empty_store() {
    let (_dir, mut store) = fresh_store();
    store.secure_vacuum().expect("vacuum should succeed");
}

#[test]
fn secure_vacuum_after_forget_returns_report() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest a large body.
    store
        .ingest(scope, LARGE_BODY, Some("source:a"), ImportanceClass::Important)
        .unwrap();

    // Forget the scope — purge_body_key_wraps_for_scope already
    // GCs the orphaned body, so secure_vacuum_after_forget should
    // find 0 additional orphans.
    store.purge_body_key_wraps_for_scope(scope).unwrap();

    // Run secure vacuum.
    let report = store.secure_vacuum_after_forget().unwrap();
    assert!(report.vacuum_completed);
    // Orphans already cleaned by purge_body_key_wraps_for_scope.
    assert_eq!(report.orphaned_bodies_purged, 0);
}

#[test]
fn secure_vacuum_after_forget_no_orphans() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    // Ingest the same body into two scopes.
    store
        .ingest(scope_a, LARGE_BODY, Some("source:a"), ImportanceClass::Important)
        .unwrap();
    store
        .ingest(scope_b, LARGE_BODY, Some("source:b"), ImportanceClass::Important)
        .unwrap();

    // Forget only scope_a — body should survive for scope_b.
    store.purge_body_key_wraps_for_scope(scope_a).unwrap();

    // Run secure vacuum — no orphans since scope_b still wraps.
    let report = store.secure_vacuum_after_forget().unwrap();
    assert!(report.vacuum_completed);
    assert_eq!(report.orphaned_bodies_purged, 0);
}

#[test]
fn secure_vacuum_reduces_file_size_after_forgetting() {
    let (dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest several large bodies to grow the database file.
    for i in 0..5 {
        let mut body = LARGE_BODY.to_vec();
        body.extend_from_slice(format!(" -- entry {i}").as_bytes());
        store
            .ingest(scope, &body, Some("source"), ImportanceClass::Important)
            .unwrap();
    }

    let size_before = db_file_size(&dir);

    // Forget the scope — all wraps deleted, bodies orphaned.
    store.purge_body_key_wraps_for_scope(scope).unwrap();

    // Run secure vacuum.
    store.secure_vacuum_after_forget().unwrap();

    let size_after = db_file_size(&dir);

    // The file should be smaller after VACUUM reclaimed the pages.
    // We can't assert an exact size, but it should be noticeably
    // smaller since all the large bodies were purged.
    assert!(
        size_after < size_before,
        "file size should decrease after VACUUM: before={size_before}, after={size_after}"
    );
}

#[test]
fn secure_vacuum_preserves_live_data() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    // Ingest into both scopes.
    // Use large bodies so they go to body_store (not inline).
    let body_a = {
        let mut v = LARGE_BODY.to_vec();
        v.extend_from_slice(b" -- A");
        v
    };
    let body_b = {
        let mut v = LARGE_BODY.to_vec();
        v.extend_from_slice(b" -- B");
        v
    };
    let result_a = store
        .ingest(scope_a, &body_a, Some("src:a"), ImportanceClass::Important)
        .unwrap();
    let result_b = store
        .ingest(scope_b, &body_b, Some("src:b"), ImportanceClass::Important)
        .unwrap();

    // Forget scope_a only.
    store.purge_body_key_wraps_for_scope(scope_a).unwrap();

    // Run secure vacuum.
    store.secure_vacuum_after_forget().unwrap();

    // scope_b's data should still be readable.
    let body = store.read_body(result_b.evidence_id).unwrap();
    assert_eq!(body, body_b.as_slice());

    // scope_a's body_store row was GC'd by purge_body_key_wraps_for_scope,
    // so read_body should fail.
    assert!(store.read_body(result_a.evidence_id).is_err());
}

#[test]
fn secure_deletion_report_fields() {
    let report = SecureDeletionReport {
        orphaned_bodies_purged: 3,
        vacuum_completed: true,
    };
    assert_eq!(report.orphaned_bodies_purged, 3);
    assert!(report.vacuum_completed);
}

#[test]
fn secure_vacuum_idempotent() {
    let (_dir, mut store) = fresh_store();

    // Running VACUUM multiple times should be safe.
    store.secure_vacuum().unwrap();
    store.secure_vacuum().unwrap();
    store.secure_vacuum().unwrap();
}

#[test]
fn secure_vacuum_after_forget_with_multiple_scopes() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let scope_c = ScopeId::new_v4();

    // Ingest the same body into all three scopes.
    store
        .ingest(scope_a, LARGE_BODY, Some("src:a"), ImportanceClass::Important)
        .unwrap();
    store
        .ingest(scope_b, LARGE_BODY, Some("src:b"), ImportanceClass::Important)
        .unwrap();
    store
        .ingest(scope_c, LARGE_BODY, Some("src:c"), ImportanceClass::Important)
        .unwrap();

    // Forget all three scopes.
    store.purge_body_key_wraps_for_scope(scope_a).unwrap();
    store.purge_body_key_wraps_for_scope(scope_b).unwrap();
    store.purge_body_key_wraps_for_scope(scope_c).unwrap();

    // Run secure vacuum — purge_body_key_wraps_for_scope already
    // GC'd the orphaned body, so 0 additional orphans.
    let report = store.secure_vacuum_after_forget().unwrap();
    assert!(report.vacuum_completed);
    assert_eq!(report.orphaned_bodies_purged, 0);

    // Running again should find no more orphans.
    let report2 = store.secure_vacuum_after_forget().unwrap();
    assert_eq!(report2.orphaned_bodies_purged, 0);
}
