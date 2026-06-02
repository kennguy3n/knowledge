//! Integration test: user-scoped connector attach → sync → observation
//! inherits user scope.
//!
//! Exercises the full path for Pattern B (user-scoped connectors):
//!
//! 1. Grant a user `editor` on a `User`-typed scope.
//! 2. Attach a connector to that scope with `ObjectType::User`.
//! 3. Verify `scope_for()` resolves the user scope.
//! 4. Verify that a channel-scoped grant does NOT satisfy a
//!    user-scoped attach (isolation).
//! 5. Exercise the domain-scoped path symmetrically.

use uuid::Uuid;

use connector_framework::error::ConnectorError;
use connector_framework::token_vault::ConnectorInstanceId;
use connector_framework::{AttachmentRegistry, ConnectorKind};
use evidence_store::ScopeId;
use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

fn setup() -> (TupleStore, NamespaceRegistry) {
    (TupleStore::new(), NamespaceRegistry::with_defaults())
}

fn grant(
    store: &mut TupleStore,
    object_type: ObjectType,
    scope: ScopeId,
    relation: Relation,
    user_id: Uuid,
) {
    store
        .insert(RelationTuple::new(
            ObjectRef::new(object_type, scope.as_uuid()),
            relation,
            SubjectRef::direct(SubjectType::User, user_id),
        ))
        .expect("insert tuple");
}

#[test]
fn user_scoped_connector_attach_sync_scope_inherit() {
    let (mut store, ns) = setup();

    let user_id = Uuid::new_v4();
    let user_scope = ScopeId::new_v4();
    let subject = SubjectRef::direct(SubjectType::User, user_id);

    // 1. Grant editor on a User-typed scope.
    grant(
        &mut store,
        ObjectType::User,
        user_scope,
        Relation::Editor,
        user_id,
    );

    // Sanity: the permission check resolves via the User namespace.
    assert!(check_permission(
        &store,
        &ns,
        ObjectRef::new(ObjectType::User, user_scope.as_uuid()),
        Relation::Editor,
        subject,
    ));

    // 2. Attach a connector to the user scope.
    let mut reg = AttachmentRegistry::new();
    let connector = ConnectorInstanceId::new_v4();
    let attachment = reg
        .attach(
            connector,
            ConnectorKind::GoogleDrive,
            user_scope,
            ObjectType::User,
            &store,
            &ns,
            subject,
        )
        .unwrap();
    assert_eq!(attachment.scope_id, user_scope);

    // 3. Observations from this connector inherit the user scope.
    assert_eq!(reg.scope_for(connector).unwrap(), user_scope);
}

#[test]
fn channel_grant_does_not_satisfy_user_scoped_attach() {
    let (mut store, ns) = setup();

    let user_id = Uuid::new_v4();
    let scope = ScopeId::new_v4();
    let subject = SubjectRef::direct(SubjectType::User, user_id);

    // Grant admin on Channel, but attempt User-scoped attach.
    grant(
        &mut store,
        ObjectType::Channel,
        scope,
        Relation::Admin,
        user_id,
    );

    let mut reg = AttachmentRegistry::new();
    let err = reg
        .attach(
            ConnectorInstanceId::new_v4(),
            ConnectorKind::Notion,
            scope,
            ObjectType::User,
            &store,
            &ns,
            subject,
        )
        .unwrap_err();
    assert!(matches!(err, ConnectorError::PermissionDenied));
}

#[test]
fn domain_scoped_connector_full_path() {
    let (mut store, ns) = setup();

    let user_id = Uuid::new_v4();
    let domain_scope = ScopeId::new_v4();
    let subject = SubjectRef::direct(SubjectType::User, user_id);

    // Grant editor on Domain scope.
    grant(
        &mut store,
        ObjectType::Domain,
        domain_scope,
        Relation::Editor,
        user_id,
    );

    // Attach.
    let mut reg = AttachmentRegistry::new();
    let connector = ConnectorInstanceId::new_v4();
    reg.attach(
        connector,
        ConnectorKind::Jira,
        domain_scope,
        ObjectType::Domain,
        &store,
        &ns,
        subject,
    )
    .unwrap();

    assert_eq!(reg.scope_for(connector).unwrap(), domain_scope);

    // Detach.
    let removed = reg
        .detach(connector, ObjectType::Domain, &store, &ns, subject)
        .unwrap();
    assert_eq!(removed.scope_id, domain_scope);
    assert!(reg.is_empty());
}

#[test]
fn user_namespace_inheritance_chain_works() {
    let ns = NamespaceRegistry::with_defaults();

    // User type should have the full inheritance chain.
    assert!(ns.implies(ObjectType::User, Relation::Owner, Relation::Viewer));
    assert!(ns.implies(ObjectType::User, Relation::Admin, Relation::Editor));
    assert!(ns.implies(ObjectType::User, Relation::Editor, Relation::Member));
    assert!(!ns.implies(ObjectType::User, Relation::Viewer, Relation::Admin));
}
