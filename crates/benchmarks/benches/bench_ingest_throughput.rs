//! `bench_ingest_throughput` — evidence-store ingest at scale.
//!
//! Ingests a deterministic corpus of realistic, mixed-language /
//! mixed-importance messages into a fresh encrypted SQLCipher store
//! and reports:
//!
//! * **msgs/sec** — the `ingest/throughput_100k` group ingests the
//!   full 100K-message corpus per iteration with
//!   `Throughput::Elements(100_000)`, so Criterion prints elements
//!   per second.
//! * **per-ingest latency** — the `ingest/single_message` group
//!   ingests one message into a fresh store per iteration; the
//!   reported median is the p50 and Criterion's sample distribution
//!   (in `target/criterion/.../estimates.json`) carries the tail
//!   used for p99.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_ingest_throughput
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use tempfile::TempDir;

use benchmarks::{importance_for, realistic_messages};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// Corpus size for the throughput sweep.
const CORPUS_SIZE: usize = 100_000;

fn fresh_store() -> (TempDir, EvidenceStore) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("ingest_bench.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open evidence store");
    (dir, store)
}

fn bench_ingest_throughput_100k(c: &mut Criterion) {
    // Build the corpus once; the measured loop only pays the ingest
    // cost, not the string-allocation cost.
    let messages = realistic_messages(CORPUS_SIZE);
    let scope = ScopeId::new_v4();

    let mut group = c.benchmark_group("ingest/throughput_100k");
    group.throughput(Throughput::Elements(CORPUS_SIZE as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(40));

    group.bench_function("mixed_languages_importance", |b| {
        b.iter_with_setup(fresh_store, |(_dir, mut store)| {
            for (i, msg) in messages.iter().enumerate() {
                let res = store
                    .ingest(
                        scope,
                        msg.as_bytes(),
                        Some("bench:ingest"),
                        importance_for(i),
                    )
                    .expect("ingest");
                black_box(res.evidence_id);
            }
        });
    });
    group.finish();
}

fn bench_ingest_single_message(c: &mut Criterion) {
    let scope = ScopeId::new_v4();
    let body = realistic_messages(1).pop().expect("one message");

    let mut group = c.benchmark_group("ingest/single_message");
    group.throughput(Throughput::Elements(1));
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(15));

    // One fresh store per iteration so each ingest hits an empty
    // table — the per-ingest latency is free of monotonic FTS bloat.
    group.bench_function("important_into_fresh_store", |b| {
        b.iter_with_setup(fresh_store, |(_dir, mut store)| {
            let res = store
                .ingest(
                    black_box(scope),
                    black_box(body.as_bytes()),
                    Some("bench:ingest-single"),
                    importance_for(1),
                )
                .expect("ingest");
            black_box(res.evidence_id);
        });
    });
    group.finish();
}

criterion_group!(
    ingest_benches,
    bench_ingest_throughput_100k,
    bench_ingest_single_message,
);
criterion_main!(ingest_benches);
