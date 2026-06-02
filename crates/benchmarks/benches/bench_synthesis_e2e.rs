//! `bench_synthesis_e2e` — channel synthesis end-to-end.
//!
//! Drives the full window-creation → synthesis → encrypted
//! publication chain for a 1 000-message channel scope using the
//! deterministic fallback adapter (`NoOpSynthesizer`). Measured
//! variants:
//!
//! * **window_to_publish_e2e** — open a window, synthesize the
//!   channel recap, and publish (encrypt) the synthesis object. This
//!   is the headline wall-clock figure.
//! * **synthesize_only** — the synthesis step in isolation.
//! * **publish_only** — the AEAD encrypt-and-seal publication step.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_synthesis_e2e
//! ```

use std::time::Duration;

use chrono::Utc;
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

use crypto::{AeadKey, AEAD_KEY_LEN};
use evidence_store::ScopeId;
use synthesis_pipeline::{
    publish_synthesis_object, ImportanceTagClass, NoOpSynthesizer, ObservationRow,
    ObservationRowKind, SynthesisInputs, SynthesisPipeline, SynthesisWindow,
    SynthesisWindowManager,
};

/// Number of messages (observation rows) in the synthesized window.
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

/// Build the 1 000-row observation set + recap seed representing a
/// busy channel window.
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

fn bench_synthesis_e2e(c: &mut Criterion) {
    let inputs = build_inputs();
    let synthesizer = NoOpSynthesizer::new();
    let scope = ScopeId::new_v4();
    let key = scope_key();
    let now = Utc::now();
    let start = now - chrono::Duration::hours(1);

    let mut group = c.benchmark_group("synthesis/channel_e2e");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(12));

    group.bench_function("window_to_publish_e2e", |b| {
        b.iter(|| {
            // 1. Open a synthesis window via the window manager.
            let mut manager = SynthesisWindowManager::new();
            let window_id = manager
                .open_window(black_box(scope), start, now)
                .expect("open window");
            let window = manager.get(window_id).expect("window present").clone();
            // 2. Synthesize the channel recap (fallback adapter).
            let obj = synthesizer
                .synthesize(&window, black_box(&inputs))
                .expect("synthesize");
            // 3. Publish — AEAD-seal the synthesis object.
            let encrypted = publish_synthesis_object(&obj, &key).expect("publish");
            black_box(encrypted);
        });
    });

    // Pre-built window for the isolated-step variants.
    let window = SynthesisWindow::new(scope, start, now).expect("window");

    group.bench_function("synthesize_only", |b| {
        b.iter(|| {
            let obj = synthesizer
                .synthesize(black_box(&window), black_box(&inputs))
                .expect("synthesize");
            black_box(obj);
        });
    });

    let obj = synthesizer
        .synthesize(&window, &inputs)
        .expect("synthesize");
    group.bench_function("publish_only", |b| {
        b.iter(|| {
            let encrypted = publish_synthesis_object(black_box(&obj), &key).expect("publish");
            black_box(encrypted);
        });
    });

    group.finish();
}

criterion_group!(synthesis_benches, bench_synthesis_e2e);
criterion_main!(synthesis_benches);
