//! `bench_hybrid_retrieval` — the three retrieval modes.
//!
//! Compares the substrate's retrieval lanes over a 10K-row scope:
//!
//! * **FTS-only** — the lexical FTS5 lane in isolation
//!   (`HybridRetriever::search_fts`).
//! * **semantic-only** — `search_hybrid` with the fan-in weights
//!   pinned to the vector lane and a deterministic
//!   [`benchmarks::MockEmbeddingModel`] plumbed in. (Candidates are
//!   still gathered from FTS — the substrate has no standalone ANN
//!   index — but every candidate is scored purely by cosine
//!   similarity against the query embedding.)
//! * **hybrid** — `search_hybrid` with the default FTS + recency +
//!   semantic fan-in and recency rerank.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_hybrid_retrieval
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use tempfile::TempDir;

use benchmarks::{importance_for, realistic_messages, MockEmbeddingModel};
use evidence_store::retrieval::{HybridRetriever, HybridWeights};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

const MASTER_KEY: [u8; 32] = [0xA5; 32];
const CORPUS_SIZE: usize = 10_000;
const SEARCH_LIMIT: usize = 20;
const QUERY: &str = "migration deadline team launch";

fn build_corpus() -> (TempDir, EvidenceStore, ScopeId) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("hybrid_bench.db");
    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open evidence store");
    let scope = ScopeId::new_v4();
    let messages = realistic_messages(CORPUS_SIZE);
    for (i, msg) in messages.iter().enumerate() {
        store
            .ingest(
                scope,
                msg.as_bytes(),
                Some("bench:hybrid"),
                importance_for(i),
            )
            .expect("ingest");
    }
    (dir, store, scope)
}

fn bench_hybrid_retrieval(c: &mut Criterion) {
    let (_dir, store, scope) = build_corpus();

    let fts_only = HybridRetriever::new(&store);
    let semantic_only = HybridRetriever::new(&store)
        .with_embedding_model(MockEmbeddingModel::default(), "mock-v1")
        .with_weights(HybridWeights {
            fts: 0.0,
            recency: 0.0,
            vector: 1.0,
        });
    let hybrid =
        HybridRetriever::new(&store).with_embedding_model(MockEmbeddingModel::default(), "mock-v1");

    let mut group = c.benchmark_group("retrieval/hybrid");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(12));

    group.bench_function("fts_only", |b| {
        b.iter(|| {
            let hits = fts_only
                .search_fts(black_box(scope), black_box(QUERY), SEARCH_LIMIT)
                .expect("search_fts");
            black_box(hits.len());
        });
    });

    group.bench_function("semantic_only", |b| {
        b.iter(|| {
            let hits = semantic_only
                .search_hybrid(black_box(scope), black_box(QUERY), SEARCH_LIMIT)
                .expect("search_hybrid");
            black_box(hits.len());
        });
    });

    group.bench_function("hybrid_fts_semantic_recency", |b| {
        b.iter(|| {
            let hits = hybrid
                .search_hybrid(black_box(scope), black_box(QUERY), SEARCH_LIMIT)
                .expect("search_hybrid");
            black_box(hits.len());
        });
    });

    group.finish();
}

criterion_group!(hybrid_benches, bench_hybrid_retrieval);
criterion_main!(hybrid_benches);
