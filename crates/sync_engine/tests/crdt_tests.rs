//! Integration tests for the add-wins CRDT and op log.

use sync_engine::{merge_logs, AddWinsSet, OpLog, SyncEngine, SyncOpKind};
use uuid::Uuid;

#[test]
fn add_then_contains() {
    let mut s = AddWinsSet::new();
    s.add("alpha");
    assert!(s.contains(&"alpha"));
}

#[test]
fn remove_drops_observed_tags() {
    let mut s = AddWinsSet::new();
    s.add("alpha");
    s.add("beta");
    s.remove(&"alpha");
    assert!(!s.contains(&"alpha"));
    assert!(s.contains(&"beta"));
}

#[test]
fn add_wins_over_concurrent_remove() {
    let mut x = AddWinsSet::new();
    let mut y = AddWinsSet::new();
    x.add("alpha");
    let mut y_view = x.clone();

    x.remove(&"alpha");
    y.add_with_tag("alpha", Uuid::new_v4());

    y_view.merge(&x);
    y_view.merge(&y);
    assert!(y_view.contains(&"alpha"));
}

#[test]
fn merge_is_commutative() {
    let mut a = AddWinsSet::new();
    a.add("a");
    let mut b = AddWinsSet::new();
    b.add("b");
    b.remove(&"a");

    let mut ab = a.clone();
    ab.merge(&b);
    let mut ba = b.clone();
    ba.merge(&a);
    assert_eq!(ab.contains(&"a"), ba.contains(&"a"));
    assert_eq!(ab.contains(&"b"), ba.contains(&"b"));
}

#[test]
fn merge_is_idempotent() {
    let mut a = AddWinsSet::new();
    a.add("x");
    a.remove(&"x");
    let snapshot = a.clone();
    a.merge(&snapshot);
    a.merge(&snapshot);
    assert!(!a.contains(&"x"));
}

#[test]
fn op_log_replay_recovers_set() {
    let mut log = OpLog::<String>::new(Uuid::new_v4());
    log.record_add("alpha".into());
    log.record_add("beta".into());
    log.record_remove(
        "alpha".into(),
        log.replay().unwrap().0.tags_for(&"alpha".to_string()),
    );
    let (set, supers) = log.replay().unwrap();
    assert!(!set.contains(&"alpha".to_string()));
    assert!(set.contains(&"beta".to_string()));
    assert!(supers.is_empty());
}

#[test]
fn supersede_records_contradiction_metadata() {
    let mut log = OpLog::<String>::new(Uuid::new_v4());
    log.record_add("v1".into());
    let observed = log.replay().unwrap().0.tags_for(&"v1".to_string());
    log.record_supersede("v1".into(), "v2".into(), observed);
    let (_set, supers) = log.replay().unwrap();
    assert_eq!(supers, vec![("v1".to_string(), "v2".to_string())]);

    // The op log itself should expose the supersede entry by kind.
    let mut found = false;
    for entry in &log.ops {
        if matches!(&entry.op, SyncOpKind::Supersede { value, successor, .. } if value == "v1" && successor == "v2")
        {
            found = true;
        }
    }
    assert!(found);
}

#[test]
fn op_log_merge_dedupes_by_replica_seq() {
    let id = Uuid::new_v4();
    let mut a = OpLog::<String>::new(id);
    a.record_add("x".into());
    let b = a.clone();

    let mut merged = a.clone();
    merged.merge(&b);
    assert_eq!(merged.ops.len(), a.ops.len());
}

#[test]
fn merge_logs_function_matches_method() {
    let mut a = OpLog::<String>::new(Uuid::new_v4());
    a.record_add("a".into());
    let mut b = OpLog::<String>::new(Uuid::new_v4());
    b.record_add("b".into());

    let merged_fn = merge_logs(&a, &b);
    let mut merged_method = a.clone();
    merged_method.merge(&b);
    assert_eq!(merged_fn.ops.len(), merged_method.ops.len());
}

#[test]
fn sync_engine_round_trip() {
    let mut e = SyncEngine::<Uuid>::new();
    let alpha = Uuid::new_v4();
    let beta = Uuid::new_v4();
    e.add(alpha);
    e.add(beta);
    e.remove(alpha);
    let (set, _) = e.state().unwrap();
    assert!(!set.contains(&alpha));
    assert!(set.contains(&beta));
}

#[test]
fn sync_engine_merge_yields_consistent_state() {
    let mut a = SyncEngine::<String>::new();
    let mut b = SyncEngine::<String>::new();
    a.add("alpha".into());
    b.add("beta".into());
    a.remove("alpha".into());

    a.merge(&b);
    b.merge(&a);
    let (state_a, _) = a.state().unwrap();
    let (state_b, _) = b.state().unwrap();
    assert_eq!(
        state_a.contains(&"alpha".to_string()),
        state_b.contains(&"alpha".to_string())
    );
    assert_eq!(
        state_a.contains(&"beta".to_string()),
        state_b.contains(&"beta".to_string())
    );
}

#[test]
fn supersession_via_engine_records_pair() {
    let mut e = SyncEngine::<String>::new();
    e.add("v1".into());
    e.supersede("v1".into(), "v2".into());
    e.add("v2".into());
    let (set, supers) = e.state().unwrap();
    assert!(set.contains(&"v2".to_string()));
    assert_eq!(supers, vec![("v1".to_string(), "v2".to_string())]);
}
