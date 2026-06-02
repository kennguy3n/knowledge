//! CRDT sync engine benchmarks.
//!
//! Measures:
//!
//! * **Merge throughput**: merging op-logs of various sizes.
//! * **Compaction**: effect of `compact_threshold` values on merge
//!   latency and delta payload size (validates `docs/COST_MODEL.md`
//!   lines 196-204).
//! * **Delta serialisation**: round-trip encode/decode at various
//!   op-log sizes.
//! * **Snapshot**: checkpoint/restore throughput.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p integration_tests --bench sync_bench
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use uuid::Uuid;

use sync_engine::SyncEngine;

/// Op-log sizes to benchmark.
const OP_LOG_SIZES: &[(&str, usize)] =
    &[("1K_ops", 1_000), ("10K_ops", 10_000), ("50K_ops", 50_000)];

/// Build a SyncEngine with `n` add operations.
fn build_engine(n: usize) -> SyncEngine<Uuid> {
    let mut engine = SyncEngine::new();
    for _ in 0..n {
        engine.add(Uuid::new_v4());
    }
    engine
}

/// Build an engine with `n` adds and `removes` removes (removes the first `removes` items).
fn build_engine_with_removes(adds: usize, removes: usize) -> SyncEngine<Uuid> {
    let mut engine = SyncEngine::new();
    let mut values = Vec::with_capacity(adds);
    for _ in 0..adds {
        let v = Uuid::new_v4();
        engine.add(v);
        values.push(v);
    }
    for v in values.iter().take(removes) {
        engine.remove(*v);
    }
    engine
}

fn bench_merge_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/merge");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(label, size) in OP_LOG_SIZES {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("two_engines", label), &(), |b, _| {
            b.iter_with_setup(
                || {
                    let engine_a = build_engine(size);
                    let engine_b = build_engine(size);
                    (engine_a, engine_b)
                },
                |(mut engine_a, engine_b)| {
                    engine_a.merge(&engine_b);
                    black_box(engine_a.op_log().len());
                },
            );
        });
    }
    group.finish();
}

fn bench_compaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/compact");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(label, size) in OP_LOG_SIZES {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(
            BenchmarkId::new("compact_after_churn", label),
            &(),
            |b, _| {
                b.iter_with_setup(
                    || {
                        // Half adds, half removes to create tombstones.
                        build_engine_with_removes(size, size / 2)
                    },
                    |mut engine| {
                        let removed = engine.compact().expect("compact");
                        black_box(removed);
                    },
                );
            },
        );
    }
    group.finish();
}

fn bench_compaction_threshold_effect(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/compact_threshold");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));

    // Per docs/COST_MODEL.md lines 196-204: compact_threshold controls
    // steady-state delta payload size. We measure merge latency at
    // different threshold settings.
    let thresholds: &[(&str, Option<usize>)] = &[
        ("5000_ops", Some(5_000)),
        ("10000_ops_default", Some(10_000)),
        ("20000_ops", Some(20_000)),
        ("disabled", None),
    ];

    for &(threshold_label, threshold) in thresholds {
        let size = 20_000usize;
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(
            BenchmarkId::new("merge_latency", threshold_label),
            &(),
            |b, _| {
                b.iter_with_setup(
                    || {
                        let mut engine_a = SyncEngine::new();
                        engine_a = engine_a.with_compact_threshold(threshold);
                        for _ in 0..size {
                            engine_a.add(Uuid::new_v4());
                        }
                        // Compact if threshold reached to simulate steady-state.
                        if threshold.is_some() {
                            let _ = engine_a.compact();
                        }
                        let engine_b = build_engine(size / 2);
                        (engine_a, engine_b)
                    },
                    |(mut engine_a, engine_b)| {
                        engine_a.merge(&engine_b);
                        black_box(engine_a.op_log().len());
                    },
                );
            },
        );
    }
    group.finish();
}

fn bench_delta_payload_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/delta_size");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    // Measure delta payload size after compaction at various thresholds.
    let thresholds: &[(&str, Option<usize>)] = &[
        ("5000_ops", Some(5_000)),
        ("10000_ops_default", Some(10_000)),
        ("disabled", None),
    ];

    for &(threshold_label, threshold) in thresholds {
        let size = 15_000usize;
        group.bench_with_input(
            BenchmarkId::new("snapshot_size", threshold_label),
            &(),
            |b, _| {
                b.iter_with_setup(
                    || {
                        let mut engine = SyncEngine::new();
                        engine = engine.with_compact_threshold(threshold);
                        for _ in 0..size {
                            engine.add(Uuid::new_v4());
                        }
                        if threshold.is_some() {
                            let _ = engine.compact();
                        }
                        engine
                    },
                    |engine| {
                        let snapshot = engine.snapshot().expect("snapshot");
                        black_box(snapshot.len());
                    },
                );
            },
        );
    }
    group.finish();
}

fn bench_snapshot_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/snapshot");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    for &(label, size) in OP_LOG_SIZES {
        let engine = build_engine(size);
        let snapshot_bytes = engine.snapshot().expect("snapshot");

        group.throughput(Throughput::Bytes(snapshot_bytes.len() as u64));
        group.bench_with_input(BenchmarkId::new("serialize", label), &(), |b, _| {
            b.iter(|| {
                let bytes = engine.snapshot().expect("snapshot");
                black_box(bytes.len());
            });
        });

        group.bench_with_input(BenchmarkId::new("deserialize", label), &(), |b, _| {
            b.iter(|| {
                let restored =
                    SyncEngine::<Uuid>::restore_snapshot(&snapshot_bytes).expect("restore");
                black_box(restored.replica_id());
            });
        });
    }
    group.finish();
}

criterion_group!(
    sync_benches,
    bench_merge_throughput,
    bench_compaction,
    bench_compaction_threshold_effect,
    bench_delta_payload_size,
    bench_snapshot_roundtrip,
);
criterion_main!(sync_benches);
