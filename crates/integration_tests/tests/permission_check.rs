//! Cross-crate test for the Zanzibar-style permission service.
//!
//! Covers the canonical query shapes the substrate's caller surface
//! exposes:
//!
//! 1. Default `Owner ⇒ Admin ⇒ Editor ⇒ Member ⇒ Viewer` inheritance
//!    chain — holding `Owner` on a tenant implies every weaker
//!    relation; holding `Viewer` does NOT imply `Editor`.
//! 2. Userset rewrite — a `(Domain, d) # editor @ (Tenant, t) # admin`
//!    tuple means *anyone holding `admin` on the tenant also holds
//!    `editor` on the domain*. Verified for both an admin (positive)
//!    and a member (negative).
//! 3. Ambient roles (`Synthesizer`, `Proposer`) are orthogonal — they
//!    do not participate in the inheritance closure.
//! 4. Negative cases: an unrelated user holds nothing.

use uuid::Uuid;

use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

#[test]
fn full_inheritance_and_userset_rewrites_resolve() {
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let domain = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let channel = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());

    let owner = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let admin = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let viewer = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let stranger = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let synthesizer = SubjectRef::direct(SubjectType::Agent, Uuid::new_v4());

    // Direct tuples on the tenant.
    store
        .insert(RelationTuple::new(tenant, Relation::Owner, owner))
        .expect("insert owner");
    store
        .insert(RelationTuple::new(tenant, Relation::Admin, admin))
        .expect("insert admin");
    store
        .insert(RelationTuple::new(channel, Relation::Viewer, viewer))
        .expect("insert viewer on channel");

    // Synthesizer ambient role on the channel — orthogonal to the
    // membership chain.
    store
        .insert(RelationTuple::new(
            channel,
            Relation::Synthesizer,
            synthesizer,
        ))
        .expect("insert synthesizer");

    // Userset rewrite: `domain # editor @ tenant # admin` — any
    // tenant-admin gets editor on the domain.
    store
        .insert(RelationTuple::new(
            domain,
            Relation::Editor,
            SubjectRef::via(SubjectType::Tenant, tenant.object_id, Relation::Admin),
        ))
        .expect("insert userset rewrite");

    // 1. Owner satisfies every weaker tenant-scoped relation.
    for wanted in [
        Relation::Owner,
        Relation::Admin,
        Relation::Editor,
        Relation::Member,
        Relation::Viewer,
    ] {
        assert!(
            check_permission(&store, &ns, tenant, wanted, owner),
            "owner must satisfy tenant#{wanted:?}"
        );
    }

    // Owner ambient roles (Synthesizer / Proposer) are NOT implied
    // by the membership chain.
    assert!(
        !check_permission(&store, &ns, tenant, Relation::Synthesizer, owner),
        "Synthesizer must not be implied by the inheritance chain"
    );
    assert!(
        !check_permission(&store, &ns, tenant, Relation::Proposer, owner),
        "Proposer must not be implied by the inheritance chain"
    );

    // 2. Admin gets editor / member / viewer on tenant, but NOT
    // owner.
    assert!(check_permission(&store, &ns, tenant, Relation::Admin, admin));
    assert!(check_permission(
        &store,
        &ns,
        tenant,
        Relation::Editor,
        admin
    ));
    assert!(check_permission(
        &store,
        &ns,
        tenant,
        Relation::Viewer,
        admin
    ));
    assert!(
        !check_permission(&store, &ns, tenant, Relation::Owner, admin),
        "admin must NOT satisfy tenant#owner"
    );

    // 3. Userset rewrite resolves: admin on the tenant gets editor
    //    on the domain, but the channel-only viewer does not.
    assert!(
        check_permission(&store, &ns, domain, Relation::Editor, admin),
        "admin satisfies domain#editor via tenant#admin rewrite"
    );
    assert!(
        check_permission(&store, &ns, domain, Relation::Viewer, admin),
        "admin satisfies domain#viewer (Editor ⇒ Member ⇒ Viewer)"
    );
    assert!(
        !check_permission(&store, &ns, domain, Relation::Owner, admin),
        "userset rewrite must not lift admin to domain#owner"
    );
    assert!(
        !check_permission(&store, &ns, domain, Relation::Editor, viewer),
        "channel viewer must NOT satisfy domain#editor"
    );

    // 4. Channel viewer holds only viewer.
    assert!(check_permission(
        &store,
        &ns,
        channel,
        Relation::Viewer,
        viewer
    ));
    assert!(
        !check_permission(&store, &ns, channel, Relation::Editor, viewer),
        "viewer must NOT satisfy channel#editor"
    );

    // 5. Synthesizer ambient role is directly grantable but is NOT
    //    implied by membership.
    assert!(check_permission(
        &store,
        &ns,
        channel,
        Relation::Synthesizer,
        synthesizer
    ));
    assert!(
        !check_permission(&store, &ns, channel, Relation::Viewer, synthesizer),
        "ambient Synthesizer must NOT imply Viewer"
    );

    // 6. Stranger holds nothing on any object.
    for object in [tenant, domain, channel] {
        for wanted in [
            Relation::Owner,
            Relation::Admin,
            Relation::Editor,
            Relation::Member,
            Relation::Viewer,
            Relation::Synthesizer,
            Relation::Proposer,
        ] {
            assert!(
                !check_permission(&store, &ns, object, wanted, stranger),
                "stranger must hold nothing on {object:?} # {wanted:?}"
            );
        }
    }
}

#[test]
fn empty_namespace_registry_falls_back_to_self_relation_only() {
    // With no namespace inheritance registered, `Owner` does NOT
    // imply `Viewer`.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::new();
    let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let owner = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    store
        .insert(RelationTuple::new(tenant, Relation::Owner, owner))
        .expect("insert owner");

    assert!(check_permission(&store, &ns, tenant, Relation::Owner, owner));
    assert!(
        !check_permission(&store, &ns, tenant, Relation::Viewer, owner),
        "without an inheritance config, Owner must not imply Viewer"
    );
}
