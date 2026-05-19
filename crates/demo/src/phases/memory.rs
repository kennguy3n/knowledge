//! Stage 3 — Memory Manager.
//!
//! Exercises the full memory-manager surface from
//! [`memory_manager`]:
//!
//! * Decay state machine (`Candidate -> Reinforced -> Consolidated ->
//!   Canonical`) via [`memory_manager::MemoryStateMachine`].
//! * Retention scoring + pinning floor via
//!   [`memory_manager::compute_retention_score`].
//! * Decay sweep via [`memory_manager::decay_sweep`] (archives
//!   ancient unreinforced candidates).
//! * [`memory_manager::UserMemoryObject`] CRUD (read / pin / unpin /
//!   forget) plus [`memory_manager::MemoryFilter`] queries.
//! * [`memory_manager::WorkingMemory`] TTL eviction.
//! * [`memory_manager::episodic::EpisodicMemory`] +
//!   [`memory_manager::episodic::SessionDetector`] +
//!   [`memory_manager::episodic::StubSummarizer`] — every observation
//!   from the observation stage is folded into per-session episodic summaries.

use std::time::Instant;

use chrono::{Duration as ChronoDuration, Utc};
use memory_manager::episodic::{
    EpisodicMemory, Observation as EpisodicObservation, SessionDetector, StubSummarizer,
};
use memory_manager::{
    compute_retention_score, MemoryFilter, MemoryObject, MemoryState, MemoryStateMachine,
    SensitivityClass, UserMemoryObject, WorkingMemory,
};
use uuid::Uuid;

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "memory";

pub fn run(
    _dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Stage 3: Memory Manager");

    // -- Build a UserMemoryObject and seed it from the observation batch.
    let scope = state
        .ingested_rows
        .first()
        .map(|r| r.scope_id)
        .expect("evidence stage must have ingested at least one row before memory stage runs");
    let mut user = UserMemoryObject::new(Uuid::new_v4(), scope);

    let bench_started = Instant::now();
    let mut state_machine_ops: u64 = 0;
    for row in &state.ingested_rows {
        let sensitivity = match row.importance {
            evidence_store::ImportanceClass::Critical => SensitivityClass::Critical,
            evidence_store::ImportanceClass::Important => SensitivityClass::Important,
            evidence_store::ImportanceClass::Useful => SensitivityClass::Useful,
            evidence_store::ImportanceClass::Noise => SensitivityClass::Noise,
        };
        let _ = user.add_observation("evidence", &row.body, sensitivity);
        state_machine_ops += 1;
    }
    let seed_count = user.objects.len() as u64;

    // -- Walk a representative subset all the way to Canonical.
    let sm = MemoryStateMachine::new();
    let walk_target = (seed_count / 4).clamp(2, 12);
    let walked_ids: Vec<Uuid> = user
        .objects
        .iter()
        .take(walk_target as usize)
        .map(|o| o.id)
        .collect();
    let mut canonicals: u64 = 0;
    for id in &walked_ids {
        let obj = user
            .objects
            .iter_mut()
            .find(|o| o.id == *id)
            .expect("walked id must exist");
        sm.reinforce(obj).expect("Candidate -> Reinforced");
        sm.consolidate(obj).expect("Reinforced -> Consolidated");
        sm.canonicalize(obj).expect("Consolidated -> Canonical");
        canonicals += 1;
        state_machine_ops += 3;
    }
    state.canonical_memory_count = canonicals;
    state.memory_object_count = seed_count;

    // -- Pin one canonical, verify retention floor.
    let pinned_id = walked_ids.first().copied().expect("at least one walked id");
    user.pin(&pinned_id).expect("pin a canonical");
    let pinned = user.read(&pinned_id).expect("pinned object exists");
    let pinned_score = pinned.retention_score;

    // -- Recompute retention manually for an unpinned candidate to
    //    exercise the public retention API.
    let cand_obj = user
        .objects
        .iter()
        .find(|o| o.state == MemoryState::Candidate)
        .cloned()
        .expect("at least one candidate remains");
    let cand_score = compute_retention_score(&cand_obj, Utc::now()).total;

    // -- Add a synthetic ancient candidate, then run the decay sweep
    //    so we observe at least one Candidate -> Archived transition.
    {
        let mut ancient = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
        ancient.created_at = Utc::now() - ChronoDuration::days(365 * 5);
        ancient.last_accessed_at = ancient.created_at;
        user.insert(ancient);
    }
    let pre_sweep = user.objects.len();
    let sweep_report = user.decay_sweep(Utc::now());
    let post_sweep_archived = user
        .list(MemoryFilter::any().with_state(MemoryState::Archived))
        .len() as u64;

    // -- Forget a non-canonical to exercise the drop branch, and
    //    forget a canonical to exercise the Deleted transition.
    let drop_id = user
        .objects
        .iter()
        .find(|o| o.state == MemoryState::Candidate)
        .map(|o| o.id);
    let mut dropped: u64 = 0;
    if let Some(id) = drop_id {
        user.forget(&id).expect("forget a candidate");
        dropped += 1;
    }
    let canonical_to_forget = walked_ids
        .iter()
        .find(|id| **id != pinned_id)
        .copied()
        .expect("at least two walked ids");
    user.forget(&canonical_to_forget)
        .expect("forget a canonical");

    let canonical_after = user
        .list(MemoryFilter::any().with_state(MemoryState::Canonical))
        .len() as u64;
    let deleted_after = user
        .list(MemoryFilter::any().with_state(MemoryState::Deleted))
        .len() as u64;

    // -- WorkingMemory with TTL eviction.
    let mut working = WorkingMemory::new(8, ChronoDuration::milliseconds(50));
    for (i, row) in state.ingested_rows.iter().take(12).enumerate() {
        working.push_with_default_ttl(row.scope_id, row.body.clone(), 0.1 + (i as f64) * 0.05);
    }
    let working_before_sleep = working.len();
    std::thread::sleep(std::time::Duration::from_millis(75));
    let working_evicted = working.evict_expired();
    let working_after = working.get_context().len();

    // -- EpisodicMemory ingest from Phase-2 observations -> sessions.
    let detector = SessionDetector::default();
    let summarizer = StubSummarizer::new();
    let mut episodic = EpisodicMemory::new(summarizer, detector);

    let mut episodic_inputs: Vec<EpisodicObservation> = state
        .ingested_rows
        .iter()
        .map(|r| EpisodicObservation::new(r.evidence_id, r.scope_id, r.occurred_at, r.body.clone()))
        .collect();
    episodic_inputs.sort_by_key(|o| (o.scope_id, o.occurred_at));
    let episodic_summaries = episodic
        .ingest(&episodic_inputs)
        .expect("episodic ingest does not fail");

    let bench_total = bench_started.elapsed();

    // -- Assertions.
    log.check(
        PHASE,
        "all walked observations reached Canonical",
        canonicals == walked_ids.len() as u64 && canonicals >= 2,
    );
    log.check(
        PHASE,
        "pinning enforces the >= 0.9 retention floor",
        pinned_score >= 0.9,
    );
    log.check(
        PHASE,
        "unpinned candidate score is below pinned floor",
        cand_score < 0.9,
    );
    log.check(
        PHASE,
        "decay sweep archived at least one ancient candidate",
        sweep_report.candidates_archived >= 1,
    );
    log.check(
        PHASE,
        "archived state is reachable via MemoryFilter",
        post_sweep_archived >= 1,
    );
    log.check(
        PHASE,
        "forget(canonical) marks the row Deleted (not removed)",
        deleted_after >= 1,
    );
    log.check(
        PHASE,
        "forget(non-canonical) physically removes the row",
        dropped == 1,
    );
    log.check(
        PHASE,
        "post-walk Canonical count == initial canonicals - canonicals forgotten",
        canonical_after == canonicals - 1,
    );
    log.check(
        PHASE,
        "WorkingMemory evicted entries past TTL",
        working_evicted > 0 && working_after == 0 && working_before_sleep > 0,
    );
    log.check(
        PHASE,
        "EpisodicMemory produced at least one summary",
        !episodic_summaries.is_empty(),
    );
    log.check(
        PHASE,
        "every episodic summary carries non-empty key_observations",
        episodic_summaries
            .iter()
            .all(|s| !s.key_observations.is_empty()),
    );

    phase.timing = started.elapsed();
    phase.stat("seed_objects", seed_count.to_string());
    phase.stat("walked_to_canonical", canonicals.to_string());
    phase.stat("pinned_score", format!("{:.3}", pinned_score));
    phase.stat("candidate_score_unpinned", format!("{:.3}", cand_score));
    phase.stat("decay_swept_objects", sweep_report.scored.to_string());
    phase.stat(
        "decay_archived_candidates",
        sweep_report.candidates_archived.to_string(),
    );
    phase.stat(
        "decay_archived_superseded",
        sweep_report.superseded_archived.to_string(),
    );
    phase.stat("decay_pre_sweep_total", pre_sweep.to_string());
    phase.stat("canonical_after_forget", canonical_after.to_string());
    phase.stat("deleted_after_forget", deleted_after.to_string());
    phase.stat("working_memory_pre_evict", working_before_sleep.to_string());
    phase.stat("working_memory_evicted", working_evicted.to_string());
    phase.stat("working_memory_live_after", working_after.to_string());
    phase.stat("episodic_summaries", episodic_summaries.len().to_string());
    phase.note(
        "MemoryStateMachine + decay_sweep + UserMemoryObject CRUD + WorkingMemory \
         (50ms TTL) + EpisodicMemory(StubSummarizer + SessionDetector::default).",
    );

    report.count("memory_seed_objects", seed_count);
    report.count("memory_canonicals", canonicals);
    report.count(
        "memory_decay_archived",
        sweep_report.candidates_archived as u64,
    );
    report.count("memory_canonical_after_forget", canonical_after);
    report.count("memory_deleted_after_forget", deleted_after);
    report.count("memory_working_evicted", working_evicted as u64);
    report.count("memory_episodic_summaries", episodic_summaries.len() as u64);
    report.add_phase(phase);
    report.add_benchmark("memory_state_machine_ops", state_machine_ops, bench_total);
}
