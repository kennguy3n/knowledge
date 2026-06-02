//! Integration test: connector attach → sync (stub) → observation
//! ingestion → scope inheritance → detach.
//!
//! Verifies that observations inherit the attached scope and are
//! inaccessible after scope DEK destroy.

use chrono::Utc;
use uuid::Uuid;

use connector_framework::{
    AttachmentRegistry, ConnectorEvent, ConnectorInstanceId, ConnectorKind, SyncRunResult,
};
use evidence_store::ImportanceClass;
use integration_tests::test_helpers::{open_store, padded_body, ScopeId};
use permission_service::{
    NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef, SubjectType,
    TupleStore,
};

#[test]
fn connector_attach_sync_detach_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let connector_id = ConnectorInstanceId::new_v4();

    // Permission setup: owner on scope_a.
    let mut tuple_store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let owner = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let obj_a = ObjectRef::new(ObjectType::Channel, scope_a.as_uuid());
    tuple_store
        .insert(RelationTuple::new(obj_a, Relation::Owner, owner))
        .unwrap();

    // 1. Attach connector.
    let mut registry = AttachmentRegistry::new();
    let attachment = registry
        .attach(
            connector_id,
            ConnectorKind::GoogleDrive,
            scope_a,
            ObjectType::Channel,
            &tuple_store,
            &ns,
            owner,
        )
        .unwrap();
    assert_eq!(attachment.scope_id, scope_a);

    // 2. Simulate sync events (using the event types directly).
    let events = vec![
        ConnectorEvent::DocumentCreated {
            document_id: connector_framework::SourceDocumentId::new("doc-1"),
            occurred_at: Utc::now(),
        },
        ConnectorEvent::DocumentCreated {
            document_id: connector_framework::SourceDocumentId::new("doc-2"),
            occurred_at: Utc::now(),
        },
    ];
    let sync_result = SyncRunResult {
        events: events.clone(),
        next_cursor: None,
    };
    assert_eq!(sync_result.events.len(), 2);

    // 3. Ingest evidence under the attached scope.
    let mut store = open_store(&db_path);
    let body1 = padded_body("meeting notes from connector sync about Atlas");
    let r1 = store
        .ingest(
            scope_a,
            &body1,
            Some("connector:doc-1"),
            ImportanceClass::Useful,
        )
        .unwrap();
    let body2 = padded_body("design doc sync via connector about metrics");
    let r2 = store
        .ingest(
            scope_a,
            &body2,
            Some("connector:doc-2"),
            ImportanceClass::Useful,
        )
        .unwrap();

    // Verify scope inheritance.
    let row1 = store.get(r1.evidence_id).unwrap().unwrap();
    let row2 = store.get(r2.evidence_id).unwrap().unwrap();
    assert_eq!(row1.scope_id, scope_a);
    assert_eq!(row2.scope_id, scope_a);

    // 4. FTS query returns hits under scope_a.
    let hits = store
        .search_fts(scope_a, "connector", 100)
        .expect("search pre-detach");
    assert!(hits.len() >= 2, "both ingested rows searchable");

    // 5. Detach connector.
    let detached = registry
        .detach(connector_id, &tuple_store, &ns, owner)
        .unwrap();
    assert_eq!(detached.scope_id, scope_a);

    // 6. Cryptographic forgetting: destroy scope DEK.
    store
        .purge_body_key_wraps_for_scope(scope_a)
        .expect("purge wraps");
    store.purge_fts_for_scope(scope_a).expect("purge fts");
    store
        .record_forgotten_scope(scope_a)
        .expect("record forgotten");
    store.delete_scope_dek(scope_a).expect("delete dek");

    // 7. Verify inaccessible after forgetting.
    let hits_post = store.search_fts(scope_a, "connector", 100).expect("search");
    assert!(hits_post.is_empty(), "FTS hits gone after DEK destroy");

    let read_err = store.read_body(r1.evidence_id);
    assert!(read_err.is_err(), "body read fails after DEK destroy");
}

#[test]
fn duplicate_connector_attach_is_rejected() {
    let scope = ScopeId::new_v4();
    let connector_id = ConnectorInstanceId::new_v4();

    let mut tuple_store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let owner = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let obj = ObjectRef::new(ObjectType::Channel, scope.as_uuid());
    tuple_store
        .insert(RelationTuple::new(obj, Relation::Owner, owner))
        .unwrap();

    let mut registry = AttachmentRegistry::new();
    registry
        .attach(
            connector_id,
            ConnectorKind::GoogleDrive,
            scope,
            ObjectType::Channel,
            &tuple_store,
            &ns,
            owner,
        )
        .unwrap();

    // Second attach with same connector id must fail.
    let err = registry.attach(
        connector_id,
        ConnectorKind::GoogleDrive,
        scope,
        ObjectType::Channel,
        &tuple_store,
        &ns,
        owner,
    );
    assert!(err.is_err(), "duplicate attachment must be rejected");
}
