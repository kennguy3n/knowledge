//! `bench_fts_query_at_scale` — FTS5 query latency at 100K rows.
//!
//! Ingests 100K mixed-language messages spread across 50 scopes
//! (2 000 rows/scope), then measures `search_fts` latency for the
//! four query shapes the substrate's retrieval surface issues:
//!
//! * **exact** — a single selective term (`migration`).
//! * **phrase** — an adjacent-token phrase (`"team decided"`).
//! * **boolean AND** — two terms joined (`team AND migration`).
//! * **prefix wildcard** — a stemmed prefix (`migrat*`).
//!
//! Criterion's reported median is the p50; the per-query sample
//! distribution in `target/criterion/.../estimates.json` carries the
//! tail used for p99.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_fts_query_at_scale
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;
use tempfile::TempDir;

use benchmarks::{importance_for, realistic_messages};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

const MASTER_KEY: [u8; 32] = [0xA5; 32];
const CORPUS_SIZE: usize = 100_000;
const SCOPE_COUNT: usize = 50;
const SEARCH_LIMIT: usize = 20;

/// The four query shapes, as `(label, fts5_query)` pairs. The
/// queries are pinned against tokens the deterministic corpus is
/// guaranteed to contain.
const QUERIES: &[(&str, &str)] = &[
    ("exact", "migration"),
    ("phrase", "\"team decided\""),
    ("boolean_and", "team AND migration"),
    ("prefix_wildcard", "migrat*"),
];

/// Build a 100K-row store across `SCOPE_COUNT` scopes once. Returned
/// `TempDir` keeps the backing file alive for the bench's lifetime.
fn build_corpus() -> (TempDir, EvidenceStore, Vec<ScopeId>) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("fts_bench.db");
    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open evidence store");
    let scopes: Vec<ScopeId> = (0..SCOPE_COUNT).map(|_| ScopeId::new_v4()).collect();
    let messages = realistic_messages(CORPUS_SIZE);
    for (i, msg) in messages.iter().enumerate() {
        store
            .ingest(
                scopes[i % SCOPE_COUNT],
                msg.as_bytes(),
                Some("bench:fts"),
                importance_for(i),
            )
            .expect("ingest");
    }
    (dir, store, scopes)
}

fn bench_fts_query_at_scale(c: &mut Criterion) {
    let (_dir, store, scopes) = build_corpus();
    // Query the first scope (2 000 rows of the 100K corpus).
    let scope = scopes[0];

    let mut group = c.benchmark_group("fts/query_at_scale");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    for &(label, query) in QUERIES {
        group.bench_with_input(BenchmarkId::from_parameter(label), &query, |b, query| {
            b.iter(|| {
                let hits = store
                    .search_fts(black_box(scope), black_box(query), SEARCH_LIMIT)
                    .expect("search_fts");
                black_box(hits.len());
            });
        });
    }
    group.finish();
}

criterion_group!(fts_benches, bench_fts_query_at_scale);
criterion_main!(fts_benches);
