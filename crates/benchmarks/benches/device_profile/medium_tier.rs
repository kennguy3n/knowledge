//! `device_profile_medium_tier` — 4–6 GB-class device profile.
//!
//! Models the mid target tier (budget Windows i5 laptop, 8 GB under
//! load; or a 6 GB Android flagship; `DeviceTier::Medium`). At this
//! tier the llama.cpp adapter serves **classification** tasks
//! (`TagImportance`, `ExtractEntities`, `PromoteObservation`) but
//! **synthesis is gated off** (only `DeviceTier::High` admits the
//! synthesis tasks — see
//! `medium_tier_runs_classification_but_not_synthesis_on_slm_adapters`).
//!
//! Measured paths — each classification task dispatched through the
//! router → llama.cpp adapter:
//!
//! * **classify/tag_importance**
//! * **classify/extract_entities**
//! * **classify/promote_observation**
//!
//! plus the storage paths against an evidence store opened in the
//! **Medium** memory profile (bounded 1 MiB SQLCipher page cache,
//! mmap kept enabled — see `evidence_store::MemoryProfile::Medium`),
//! the profile the FFI runtime selects for `DeviceTier::Medium`:
//!
//! * **store/ingest_medium_memory** — ingest a deterministic corpus.
//! * **store/fts_medium_memory** — `search_fts` against that store.
//!
//! ## Provenance / what this measures
//!
//! The llama.cpp transport is the in-process `MockLlamaServerClient`,
//! **not** a real `llama-server`. These numbers therefore capture the
//! router dispatch + adapter plumbing + latency-instrumentation
//! overhead (the `knowledge_slm_dispatch_duration_seconds` recording
//! path) with the model replaced by a constant-time canned response —
//! they are **measured-in-CI**. The real model-inference latency
//! (prompt eval + token generation on the device's CPU/GPU) is
//! **to-be-measured-on-device** and is reported separately in
//! docs/technical/benchmarks.md "SLM latency".
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench device_profile_medium_tier
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;

use criterion::Throughput;
use tempfile::TempDir;

use benchmarks::{importance_for, realistic_messages};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, MemoryProfile, ScopeId};
use inference_router::adapters::llama_cpp::MockLlamaServerClient;
use inference_router::adapters::mlx::MlxAdapter;
use inference_router::{
    DeviceTier, FallbackAdapter, InferenceRouter, InferenceTask, LlamaCppAdapter, RouterConfig,
};

const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// Corpus size for the Medium-tier store sweeps. Larger than the
/// Low-tier corpus (a 4 GiB device has a bigger working set) but still
/// bounded so the 1 MiB page cache is the lever under test rather than
/// raw corpus size.
const CORPUS_SIZE: usize = 20_000;
const SEARCH_LIMIT: usize = 20;

/// Open a fresh evidence store in the Medium memory profile (1 MiB
/// page cache, mmap kept enabled).
fn medium_memory_store() -> (TempDir, EvidenceStore) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("medium_tier.db");
    let cfg = EvidenceStoreConfig {
        memory_profile: MemoryProfile::Medium,
        ..Default::default()
    };
    let store = EvidenceStore::open(&path, &MASTER_KEY, cfg).expect("open medium-memory store");
    (dir, store)
}

/// Build a populated Medium-memory store for the FTS sweep.
fn build_corpus() -> (TempDir, EvidenceStore, ScopeId) {
    let (dir, mut store) = medium_memory_store();
    let scope = ScopeId::new_v4();
    for (i, msg) in realistic_messages(CORPUS_SIZE).iter().enumerate() {
        store
            .ingest(
                scope,
                msg.as_bytes(),
                Some("bench:medium"),
                importance_for(i),
            )
            .expect("ingest");
    }
    (dir, store, scope)
}

fn bench_medium_tier_store(c: &mut Criterion) {
    let messages = realistic_messages(CORPUS_SIZE);
    let scope = ScopeId::new_v4();

    let mut ingest = c.benchmark_group("medium_tier/store");
    ingest.throughput(Throughput::Elements(CORPUS_SIZE as u64));
    ingest.sample_size(10);
    ingest.measurement_time(Duration::from_secs(20));
    ingest.bench_function("ingest_medium_memory", |b| {
        b.iter_with_setup(medium_memory_store, |(_dir, mut store)| {
            for (i, msg) in messages.iter().enumerate() {
                let res = store
                    .ingest(
                        scope,
                        msg.as_bytes(),
                        Some("bench:medium"),
                        importance_for(i),
                    )
                    .expect("ingest");
                black_box(res.evidence_id);
            }
        });
    });
    ingest.finish();

    let (_dir, store, scope) = build_corpus();
    let mut fts = c.benchmark_group("medium_tier/fts");
    fts.sample_size(100);
    fts.measurement_time(Duration::from_secs(10));
    fts.bench_function("fts_medium_memory", |b| {
        b.iter(|| {
            let hits = store
                .search_fts(black_box(scope), black_box("migration"), SEARCH_LIMIT)
                .expect("search_fts");
            black_box(hits.len());
        });
    });
    fts.finish();
}

/// The three classification tasks served at the Medium tier, each with
/// a representative body the substrate would feed the classifier.
const CLASSIFY_TASKS: &[(&str, InferenceTask, &str)] = &[
    (
        "tag_importance",
        InferenceTask::TagImportance,
        "management approved the new vendor contract and ratified the budget marker-1",
    ),
    (
        "extract_entities",
        InferenceTask::ExtractEntities,
        "the migration is scheduled for Monday and will require two hours of downtime marker-2",
    ),
    (
        "promote_observation",
        InferenceTask::PromoteObservation,
        "the team decided to postpone the launch until Q2 marker-3",
    ),
];

/// Build a Medium-tier router whose llama.cpp adapter is backed by the
/// in-process mock transport (reachable, constant-time response). MLX
/// is present but unavailable off Apple Silicon; the fallback backstops
/// the ladder. `bootstrap()` runs the (synchronous, in-process) probe.
fn medium_tier_router() -> InferenceRouter {
    let cfg = RouterConfig::default().with_device_tier(DeviceTier::Medium);
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
    let llama = LlamaCppAdapter::new(
        cfg.clone(),
        Box::new(MockLlamaServerClient::ok(
            r#"{"class":"important","confidence":0.82}"#,
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

fn bench_medium_tier_classify(c: &mut Criterion) {
    let router = medium_tier_router();

    let mut group = c.benchmark_group("medium_tier/classify");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    for &(label, task, body) in CLASSIFY_TASKS {
        group.bench_with_input(BenchmarkId::from_parameter(label), &body, |b, body| {
            b.iter(|| {
                let out = router
                    .dispatch(black_box(task), black_box(*body))
                    .expect("dispatch");
                black_box(out);
            });
        });
    }
    group.finish();
}

criterion_group!(
    medium_tier_benches,
    bench_medium_tier_classify,
    bench_medium_tier_store
);
criterion_main!(medium_tier_benches);
