//! Tests for [`UserMemoryObject`] CRUD + decay sweep.

use chrono::{Duration, Utc};
use evidence_store::ScopeId;
use memory_manager::{
    MemoryFilter, MemoryState, MemoryStateMachine, SensitivityClass, UserMemoryObject,
};
use uuid::Uuid;

fn fresh_umo() -> UserMemoryObject {
    UserMemoryObject::new(Uuid::new_v4(), ScopeId::new_v4())
}

#[test]
fn add_observation_creates_candidate() {
    let mut u = fresh_umo();
    let id = u.add_observation("task", "ship the launch", SensitivityClass::Important);
    let obj = u.read(&id).unwrap();
    assert_eq!(obj.state, MemoryState::Candidate);
    assert_eq!(obj.sensitivity_class, SensitivityClass::Important);
}

#[test]
fn pin_promotes_candidate_to_reinforced() {
    let mut u = fresh_umo();
    let id = u.add_observation("fact", "x", SensitivityClass::Useful);
    u.pin(&id).unwrap();
    let obj = u.read(&id).unwrap();
    assert_eq!(obj.state, MemoryState::Reinforced);
    assert_eq!(obj.pin_count, 1);
    assert!(obj.retention_score >= 0.9);
}

#[test]
fn unpin_does_not_demote_state_only_decrements_counter() {
    let mut u = fresh_umo();
    let id = u.add_observation("fact", "x", SensitivityClass::Useful);
    u.pin(&id).unwrap();
    u.unpin(&id).unwrap();
    let obj = u.read(&id).unwrap();
    assert_eq!(obj.state, MemoryState::Reinforced);
    assert_eq!(obj.pin_count, 0);
}

#[test]
fn forget_removes_non_canonical_objects() {
    let mut u = fresh_umo();
    let id = u.add_observation("fact", "x", SensitivityClass::Useful);
    u.forget(&id).unwrap();
    assert!(u.read(&id).is_none());
}

#[test]
fn forget_canonical_marks_deleted() {
    let mut u = fresh_umo();
    let id = u.add_observation("fact", "x", SensitivityClass::Critical);
    let sm = MemoryStateMachine::new();
    let obj = u.objects.iter_mut().find(|o| o.id == id).expect("present");
    sm.reinforce(obj).unwrap();
    sm.consolidate(obj, Utc::now()).unwrap();
    sm.canonicalize(obj).unwrap();
    u.forget(&id).unwrap();
    assert_eq!(u.read(&id).unwrap().state, MemoryState::Deleted);
}

#[test]
fn list_filters_by_state_and_observation_type() {
    let mut u = fresh_umo();
    let _ = u.add_observation("task", "a", SensitivityClass::Useful);
    let id_b = u.add_observation("fact", "b", SensitivityClass::Useful);
    let _ = u.add_observation("task", "c", SensitivityClass::Useful);
    u.pin(&id_b).unwrap();

    let tasks = u.list(&MemoryFilter::any().with_observation_type("task"));
    assert_eq!(tasks.len(), 2);

    let reinforced = u.list(&MemoryFilter::any().with_state(MemoryState::Reinforced));
    assert_eq!(reinforced.len(), 1);
    assert_eq!(reinforced[0].id, id_b);
}

#[test]
fn decay_sweep_archives_old_candidates_and_reports_counters() {
    let mut u = fresh_umo();
    let _ = u.add_observation("fact", "fresh", SensitivityClass::Useful);
    let stale_id = u.add_observation("fact", "stale", SensitivityClass::Useful);

    // Backdate the stale candidate so the sweep archives it.
    {
        let stale = u
            .objects
            .iter_mut()
            .find(|o| o.id == stale_id)
            .expect("present");
        stale.created_at = Utc::now() - Duration::days(365 * 2);
        stale.last_accessed_at = stale.created_at;
    }

    let report = u.decay_sweep(Utc::now());
    assert_eq!(report.scored, 2);
    assert_eq!(report.candidates_archived, 1);
    assert_eq!(u.read(&stale_id).unwrap().state, MemoryState::Archived);
}

#[test]
fn decay_sweep_respects_pinned_candidates() {
    let mut u = fresh_umo();
    let id = u.add_observation("fact", "important", SensitivityClass::Useful);
    u.pin(&id).unwrap();
    {
        let obj = u.objects.iter_mut().find(|o| o.id == id).expect("present");
        obj.created_at = Utc::now() - Duration::days(365 * 5);
        obj.last_accessed_at = obj.created_at;
    }
    let report = u.decay_sweep(Utc::now());
    assert_eq!(report.candidates_archived, 0);
    let obj = u.read(&id).unwrap();
    // Pinning auto-reinforces the candidate, so the sweep doesn't see
    // a candidate at all — and even if it did, the >=0.9 floor would
    // protect it.
    assert_eq!(obj.state, MemoryState::Reinforced);
    assert!(obj.retention_score >= 0.9);
}
