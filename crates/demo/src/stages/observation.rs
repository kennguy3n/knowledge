//! Stage 2 — Observation Extraction.
//!
//! Runs the [`observation_engine::LexiconExtractor`] over every
//! evidence row that the evidence stage stored on the inline / body-table paths
//! (ring-buffer rows are noise and intentionally don't extract). Each
//! extracted [`observation_engine::Observation`] is also scored
//! against the channel-scope
//! [`observation_engine::ChannelPromotionPolicy`] using the public
//! [`observation_engine::should_promote`] entry point so the demo
//! exercises the promotion gate, not just the raw extractor.

use std::collections::BTreeMap;
use std::time::Instant;

use evidence_store::{ImportanceClassifier, LexiconClassifier};
use observation_engine::{
    should_promote, ChannelPromotionPolicy, LexiconExtractor, ObservationExtractor,
    ObservationType, PromotionReason,
};

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::report::{DemoReport, StageReport};
use crate::stages::runtime::RuntimeState;

const STAGE: &str = "observation";

pub fn run(
    _dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut stage = StageReport::new("Stage 2: Observation Extraction");

    let extractor = LexiconExtractor::english_default();
    let classifier = LexiconClassifier::english_default();
    let policy = ChannelPromotionPolicy::default();

    let mut by_type: BTreeMap<&'static str, u64> = BTreeMap::new();
    by_type.insert("entity", 0);
    by_type.insert("fact", 0);
    by_type.insert("task", 0);
    by_type.insert("decision", 0);
    by_type.insert("claim", 0);
    by_type.insert("question", 0);

    let mut total_obs: u64 = 0;
    let mut promoted: u64 = 0;
    let mut rejected_importance: u64 = 0;
    let mut rejected_corroboration: u64 = 0;
    let mut rejected_noise: u64 = 0;
    let mut rows_processed: u64 = 0;
    let mut all_observations = Vec::with_capacity(state.ingested_rows.len() * 4);

    let bench_started = Instant::now();
    for row in &state.ingested_rows {
        rows_processed += 1;
        let observations = extractor.extract(&row.body, row.scope_id);
        let importance = classifier.classify(&row.body);

        let batch_size = observations.len() as f64;
        let batch_noise = if batch_size == 0.0 {
            0.0
        } else {
            // Local "noise ratio" approximation: fraction of pure
            // entity-only observations in this row's batch. Entity-only
            // batches without facts/decisions/tasks are the noisier
            // shape that the policy is meant to filter out.
            let entity_count = observations
                .iter()
                .filter(|o| matches!(o.observation_type, ObservationType::Entity))
                .count() as f64;
            (entity_count / batch_size).clamp(0.0, 1.0)
        };

        for obs in observations {
            total_obs += 1;
            let tag = match obs.observation_type {
                ObservationType::Entity => "entity",
                ObservationType::Fact => "fact",
                ObservationType::Task => "task",
                ObservationType::Decision => "decision",
                ObservationType::Claim => "claim",
                ObservationType::Question => "question",
            };
            *by_type.entry(tag).or_default() += 1;

            // Corroboration count is the number of other ingested rows
            // whose body contains the observation's content as a
            // substring. Cheap O(n*m) — fine for the demo dataset.
            let corroboration = u32::try_from(
                state
                    .ingested_rows
                    .iter()
                    .filter(|other| other.body.contains(obs.content.trim()))
                    .count(),
            )
            .unwrap_or(u32::MAX);

            let decision = should_promote(&obs, importance, corroboration, batch_noise, &policy);
            match decision.reason {
                PromotionReason::Promoted => promoted += 1,
                PromotionReason::BelowImportanceFloor => rejected_importance += 1,
                PromotionReason::InsufficientCorroboration => rejected_corroboration += 1,
                PromotionReason::BatchTooNoisy => rejected_noise += 1,
            }
            all_observations.push(obs);
        }
    }
    let bench_total = bench_started.elapsed();

    log.check(
        STAGE,
        "at least one observation extracted per non-noise row on average",
        rows_processed > 0 && total_obs >= rows_processed,
    );
    log.check(
        STAGE,
        "extractor produced at least one decision",
        by_type.get("decision").copied().unwrap_or(0) > 0,
    );
    log.check(
        STAGE,
        "extractor produced at least one task",
        by_type.get("task").copied().unwrap_or(0) > 0,
    );
    log.check(
        STAGE,
        "extractor produced at least one fact",
        by_type.get("fact").copied().unwrap_or(0) > 0,
    );
    log.check(
        STAGE,
        "extractor produced at least one entity",
        by_type.get("entity").copied().unwrap_or(0) > 0,
    );
    log.check(
        STAGE,
        "promotion gate accepted at least one observation",
        promoted > 0,
    );
    log.check(
        STAGE,
        "promotion gate rejected below-importance observations",
        rejected_importance > 0,
    );
    log.check(
        STAGE,
        "promoted + rejected == total observations",
        promoted + rejected_importance + rejected_corroboration + rejected_noise == total_obs,
    );

    stage.timing = started.elapsed();
    stage.stat("rows_processed", rows_processed.to_string());
    stage.stat("total_observations", total_obs.to_string());
    stage.stat("promoted", promoted.to_string());
    stage.stat("rejected_below_importance", rejected_importance.to_string());
    stage.stat(
        "rejected_insufficient_corroboration",
        rejected_corroboration.to_string(),
    );
    stage.stat("rejected_batch_too_noisy", rejected_noise.to_string());
    for (k, v) in &by_type {
        stage.stat(format!("type_{k}"), v.to_string());
    }
    stage.note(
        "LexiconExtractor (english_default) -> ChannelPromotionPolicy::default; \
         corroboration scored against the full ingested batch."
            .to_string(),
    );

    report.count("observations_total", total_obs);
    report.count("observations_promoted", promoted);
    report.count("observations_rejected_importance", rejected_importance);
    report.count(
        "observations_rejected_corroboration",
        rejected_corroboration,
    );
    report.count("observations_rejected_noise", rejected_noise);
    for (k, v) in &by_type {
        report.count(format!("observations_type_{k}"), *v);
    }
    report.add_stage(stage);
    report.add_benchmark("observation_extract_per_row", rows_processed, bench_total);

    state.observations = all_observations;
}
