//! End-to-end integration test for the memory manager.
//!
//! Walks a [`UserMemoryObject`] through the full lifecycle:
//! ingestion -> pin/unpin -> decay sweep -> retention scoring ->
//! supersession -> forget.

use chrono::{Duration, Utc};
use evidence_store::ScopeId;
use memory_manager::{
    compute_retention_score, MemoryFilter, MemoryState, MemoryStateMachine, SensitivityClass,
    UserMemoryObject,
};
use uuid::Uuid;

#[test]
fn full_lifecycle_walks_state_machine_and_scoring() {
    let mut u = UserMemoryObject::new(Uuid::new_v4(), ScopeId::new_v4());

    // Ingestion — three candidate observations.
    let pin_id = u.add_observation("task", "draft the RFC", SensitivityClass::Important);
    let stale_id = u.add_observation("fact", "ancient note", SensitivityClass::Useful);
    let canon_id = u.add_observation("decision", "approved policy", SensitivityClass::Critical);

    // Pin one — promotes to Reinforced and floors retention >= 0.9.
    u.pin(&pin_id).unwrap();
    assert_eq!(u.read(&pin_id).unwrap().state, MemoryState::Reinforced);
    assert!(u.read(&pin_id).unwrap().retention_score >= 0.9);

    // Walk the canonical path on the second one.
    let sm = MemoryStateMachine::new();
    {
        let obj = u
            .objects
            .iter_mut()
            .find(|o| o.id == canon_id)
            .expect("present");
        sm.reinforce(obj).unwrap();
        sm.consolidate(obj, Utc::now()).unwrap();
        sm.canonicalize(obj).unwrap();
    }

    // Backdate the stale candidate so the next decay sweep archives
    // it.
    {
        let stale = u
            .objects
            .iter_mut()
            .find(|o| o.id == stale_id)
            .expect("present");
        stale.created_at = Utc::now() - Duration::days(365 * 5);
        stale.last_accessed_at = stale.created_at;
    }

    // Decay sweep runs over all three.
    let report = u.decay_sweep(Utc::now());
    assert_eq!(report.scored, 3);
    assert_eq!(report.candidates_archived, 1);
    assert_eq!(u.read(&stale_id).unwrap().state, MemoryState::Archived);
    // Pinned (now Reinforced) and Canonical objects are untouched.
    assert_eq!(u.read(&pin_id).unwrap().state, MemoryState::Reinforced);
    assert_eq!(u.read(&canon_id).unwrap().state, MemoryState::Canonical);

    // Supersede the canonical row by a fresh canonical claim.
    let new_canon = uuid::Uuid::new_v4();
    {
        let canon = u
            .objects
            .iter_mut()
            .find(|o| o.id == canon_id)
            .expect("present");
        sm.supersede(canon, new_canon, Utc::now()).unwrap();
    }
    assert_eq!(u.read(&canon_id).unwrap().state, MemoryState::Superseded);
    assert_eq!(u.read(&canon_id).unwrap().superseded_by, Some(new_canon));

    // List filters work over the mixed-state set.
    let archived = u.list(&MemoryFilter::any().with_state(MemoryState::Archived));
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, stale_id);

    // Retention scoring on the pinned object stays high.
    let pinned = u.read(&pin_id).unwrap();
    let score = compute_retention_score(pinned, Utc::now());
    assert!(score.total >= 0.9);
    assert!(score.pinning > 0.5);

    // Forget the superseded row — Superseded falls under "non-canonical
    // -> drop".
    u.forget(&canon_id).unwrap();
    assert!(u.read(&canon_id).is_none());
}
