//! Integration tests for the permission service.

use uuid::Uuid;

use permission_service::{
    check_permission, NamespaceConfig, NamespaceRegistry, ObjectRef, ObjectType, PermissionError,
    Relation, RelationTuple, SubjectRef, SubjectType, TupleStore,
};

#[test]
fn tuple_crud_round_trip() {
    let mut store = TupleStore::new();
    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let user = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let t = RelationTuple::new(tenant, Relation::Owner, user);

    store.insert(t).unwrap();
    assert!(store.contains(&t));
    assert_eq!(store.len(), 1);

    // Duplicate insert errors.
    let err = store.insert(t).unwrap_err();
    assert_eq!(err, PermissionError::DuplicateTuple);

    store.remove(&t).unwrap();
    assert!(!store.contains(&t));

    // Removing a non-existent tuple errors.
    let err = store.remove(&t).unwrap_err();
    assert_eq!(err, PermissionError::NotFound);
}

#[test]
fn upsert_is_idempotent() {
    let mut store = TupleStore::new();
    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let user = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let t = RelationTuple::new(tenant, Relation::Owner, user);
    assert!(store.upsert(t));
    assert!(!store.upsert(t));
    assert_eq!(store.len(), 1);
}

#[test]
fn inheritance_chain_owner_to_viewer() {
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    // Alice is the tenant owner.
    store
        .insert(RelationTuple::new(tenant, Relation::Owner, alice))
        .unwrap();

    // Owner implies Admin / Editor / Member / Viewer per the default
    // inheritance chain.
    for wanted in [
        Relation::Owner,
        Relation::Admin,
        Relation::Editor,
        Relation::Member,
        Relation::Viewer,
    ] {
        assert!(
            check_permission(&store, &ns, tenant, wanted, alice),
            "owner should imply {}",
            wanted.as_str()
        );
    }

    // The reverse should not hold.
    let bob = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    store
        .insert(RelationTuple::new(tenant, Relation::Viewer, bob))
        .unwrap();
    assert!(check_permission(&store, &ns, tenant, Relation::Viewer, bob));
    for higher in [
        Relation::Owner,
        Relation::Admin,
        Relation::Editor,
        Relation::Member,
    ] {
        assert!(
            !check_permission(&store, &ns, tenant, higher, bob),
            "viewer should not imply {}",
            higher.as_str()
        );
    }
}

#[test]
fn userset_rewrite_via_subject_relation() {
    // (Domain, d-1) # editor @ (Tenant, t-1) # admin
    // (Tenant, t-1) # admin @ (User, u-7)
    //
    // Then u-7 should have editor on d-1 (tenant admin -> domain
    // editor via the userset rewrite, then expanded back through the
    // domain's own namespace to viewer / member as well).
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let domain = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    store
        .insert(RelationTuple::new(tenant, Relation::Admin, alice))
        .unwrap();
    store
        .insert(RelationTuple::new(
            domain,
            Relation::Editor,
            SubjectRef::via(SubjectType::Tenant, tenant.object_id, Relation::Admin),
        ))
        .unwrap();

    assert!(check_permission(
        &store,
        &ns,
        domain,
        Relation::Editor,
        alice
    ));
    assert!(check_permission(
        &store,
        &ns,
        domain,
        Relation::Member,
        alice
    ));
    // Alice is an admin on the tenant, not the domain, so she should
    // NOT inherit `Owner` on the domain.
    assert!(!check_permission(
        &store,
        &ns,
        domain,
        Relation::Owner,
        alice
    ));
}

#[test]
fn negative_case_unrelated_subject() {
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let mallory = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    store
        .insert(RelationTuple::new(tenant, Relation::Owner, alice))
        .unwrap();
    assert!(!check_permission(
        &store,
        &ns,
        tenant,
        Relation::Viewer,
        mallory
    ));
    assert!(!check_permission(
        &store,
        &ns,
        tenant,
        Relation::Owner,
        mallory
    ));
}

#[test]
fn synthesizer_relation_is_orthogonal() {
    // Synthesizer is not part of the Owner ⇒ Viewer chain. An owner
    // is NOT automatically a synthesizer; granting Synthesizer
    // explicitly is required.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let channel = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    store
        .insert(RelationTuple::new(channel, Relation::Owner, alice))
        .unwrap();
    assert!(!check_permission(
        &store,
        &ns,
        channel,
        Relation::Synthesizer,
        alice
    ));
    store
        .insert(RelationTuple::new(channel, Relation::Synthesizer, alice))
        .unwrap();
    assert!(check_permission(
        &store,
        &ns,
        channel,
        Relation::Synthesizer,
        alice
    ));
}

#[test]
fn empty_namespace_falls_back_to_direct() {
    // With an empty namespace registry, only direct tuple lookups
    // succeed.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::new();
    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    store
        .insert(RelationTuple::new(tenant, Relation::Owner, alice))
        .unwrap();
    assert!(check_permission(
        &store,
        &ns,
        tenant,
        Relation::Owner,
        alice
    ));
    assert!(!check_permission(
        &store,
        &ns,
        tenant,
        Relation::Member,
        alice
    ));
}

#[test]
fn closure_in_custom_namespace() {
    // A custom namespace where Editor implies Viewer only.
    let mut ns = NamespaceRegistry::new();
    ns.register(
        NamespaceConfig::new(ObjectType::Channel).imply(Relation::Editor, &[Relation::Viewer]),
    )
    .unwrap();

    let mut store = TupleStore::new();
    let chan = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    store
        .insert(RelationTuple::new(chan, Relation::Editor, alice))
        .unwrap();
    assert!(check_permission(&store, &ns, chan, Relation::Editor, alice));
    assert!(check_permission(&store, &ns, chan, Relation::Viewer, alice));
    assert!(!check_permission(&store, &ns, chan, Relation::Owner, alice));
}
