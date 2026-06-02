//! Integration test: two `SyncEngine` instances → mutate independently
//! → exchange deltas → verify convergence.

use uuid::Uuid;

use sync_engine::delta::{apply_delta, encode_delta_since};
use sync_engine::SyncEngine;

#[test]
fn concurrent_adds_converge_via_delta_sync() {
    let mut engine_a = SyncEngine::<String>::new();
    let mut engine_b = SyncEngine::<String>::new();

    engine_a.add("alpha".into());
    engine_a.add("bravo".into());

    engine_b.add("charlie".into());
    engine_b.add("delta".into());

    // Exchange deltas.
    let delta_a = encode_delta_since(engine_a.op_log(), 0).unwrap();
    let delta_b = encode_delta_since(engine_b.op_log(), 0).unwrap();

    let absorbed_b = apply_delta(&mut engine_b, &delta_a).unwrap();
    let absorbed_a = apply_delta(&mut engine_a, &delta_b).unwrap();

    assert!(absorbed_a > 0);
    assert!(absorbed_b > 0);

    let (state_a, _) = engine_a.state().unwrap();
    let (state_b, _) = engine_b.state().unwrap();

    for val in &["alpha", "bravo", "charlie", "delta"] {
        assert!(state_a.contains(&val.to_string()), "{val} missing in A");
        assert!(state_b.contains(&val.to_string()), "{val} missing in B");
    }
}

#[test]
fn add_wins_over_concurrent_remove() {
    let mut engine_a = SyncEngine::<String>::new();
    let mut engine_b = SyncEngine::<String>::new();

    // Both start with "x".
    engine_a.add("x".into());
    let delta_init = encode_delta_since(engine_a.op_log(), 0).unwrap();
    apply_delta(&mut engine_b, &delta_init).unwrap();

    // Snapshot each engine's clock *for its own local ops* before
    // the concurrent mutation round.
    let clock_a_local = engine_a.op_log().clock;
    let clock_b_local = engine_b.op_log().clock;

    // A removes "x"; B re-adds "x" concurrently.
    engine_a.remove("x".into());
    engine_b.add("x".into());

    // Exchange deltas. Each receiver passes the sender's last-seen
    // local clock so only new ops from the sender are transmitted.
    let delta_a = encode_delta_since(engine_a.op_log(), clock_a_local).unwrap();
    let delta_b = encode_delta_since(engine_b.op_log(), clock_b_local).unwrap();

    apply_delta(&mut engine_b, &delta_a).unwrap();
    apply_delta(&mut engine_a, &delta_b).unwrap();

    // Add-wins semantics: "x" survives.
    let (state_a, _) = engine_a.state().unwrap();
    let (state_b, _) = engine_b.state().unwrap();
    assert!(state_a.contains(&"x".to_string()), "add-wins: x in A");
    assert!(state_b.contains(&"x".to_string()), "add-wins: x in B");
}

#[test]
fn supersession_propagates_via_delta() {
    let mut engine_a = SyncEngine::<String>::new();

    engine_a.add("old".into());
    engine_a.supersede("old".into(), "new".into());
    // Supersede tombstones "old" but does not add "new" to the
    // set — the caller must add the successor explicitly.
    engine_a.add("new".into());

    let delta = encode_delta_since(engine_a.op_log(), 0).unwrap();
    let mut engine_b = SyncEngine::<String>::new();
    apply_delta(&mut engine_b, &delta).unwrap();

    let (state, supers) = engine_b.state().unwrap();
    assert!(
        !state.contains(&"old".to_string()),
        "old removed by supersession"
    );
    assert!(state.contains(&"new".to_string()), "new present");
    assert!(
        supers.contains(&("old".to_string(), "new".to_string())),
        "supersession record propagated"
    );
}

#[test]
fn idempotent_merge_does_not_duplicate() {
    let mut engine_a = SyncEngine::<String>::new();
    engine_a.add("item".into());

    let delta = encode_delta_since(engine_a.op_log(), 0).unwrap();
    let mut engine_b = SyncEngine::<String>::new();
    apply_delta(&mut engine_b, &delta).unwrap();
    // Apply same delta again.
    let absorbed = apply_delta(&mut engine_b, &delta).unwrap();
    assert_eq!(absorbed, 0, "re-applying same delta absorbs nothing");

    let (state, _) = engine_b.state().unwrap();
    assert!(state.contains(&"item".to_string()));
}

#[test]
fn uuid_typed_engine_converges() {
    let mut engine_a = SyncEngine::<Uuid>::new();
    let mut engine_b = SyncEngine::<Uuid>::new();

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    engine_a.add(id1);
    engine_b.add(id2);

    let delta_a = encode_delta_since(engine_a.op_log(), 0).unwrap();
    let delta_b = encode_delta_since(engine_b.op_log(), 0).unwrap();

    apply_delta(&mut engine_b, &delta_a).unwrap();
    apply_delta(&mut engine_a, &delta_b).unwrap();

    let (state_a, _) = engine_a.state().unwrap();
    let (state_b, _) = engine_b.state().unwrap();

    assert!(state_a.contains(&id1));
    assert!(state_a.contains(&id2));
    assert!(state_b.contains(&id1));
    assert!(state_b.contains(&id2));
}
