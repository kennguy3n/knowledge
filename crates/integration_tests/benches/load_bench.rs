//! Multi-scope realistic-device load benchmark.
//!
//! Simulates the steady-state read/write mix on a power-user device:
//!
//! * One `EvidenceStore` (SQLCipher single-writer)
//! * `SCOPE_COUNT` scopes (≈ one per active channel/domain)
//! * `EVIDENCE_PER_SCOPE` ingested rows per scope (mixed importance)
//! * A burst of FTS queries spread across every scope
//!
//! The bench reports:
//!
//! * `ingest_throughput` — wall time to ingest every row, divided by
//!   `SCOPE_COUNT * EVIDENCE_PER_SCOPE`. Criterion's stats give us
//!   p50/p95 throughput across iterations.
//! * `fts_query_latency` — wall time per FTS hit across every scope.
//!   Criterion's histogram surfaces the p50/p99 latency.
//!
//! Single-threaded by design: SQLite is single-writer, and the
//! evidence store does not hold a connection pool. Concurrency
//! testing belongs in a separate harness that drives multiple
//! processes against independent databases.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p integration_tests --bench load_bench
//! cargo bench -p integration_tests --bench load_bench -- ingest
//! cargo bench -p integration_tests --bench load_bench -- fts
//! ```

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use tempfile::TempDir;

use evidence_store::{EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId};

/// Bytes of the master key used by every bench iteration.
const MASTER_KEY: [u8; 32] = [0xA7; 32];

/// Number of scopes opened per iteration. 50 scopes is a realistic
/// upper bound for a power-user device — one per active channel
/// plus a handful of domains and tenant scopes.
const SCOPE_COUNT: usize = 50;
/// Evidence rows per scope per iteration. 100 rows per scope, 50
/// scopes = 5_000 rows per iteration — about a day's worth of
/// activity for a heavy user.
const EVIDENCE_PER_SCOPE: usize = 100;

/// Total evidence rows ingested per iteration.
const TOTAL_EVIDENCE: usize = SCOPE_COUNT * EVIDENCE_PER_SCOPE;

/// Per-row payload size. 256 B is below the default inline
/// threshold, so every row takes the inline path — same as a
/// typical short evidence snippet (chat message, email subject,
/// summary header).
const BODY_SIZE: usize = 256;

/// FTS query string. Every body embeds the keyword once per
/// `EVIDENCE_PER_SCOPE` rows so the index has guaranteed hits.
const SHARED_KEYWORD: &str = "loadbench-keyword";

fn body_for(scope_idx: usize, evidence_idx: usize) -> Vec<u8> {
    // Each row has a unique token (for selectivity) and the shared
    // keyword (so the FTS bench has guaranteed hits per scope).
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

/// Build a fresh store with `SCOPE_COUNT` scopes, each holding
/// `EVIDENCE_PER_SCOPE` ingested rows. Returns the temp dir
/// (must outlive the store so it is not dropped early), the store,
/// and the vector of scope ids in insertion order.
fn build_loaded_store() -> (TempDir, EvidenceStore, Vec<ScopeId>) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open evidence store");
    let mut scopes = Vec::with_capacity(SCOPE_COUNT);
    for s in 0..SCOPE_COUNT {
        let scope = ScopeId::new_v4();
        for e in 0..EVIDENCE_PER_SCOPE {
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
    let mut group = c.benchmark_group("integration/load/ingest");
    group.throughput(Throughput::Elements(TOTAL_EVIDENCE as u64));
    // The full multi-scope ingest is expensive (every row hits
    // SQLCipher's AEAD); 10 samples keeps the bench wall time
    // bounded without breaking Criterion's variance estimator.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    group.bench_function("multi_scope_5000_rows", |b| {
        b.iter_with_setup(
            || {
                let dir = TempDir::new().expect("tempdir");
                let path = dir.path().join("evidence.db");
                let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
                    .expect("open evidence store");
                let scopes: Vec<ScopeId> = (0..SCOPE_COUNT).map(|_| ScopeId::new_v4()).collect();
                (dir, store, scopes)
            },
            |(_dir, mut store, scopes)| {
                for (s, scope) in scopes.iter().enumerate() {
                    for e in 0..EVIDENCE_PER_SCOPE {
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
    group.finish();
}

fn bench_fts_query_latency(c: &mut Criterion) {
    let (_dir, store, scopes) = build_loaded_store();

    let mut group = c.benchmark_group("integration/load/fts");
    // One iteration = one FTS query per scope. The throughput
    // counter reports queries-per-second; Criterion's per-iteration
    // distribution gives us p50/p99 query latency.
    group.throughput(Throughput::Elements(SCOPE_COUNT as u64));
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(15));
    group.bench_function("shared_keyword_per_scope", |b| {
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
    group.finish();
}

criterion_group!(
    load_benches,
    bench_ingest_throughput,
    bench_fts_query_latency
);
criterion_main!(load_benches);
