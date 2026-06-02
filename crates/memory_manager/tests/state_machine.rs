//! Comprehensive tests for the decay state machine.
//!
//! Covers every valid transition and a representative selection of
//! invalid transitions per `ARCHITECTURE.md` §7.

use evidence_store::ScopeId;
use memory_manager::{
    MemoryError, MemoryObject, MemoryState, MemoryStateMachine, SensitivityClass,
};
use uuid::Uuid;

fn fresh() -> MemoryObject {
    MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful)
}

#[test]
fn initial_state_is_candidate() {
    let obj = fresh();
    assert_eq!(obj.state, MemoryState::Candidate);
}

#[test]
fn candidate_to_reinforced_is_valid() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Reinforced);
}

#[test]
fn candidate_to_archived_is_valid() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.archive_candidate(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Archived);
}

#[test]
fn reinforced_to_consolidated_is_valid() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    sm.consolidate(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Consolidated);
}

#[test]
fn consolidated_to_canonical_is_valid() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    sm.consolidate(&mut obj).unwrap();
    sm.canonicalize(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Canonical);
}

#[test]
fn canonical_to_superseded_is_valid_and_records_supersedor() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    sm.consolidate(&mut obj).unwrap();
    sm.canonicalize(&mut obj).unwrap();
    let new_canonical = Uuid::new_v4();
    sm.supersede(&mut obj, new_canonical).unwrap();
    assert_eq!(obj.state, MemoryState::Superseded);
    assert_eq!(obj.superseded_by, Some(new_canonical));
}

#[test]
fn canonical_to_deleted_is_valid() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    sm.consolidate(&mut obj).unwrap();
    sm.canonicalize(&mut obj).unwrap();
    sm.delete_canonical(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Deleted);
}

#[test]
fn superseded_to_archived_is_valid() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    sm.consolidate(&mut obj).unwrap();
    sm.canonicalize(&mut obj).unwrap();
    sm.supersede(&mut obj, Uuid::new_v4()).unwrap();
    sm.archive_superseded(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Archived);
}

#[test]
fn archived_to_deleted_is_valid() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.archive_candidate(&mut obj).unwrap();
    sm.delete_archived(&mut obj).unwrap();
    assert_eq!(obj.state, MemoryState::Deleted);
}

#[test]
fn invalid_candidate_to_canonical_rejected() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    let err = sm.canonicalize(&mut obj).unwrap_err();
    assert_eq!(err,
        MemoryError::InvalidTransition {
            from: MemoryState::Candidate,
            to: MemoryState::Canonical
        }
    );
    // Object state must be unchanged on rejection.
    assert_eq!(obj.state, MemoryState::Candidate);
}

#[test]
fn invalid_consolidated_to_archived_rejected() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    sm.consolidate(&mut obj).unwrap();
    // Consolidated cannot archive directly; it must go through
    // Canonical -> Superseded -> Archived (or be deleted via the
    // canonical path).
    let err = sm.archive_superseded(&mut obj).unwrap_err();
    assert!(matches!(err, MemoryError::InvalidTransition { .. }));
    assert_eq!(obj.state, MemoryState::Consolidated);
}

#[test]
fn invalid_archived_to_canonical_rejected() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.archive_candidate(&mut obj).unwrap();
    let err = sm.canonicalize(&mut obj).unwrap_err();
    assert!(matches!(err, MemoryError::InvalidTransition { .. }));
}

#[test]
fn invalid_deleted_anywhere_rejected() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.archive_candidate(&mut obj).unwrap();
    sm.delete_archived(&mut obj).unwrap();
    // Once Deleted, every transition must be rejected.
    assert!(sm.reinforce(&mut obj).is_err());
    assert!(sm.consolidate(&mut obj).is_err());
    assert!(sm.canonicalize(&mut obj).is_err());
    assert!(sm.archive_candidate(&mut obj).is_err());
    assert!(sm.archive_superseded(&mut obj).is_err());
    assert!(sm.delete_archived(&mut obj).is_err());
    assert!(sm.delete_canonical(&mut obj).is_err());
}

#[test]
fn invalid_reinforced_to_canonical_rejected() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    let err = sm.canonicalize(&mut obj).unwrap_err();
    assert!(matches!(err, MemoryError::InvalidTransition { .. }));
}

#[test]
fn invalid_superseded_to_canonical_rejected() {
    let mut obj = fresh();
    let sm = MemoryStateMachine::new();
    sm.reinforce(&mut obj).unwrap();
    sm.consolidate(&mut obj).unwrap();
    sm.canonicalize(&mut obj).unwrap();
    sm.supersede(&mut obj, Uuid::new_v4()).unwrap();
    let err = sm.canonicalize(&mut obj).unwrap_err();
    assert!(matches!(err, MemoryError::InvalidTransition { .. }));
}
