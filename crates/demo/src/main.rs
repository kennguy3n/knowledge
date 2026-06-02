//! End-to-end demo binary for the Knowledge substrate.
//!
//! Drives all twelve plane subsystems — evidence ingestion, observation
//! extraction, memory management, concept graph, synthesis pipeline,
//! permissions, crypto, export, agent contract, reasoning, connectors,
//! and audit — over a realistic synthetic dataset and writes a markdown
//! report to `results/demo_results.md`.
//!
//! Run from the workspace root with `cargo run --bin demo`.

#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use std::time::Instant;

mod assertions;
mod dataset;
mod report;
mod stages;

use crate::assertions::AssertionLog;
use crate::report::DemoReport;
use crate::stages::runtime::RuntimeState;

fn main() -> std::io::Result<()> {
    let started = Instant::now();
    let mut report = DemoReport::new();
    let mut log = AssertionLog::new();
    let mut state = RuntimeState::new();

    println!("knowledge demo: starting full-pipeline drive…");

    let dataset = dataset::build_dataset();
    report.dataset_size = dataset.messages.len();

    stages::evidence::run(&dataset, &mut state, &mut report, &mut log);
    stages::observation::run(&dataset, &mut state, &mut report, &mut log);
    stages::memory::run(&dataset, &mut state, &mut report, &mut log);
    stages::concept_graph::run(&dataset, &mut state, &mut report, &mut log);
    stages::synthesis::run(&dataset, &mut state, &mut report, &mut log);
    stages::permissions::run(&dataset, &mut state, &mut report, &mut log);
    stages::crypto::run(&dataset, &mut state, &mut report, &mut log);
    stages::export::run(&dataset, &mut state, &mut report, &mut log);
    stages::agent::run(&dataset, &mut state, &mut report, &mut log);
    stages::reasoning::run(&dataset, &mut state, &mut report, &mut log);
    stages::connectors::run(&dataset, &mut state, &mut report, &mut log);
    stages::audit::run(&dataset, &mut state, &mut report, &mut log);

    report.total_wall_clock = started.elapsed();
    report.attach_assertions(&log);

    let out_path = std::env::var("DEMO_RESULTS_PATH")
        .unwrap_or_else(|_| "results/demo_results.md".to_string());
    let parent = std::path::Path::new(&out_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    std::fs::write(&out_path, report.render_markdown())?;

    println!("demo complete in {:?}", report.total_wall_clock);
    println!("results written to {out_path}");
    println!(
        "{} of {} assertions passed",
        log.passed_count(),
        log.total_count()
    );

    if log.has_failures() {
        std::process::exit(1);
    }

    Ok(())
}
