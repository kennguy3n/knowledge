//! Observation extraction throughput benchmark.
//!
//! Measures observations/sec on a representative workload similar
//! to the golden dataset used in the quality evaluation tests.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p integration_tests --bench observation_throughput
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use evidence_store::ScopeId;
use observation_engine::{LexiconExtractor, ObservationExtractor};

/// Representative inputs covering the same categories as the golden
/// dataset: English business, multilingual, edge cases.
const BENCH_INPUTS: &[&str] = &[
    // English business
    "TODO: Send the meeting notes to the team by end of day.",
    "The team decided to postpone the launch until Q2.",
    "@Alice please review the security audit report before Friday.",
    "The migration is scheduled for next Monday and will require two hours of downtime.",
    "What is the status of the API refactoring work?",
    "Management approved the new vendor contract yesterday.",
    "We agreed to use Kubernetes for the deployment. ACTION: @Bob set up the cluster by Thursday.",
    "The frontend team shipped the new dashboard component last sprint.",
    "The board ratified the 2024 budget proposal unanimously.",
    "Can someone confirm whether the staging environment is up?",
    // Multilingual
    "今日の会議は何時に始まりますか？",
    "Por favor revise el informe antes del viernes.",
    "L'équipe a décidé de reporter le lancement au prochain trimestre.",
    "Die neue Software wird am Montag bereitgestellt und alle Abteilungen müssen aktualisieren.",
    "متى سيتم إطلاق المنتج الجديد في السوق العربي؟",
    // Edge cases
    "Check https://github.com/org/repo/pull/123 and https://jira.example.com/PROJ-456 for context.",
    "URGENT: UPDATE THE FIREWALL RULES BEFORE THE AUDIT ON FRIDAY.",
    "The Q3 roadmap was approved by the VP of Engineering last Tuesday. \
     @Dana please prepare the sprint planning document. \
     The infrastructure team reported that latency dropped by 40% after the optimization. \
     How should we handle the remaining technical debt?",
    "The server processed 1.2 million requests yesterday with 99.99% uptime.",
    "When is the deadline? Who is responsible for the deliverable?",
];

fn bench_extraction_throughput(c: &mut Criterion) {
    let extractor = LexiconExtractor::default();
    let scope = ScopeId::new_v4();

    let mut group = c.benchmark_group("observation_extraction");
    group.throughput(Throughput::Elements(BENCH_INPUTS.len() as u64));

    group.bench_function("extract_all_inputs", |b| {
        b.iter(|| {
            for input in BENCH_INPUTS {
                black_box(extractor.extract(black_box(input), scope));
            }
        });
    });

    group.finish();

    // Per-byte throughput variant for comparison.
    let total_bytes: u64 = BENCH_INPUTS.iter().map(|s| s.len() as u64).sum();
    let mut byte_group = c.benchmark_group("observation_extraction_bytes");
    byte_group.throughput(Throughput::Bytes(total_bytes));

    byte_group.bench_function("extract_all_bytes", |b| {
        b.iter(|| {
            for input in BENCH_INPUTS {
                black_box(extractor.extract(black_box(input), scope));
            }
        });
    });

    byte_group.finish();
}

criterion_group!(benches, bench_extraction_throughput);
criterion_main!(benches);
