//! Integration tests for the Phase 3 [`DomainMemoryObject`].
//!
//! Per `PHASES.md` Phase 3 acceptance criteria, the domain memory
//! object covers:
//!
//! * CRUD over workstreams / dependencies / risks / procedures.
//! * The lifecycle marks (complete, resolve, deprecate).
//! * The decay sweep over completed / resolved items.
//! * Hierarchy validation: domain memory tracks the channel scopes it
//!   consumes from (the input contract for domain synthesis).

use chrono::{Duration, Utc};
use evidence_store::ScopeId;

use memory_manager::domain_memory::{DomainDecayReport, DomainMemoryObject, Procedure, Workstream};
use memory_manager::{
    Dependency, MemoryError, MemoryState, Risk, SensitivityClass,
    DEFAULT_COMPLETED_WORKSTREAM_TTL_DAYS, DEFAULT_RESOLVED_RISK_TTL_DAYS,
};

#[test]
fn fresh_domain_memory_is_empty() {
    let scope = ScopeId::new_v4();
    let dom = DomainMemoryObject::new(scope);
    assert_eq!(dom.scope_id, scope);
    assert!(dom.workstreams.is_empty());
    assert!(dom.dependencies.is_empty());
    assert!(dom.risks.is_empty());
    assert!(dom.procedures.is_empty());
    assert!(dom.channel_scopes.is_empty());
    assert!(dom.last_synthesis_window.is_none());
}

#[test]
fn workstream_crud_round_trip() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);

    let id = dom.add_workstream(Workstream::new(scope, "Q3 launch readiness").with_owner("@sara"));
    assert_eq!(dom.workstreams.len(), 1);
    assert_eq!(dom.list_active_workstreams().len(), 1);

    dom.complete_workstream(id).unwrap();
    assert!(dom.workstreams[0].is_complete());
    assert!(dom.list_active_workstreams().is_empty());
}

#[test]
fn dependency_crud_round_trip() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);

    let id = dom.add_dependency(Dependency::new(scope, "API contract -> SDK release"));
    assert_eq!(dom.list_open_dependencies().len(), 1);

    dom.resolve_dependency(id).unwrap();
    assert!(dom.dependencies[0].is_resolved());
    assert!(dom.list_open_dependencies().is_empty());
}

#[test]
fn risk_crud_round_trip() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);

    let id = dom.add_risk(Risk::new(scope, "vendor outage on Region A"));
    assert_eq!(dom.list_open_risks().len(), 1);

    dom.resolve_risk(id).unwrap();
    assert!(dom.risks[0].is_resolved());
    assert!(dom.list_open_risks().is_empty());
}

#[test]
fn procedure_crud_round_trip() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);

    let id = dom.add_procedure(Procedure::new(scope, "deploy on green CI"));
    assert_eq!(dom.list_active_procedures().len(), 1);
    assert_eq!(
        dom.procedures[0].memory.sensitivity_class,
        SensitivityClass::Critical
    );

    dom.deprecate_procedure(id).unwrap();
    assert!(dom.procedures[0].is_deprecated());
    assert!(dom.list_active_procedures().is_empty());
}

#[test]
fn unknown_ids_yield_not_found() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);
    let bogus = uuid::Uuid::new_v4();
    assert!(matches!(
        dom.complete_workstream(bogus).unwrap_err(),
        MemoryError::NotFound(_)
    ));
    assert!(matches!(
        dom.resolve_dependency(bogus).unwrap_err(),
        MemoryError::NotFound(_)
    ));
    assert!(matches!(
        dom.resolve_risk(bogus).unwrap_err(),
        MemoryError::NotFound(_)
    ));
    assert!(matches!(
        dom.deprecate_procedure(bogus).unwrap_err(),
        MemoryError::NotFound(_)
    ));
}

#[test]
fn decay_sweep_archives_old_completed_workstreams() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);
    let id = dom.add_workstream(Workstream::new(scope, "ancient launch"));
    dom.complete_workstream(id).unwrap();
    // Backdate the completion past the per-class TTL.
    dom.workstreams[0].completed_at =
        Some(Utc::now() - Duration::days(DEFAULT_COMPLETED_WORKSTREAM_TTL_DAYS + 1));

    let report = dom.decay_sweep(Utc::now());
    assert_eq!(
        report,
        DomainDecayReport {
            workstreams_archived: 1,
            risks_archived: 0,
            dependencies_archived: 0,
        }
    );
    assert!(dom.workstreams.is_empty());
}

#[test]
fn decay_sweep_archives_old_resolved_risks_and_dependencies() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);
    let r = dom.add_risk(Risk::new(scope, "stale risk"));
    let d = dom.add_dependency(Dependency::new(scope, "stale dep"));
    dom.resolve_risk(r).unwrap();
    dom.resolve_dependency(d).unwrap();

    dom.risks[0].resolved_at =
        Some(Utc::now() - Duration::days(DEFAULT_RESOLVED_RISK_TTL_DAYS + 1));
    dom.dependencies[0].resolved_at =
        Some(Utc::now() - Duration::days(DEFAULT_RESOLVED_RISK_TTL_DAYS + 1));

    let report = dom.decay_sweep(Utc::now());
    assert_eq!(report.risks_archived, 1);
    assert_eq!(report.dependencies_archived, 1);
    assert!(dom.risks.is_empty());
    assert!(dom.dependencies.is_empty());
}

#[test]
fn decay_sweep_never_archives_procedures() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);
    let id = dom.add_procedure(Procedure::new(scope, "deploy on green CI"));
    dom.deprecate_procedure(id).unwrap();
    // Even if a procedure has been "deprecated" forever, the decay
    // sweep is not the mechanism that removes it. `Critical`-class
    // items only leave the list via explicit deprecation, but they
    // remain in the procedures vector for audit.
    dom.procedures[0].deprecated_at = Some(Utc::now() - Duration::days(365 * 5));

    let report = dom.decay_sweep(Utc::now());
    assert_eq!(report.workstreams_archived, 0);
    assert_eq!(report.risks_archived, 0);
    assert_eq!(report.dependencies_archived, 0);
    assert_eq!(dom.procedures.len(), 1, "procedures are never decayed");
    // Underlying memory state stays Candidate; passive decay is a
    // no-op for `Critical` items.
    assert_eq!(dom.procedures[0].memory.state, MemoryState::Candidate);
}

#[test]
fn attach_channel_scope_records_input_contract() {
    let domain_scope = ScopeId::new_v4();
    let channel_a = ScopeId::new_v4();
    let channel_b = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(domain_scope);

    dom.attach_channel_scope(channel_a);
    dom.attach_channel_scope(channel_b);
    // Idempotent: attaching the same scope twice does not duplicate.
    dom.attach_channel_scope(channel_a);

    assert_eq!(dom.channel_scopes, vec![channel_a, channel_b]);
}

#[test]
fn update_recap_records_synthesis_window() {
    let scope = ScopeId::new_v4();
    let mut dom = DomainMemoryObject::new(scope);
    let window = uuid::Uuid::new_v4();
    dom.update_recap("week-of-may-7 cross-channel summary", Some(window));
    assert_eq!(dom.recap, "week-of-may-7 cross-channel summary");
    assert_eq!(dom.last_synthesis_window, Some(window));
}
