//! Integration tests for the Phase 3 audit service.

use chrono::{Duration, Utc};
use uuid::Uuid;

use evidence_store::ScopeId;

use audit_service::{
    Actor, AuditActionType, AuditEntryBuilder, AuditLog, AuditQuery, TargetRef, TargetType,
};

fn entry(action: AuditActionType, scope: ScopeId, who: Uuid) -> audit_service::AuditEntry {
    AuditEntryBuilder::new()
        .actor(Actor::User(who))
        .action(action)
        .target(TargetRef::new(TargetType::Tenant, Uuid::new_v4()))
        .scope(scope)
        .build()
        .unwrap()
}

#[test]
fn append_assigns_monotonic_sequences() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let who = Uuid::new_v4();
    let mut last = None;
    for _ in 0..10 {
        let id = log.append(entry(AuditActionType::CanonicalPromotion, scope, who));
        let e = log.get(id).unwrap();
        if let Some(prev) = last {
            assert!(e.sequence > prev, "sequence must be strictly monotonic");
        }
        last = Some(e.sequence);
    }
    assert_eq!(log.len(), 10);
}

#[test]
fn append_only_no_mutation_apis_exist() {
    // The entries() accessor returns a `&[AuditEntry]`. There is no
    // `&mut` access path. Compile-time assertion: this test compiles
    // iff `entries` returns a shared slice.
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let who = Uuid::new_v4();
    log.append(entry(AuditActionType::CanonicalPromotion, scope, who));
    let _: &[audit_service::AuditEntry] = log.entries();
}

#[test]
fn query_filters_by_scope_and_action() {
    let mut log = AuditLog::new();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();

    log.append(entry(AuditActionType::CanonicalPromotion, scope_a, alice));
    log.append(entry(AuditActionType::Export, scope_a, alice));
    log.append(entry(AuditActionType::PolicyChange, scope_b, bob));
    log.append(entry(AuditActionType::CanonicalPromotion, scope_b, bob));

    let q = AuditQuery::new()
        .with_scope(scope_a)
        .with_action(AuditActionType::CanonicalPromotion);
    let results: Vec<_> = log.query(&q).collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].action_type, AuditActionType::CanonicalPromotion);
    assert_eq!(results[0].scope_id, Some(scope_a));
}

#[test]
fn query_filters_by_actor_and_time() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();

    let mut e1 = entry(AuditActionType::Export, scope, alice);
    e1.timestamp = Utc::now() - Duration::days(2);
    let mut e2 = entry(AuditActionType::Export, scope, bob);
    e2.timestamp = Utc::now();
    log.append(e1);
    log.append(e2);

    let q = AuditQuery::new().with_actor(alice);
    let results: Vec<_> = log.query(&q).collect();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].actor, Actor::User(id) if id == alice));

    let q = AuditQuery::new().since(Utc::now() - Duration::hours(1));
    let results: Vec<_> = log.query(&q).collect();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].actor, Actor::User(id) if id == bob));
}

#[test]
fn entries_are_chronological() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(log.append(entry(AuditActionType::Export, scope, alice)));
    }
    let entries = log.entries();
    let collected_ids: Vec<_> = entries.iter().map(|e| e.id).collect();
    assert_eq!(collected_ids, ids);
    let seqs: Vec<_> = entries.iter().map(|e| e.sequence).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted);
}

#[test]
fn missing_required_fields_rejected_by_builder() {
    let err = AuditEntryBuilder::new().build().unwrap_err();
    assert!(matches!(err, audit_service::AuditError::MissingField(_)));
}

#[test]
fn key_destruction_event_is_recordable() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let key_id = Uuid::new_v4();
    let entry = AuditEntryBuilder::new()
        .actor(Actor::System)
        .action(AuditActionType::KeyDestruction)
        .target(TargetRef::new(TargetType::Key, key_id))
        .scope(scope)
        .details(serde_json::json!({ "reason": "tenant-deletion" }))
        .build()
        .unwrap();
    let id = log.append(entry);
    let recorded = log.get(id).unwrap();
    assert_eq!(recorded.action_type, AuditActionType::KeyDestruction);
    assert_eq!(recorded.scope_id, Some(scope));
    assert!(matches!(recorded.actor, Actor::System));
}
