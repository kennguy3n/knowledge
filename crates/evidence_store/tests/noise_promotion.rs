//! Integration tests for noise promotion and retroactive reclassification.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, StoragePath,
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
fn promote_from_ring_buffer_creates_evidence_row() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest as Noise — goes to ring buffer.
    let noise_result = store
        .ingest(
            scope,
            b"+1 on the PostgreSQL decision",
            Some("slack:msg:C001"),
            ImportanceClass::Noise,
        )
        .unwrap();
    assert_eq!(noise_result.storage_path, StoragePath::RingBuffer);

    // Read ring buffer entries to find the id.
    let entries = store.ring_buffer_read_window(scope).unwrap();
    assert_eq!(entries.len(), 1);
    let rb_id = entries[0].id;

    // Promote the ring buffer entry to Important.
    let promoted = store
        .promote_from_ring_buffer(rb_id, scope, ImportanceClass::Important)
        .unwrap()
        .expect("promotion should succeed");

    assert_ne!(promoted.storage_path, StoragePath::RingBuffer);

    // Ring buffer should now be empty.
    let entries = store.ring_buffer_read_window(scope).unwrap();
    assert!(entries.is_empty());

    // The promoted evidence should be readable.
    let body = store.read_body(promoted.evidence_id).unwrap();
    assert_eq!(body, b"+1 on the PostgreSQL decision");

    // The promoted evidence should be searchable via FTS.
    let results = store.search_fts(scope, "PostgreSQL", 10).unwrap();
    assert!(results.contains(&promoted.evidence_id));
}

#[test]
fn promote_nonexistent_ring_buffer_entry_returns_none() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let result = store
        .promote_from_ring_buffer(999, scope, ImportanceClass::Useful)
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn reclassify_stores_override() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest as Useful.
    let result = store
        .ingest(
            scope,
            b"Important decision about the database migration.",
            Some("slack:msg:C001"),
            ImportanceClass::Useful,
        )
        .unwrap();

    // Reclassify to Critical.
    store
        .reclassify(
            result.evidence_id,
            ImportanceClass::Critical,
            Some("user override"),
        )
        .unwrap();

    // Check the override.
    let override_result = store
        .get_reclassification_override(result.evidence_id)
        .unwrap();
    assert!(override_result.is_some());
    let (class, reason) = override_result.unwrap();
    assert_eq!(class, ImportanceClass::Critical);
    assert_eq!(reason.as_deref(), Some("user override"));
}

#[test]
fn effective_importance_uses_override_when_present() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let result = store
        .ingest(
            scope,
            b"Decision to use PostgreSQL.",
            Some("slack:msg:C001"),
            ImportanceClass::Useful,
        )
        .unwrap();

    // Without override, effective importance is the original.
    assert_eq!(
        store.effective_importance(result.evidence_id).unwrap(),
        ImportanceClass::Useful
    );

    // Reclassify to Critical.
    store
        .reclassify(result.evidence_id, ImportanceClass::Critical, None)
        .unwrap();

    // With override, effective importance is Critical.
    assert_eq!(
        store.effective_importance(result.evidence_id).unwrap(),
        ImportanceClass::Critical
    );
}

#[test]
fn reclassify_is_idempotent_and_updates() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let result = store
        .ingest(
            scope,
            b"Decision about the migration plan.",
            Some("slack:msg:C001"),
            ImportanceClass::Useful,
        )
        .unwrap();

    // First reclassification.
    store
        .reclassify(
            result.evidence_id,
            ImportanceClass::Important,
            Some("first override"),
        )
        .unwrap();

    // Second reclassification — should update, not duplicate.
    store
        .reclassify(
            result.evidence_id,
            ImportanceClass::Critical,
            Some("second override"),
        )
        .unwrap();

    let (class, reason) = store
        .get_reclassification_override(result.evidence_id)
        .unwrap()
        .unwrap();
    assert_eq!(class, ImportanceClass::Critical);
    assert_eq!(reason.as_deref(), Some("second override"));
}

#[test]
fn no_override_returns_original_importance() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let result = store
        .ingest(
            scope,
            b"Useful note about the project.",
            Some("slack:msg:C001"),
            ImportanceClass::Useful,
        )
        .unwrap();

    // No override — should return original.
    assert!(store
        .get_reclassification_override(result.evidence_id)
        .unwrap()
        .is_none());

    assert_eq!(
        store.effective_importance(result.evidence_id).unwrap(),
        ImportanceClass::Useful
    );
}

#[test]
fn promote_from_ring_buffer_wrong_scope_returns_none() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    // Ingest as Noise in scope_a.
    store
        .ingest(
            scope_a,
            b"noise message",
            Some("slack:msg:A"),
            ImportanceClass::Noise,
        )
        .unwrap();

    let entries = store.ring_buffer_read_window(scope_a).unwrap();
    let rb_id = entries[0].id;

    // Try to promote from scope_b — should fail (entry not found
    // for this scope).
    let result = store
        .promote_from_ring_buffer(rb_id, scope_b, ImportanceClass::Important)
        .unwrap();
    assert!(result.is_none());

    // Ring buffer entry should still be there for scope_a.
    let entries = store.ring_buffer_read_window(scope_a).unwrap();
    assert_eq!(entries.len(), 1);
}

#[test]
fn delete_ring_buffer_entry() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    store
        .ingest(
            scope,
            b"noise message 1",
            Some("slack:msg:A"),
            ImportanceClass::Noise,
        )
        .unwrap();
    store
        .ingest(
            scope,
            b"noise message 2",
            Some("slack:msg:B"),
            ImportanceClass::Noise,
        )
        .unwrap();

    let entries = store.ring_buffer_read_window(scope).unwrap();
    assert_eq!(entries.len(), 2);

    // Delete the first entry.
    store.delete_ring_buffer_entry(entries[0].id).unwrap();

    let entries = store.ring_buffer_read_window(scope).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, b"noise message 2");
}
