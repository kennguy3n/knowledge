//! Adversarial tests for the permission service — audit preparation.
//!
//! These tests verify security-critical invariants:
//!
//! * **Privilege escalation**: indirect relation paths cannot bypass
//!   explicit denials or exceed the granted privilege level.
//! * **Cycle detection**: the reachability walker terminates on cyclic
//!   relation graphs without panicking or hanging.
//! * **Performance**: pathological relation graphs (deep chains, wide
//!   fan-outs) do not cause stack overflow or excessive latency.

use std::time::{Duration, Instant};

use uuid::Uuid;

use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

// ---------------------------------------------------------------------------
// 1. Privilege escalation — indirect paths cannot exceed granted level
// ---------------------------------------------------------------------------

#[test]
fn viewer_cannot_escalate_via_indirect_userset_rewrite() {
    // Setup: Alice is Viewer on Tenant T1. Domain D1 has an Editor
    // userset rewrite pointing at T1's Viewers. Alice should gain
    // Editor on D1 (and below), but NOT Admin or Owner on D1.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let t1 = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let d1 = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    // Alice is a Viewer on Tenant T1.
    store
        .insert(RelationTuple::new(t1, Relation::Viewer, alice))
        .unwrap();

    // Domain D1 grants Editor to anyone who is a Viewer on T1.
    store
        .insert(RelationTuple::new(
            d1,
            Relation::Editor,
            SubjectRef::via(SubjectType::Tenant, t1.object_id, Relation::Viewer),
        ))
        .unwrap();

    // Alice should have Editor (and below) on D1.
    assert!(check_permission(&store, &ns, d1, Relation::Editor, alice));
    assert!(check_permission(&store, &ns, d1, Relation::Member, alice));
    assert!(check_permission(&store, &ns, d1, Relation::Viewer, alice));

    // Alice must NOT have Admin or Owner on D1.
    assert!(!check_permission(&store, &ns, d1, Relation::Admin, alice));
    assert!(!check_permission(&store, &ns, d1, Relation::Owner, alice));
}

#[test]
fn member_cannot_escalate_through_multi_hop_rewrite() {
    // Setup: Alice is Member on Channel C1. C1's membership is
    // granted via Domain D1's Member userset, which in turn is
    // granted via Tenant T1's Viewer userset. No transitive hop
    // should elevate Alice beyond what each link explicitly grants.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let t1 = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let d1 = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let c1 = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    // Chain: Alice -> Viewer on T1 -> Member on D1 -> Viewer on C1.
    store
        .insert(RelationTuple::new(t1, Relation::Viewer, alice))
        .unwrap();
    store
        .insert(RelationTuple::new(
            d1,
            Relation::Member,
            SubjectRef::via(SubjectType::Tenant, t1.object_id, Relation::Viewer),
        ))
        .unwrap();
    store
        .insert(RelationTuple::new(
            c1,
            Relation::Viewer,
            SubjectRef::via(SubjectType::Domain, d1.object_id, Relation::Member),
        ))
        .unwrap();

    // Alice should be a Viewer on C1 (and only Viewer).
    assert!(check_permission(&store, &ns, c1, Relation::Viewer, alice));
    assert!(!check_permission(&store, &ns, c1, Relation::Member, alice));
    assert!(!check_permission(&store, &ns, c1, Relation::Editor, alice));
    assert!(!check_permission(&store, &ns, c1, Relation::Admin, alice));
    assert!(!check_permission(&store, &ns, c1, Relation::Owner, alice));
}

#[test]
fn orthogonal_relations_do_not_cross_contaminate() {
    // Synthesizer and Proposer are orthogonal to the inheritance
    // chain. Granting Owner should NOT imply Synthesizer; granting
    // Synthesizer should NOT imply Viewer.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let ch = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let bob = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    store
        .insert(RelationTuple::new(ch, Relation::Owner, alice))
        .unwrap();
    store
        .insert(RelationTuple::new(ch, Relation::Synthesizer, bob))
        .unwrap();

    // Alice (Owner) should NOT have Synthesizer or Proposer.
    assert!(!check_permission(
        &store,
        &ns,
        ch,
        Relation::Synthesizer,
        alice
    ));
    assert!(!check_permission(
        &store,
        &ns,
        ch,
        Relation::Proposer,
        alice
    ));

    // Bob (Synthesizer) should NOT have Viewer or any other
    // inheritance-chain relation.
    assert!(!check_permission(&store, &ns, ch, Relation::Viewer, bob));
    assert!(!check_permission(&store, &ns, ch, Relation::Member, bob));
    assert!(!check_permission(&store, &ns, ch, Relation::Owner, bob));
}

#[test]
fn unrelated_object_grants_do_not_leak() {
    // Alice is Owner on Tenant T1, but should have no access to
    // Tenant T2 (a completely separate object).
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let t1 = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let t2 = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    store
        .insert(RelationTuple::new(t1, Relation::Owner, alice))
        .unwrap();

    assert!(check_permission(&store, &ns, t1, Relation::Owner, alice));
    assert!(!check_permission(&store, &ns, t2, Relation::Viewer, alice));
    assert!(!check_permission(&store, &ns, t2, Relation::Owner, alice));
}

// ---------------------------------------------------------------------------
// 2. Cycle detection — the walker must terminate on cyclic graphs
// ---------------------------------------------------------------------------

#[test]
fn direct_self_referencing_cycle_terminates() {
    // Object A's Viewer set includes "anyone who is a Viewer on A"
    // — a direct self-loop. The walker must not infinite-loop.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let a = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    // Self-referencing userset rewrite: (A, Viewer) -> (A # Viewer).
    store
        .insert(RelationTuple::new(
            a,
            Relation::Viewer,
            SubjectRef::via(SubjectType::Domain, a.object_id, Relation::Viewer),
        ))
        .unwrap();

    // Should terminate and return false (Alice has no direct grant).
    assert!(!check_permission(&store, &ns, a, Relation::Viewer, alice));
}

#[test]
fn two_node_cycle_terminates() {
    // A's Viewer -> B # Viewer, B's Viewer -> A # Viewer.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let a = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let b = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    store
        .insert(RelationTuple::new(
            a,
            Relation::Viewer,
            SubjectRef::via(SubjectType::Domain, b.object_id, Relation::Viewer),
        ))
        .unwrap();
    store
        .insert(RelationTuple::new(
            b,
            Relation::Viewer,
            SubjectRef::via(SubjectType::Domain, a.object_id, Relation::Viewer),
        ))
        .unwrap();

    // Neither direction should hang.
    assert!(!check_permission(&store, &ns, a, Relation::Viewer, alice));
    assert!(!check_permission(&store, &ns, b, Relation::Viewer, alice));
}

#[test]
fn three_node_cycle_terminates() {
    // A -> B -> C -> A cycle via userset rewrites.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let a = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let b = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let c = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    store
        .insert(RelationTuple::new(
            a,
            Relation::Member,
            SubjectRef::via(SubjectType::Domain, b.object_id, Relation::Member),
        ))
        .unwrap();
    store
        .insert(RelationTuple::new(
            b,
            Relation::Member,
            SubjectRef::via(SubjectType::Domain, c.object_id, Relation::Member),
        ))
        .unwrap();
    store
        .insert(RelationTuple::new(
            c,
            Relation::Member,
            SubjectRef::via(SubjectType::Domain, a.object_id, Relation::Member),
        ))
        .unwrap();

    assert!(!check_permission(&store, &ns, a, Relation::Member, alice));
    assert!(!check_permission(&store, &ns, b, Relation::Member, alice));
    assert!(!check_permission(&store, &ns, c, Relation::Member, alice));
}

#[test]
fn cycle_with_valid_grant_still_resolves_true() {
    // A -> B -> A cycle, but Alice is directly granted Member on B.
    // The walker should find Alice through the cycle and return true.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let a = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let b = ObjectRef::new(ObjectType::Domain, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    store
        .insert(RelationTuple::new(
            a,
            Relation::Member,
            SubjectRef::via(SubjectType::Domain, b.object_id, Relation::Member),
        ))
        .unwrap();
    store
        .insert(RelationTuple::new(
            b,
            Relation::Member,
            SubjectRef::via(SubjectType::Domain, a.object_id, Relation::Member),
        ))
        .unwrap();
    // Alice has a direct grant on B.
    store
        .insert(RelationTuple::new(b, Relation::Member, alice))
        .unwrap();

    // Alice should be reachable from A via A->B (where she has a
    // direct grant).
    assert!(check_permission(&store, &ns, a, Relation::Member, alice));
    assert!(check_permission(&store, &ns, b, Relation::Member, alice));
}

// ---------------------------------------------------------------------------
// 3. Performance — pathological graphs must not cause stack overflow
//    or excessive latency
// ---------------------------------------------------------------------------

#[test]
fn deep_chain_does_not_stack_overflow() {
    // Build a chain of 500 objects, each pointing to the next via
    // userset rewrite: O_0 -> O_1 -> ... -> O_499, then grant
    // Alice on O_499. Walk from O_0 should find Alice.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let depth = 500;
    let objects: Vec<ObjectRef> = (0..depth)
        .map(|_| ObjectRef::new(ObjectType::Domain, Uuid::new_v4()))
        .collect();

    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    for i in 0..depth - 1 {
        store
            .insert(RelationTuple::new(
                objects[i],
                Relation::Viewer,
                SubjectRef::via(
                    SubjectType::Domain,
                    objects[i + 1].object_id,
                    Relation::Viewer,
                ),
            ))
            .unwrap();
    }
    // Grant Alice on the last object.
    store
        .insert(RelationTuple::new(
            objects[depth - 1],
            Relation::Viewer,
            alice,
        ))
        .unwrap();

    let start = Instant::now();
    let result = check_permission(&store, &ns, objects[0], Relation::Viewer, alice);
    let elapsed = start.elapsed();

    assert!(result, "Alice should be reachable through the deep chain");
    assert!(
        elapsed < Duration::from_secs(5),
        "Deep chain walk took {:?} — exceeds 5s threshold",
        elapsed
    );
}

#[test]
fn wide_fan_out_does_not_cause_excessive_latency() {
    // Build a single object with 1000 tuples fanning out to
    // distinct subjects (none of which is Alice). Verify that
    // a negative check completes in reasonable time.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let root = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    for _ in 0..1000 {
        let user = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
        store
            .insert(RelationTuple::new(root, Relation::Viewer, user))
            .unwrap();
    }

    let start = Instant::now();
    let result = check_permission(&store, &ns, root, Relation::Viewer, alice);
    let elapsed = start.elapsed();

    assert!(!result, "Alice has no grant");
    assert!(
        elapsed < Duration::from_secs(2),
        "Wide fan-out check took {:?} — exceeds 2s threshold",
        elapsed
    );
}

#[test]
fn wide_fan_out_with_userset_rewrites() {
    // 100 objects, each with a userset rewrite pointing to the
    // same root. Only the last rewrite chain leads to Alice.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let root = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    let intermediates: Vec<ObjectRef> = (0..100)
        .map(|_| ObjectRef::new(ObjectType::Domain, Uuid::new_v4()))
        .collect();

    for obj in &intermediates {
        store
            .insert(RelationTuple::new(
                root,
                Relation::Member,
                SubjectRef::via(SubjectType::Domain, obj.object_id, Relation::Member),
            ))
            .unwrap();
    }

    // Only the last intermediate grants Alice.
    let last = intermediates.last().unwrap();
    store
        .insert(RelationTuple::new(*last, Relation::Member, alice))
        .unwrap();

    let start = Instant::now();
    let result = check_permission(&store, &ns, root, Relation::Member, alice);
    let elapsed = start.elapsed();

    assert!(
        result,
        "Alice should be reachable through the last intermediate"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "Wide fan-out with rewrites took {:?} — exceeds 2s threshold",
        elapsed
    );
}

#[test]
fn combined_deep_and_wide_graph() {
    // 50 levels deep × 10 wide at each level = 500 objects total.
    // Alice is granted at the deepest, widest point.
    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let alice = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let depth = 50;
    let width = 10;

    // Build levels. Each level has `width` objects, and each object
    // at level L has a userset rewrite pointing to every object at
    // level L+1.
    let mut levels: Vec<Vec<ObjectRef>> = Vec::new();
    for _ in 0..depth {
        let level: Vec<ObjectRef> = (0..width)
            .map(|_| ObjectRef::new(ObjectType::Domain, Uuid::new_v4()))
            .collect();
        levels.push(level);
    }

    for l in 0..depth - 1 {
        // Each object at level L rewrites to the first object at
        // level L+1 (keeping it tractable).
        let next_obj = levels[l + 1][0];
        for obj in &levels[l] {
            store
                .insert(RelationTuple::new(
                    *obj,
                    Relation::Viewer,
                    SubjectRef::via(SubjectType::Domain, next_obj.object_id, Relation::Viewer),
                ))
                .unwrap();
        }
    }

    // Grant Alice on the first object of the deepest level.
    store
        .insert(RelationTuple::new(
            levels[depth - 1][0],
            Relation::Viewer,
            alice,
        ))
        .unwrap();

    let start = Instant::now();
    let result = check_permission(&store, &ns, levels[0][0], Relation::Viewer, alice);
    let elapsed = start.elapsed();

    assert!(
        result,
        "Alice should be reachable through the deep+wide graph"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "Deep+wide graph walk took {:?} — exceeds 5s threshold",
        elapsed
    );
}
