//! Criterion benchmarks for the lifecycle simulation.
//!
//! Run with: `cargo bench -p lifecycle_sim`

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use lifecycle_sim::{run_simulation, DriverKind, ScalePreset};

fn bench_ingest_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingest_throughput");
    group.sample_size(10);

    for preset in [ScalePreset::Quick] {
        let label = match preset {
            ScalePreset::Quick => "quick",
            ScalePreset::Standard => "standard",
            ScalePreset::Stress => "stress",
        };
        group.bench_with_input(BenchmarkId::new("rust_native", label), &preset, |b, &preset| {
            b.iter(|| {
                let report = run_simulation(preset, DriverKind::RustNative, 42, None);
                assert!(report.summary.pass_rate > 0.95, "pass rate too low");
            });
        });
    }

    group.finish();
}

fn bench_dataset_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("dataset_generation");
    group.sample_size(10);

    for preset in [ScalePreset::Quick] {
        let label = match preset {
            ScalePreset::Quick => "quick",
            ScalePreset::Standard => "standard",
            ScalePreset::Stress => "stress",
        };
        group.bench_with_input(BenchmarkId::new("generate", label), &preset, |b, &preset| {
            let config = preset.config();
            b.iter(|| {
                let dataset = lifecycle_sim::dataset::generate_dataset(config.clone());
                assert!(!dataset.turns.is_empty());
            });
        });
    }

    group.finish();
}

/// Benchmark multilingual dataset generation across different language counts.
fn bench_multilingual(c: &mut Criterion) {
    let mut group = c.benchmark_group("multilingual");
    group.sample_size(10);

    let config = ScalePreset::Quick.config();

    group.bench_function("generate_all_languages", |b| {
        b.iter(|| {
            let dataset = lifecycle_sim::dataset::generate_dataset(config.clone());
            let lang_count: std::collections::HashSet<&str> =
                dataset.turns.iter().map(|t| t.language.as_str()).collect();
            assert!(lang_count.len() > 1, "should have multiple languages");
        });
    });

    group.finish();
}

/// Benchmark media generation and attachment.
fn bench_media(c: &mut Criterion) {
    let mut group = c.benchmark_group("media");
    group.sample_size(10);

    group.bench_function("load_media", |b| {
        b.iter(|| {
            let media = lifecycle_sim::media::load_media();
            assert!(!media.is_empty());
        });
    });

    group.bench_function("generate_dataset_with_media", |b| {
        let config = ScalePreset::Quick.config();
        b.iter(|| {
            let dataset = lifecycle_sim::dataset::generate_dataset(config.clone());
            let media_count = dataset.turns.iter().filter(|t| t.media.is_some()).count();
            assert!(media_count > 0, "should have media attachments");
        });
    });

    group.finish();
}

/// Benchmark synthesis trigger and status check.
fn bench_synthesis(c: &mut Criterion) {
    let mut group = c.benchmark_group("synthesis");
    group.sample_size(10);

    group.bench_function("trigger_synthesis", |b| {
        b.iter(|| {
            use lifecycle_sim::drivers::LifecycleDriver;
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("bench_synth.db");
            let mut driver = lifecycle_sim::drivers::rust_native::RustNativeDriver::new(db_path);
            let scope = evidence_store::ScopeId::new_v4();
            let result = driver.trigger_synthesis(scope).unwrap();
            assert!(!result.window_id.is_empty());
        });
    });

    group.finish();
}

/// Benchmark cryptographic forgetting lifecycle.
fn bench_forget(c: &mut Criterion) {
    let mut group = c.benchmark_group("forget");
    group.sample_size(10);

    group.bench_function("forget_scope", |b| {
        b.iter(|| {
            use lifecycle_sim::drivers::LifecycleDriver;
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("bench_forget.db");
            let mut driver = lifecycle_sim::drivers::rust_native::RustNativeDriver::new(db_path);
            let scope = evidence_store::ScopeId::new_v4();
            driver.ingest(scope, b"test data", "bench", evidence_store::ImportanceClass::Important).unwrap();
            driver.forget_scope(scope).unwrap();
            let tombstones = driver.load_forgotten_scopes().unwrap();
            assert!(tombstones.contains(&scope));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_ingest_throughput,
    bench_dataset_generation,
    bench_multilingual,
    bench_media,
    bench_synthesis,
    bench_forget
);
criterion_main!(benches);
