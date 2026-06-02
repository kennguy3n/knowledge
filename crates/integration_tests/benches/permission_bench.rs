//! Permission service (Zanzibar) benchmarks.
//!
//! Measures:
//!
//! * **Permission check latency**: reachability at various
//!   relation-graph depths.
//! * **TupleStore operations**: insert/lookup throughput.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p integration_tests --bench permission_bench
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use uuid::Uuid;

use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

/// Build a permission graph with nested group indirection at the given depth.
///
/// Creates a chain:
///   user -- Member --> channel_0 -- Member (via) --> channel_1 -- ... --> target_channel
///
/// At depth 0: user has direct Viewer on the target.
/// At depth N: user has Member on channel_0, channel_0#Member has Member
/// on channel_1, ..., channel_{N-1}#Member has Viewer on target.
fn build_deep_graph(depth: usize) -> (TupleStore, NamespaceRegistry, ObjectRef, SubjectRef) {
    let mut store = TupleStore::new();
    let registry = NamespaceRegistry::with_defaults();

    let target = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());
    let user_id = Uuid::new_v4();
    let user_subject = SubjectRef::direct(SubjectType::User, user_id);

    if depth == 0 {
        // Direct access: user is Viewer of target.
        store
            .insert(RelationTuple::new(target, Relation::Viewer, user_subject))
            .unwrap();
    } else {
        // Build a chain of channels.
        let chain_ids: Vec<Uuid> = (0..depth).map(|_| Uuid::new_v4()).collect();

        // User is Member of the first channel in the chain.
        store
            .insert(RelationTuple::new(
                ObjectRef::new(ObjectType::Channel, chain_ids[0]),
                Relation::Member,
                user_subject,
            ))
            .unwrap();

        // Each channel_i#Member is Member of channel_{i+1}.
        for i in 0..depth.saturating_sub(1) {
            store
                .insert(RelationTuple::new(
                    ObjectRef::new(ObjectType::Channel, chain_ids[i + 1]),
                    Relation::Member,
                    SubjectRef::via(SubjectType::Channel, chain_ids[i], Relation::Member),
                ))
                .unwrap();
        }

        // Last channel#Member has Viewer on target.
        store
            .insert(RelationTuple::new(
                target,
                Relation::Viewer,
                SubjectRef::via(
                    SubjectType::Channel,
                    *chain_ids.last().unwrap(),
                    Relation::Member,
                ),
            ))
            .unwrap();
    }

    (store, registry, target, user_subject)
}

fn bench_permission_check_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("permission/check_depth");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    let depths: &[(&str, usize)] = &[
        ("direct", 0),
        ("depth_1", 1),
        ("depth_3", 3),
        ("depth_5", 5),
        ("depth_10", 10),
    ];

    for &(label, depth) in depths {
        let (store, registry, object, subject) = build_deep_graph(depth);
        group.bench_with_input(BenchmarkId::new("reachability", label), &(), |b, _| {
            b.iter(|| {
                let allowed =
                    check_permission(&store, &registry, object, Relation::Viewer, subject);
                black_box(allowed);
            });
        });
    }
    group.finish();
}

fn bench_tuple_store_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("permission/tuple_insert");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    let sizes: &[(&str, usize)] = &[
        ("1K_tuples", 1_000),
        ("10K_tuples", 10_000),
        ("50K_tuples", 50_000),
    ];

    for &(label, size) in sizes {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("insert", label), &(), |b, _| {
            b.iter(|| {
                let mut store = TupleStore::new();
                for _ in 0..size {
                    let tuple = RelationTuple::new(
                        ObjectRef::new(ObjectType::Channel, Uuid::new_v4()),
                        Relation::Viewer,
                        SubjectRef::direct(SubjectType::User, Uuid::new_v4()),
                    );
                    store.insert(tuple).unwrap();
                }
                black_box(store.len());
            });
        });
    }
    group.finish();
}

fn bench_tuple_store_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("permission/tuple_lookup");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    // Pre-populate store with 10K tuples, then benchmark contains() lookups.
    let mut store = TupleStore::new();
    let mut tuples = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let tuple = RelationTuple::new(
            ObjectRef::new(ObjectType::Channel, Uuid::new_v4()),
            Relation::Viewer,
            SubjectRef::direct(SubjectType::User, Uuid::new_v4()),
        );
        store.insert(tuple).unwrap();
        tuples.push(tuple);
    }

    group.throughput(Throughput::Elements(1_000));
    group.bench_function("contains_1K_in_10K_store", |b| {
        b.iter(|| {
            let mut found = 0usize;
            for t in tuples.iter().take(1_000) {
                if store.contains(t) {
                    found += 1;
                }
            }
            black_box(found);
        });
    });
    group.finish();
}

fn bench_permission_many_tuples(c: &mut Criterion) {
    let mut group = c.benchmark_group("permission/check_wide");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    // Wide graph: many users have access to the same object.
    let registry = NamespaceRegistry::with_defaults();
    let mut store = TupleStore::new();
    let target = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());

    // Insert 1000 different users as Viewer on target.
    let mut users = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let user = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
        store
            .insert(RelationTuple::new(target, Relation::Viewer, user))
            .unwrap();
        users.push(user);
    }

    // Check the last user (worst-case linear scan).
    let last_user = *users.last().unwrap();
    group.bench_function("check_in_1K_viewers", |b| {
        b.iter(|| {
            let allowed = check_permission(&store, &registry, target, Relation::Viewer, last_user);
            black_box(allowed);
        });
    });
    group.finish();
}

criterion_group!(
    permission_benches,
    bench_permission_check_depth,
    bench_tuple_store_insert,
    bench_tuple_store_lookup,
    bench_permission_many_tuples,
);
criterion_main!(permission_benches);
