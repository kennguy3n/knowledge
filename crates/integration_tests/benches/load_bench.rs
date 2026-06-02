//! Evidence store ingestion throughput & retrieval latency benchmarks.
//!
//! Measures:
//!
//! * **Ingestion throughput**: messages/sec at various corpus sizes
//!   (1K, 10K, 100K rows across multiple scopes).
//! * **FTS retrieval latency**: lexical search at various corpus sizes.
//!
//! Single-threaded by design: SQLite is single-writer, and the
//! evidence store does not hold a connection pool.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p integration_tests --bench load_bench
//! cargo bench -p integration_tests --bench load_bench -- ingest
//! cargo bench -p integration_tests --bench load_bench -- fts
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use tempfile::TempDir;

use evidence_store::{EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId};

/// Bytes of the master key used by every bench iteration.
const MASTER_KEY: [u8; 32] = [0xA7; 32];

/// Per-row payload size. 256 B is below the default inline
/// threshold, so every row takes the inline path.
const BODY_SIZE: usize = 256;

/// FTS query string embedded in every row.
const SHARED_KEYWORD: &str = "loadbench-keyword";

/// Benchmark configurations: (label, scope_count, evidence_per_scope).
const CONFIGS: &[(&str, usize, usize)] = &[
    ("1K_rows_10x100", 10, 100),
    ("10K_rows_50x200", 50, 200),
    ("100K_rows_100x1000", 100, 1000),
];

fn body_for(scope_idx: usize, evidence_idx: usize) -> Vec<u8> {
    let prefix = format!(
        "{SHARED_KEYWORD} scope-{scope_idx} evidence-{evidence_idx} \
         channel-recap commitment migration deadline owner "
    );
    let mut body = prefix.into_bytes();
    if body.len() < BODY_SIZE {
        body.resize(BODY_SIZE, b' ');
    }
    body
}

fn open_store(dir: &TempDir) -> EvidenceStore {
    let path = dir.path().join("evidence.db");
    EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).expect("open store")
}

fn build_loaded_store(
    scope_count: usize,
    evidence_per_scope: usize,
) -> (TempDir, EvidenceStore, Vec<ScopeId>) {
    let dir = TempDir::new().expect("tempdir");
    let mut store = open_store(&dir);
    let mut scopes = Vec::with_capacity(scope_count);
    for s in 0..scope_count {
        let scope = ScopeId::new_v4();
        for e in 0..evidence_per_scope {
            store
                .ingest(
                    scope,
                    &body_for(s, e),
                    Some("bench:load_bench"),
                    ImportanceClass::Useful,
                )
                .expect("ingest");
        }
        scopes.push(scope);
    }
    (dir, store, scopes)
}

fn bench_ingest_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("evidence/ingest");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    for &(label, scope_count, per_scope) in CONFIGS {
        let total = scope_count * per_scope;
        group.throughput(Throughput::Elements(total as u64));
        group.bench_with_input(BenchmarkId::new("throughput", label), &(), |b, _| {
            b.iter_with_setup(
                || {
                    let dir = TempDir::new().expect("tempdir");
                    let store = open_store(&dir);
                    let scopes: Vec<ScopeId> =
                        (0..scope_count).map(|_| ScopeId::new_v4()).collect();
                    (dir, store, scopes)
                },
                |(_dir, mut store, scopes)| {
                    for (s, scope) in scopes.iter().enumerate() {
                        for e in 0..per_scope {
                            store
                                .ingest(
                                    *scope,
                                    &body_for(s, e),
                                    Some("bench:load_bench"),
                                    ImportanceClass::Useful,
                                )
                                .expect("ingest");
                        }
                    }
                    black_box(store);
                },
            );
        });
    }
    group.finish();
}

fn bench_fts_query_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("evidence/fts_retrieval");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(15));

    for &(label, scope_count, per_scope) in CONFIGS {
        // Build the corpus once, then bench repeated queries.
        let (_dir, store, scopes) = build_loaded_store(scope_count, per_scope);
        group.throughput(Throughput::Elements(scope_count as u64));
        group.bench_with_input(BenchmarkId::new("latency", label), &(), |b, _| {
            b.iter(|| {
                let mut total: usize = 0;
                for scope in &scopes {
                    let hits = store
                        .search_fts(*scope, black_box(SHARED_KEYWORD), 16)
                        .expect("search_fts");
                    total = total.saturating_add(hits.len());
                }
                black_box(total);
            });
        });
    }
    group.finish();
}

fn bench_retrieval_recent(c: &mut Criterion) {
    let mut group = c.benchmark_group("evidence/recent_retrieval");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));

    let (_dir, store, scopes) = build_loaded_store(50, 200);
    group.throughput(Throughput::Elements(50));
    group.bench_function("recent_ids_50_scopes", |b| {
        b.iter(|| {
            let mut total: usize = 0;
            for scope in &scopes {
                let ids = store
                    .recent_evidence_ids_for_scope(*scope, 50)
                    .expect("recent_ids");
                total = total.saturating_add(ids.len());
            }
            black_box(total);
        });
    });
    group.finish();
}

criterion_group!(
    load_benches,
    bench_ingest_throughput,
    bench_fts_query_latency,
    bench_retrieval_recent
);
criterion_main!(load_benches);
