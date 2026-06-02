//! Integration test: multi-scope isolation.
//!
//! Create multiple scopes with different users → verify cross-scope
//! isolation (user A cannot read user B's evidence, observations, or
//! concepts).

use uuid::Uuid;

use integration_tests::test_helpers::{open_store, padded_body, ImportanceClass, ScopeId};
use observation_engine::{LexiconExtractor, ObservationExtractor};
use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

#[test]
fn cross_scope_evidence_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let user_a = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let user_b = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    // Permission setup: user_a owns scope_a; user_b owns scope_b.
    let mut tuple_store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let obj_a = ObjectRef::new(ObjectType::Channel, scope_a.as_uuid());
    let obj_b = ObjectRef::new(ObjectType::Channel, scope_b.as_uuid());
    tuple_store
        .insert(RelationTuple::new(obj_a, Relation::Owner, user_a))
        .unwrap();
    tuple_store
        .insert(RelationTuple::new(obj_b, Relation::Owner, user_b))
        .unwrap();

    // Verify owners.
    assert!(check_permission(
        &tuple_store,
        &ns,
        obj_a,
        Relation::Owner,
        user_a
    ));
    assert!(check_permission(
        &tuple_store,
        &ns,
        obj_b,
        Relation::Owner,
        user_b
    ));

    // Cross-scope: user_a is NOT owner of scope_b and vice versa.
    assert!(!check_permission(
        &tuple_store,
        &ns,
        obj_b,
        Relation::Owner,
        user_a
    ));
    assert!(!check_permission(
        &tuple_store,
        &ns,
        obj_a,
        Relation::Owner,
        user_b
    ));

    // Ingest evidence into both scopes.
    let mut store = open_store(&db_path);
    let body_a = padded_body("scope A secret project about Atlas planning");
    let r_a = store
        .ingest(scope_a, &body_a, None, ImportanceClass::Important)
        .unwrap();
    let body_b = padded_body("scope B private metrics dashboard design");
    let r_b = store
        .ingest(scope_b, &body_b, None, ImportanceClass::Important)
        .unwrap();

    // Evidence belongs to correct scopes.
    let row_a = store.get(r_a.evidence_id).unwrap().unwrap();
    let row_b = store.get(r_b.evidence_id).unwrap().unwrap();
    assert_eq!(row_a.scope_id, scope_a);
    assert_eq!(row_b.scope_id, scope_b);

    // FTS: scope_a sees only its own evidence.
    let hits_a = store.search_fts(scope_a, "Atlas", 100).unwrap();
    assert!(!hits_a.is_empty(), "scope_a has Atlas");
    let hits_b_atlas = store.search_fts(scope_b, "Atlas", 100).unwrap();
    assert!(hits_b_atlas.is_empty(), "scope_b should not see Atlas");

    // FTS: scope_b sees only its own evidence.
    let hits_b = store.search_fts(scope_b, "metrics", 100).unwrap();
    assert!(!hits_b.is_empty(), "scope_b has metrics");
    let hits_a_metrics = store.search_fts(scope_a, "metrics", 100).unwrap();
    assert!(hits_a_metrics.is_empty(), "scope_a should not see metrics");
}

#[test]
fn cross_scope_observation_isolation() {
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    let extractor = LexiconExtractor::default();

    let obs_a = extractor.extract("We decided to ship Atlas next quarter", scope_a);
    let obs_b = extractor.extract("TODO deploy the monitoring stack", scope_b);

    assert!(!obs_a.is_empty());
    assert!(!obs_b.is_empty());

    // All observations in scope_a have scope_a's id.
    for o in &obs_a {
        assert_eq!(o.scope_id, scope_a, "observation should belong to scope_a");
    }

    // All observations in scope_b have scope_b's id.
    for o in &obs_b {
        assert_eq!(o.scope_id, scope_b, "observation should belong to scope_b");
    }
}

#[test]
fn scope_forgetting_does_not_affect_other_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    let mut store = open_store(&db_path);

    let body_a = padded_body("scope A data that will be forgotten");
    let r_a = store
        .ingest(scope_a, &body_a, None, ImportanceClass::Useful)
        .unwrap();
    let body_b = padded_body("scope B data that must survive");
    let r_b = store
        .ingest(scope_b, &body_b, None, ImportanceClass::Useful)
        .unwrap();

    // Forget scope_a.
    store.purge_body_key_wraps_for_scope(scope_a).unwrap();
    store.purge_fts_for_scope(scope_a).unwrap();
    store.record_forgotten_scope(scope_a).unwrap();
    store.delete_scope_dek(scope_a).unwrap();

    // scope_a's evidence is unreadable.
    assert!(store.read_body(r_a.evidence_id).is_err());

    // scope_b's evidence is still accessible.
    let body_read = store.read_body(r_b.evidence_id).unwrap();
    assert_eq!(body_read, body_b);
}

#[test]
fn viewer_cannot_write_to_scope() {
    let scope = ScopeId::new_v4();
    let owner = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let viewer = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    let mut tuple_store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let obj = ObjectRef::new(ObjectType::Channel, scope.as_uuid());

    tuple_store
        .insert(RelationTuple::new(obj, Relation::Owner, owner))
        .unwrap();
    tuple_store
        .insert(RelationTuple::new(obj, Relation::Viewer, viewer))
        .unwrap();

    // Owner has Editor (via Owner ⇒ Admin ⇒ Editor chain).
    assert!(check_permission(
        &tuple_store,
        &ns,
        obj,
        Relation::Editor,
        owner
    ));

    // Viewer cannot edit.
    assert!(!check_permission(
        &tuple_store,
        &ns,
        obj,
        Relation::Editor,
        viewer
    ));

    // But viewer can view.
    assert!(check_permission(
        &tuple_store,
        &ns,
        obj,
        Relation::Viewer,
        viewer
    ));
}
