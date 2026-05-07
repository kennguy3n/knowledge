//! Integration tests for the Phase 3 tenant service.

use uuid::Uuid;

use permission_service::Relation;
use tenant_service::{
    StorageConfig, SynthesisConfig, Tenant, TenantConfig, TenantError, TenantMemberStatus,
    TenantRegistry, TenantStatus,
};

#[test]
fn create_and_lookup_tenant() {
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();
    let t = reg.get(id).unwrap();
    assert_eq!(t.status, TenantStatus::Active);
    assert_eq!(t.name, "Acme Corp");
    assert!(!t.config.root_key.destroyed);
}

#[test]
fn suspend_activate_delete_lifecycle() {
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();

    reg.suspend(id).unwrap();
    assert_eq!(reg.get(id).unwrap().status, TenantStatus::Suspended);

    reg.activate(id).unwrap();
    assert_eq!(reg.get(id).unwrap().status, TenantStatus::Active);

    reg.delete(id).unwrap();
    assert_eq!(reg.get(id).unwrap().status, TenantStatus::Deleted);
    assert!(reg.get(id).unwrap().config.root_key.destroyed);

    // Cannot transition out of Deleted.
    let err = reg.activate(id).unwrap_err();
    assert!(matches!(
        err,
        TenantError::InvalidLifecycleTransition { .. }
    ));
}

#[test]
fn invalid_lifecycle_transitions_are_rejected() {
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();

    // Active -> Active is a no-op transition; rejected.
    let err = reg.activate(id).unwrap_err();
    assert!(matches!(
        err,
        TenantError::InvalidLifecycleTransition { .. }
    ));
}

#[test]
fn invalid_config_storage_caps_rejected() {
    let mut reg = TenantRegistry::new();
    let cfg = TenantConfig {
        storage: StorageConfig {
            soft_cap_bytes: Some(1_000),
            hard_cap_bytes: Some(100),
            server_cold_tier: false,
        },
        ..TenantConfig::new()
    };
    let err = reg.create("Acme Corp", cfg).unwrap_err();
    assert!(matches!(err, TenantError::InvalidConfig(_)));
}

#[test]
fn invalid_config_short_synthesis_window_rejected() {
    let mut reg = TenantRegistry::new();
    let cfg = TenantConfig {
        synthesis: SynthesisConfig {
            tenant_synthesis_enabled: true,
            tenant_window_secs: 5,
            domain_window_secs: 5,
            managed_endpoint: None,
        },
        ..TenantConfig::new()
    };
    let err = reg.create("Acme Corp", cfg).unwrap_err();
    assert!(matches!(err, TenantError::InvalidConfig(_)));
}

#[test]
fn member_provisioning_round_trip() {
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();
    let user = Uuid::new_v4();

    let m = reg.add_member(id, user, Relation::Admin).unwrap();
    assert_eq!(m.role, Relation::Admin);
    assert_eq!(m.status, TenantMemberStatus::Active);

    // Duplicate provisioning errors.
    let err = reg.add_member(id, user, Relation::Member).unwrap_err();
    assert!(matches!(err, TenantError::MemberAlreadyProvisioned(_)));

    reg.update_role(id, user, Relation::Member).unwrap();
    assert_eq!(reg.get_member(id, user).unwrap().role, Relation::Member);

    reg.remove_member(id, user).unwrap();
    assert_eq!(
        reg.get_member(id, user).unwrap().status,
        TenantMemberStatus::Removed
    );
}

#[test]
fn add_member_to_deleted_tenant_is_rejected() {
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();
    reg.delete(id).unwrap();
    let user = Uuid::new_v4();
    let err = reg.add_member(id, user, Relation::Admin).unwrap_err();
    assert!(matches!(
        err,
        TenantError::InvalidLifecycleTransition { .. }
    ));
}

#[test]
fn member_mutations_rejected_on_suspended_tenant() {
    // Per `TenantStatus::Suspended` docs ("no synthesis, no member
    // changes, no connector traffic"), every membership mutator must
    // refuse while the tenant is suspended.
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();
    let user = Uuid::new_v4();
    reg.add_member(id, user, Relation::Admin).unwrap();
    reg.suspend(id).unwrap();

    let err = reg
        .add_member(id, Uuid::new_v4(), Relation::Member)
        .unwrap_err();
    assert!(matches!(
        err,
        TenantError::InvalidLifecycleTransition { .. }
    ));

    let err = reg.update_role(id, user, Relation::Member).unwrap_err();
    assert!(matches!(
        err,
        TenantError::InvalidLifecycleTransition { .. }
    ));

    let err = reg.remove_member(id, user).unwrap_err();
    assert!(matches!(
        err,
        TenantError::InvalidLifecycleTransition { .. }
    ));

    // Reactivation re-opens the membership surface.
    reg.activate(id).unwrap();
    reg.update_role(id, user, Relation::Member).unwrap();
    reg.remove_member(id, user).unwrap();
}

#[test]
fn removed_members_are_immutable_audit_artefacts() {
    // Per the docs on `remove_member`, a removed membership row is
    // kept around as an audit artefact. It must not be re-removed
    // (so the removal timestamp stays single-valued) and its role
    // must not be mutable (so the role history is monotonic). Both
    // operations should error with `MemberAlreadyRemoved`.
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();
    let user = Uuid::new_v4();
    reg.add_member(id, user, Relation::Admin).unwrap();
    reg.remove_member(id, user).unwrap();
    assert_eq!(
        reg.get_member(id, user).unwrap().status,
        TenantMemberStatus::Removed
    );

    let err = reg.update_role(id, user, Relation::Member).unwrap_err();
    assert_eq!(err, TenantError::MemberAlreadyRemoved(user));

    let err = reg.remove_member(id, user).unwrap_err();
    assert_eq!(err, TenantError::MemberAlreadyRemoved(user));

    // The role on the audit row is whatever it was at removal time,
    // not the role we just tried to set.
    assert_eq!(reg.get_member(id, user).unwrap().role, Relation::Admin);
}

#[test]
fn removed_member_can_be_reprovisioned() {
    // Per the docs on `add_member`, an employee who left (status =
    // Removed) and returned should be re-provisionable via
    // `add_member`. The audit log keeps the original removal entry,
    // so this does not erase history — it just reopens the surface.
    let mut reg = TenantRegistry::new();
    let id = reg.create("Acme Corp", TenantConfig::new()).unwrap();
    let user = Uuid::new_v4();

    let first = reg.add_member(id, user, Relation::Admin).unwrap();
    reg.remove_member(id, user).unwrap();
    assert_eq!(
        reg.get_member(id, user).unwrap().status,
        TenantMemberStatus::Removed
    );

    let second = reg.add_member(id, user, Relation::Member).unwrap();
    assert_eq!(second.role, Relation::Member);
    assert_eq!(second.status, TenantMemberStatus::Active);
    // The fresh row replaces the audit artefact in the registry —
    // status / role / provisioned_at all reflect the re-provisioning.
    let row = reg.get_member(id, user).unwrap();
    assert_eq!(row.status, TenantMemberStatus::Active);
    assert_eq!(row.role, Relation::Member);
    // ...and a still-active member cannot be re-provisioned.
    let err = reg.add_member(id, user, Relation::Editor).unwrap_err();
    assert_eq!(err, TenantError::MemberAlreadyProvisioned(user));
    // Sanity: the original `add_member` did successfully run before
    // the removal cycle.
    assert_eq!(first.role, Relation::Admin);
}

#[test]
fn list_members_filters_by_tenant() {
    let mut reg = TenantRegistry::new();
    let a = reg.create("A", TenantConfig::new()).unwrap();
    let b = reg.create("B", TenantConfig::new()).unwrap();
    reg.add_member(a, Uuid::new_v4(), Relation::Admin).unwrap();
    reg.add_member(a, Uuid::new_v4(), Relation::Member).unwrap();
    reg.add_member(b, Uuid::new_v4(), Relation::Member).unwrap();
    assert_eq!(reg.list_members(a).len(), 2);
    assert_eq!(reg.list_members(b).len(), 1);
}

#[test]
fn unknown_tenant_yields_not_found() {
    let reg = TenantRegistry::new();
    let bogus = tenant_service::TenantId::new_v4();
    let err = reg.get(bogus).unwrap_err();
    assert!(matches!(err, TenantError::NotFound(_)));
}

#[test]
fn tenant_struct_default_config_is_valid() {
    let t = Tenant::new("Example", TenantConfig::default());
    t.config.validate().unwrap();
}
