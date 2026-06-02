//! End-to-end pipeline tests: raw text in -> Candidate observations
//! out, with the right [`ObservationType`] tags.

use evidence_store::ScopeId;
use memory_manager::MemoryState;
use observation_engine::{pipeline::default_pipeline, ObservationError, ObservationType};

#[test]
fn pipeline_rejects_empty_input() {
    let pipeline = default_pipeline();
    let res = pipeline.run("    ", ScopeId::new_v4());
    assert!(matches!(res, Err(ObservationError::EmptyInput)));
}

#[test]
fn noise_input_yields_no_observations() {
    let pipeline = default_pipeline();
    let obs = pipeline.run("hi", ScopeId::new_v4()).unwrap();
    assert!(obs.is_empty());
}

#[test]
fn substantive_text_yields_typed_candidate_observations() {
    let pipeline = default_pipeline();
    let scope = ScopeId::new_v4();
    let obs = pipeline
        .run(
            "We decided to ship the launch on Friday. \
             TODO: draft the RFC for @Sara. \
             The Migration deadline is next Friday.",
            scope,
        )
        .unwrap();

    // All observations must be Candidate.
    assert!(obs.iter().all(|o| o.memory_state == MemoryState::Candidate));

    // Types observed.
    let types: Vec<_> = obs.iter().map(|o| o.observation_type).collect();
    assert!(types.contains(&ObservationType::Decision));
    assert!(types.contains(&ObservationType::Task));
    assert!(types.contains(&ObservationType::Entity));
    assert!(types.contains(&ObservationType::Fact));

    // Scope is propagated.
    assert!(obs.iter().all(|o| o.scope_id == scope));
}
