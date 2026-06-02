//! Synthesis pipeline benchmarks.
//!
//! Measures the end-to-end Channel → Domain → Tenant synthesis
//! latency using the `NoOpSynthesizer` (which exercises the full
//! pipeline machinery without actual LLM inference).
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p integration_tests --bench synthesis_bench
//! ```

use std::time::Duration;

use chrono::Utc;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use evidence_store::ScopeId;
use synthesis_pipeline::{
    NoOpSynthesizer, SynthesisInputs, SynthesisPipeline, SynthesisWindow, SynthesisWindowManager,
};

/// Window and scope counts to benchmark.
const WINDOW_COUNTS: &[(&str, usize)] = &[
    ("10_windows", 10),
    ("100_windows", 100),
    ("1000_windows", 1_000),
];

fn bench_channel_synthesis(c: &mut Criterion) {
    let mut group = c.benchmark_group("synthesis/channel");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    let synthesizer = NoOpSynthesizer::new();
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    let window =
        SynthesisWindow::new(scope, now - chrono::Duration::hours(1), now).expect("window");
    let inputs = SynthesisInputs::from_recap("Test recap: team discussed migration timeline and assigned tasks to engineering leads. Key decisions included API versioning strategy.");

    group.bench_function("single_channel_recap", |b| {
        b.iter(|| {
            let obj = synthesizer.synthesize(&window, &inputs).expect("synth");
            black_box(obj);
        });
    });
    group.finish();
}

fn bench_synthesis_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("synthesis/batch");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(15));

    let synthesizer = NoOpSynthesizer::new();

    for &(label, count) in WINDOW_COUNTS {
        group.throughput(Throughput::Elements(count as u64));

        // Pre-build windows and inputs.
        let now = Utc::now();
        let windows_and_inputs: Vec<_> = (0..i64::try_from(count).unwrap())
            .map(|i| {
                let scope = ScopeId::new_v4();
                let offset_end = chrono::Duration::minutes(i * 10);
                let offset_start = chrono::Duration::minutes((i + 1) * 10);
                let start = now - offset_start;
                let end = now - offset_end;
                let window = SynthesisWindow::new(scope, start, end).expect("window");
                let inputs = SynthesisInputs::from_recap(format!(
                    "Channel {i}: discussion about project milestones and deliverables."
                ));
                (window, inputs)
            })
            .collect();

        group.bench_with_input(BenchmarkId::new("batch_synth", label), &(), |b, _| {
            b.iter(|| {
                let mut total_payload = 0usize;
                for (window, inputs) in &windows_and_inputs {
                    let obj = synthesizer.synthesize(window, inputs).expect("synth");
                    total_payload += obj.payload.len();
                }
                black_box(total_payload);
            });
        });
    }
    group.finish();
}

fn bench_window_manager(c: &mut Criterion) {
    let mut group = c.benchmark_group("synthesis/window_manager");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    for &(label, count) in WINDOW_COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("open_windows", label), &(), |b, _| {
            b.iter(|| {
                let mut manager = SynthesisWindowManager::new();
                let now = Utc::now();
                for i in 0..i64::try_from(count).unwrap() {
                    let scope = ScopeId::new_v4();
                    let offset_end = chrono::Duration::minutes(i * 10);
                    let offset_start = chrono::Duration::minutes((i + 1) * 10);
                    let start = now - offset_start;
                    let end = now - offset_end;
                    manager.open_window(scope, start, end).expect("open");
                }
                black_box(manager.len());
            });
        });
    }
    group.finish();
}

fn bench_window_scope_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("synthesis/window_query");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    // Build manager with 1000 windows across 50 scopes.
    let mut manager = SynthesisWindowManager::new();
    let now = Utc::now();
    let scopes: Vec<ScopeId> = (0..50).map(|_| ScopeId::new_v4()).collect();
    for i in 0i64..1000 {
        let scope = scopes[usize::try_from(i).unwrap() % 50];
        let offset_end = chrono::Duration::minutes(i * 10);
        let offset_start = chrono::Duration::minutes((i + 1) * 10);
        let start = now - offset_start;
        let end = now - offset_end;
        manager.open_window(scope, start, end).expect("open");
    }

    group.throughput(Throughput::Elements(50));
    group.bench_function("windows_for_50_scopes", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for scope in &scopes {
                let windows = manager.windows_for(*scope);
                total += windows.len();
            }
            black_box(total);
        });
    });
    group.finish();
}

criterion_group!(
    synthesis_benches,
    bench_channel_synthesis,
    bench_synthesis_batch,
    bench_window_manager,
    bench_window_scope_query,
);
criterion_main!(synthesis_benches);
