//! Integration test: evidence ingest → retention score → decay state
//! transitions → cryptographic forgetting (DEK destroy) removes access.

use chrono::{Duration, Utc};
use uuid::Uuid;

use integration_tests::test_helpers::{open_store, padded_body, ImportanceClass, ScopeId};
use memory_manager::{
    compute_retention_score, decay_sweep, MemoryObject, MemoryState, MemoryStateMachine,
    SensitivityClass,
};

#[test]
fn ingest_verify_retention_and_decay_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let scope = ScopeId::new_v4();
    let mut store = open_store(&db_path);
    let body = padded_body("knowledge substrate architecture meeting notes");
    let result = store
        .ingest(scope, &body, None, ImportanceClass::Useful)
        .unwrap();
    assert!(store.get(result.evidence_id).unwrap().is_some());

    // 1. Fresh candidate object.
    let now = Utc::now();
    let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
    assert_eq!(obj.state, MemoryState::Candidate);
    assert!(obj.retention_score.abs() < f64::EPSILON);

    let score = compute_retention_score(&obj, now);
    // Brand-new object has some positive retention from recency.
    assert!(score.total >= 0.0);

    // 2. Walk the state machine: Candidate → Reinforced → Consolidated
    //    → Canonical → Superseded → Archived → Deleted.
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Reinforced);

    sm.consolidate(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Consolidated);

    sm.canonicalize(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Canonical);

    sm.supersede(&mut obj, Uuid::new_v4()).unwrap();
    assert_eq!(obj.state, MemoryState::Superseded);
    assert!(obj.superseded_by.is_some());

    sm.archive_superseded(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Archived);

    sm.delete_archived(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Deleted);

    // 3. Cryptographic forgetting: body becomes unreadable.
    store
        .purge_body_key_wraps_for_scope(scope)
        .expect("purge wraps");
    store.purge_fts_for_scope(scope).expect("purge fts");
    store
        .record_forgotten_scope(scope)
        .expect("record forgotten");
    store.delete_scope_dek(scope).expect("delete dek");

    let read_err = store.read_body(result.evidence_id);
    assert!(read_err.is_err(), "body inaccessible after DEK destroy");
}

#[test]
fn decay_sweep_archives_stale_candidates() {
    let scope = ScopeId::new_v4();
    let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
    // Simulate age: push created_at far back.
    obj.created_at = Utc::now() - Duration::days(365);
    obj.last_accessed_at = obj.created_at;

    let now = Utc::now();
    let mut objects = vec![obj];
    let report = decay_sweep(&mut objects, now);
    assert_eq!(report.scored, 1);
    assert_eq!(
        objects[0].state,
        MemoryState::Archived,
        "stale candidate should be archived"
    );
}

#[test]
fn pinning_boosts_retention_and_survives_sweep() {
    let scope = ScopeId::new_v4();
    let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
    obj.created_at = Utc::now() - Duration::days(365);
    obj.last_accessed_at = obj.created_at;
    // Pin the object.
    obj.pin_count = 1;
    obj.last_accessed_at = Utc::now();

    let now = Utc::now();
    let score = compute_retention_score(&obj, now);
    assert!(
        score.total > 0.5,
        "pinned object should have high retention"
    );

    // Sweep-level: pinned candidate survives despite old age.
    let mut objects = vec![obj];
    let _report = decay_sweep(&mut objects, now);
    assert_eq!(
        objects[0].state,
        MemoryState::Candidate,
        "pinned candidate must not be archived by sweep"
    );
}

#[test]
fn retrieval_and_corroboration_boost_retention() {
    let scope = ScopeId::new_v4();
    let mut base = MemoryObject::new_candidate(scope, SensitivityClass::Useful);

    let now = Utc::now();
    let base_score = compute_retention_score(&base, now).total;

    // Boost with retrievals + corroborations.
    base.record_retrieval(now);
    base.record_retrieval(now);
    base.record_corroboration(now);

    let boosted_score = compute_retention_score(&base, now).total;
    assert!(
        boosted_score > base_score,
        "retrieval/corroboration should increase retention score"
    );
}

#[test]
fn critical_class_immune_to_passive_decay() {
    let scope = ScopeId::new_v4();
    let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Critical);
    obj.created_at = Utc::now() - Duration::days(365);
    obj.last_accessed_at = obj.created_at;

    let now = Utc::now();
    let mut objects = vec![obj];
    let _report = decay_sweep(&mut objects, now);
    assert_eq!(
        objects[0].state,
        MemoryState::Candidate,
        "Critical-class candidates should not be archived by passive decay"
    );
}

#[test]
fn invalid_state_transitions_rejected() {
    let sm = MemoryStateMachine::new();
    let scope = ScopeId::new_v4();

    // Candidate -> Consolidated (skipping Reinforced) is invalid.
    let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
    assert!(sm.consolidate(&mut obj).is_err());

    // Candidate -> Canonical (skipping Reinforced + Consolidated) is invalid.
    let mut obj2 = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
    assert!(sm.canonicalize(&mut obj2).is_err());
}
