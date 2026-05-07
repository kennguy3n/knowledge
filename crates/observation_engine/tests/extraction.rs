//! Tests for the lexicon-first extraction.

use evidence_store::ScopeId;
use observation_engine::{LexiconExtractor, ObservationExtractor, ObservationType};

fn ext() -> LexiconExtractor {
    LexiconExtractor::default()
}

#[test]
fn extracts_at_mentions_as_entities() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("@Sara please draft the RFC", scope);
    let mentions: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .map(|o| o.content.as_str())
        .collect();
    assert!(mentions.contains(&"@Sara"));
}

#[test]
fn extracts_capitalised_entities_dropping_stopwords() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("The Migration ships on Friday with help from Acme.", scope);
    let entities: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .map(|o| o.content.as_str())
        .collect();
    assert!(entities.contains(&"Migration"));
    assert!(entities.contains(&"Acme"));
    // "The" / "Friday" are stop-words and must not surface as entities.
    assert!(!entities.contains(&"The"));
    assert!(!entities.contains(&"Friday"));
}

#[test]
fn detects_tasks_via_keyword_and_imperative_verb() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("TODO: draft the RFC. Schedule a review by Friday.", scope);
    let tasks: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Task)
        .map(|o| o.content.as_str())
        .collect();
    assert!(tasks.iter().any(|s| s.contains("TODO")));
    assert!(tasks.iter().any(|s| s.starts_with("Schedule")));
}

#[test]
fn detects_decisions() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract(
        "We agreed to ship next Friday. We approved the new policy.",
        scope,
    );
    let decisions: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Decision)
        .collect();
    assert_eq!(decisions.len(), 2);
}

#[test]
fn declarative_sentences_become_facts() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("The migration ships next Friday.", scope);
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Fact));
}

/// Regression — `"follow up"` / `"follow-up"` used to live on the
/// imperative-verb list, where `starts_with_imperative` could never
/// match them (it compares the first alphabetic-only token only).
/// They now live on the keyword list, which uses substring
/// matching, so the lexicon actually fires on these phrasings.
#[test]
fn follow_up_phrasings_are_detected_as_tasks() {
    let scope = ScopeId::new_v4();
    for line in [
        "follow up with @Sara about the launch",
        "follow-up: ping the design review",
    ] {
        let obs = ext().extract(line, scope);
        assert!(
            obs.iter()
                .any(|o| o.observation_type == ObservationType::Task),
            "lexicon must surface a task for: {line:?}",
        );
    }
}

#[test]
fn fresh_observations_are_candidate_state() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("Friday is the deadline for the migration.", scope);
    assert!(obs
        .iter()
        .all(|o| o.memory_state == memory_manager::MemoryState::Candidate));
}
