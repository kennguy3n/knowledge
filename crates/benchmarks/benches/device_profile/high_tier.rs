//! `device_profile_high_tier` — 8 GB+ device profile, full synthesis.
//!
//! Models the top target tier (M2 MacBook Air 8 GB; a desktop; or the
//! `high`-pinned server deployment — `DeviceTier::High`). This is the
//! only tier that admits the **synthesis** tasks (`SynthSummary`,
//! `SynthConcept`, `AdjudicateContradiction`) on the SLM adapters, so
//! it exercises the full end-to-end synthesis chain plus the router's
//! synthesis-dispatch + latency-instrumentation path.
//!
//! Measured paths:
//!
//! * **synthesis/window_to_publish_e2e** — open a window → synthesize a
//!   1 000-message channel recap → AEAD-publish, via the deterministic
//!   `NoOpSynthesizer` fallback adapter. This is the headline
//!   wall-clock figure and is fully **measured-in-CI** (no model file,
//!   no network).
//! * **synthesis/router_dispatch** — a `SynthSummary` dispatched
//!   through the router → llama.cpp adapter. Like the medium-tier
//!   classification bench, the transport is the in-process
//!   `MockLlamaServerClient`, so this captures the router +
//!   latency-recording overhead, **not** real token-generation
//!   latency. Real on-device synthesis latency (cold model load + multi
//!   hundred-token generation) is **to-be-measured-on-device** — see
//!   docs/technical/benchmarks.md "SLM latency".
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench device_profile_high_tier
//! ```

use std::time::Duration;

use chrono::Utc;
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

use crypto::{AeadKey, AEAD_KEY_LEN};
use evidence_store::ScopeId;
use inference_router::adapters::llama_cpp::MockLlamaServerClient;
use inference_router::adapters::mlx::MlxAdapter;
use inference_router::{
    DeviceTier, FallbackAdapter, InferenceRouter, InferenceTask, LlamaCppAdapter, RouterConfig,
};
use synthesis_pipeline::{
    publish_synthesis_object, ImportanceTagClass, NoOpSynthesizer, ObservationRow,
    ObservationRowKind, SynthesisInputs, SynthesisPipeline, SynthesisWindow,
    SynthesisWindowManager,
};

/// Messages (observation rows) in the synthesized window.
const WINDOW_MESSAGES: usize = 1_000;

fn scope_key() -> AeadKey {
    [0xBB; AEAD_KEY_LEN]
}

fn observation_kind(index: usize) -> ObservationRowKind {
    match index % 6 {
        0 => ObservationRowKind::Entity,
        1 => ObservationRowKind::Fact,
        2 => ObservationRowKind::Task,
        3 => ObservationRowKind::Decision,
        4 => ObservationRowKind::Claim,
        _ => ObservationRowKind::Question,
    }
}

fn importance_tag(index: usize) -> ImportanceTagClass {
    match index % 20 {
        0 => ImportanceTagClass::Critical,
        1..=6 => ImportanceTagClass::Important,
        7..=16 => ImportanceTagClass::Useful,
        _ => ImportanceTagClass::Noise,
    }
}

fn build_inputs() -> SynthesisInputs {
    let messages = benchmarks::realistic_messages(WINDOW_MESSAGES);
    let observations: Vec<ObservationRow> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| ObservationRow {
            kind: observation_kind(i),
            content: m.clone(),
            importance: importance_tag(i),
            confidence: 0.5 + f64::from(u8::try_from(i % 50).unwrap_or(0)) / 100.0,
        })
        .collect();
    SynthesisInputs {
        observations,
        recap_seed: "Channel recap: migration timeline, launch decisions, and task assignments \
                     across a one-thousand-message window."
            .into(),
    }
}

fn bench_high_tier_synthesis_e2e(c: &mut Criterion) {
    let inputs = build_inputs();
    let synthesizer = NoOpSynthesizer::new();
    let scope = ScopeId::new_v4();
    let key = scope_key();
    let now = Utc::now();
    let start = now - chrono::Duration::hours(1);

    let mut group = c.benchmark_group("high_tier/synthesis");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(12));

    group.bench_function("window_to_publish_e2e", |b| {
        b.iter(|| {
            let mut manager = SynthesisWindowManager::new();
            let window_id = manager
                .open_window(black_box(scope), start, now)
                .expect("open window");
            let window = manager.get(window_id).expect("window present").clone();
            let obj = synthesizer
                .synthesize(&window, black_box(&inputs))
                .expect("synthesize");
            let encrypted = publish_synthesis_object(&obj, &key).expect("publish");
            black_box(encrypted);
        });
    });

    let _ = SynthesisWindow::new(scope, start, now).expect("window");
    group.finish();
}

/// High-tier router: llama.cpp adapter (mock transport) admitted for
/// synthesis because the tier is `High`. Returns a canned summary so
/// the measured cost is the router dispatch + latency recording, not
/// model generation.
fn high_tier_router() -> InferenceRouter {
    let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
    let llama = LlamaCppAdapter::new(
        cfg.clone(),
        Box::new(MockLlamaServerClient::ok(
            "Recap: the team postponed the launch to Q2 and assigned the migration to engineering.",
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

fn bench_high_tier_router_dispatch(c: &mut Criterion) {
    let router = high_tier_router();
    let prompt = "Summarize the channel window: migration timeline and launch decisions.";

    let mut group = c.benchmark_group("high_tier/synthesis");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("router_dispatch", |b| {
        b.iter(|| {
            let out = router
                .dispatch(black_box(InferenceTask::SynthSummary), black_box(prompt))
                .expect("dispatch");
            black_box(out);
        });
    });
    group.finish();
}

criterion_group!(
    high_tier_benches,
    bench_high_tier_synthesis_e2e,
    bench_high_tier_router_dispatch,
);
criterion_main!(high_tier_benches);
