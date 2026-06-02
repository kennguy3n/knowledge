//! `bench_permission_check` — Zanzibar reachability at scale.
//!
//! Builds a `TupleStore` of ~10K relation tuples whose positive path
//! is a depth-5 group-indirection chain and whose target object has
//! fan-out 100 (100 group subjects granted `Viewer`), then times
//! `check_permission` reachability for:
//!
//! * **allowed_depth_5** — a user that reaches the target through the
//!   five-hop chain (the worst-case positive).
//! * **denied** — an unconnected user (the BFS must exhaust the
//!   reachable set before returning `false`).
//!
//! `Throughput::Elements(1)` makes Criterion print checks/sec; the
//! sample tail in `target/criterion/.../estimates.json` gives p99.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_permission_check
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use uuid::Uuid;

use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

const TOTAL_TUPLES: usize = 10_000;
const CHAIN_DEPTH: usize = 5;
const FAN_OUT: usize = 100;

/// Build the ~10K-tuple graph described in the module docs and return
/// the store, registry, target object, a reachable subject, and an
/// unconnected subject.
fn build_graph() -> (
    TupleStore,
    NamespaceRegistry,
    ObjectRef,
    SubjectRef,
    SubjectRef,
) {
    let mut store = TupleStore::new();
    let registry = NamespaceRegistry::with_defaults();
    let target = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());

    // Depth-5 positive chain: user -Member-> c0 -Member-> ... -> c4,
    // and c4#Member is Viewer of target.
    let user_id = Uuid::new_v4();
    let user_subject = SubjectRef::direct(SubjectType::User, user_id);
    let chain_ids: Vec<Uuid> = (0..CHAIN_DEPTH).map(|_| Uuid::new_v4()).collect();
    store
        .insert(RelationTuple::new(
            ObjectRef::new(ObjectType::Channel, chain_ids[0]),
            Relation::Member,
            user_subject,
        ))
        .expect("insert");
    for i in 0..CHAIN_DEPTH.saturating_sub(1) {
        store
            .insert(RelationTuple::new(
                ObjectRef::new(ObjectType::Channel, chain_ids[i + 1]),
                Relation::Member,
                SubjectRef::via(SubjectType::Channel, chain_ids[i], Relation::Member),
            ))
            .expect("insert");
    }
    store
        .insert(RelationTuple::new(
            target,
            Relation::Viewer,
            SubjectRef::via(
                SubjectType::Channel,
                *chain_ids.last().expect("non-empty chain"),
                Relation::Member,
            ),
        ))
        .expect("insert");

    // Fan-out 100: the target grants Viewer to 100 other group
    // subjects, so the reachability BFS over the target's inbound
    // edges fans out 100-wide.
    for _ in 0..FAN_OUT {
        store
            .insert(RelationTuple::new(
                target,
                Relation::Viewer,
                SubjectRef::via(SubjectType::Channel, Uuid::new_v4(), Relation::Member),
            ))
            .expect("insert");
    }

    // Pad with filler direct-membership tuples on distinct channels
    // until the store holds ~TOTAL_TUPLES tuples.
    while store.len() < TOTAL_TUPLES {
        store
            .insert(RelationTuple::new(
                ObjectRef::new(ObjectType::Channel, Uuid::new_v4()),
                Relation::Member,
                SubjectRef::direct(SubjectType::User, Uuid::new_v4()),
            ))
            .expect("insert");
    }

    let denied = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    (store, registry, target, user_subject, denied)
}

fn bench_permission_check(c: &mut Criterion) {
    let (store, registry, target, allowed, denied) = build_graph();

    // Sanity: the chain user is reachable, the stranger is not.
    assert!(
        check_permission(&store, &registry, target, Relation::Viewer, allowed),
        "depth-5 chain must resolve to allowed"
    );
    assert!(
        !check_permission(&store, &registry, target, Relation::Viewer, denied),
        "unconnected subject must be denied"
    );

    let mut group = c.benchmark_group("permission/check_10k_tuples");
    group.throughput(Throughput::Elements(1));
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    for (label, subject) in [("allowed_depth_5", allowed), ("denied", denied)] {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &subject,
            |b, subject| {
                b.iter(|| {
                    let allowed = check_permission(
                        black_box(&store),
                        black_box(&registry),
                        black_box(target),
                        Relation::Viewer,
                        black_box(*subject),
                    );
                    black_box(allowed);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(permission_benches, bench_permission_check);
criterion_main!(permission_benches);
