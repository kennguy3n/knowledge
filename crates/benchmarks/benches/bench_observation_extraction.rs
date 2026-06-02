//! `bench_observation_extraction` — `default_pipeline` throughput.
//!
//! Runs the observation-engine `default_pipeline` (Lexicon extractor
//! and lexicon classifier, with language detection) over 10K
//! mixed-language messages and reports:
//!
//! * **observations/sec** — the `observation/pipeline_10k` group runs
//!   the whole 10K corpus per iteration with
//!   `Throughput::Elements(10_000)`.
//! * **per-language latency** — the `observation/by_language` group
//!   times the pipeline over each language bucket separately so the
//!   per-message cost can be compared across scripts.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_observation_extraction
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use benchmarks::{messages_by_language, realistic_messages};
use evidence_store::ScopeId;
use observation_engine::default_pipeline;

const CORPUS_SIZE: usize = 10_000;

fn bench_observation_extraction(c: &mut Criterion) {
    let pipeline = default_pipeline();
    let scope = ScopeId::new_v4();
    let messages = realistic_messages(CORPUS_SIZE);

    let mut group = c.benchmark_group("observation/pipeline_10k");
    group.throughput(Throughput::Elements(CORPUS_SIZE as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    group.bench_function("mixed_language", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for msg in &messages {
                let obs = pipeline.run(black_box(msg), scope).expect("pipeline run");
                total += obs.len();
            }
            black_box(total);
        });
    });
    group.finish();

    // Per-language latency: time the pipeline over each language
    // bucket so the per-message cost can be compared across scripts.
    let buckets = messages_by_language(CORPUS_SIZE);
    let mut lang_group = c.benchmark_group("observation/by_language");
    lang_group.sample_size(10);
    lang_group.measurement_time(Duration::from_secs(10));
    for (label, msgs) in &buckets {
        lang_group.throughput(Throughput::Elements(msgs.len() as u64));
        lang_group.bench_with_input(BenchmarkId::from_parameter(label), msgs, |b, msgs| {
            b.iter(|| {
                let mut total = 0usize;
                for msg in msgs {
                    let obs = pipeline.run(black_box(msg), scope).expect("pipeline run");
                    total += obs.len();
                }
                black_box(total);
            });
        });
    }
    lang_group.finish();
}

criterion_group!(observation_benches, bench_observation_extraction);
criterion_main!(observation_benches);
