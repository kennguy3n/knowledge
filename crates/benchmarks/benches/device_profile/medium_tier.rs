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

use inference_router::adapters::llama_cpp::MockLlamaServerClient;
use inference_router::adapters::mlx::MlxAdapter;
use inference_router::{
    DeviceTier, FallbackAdapter, InferenceRouter, InferenceTask, LlamaCppAdapter, RouterConfig,
};

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

criterion_group!(medium_tier_benches, bench_medium_tier_classify);
criterion_main!(medium_tier_benches);
