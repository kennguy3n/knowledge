//! `bench_decay_sweep` — retention decay over 100K objects.
//!
//! Builds 100K `MemoryObject`s with a realistic spread of ages,
//! access recency, and corroboration / pin counters, then times a
//! full `decay_sweep` (retention scoring + candidate-archive / TTL
//! transitions) over the whole slice.
//!
//! `Throughput::Elements(100_000)` makes Criterion print rows/sec;
//! the `decay/single_row` group isolates the per-row cost whose
//! median is the p50 and whose sample tail (in
//! `target/criterion/.../estimates.json`) yields p99.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_decay_sweep
//! ```

use std::time::Duration;

use chrono::Utc;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;

use evidence_store::ScopeId;
use memory_manager::{decay_sweep, MemoryObject, SensitivityClass};

const ROW_COUNT: usize = 100_000;

fn sensitivity(index: usize) -> SensitivityClass {
    match index % 4 {
        0 => SensitivityClass::Critical,
        1 => SensitivityClass::Important,
        2 => SensitivityClass::Useful,
        _ => SensitivityClass::Noise,
    }
}

/// Build `n` objects spread across ages / access recency / counters
/// so the sweep exercises both the "still warm" and the
/// "archive a cold candidate" branches.
fn build_objects(n: usize) -> Vec<MemoryObject> {
    let now = Utc::now();
    let scope = ScopeId::new_v4();
    (0..n)
        .map(|i| {
            let mut obj = MemoryObject::new_candidate(scope, sensitivity(i));
            // Ages fan out to 0..365 days; recency to 0..720 hours.
            obj.created_at = now - chrono::Duration::days(i64::try_from(i % 365).unwrap_or(0));
            obj.last_accessed_at =
                now - chrono::Duration::hours(i64::try_from(i % 720).unwrap_or(0));
            obj.retrieval_count = u32::try_from(i % 50).unwrap_or(0);
            obj.pin_count = u32::try_from(i % 5).unwrap_or(0);
            obj.corroboration_count = u32::try_from(i % 8).unwrap_or(0);
            obj
        })
        .collect()
}

fn bench_decay_sweep(c: &mut Criterion) {
    let base = build_objects(ROW_COUNT);
    let now = Utc::now();

    let mut group = c.benchmark_group("decay/sweep_100k");
    group.throughput(Throughput::Elements(ROW_COUNT as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("full_sweep", |b| {
        // Clone in setup so the measured region only pays for the
        // sweep, not the 100K-object allocation. `decay_sweep`
        // mutates the slice (writes `retention_score`, transitions
        // state), so each iteration needs a fresh copy.
        b.iter_with_setup(
            || base.clone(),
            |mut objects| {
                let report = decay_sweep(black_box(&mut objects), now);
                black_box(report.scored);
            },
        );
    });
    group.finish();

    let single = build_objects(1);
    let mut single_group = c.benchmark_group("decay/single_row");
    single_group.throughput(Throughput::Elements(1));
    single_group.sample_size(100);
    single_group.bench_function("score_and_transition", |b| {
        b.iter_with_setup(
            || single.clone(),
            |mut objects| {
                let report = decay_sweep(black_box(&mut objects), now);
                black_box(report.scored);
            },
        );
    });
    single_group.finish();
}

criterion_group!(decay_benches, bench_decay_sweep);
criterion_main!(decay_benches);
