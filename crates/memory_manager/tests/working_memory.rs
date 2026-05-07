//! Tests for [`WorkingMemory`] — TTL eviction, capacity limits, ordering.

use chrono::{Duration, Utc};
use evidence_store::ScopeId;
use memory_manager::{WorkingMemory, WorkingMemoryEntry};

#[test]
fn entries_are_returned_in_insertion_order() {
    let mut wm = WorkingMemory::new(8, Duration::seconds(60));
    let scope = ScopeId::new_v4();
    wm.push_with_default_ttl(scope, "first", 0.5);
    wm.push_with_default_ttl(scope, "second", 0.5);
    wm.push_with_default_ttl(scope, "third", 0.5);
    let ctx = wm.get_context();
    let contents: Vec<_> = ctx.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(contents, vec!["first", "second", "third"]);
}

#[test]
fn capacity_cap_evicts_lowest_relevance_entries() {
    let mut wm = WorkingMemory::new(2, Duration::seconds(60));
    let scope = ScopeId::new_v4();
    wm.push_with_default_ttl(scope, "low", 0.1);
    wm.push_with_default_ttl(scope, "mid", 0.5);
    wm.push_with_default_ttl(scope, "high", 0.9);
    let contents: Vec<_> = wm
        .get_context()
        .iter()
        .map(|e| e.content.as_str())
        .collect();
    assert_eq!(contents, vec!["mid", "high"]);
}

#[test]
fn expired_entries_are_skipped_and_evicted_on_demand() {
    let mut wm = WorkingMemory::new(8, Duration::seconds(60));
    let scope = ScopeId::new_v4();

    // Manually push an entry with a TTL already expired.
    let stale = WorkingMemoryEntry {
        id: uuid::Uuid::new_v4(),
        content: "stale".into(),
        scope_id: scope,
        inserted_at: Utc::now() - Duration::seconds(120),
        ttl: Duration::seconds(10),
        relevance_score: 0.5,
    };
    wm.push(stale);
    wm.push_with_default_ttl(scope, "fresh", 0.5);

    let live: Vec<_> = wm
        .get_context()
        .iter()
        .map(|e| e.content.as_str())
        .collect();
    assert_eq!(live, vec!["fresh"]);

    // evict_expired drops the row from the underlying storage too.
    let evicted = wm.evict_expired();
    assert_eq!(evicted, 0); // already evicted on push
    assert_eq!(wm.len(), 1);
}

#[test]
fn clear_drops_everything() {
    let mut wm = WorkingMemory::new(8, Duration::seconds(60));
    let scope = ScopeId::new_v4();
    for _ in 0..5 {
        wm.push_with_default_ttl(scope, "x", 0.5);
    }
    assert_eq!(wm.len(), 5);
    wm.clear();
    assert!(wm.is_empty());
    assert!(wm.get_context().is_empty());
}

#[test]
fn capacity_floor_is_at_least_one() {
    // Asking for a zero-capacity window is treated as 1 (the
    // implementation enforces a minimum capacity).
    let mut wm = WorkingMemory::new(0, Duration::seconds(60));
    let scope = ScopeId::new_v4();
    wm.push_with_default_ttl(scope, "only", 0.5);
    assert_eq!(wm.max_entries(), 1);
    assert_eq!(wm.get_context().len(), 1);
}
