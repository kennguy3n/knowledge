//! Integration tests for the Phase 3 [`TenantMemoryObject`].
//!
//! Per `PROPOSAL.md` §4.3, tenant / institutional memory has "no
//! ordinary decay — only explicit deprecation". The tests below
//! exercise that invariant: items default to `Critical`, the global
//! decay sweep is a no-op for `Critical` rows, and the only path off
//! the active list is via `deprecate_*` methods.

use chrono::Duration;
use evidence_store::ScopeId;
use uuid::Uuid;

use memory_manager::tenant_memory::{
    ApprovedDocumentRef, CanonicalPolicy, ProductTaxonomyEntry, StableOrgKnowledge,
    TenantMemoryError, TenantMemoryObject,
};
use memory_manager::{decay_sweep, MemoryState, SensitivityClass};

#[test]
fn fresh_tenant_memory_is_empty() {
    let scope = ScopeId::new_v4();
    let t = TenantMemoryObject::new(scope);
    assert_eq!(t.scope_id, scope);
    assert!(t.policies.is_empty());
    assert!(t.taxonomy.is_empty());
    assert!(t.stable_facts.is_empty());
    assert!(t.domain_scopes.is_empty());
    assert!(t.approved_documents.is_empty());
}

#[test]
fn policy_defaults_to_critical_sensitivity() {
    let scope = ScopeId::new_v4();
    let policy = CanonicalPolicy::new(scope, "PII never leaves jurisdiction");
    assert_eq!(policy.memory.sensitivity_class, SensitivityClass::Critical);
}

#[test]
fn taxonomy_and_stable_facts_default_to_critical() {
    let scope = ScopeId::new_v4();
    let entry = ProductTaxonomyEntry::new(scope, "Identity / IAM / OIDC");
    let fact = StableOrgKnowledge::new(scope, "on-call rotation handoff: Tuesdays 09:00 UTC");
    assert_eq!(entry.memory.sensitivity_class, SensitivityClass::Critical);
    assert_eq!(fact.memory.sensitivity_class, SensitivityClass::Critical);
}

#[test]
fn add_and_deprecate_policy() {
    let scope = ScopeId::new_v4();
    let mut t = TenantMemoryObject::new(scope);
    let id = t
        .add_policy(CanonicalPolicy::new(scope, "MFA required for admins"))
        .unwrap();
    assert_eq!(t.list_active_policies().len(), 1);

    let new_id = Uuid::new_v4();
    t.deprecate_policy(id, Some(new_id)).unwrap();
    assert!(t.list_active_policies().is_empty());
    assert_eq!(t.policies[0].superseded_by, Some(new_id));
    assert_eq!(t.policies[0].memory.superseded_by, Some(new_id));
}

#[test]
fn deprecate_unknown_returns_not_found() {
    let scope = ScopeId::new_v4();
    let mut t = TenantMemoryObject::new(scope);
    let bogus = Uuid::new_v4();
    assert!(matches!(
        t.deprecate_policy(bogus, None).unwrap_err(),
        TenantMemoryError::NotFound(_)
    ));
    assert!(matches!(
        t.deprecate_taxonomy_entry(bogus).unwrap_err(),
        TenantMemoryError::NotFound(_)
    ));
    assert!(matches!(
        t.deprecate_stable_fact(bogus, None).unwrap_err(),
        TenantMemoryError::NotFound(_)
    ));
}

#[test]
fn add_policy_rejects_non_critical_memory() {
    let scope = ScopeId::new_v4();
    let mut t = TenantMemoryObject::new(scope);
    let mut policy = CanonicalPolicy::new(scope, "MFA required");
    policy.memory.sensitivity_class = SensitivityClass::Important;
    let err = t.add_policy(policy).unwrap_err();
    assert_eq!(
        err,
        TenantMemoryError::NotCritical(SensitivityClass::Important)
    );
}

#[test]
fn try_passive_decay_is_forbidden() {
    let t = TenantMemoryObject::new(ScopeId::new_v4());
    assert_eq!(
        t.try_passive_decay().unwrap_err(),
        TenantMemoryError::PassiveDecayForbidden
    );
}

#[test]
fn global_decay_sweep_is_a_noop_on_critical_items() {
    // The substrate's existing decay sweep only acts on Candidate
    // (low score) and Superseded (TTL elapsed) rows. Critical items
    // are typically pinned and reinforced in production; even when
    // they sit in the Candidate state, retention scoring keeps the
    // sweep from archiving them. The point of this test is the
    // *contract*: the global sweep does not surface a path that
    // archives a Critical item via passive decay alone.
    let scope = ScopeId::new_v4();
    let mut t = TenantMemoryObject::new(scope);
    let policy_id = t
        .add_policy(CanonicalPolicy::new(scope, "MFA required"))
        .unwrap();
    let entry_id = t
        .add_taxonomy_entry(ProductTaxonomyEntry::new(scope, "Identity / IAM"))
        .unwrap();
    let fact_id = t
        .add_stable_fact(StableOrgKnowledge::new(scope, "on-call: Tuesdays 09 UTC"))
        .unwrap();

    // Hand the underlying memory objects to the global sweep.
    let mut objs = vec![
        t.policies
            .iter()
            .find(|p| p.memory.id == policy_id)
            .unwrap()
            .memory
            .clone(),
        t.taxonomy
            .iter()
            .find(|e| e.memory.id == entry_id)
            .unwrap()
            .memory
            .clone(),
        t.stable_facts
            .iter()
            .find(|f| f.memory.id == fact_id)
            .unwrap()
            .memory
            .clone(),
    ];
    // Backdate as if the rows had been sitting around forever.
    for o in &mut objs {
        o.created_at = chrono::Utc::now() - Duration::days(365 * 5);
        o.last_accessed_at = o.created_at;
    }

    let report = decay_sweep(&mut objs, chrono::Utc::now());
    assert_eq!(
        report.candidates_archived, 0,
        "tenant-memory items must never be archived by passive decay; \
         only explicit deprecation is allowed (PROPOSAL.md §4.3)"
    );
    assert_eq!(report.superseded_archived, 0);
    for o in objs {
        assert_eq!(o.state, MemoryState::Candidate);
    }
}

#[test]
fn approved_documents_admit_revoke_round_trip() {
    let scope = ScopeId::new_v4();
    let mut t = TenantMemoryObject::new(scope);
    let doc = ApprovedDocumentRef::new("Tenant Policy v3.2", "compliance-officer");
    let id = doc.id;
    t.admit_approved_document(doc.clone());
    // Idempotent on id.
    t.admit_approved_document(doc);
    assert_eq!(t.approved_documents.len(), 1);

    t.revoke_approved_document(id).unwrap();
    assert!(t.approved_documents.is_empty());
    assert!(matches!(
        t.revoke_approved_document(id).unwrap_err(),
        TenantMemoryError::NotFound(_)
    ));
}

#[test]
fn attach_domain_scope_records_input_contract() {
    let tenant_scope = ScopeId::new_v4();
    let domain_a = ScopeId::new_v4();
    let domain_b = ScopeId::new_v4();
    let mut t = TenantMemoryObject::new(tenant_scope);
    t.attach_domain_scope(domain_a);
    t.attach_domain_scope(domain_b);
    t.attach_domain_scope(domain_a); // idempotent
    assert_eq!(t.domain_scopes, vec![domain_a, domain_b]);
}

#[test]
fn update_summary_records_synthesis_window() {
    let scope = ScopeId::new_v4();
    let mut t = TenantMemoryObject::new(scope);
    let window = Uuid::new_v4();
    t.update_summary("tenant-level summary", Some(window));
    assert_eq!(t.summary, "tenant-level summary");
    assert_eq!(t.last_synthesis_window, Some(window));
}
