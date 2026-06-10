//! `device_profile_low_tier` — constrained 2 GB-class device profile.
//!
//! Models the cheapest target tier (budget Android, e.g. Redmi Note 12
//! 4 GB running under memory pressure; `DeviceTier::Low`). At this tier
//! the substrate runs **encoder-only**: the on-device SLM adapters
//! (MLX, llama.cpp) are gated off (see `low_tier_blocks_slm_adapters`),
//! so classification resolves through the `FallbackAdapter`, and the
//! evidence store opens in **low-memory mode** (bounded 512 KiB
//! SQLCipher page cache, mmap disabled — see
//! `evidence_store::MemoryProfile::Low`).
//!
//! Measured paths:
//!
//! * **ingest/low_memory** — ingest a deterministic corpus into a
//!   low-memory store; reported as rows/sec.
//! * **fts/low_memory** — `search_fts` latency against that store with
//!   the bounded page cache (the memory/throughput trade the tier
//!   makes shows up here as extra page faults).
//! * **decay/sweep** — a full retention `decay_sweep`, the periodic
//!   maintenance pass that must stay cheap on a constrained device.
//! * **classify/fallback** — encoder-only importance classification
//!   through the gated three-adapter ladder (resolves via fallback).
//!
//! All four run fully in-process with no network and no model file, so
//! the numbers are measured-in-CI on the host running `cargo bench`.
//! See docs/technical/benchmarks.md "Device profiles" for provenance.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench device_profile_low_tier
//! ```

use std::time::Duration;

use chrono::Utc;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use tempfile::TempDir;

use benchmarks::{importance_for, realistic_messages};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, MemoryProfile, ScopeId};
use inference_router::adapters::llama_cpp::MockLlamaServerClient;
use inference_router::adapters::mlx::MlxAdapter;
use inference_router::{
    DeviceTier, FallbackAdapter, InferenceRouter, InferenceTask, LlamaCppAdapter, RouterConfig,
};
use memory_manager::{decay_sweep, MemoryObject, SensitivityClass};

const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// Corpus size for the low-tier ingest / FTS sweeps. Deliberately
/// smaller than the 100K hot-path benches: a 2 GB device handles a
/// bounded working set, and the bounded page cache makes a 100K build
/// dominated by page-fault noise rather than the per-row cost.
const CORPUS_SIZE: usize = 10_000;
const SEARCH_LIMIT: usize = 20;

/// Decay-sweep object count — the periodic maintenance pass on a
/// constrained device still runs over the resident object set.
const DECAY_ROWS: usize = 10_000;

/// Open a fresh evidence store in low-memory mode (the Low-tier
/// profile: 512 KiB page cache, mmap disabled).
fn low_memory_store() -> (TempDir, EvidenceStore) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("low_tier.db");
    let cfg = EvidenceStoreConfig {
        memory_profile: MemoryProfile::Low,
        ..Default::default()
    };
    let store = EvidenceStore::open(&path, &MASTER_KEY, cfg).expect("open low-memory store");
    (dir, store)
}

/// Build a populated low-memory store for the FTS sweep.
fn build_corpus() -> (TempDir, EvidenceStore, ScopeId) {
    let (dir, mut store) = low_memory_store();
    let scope = ScopeId::new_v4();
    for (i, msg) in realistic_messages(CORPUS_SIZE).iter().enumerate() {
        store
            .ingest(scope, msg.as_bytes(), Some("bench:low"), importance_for(i))
            .expect("ingest");
    }
    (dir, store, scope)
}

fn bench_low_tier_ingest(c: &mut Criterion) {
    let messages = realistic_messages(CORPUS_SIZE);
    let scope = ScopeId::new_v4();

    let mut group = c.benchmark_group("low_tier/ingest");
    group.throughput(Throughput::Elements(CORPUS_SIZE as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("encoder_only_low_memory", |b| {
        b.iter_with_setup(low_memory_store, |(_dir, mut store)| {
            for (i, msg) in messages.iter().enumerate() {
                let res = store
                    .ingest(scope, msg.as_bytes(), Some("bench:low"), importance_for(i))
                    .expect("ingest");
                black_box(res.evidence_id);
            }
        });
    });
    group.finish();
}

fn bench_low_tier_fts(c: &mut Criterion) {
    let (_dir, store, scope) = build_corpus();

    let mut group = c.benchmark_group("low_tier/fts");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("query_low_memory", |b| {
        b.iter(|| {
            let hits = store
                .search_fts(black_box(scope), black_box("migration"), SEARCH_LIMIT)
                .expect("search_fts");
            black_box(hits.len());
        });
    });
    group.finish();
}

fn sensitivity(index: usize) -> SensitivityClass {
    match index % 4 {
        0 => SensitivityClass::Critical,
        1 => SensitivityClass::Important,
        2 => SensitivityClass::Useful,
        _ => SensitivityClass::Noise,
    }
}

fn build_decay_objects(n: usize) -> Vec<MemoryObject> {
    let now = Utc::now();
    let scope = ScopeId::new_v4();
    (0..n)
        .map(|i| {
            let mut obj = MemoryObject::new_candidate(scope, sensitivity(i));
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

fn bench_low_tier_decay(c: &mut Criterion) {
    let base = build_decay_objects(DECAY_ROWS);
    let now = Utc::now();

    let mut group = c.benchmark_group("low_tier/decay");
    group.throughput(Throughput::Elements(DECAY_ROWS as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    group.bench_function("full_sweep", |b| {
        b.iter_with_setup(
            || base.clone(),
            |mut objects| {
                let report = decay_sweep(black_box(&mut objects), now);
                black_box(report.scored);
            },
        );
    });
    group.finish();
}

/// Build a Low-tier router with the full three-adapter ladder. The
/// tier gating disables MLX + llama.cpp at probe time (even though the
/// mock server is reachable), so classification resolves through the
/// always-available `FallbackAdapter` — exactly the production
/// Low-tier path.
fn low_tier_router() -> InferenceRouter {
    let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), true);
    let llama = LlamaCppAdapter::new(
        cfg.clone(),
        Box::new(MockLlamaServerClient::ok(
            r#"{"class":"useful","confidence":0.7}"#,
        )),
    );
    let fallback = FallbackAdapter::new();
    let router = InferenceRouter::new(
        cfg,
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();
    router
}

fn bench_low_tier_classify(c: &mut Criterion) {
    let router = low_tier_router();
    // A body containing a 'useful'-class token so the encoder-only
    // fallback classifier does real lexicon scoring.
    let body = "please review the security report before friday marker-1";

    let mut group = c.benchmark_group("low_tier/classify");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("tag_importance_fallback", |b| {
        b.iter(|| {
            let out = router
                .dispatch(black_box(InferenceTask::TagImportance), black_box(body))
                .expect("dispatch");
            black_box(out);
        });
    });
    group.finish();
}

criterion_group!(
    low_tier_benches,
    bench_low_tier_ingest,
    bench_low_tier_fts,
    bench_low_tier_decay,
    bench_low_tier_classify,
);
criterion_main!(low_tier_benches);
