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
    // "The" is a capitalised-token stop-word and must not surface as
    // an entity.
    assert!(!entities.contains(&"The"));
    // "Friday" is intentionally surfaced as a *date-ref* entity by
    // the hardening — the capitalised-token stop-word list
    // still filters it out of the capitalised-token pass, but
    // [`extract_date_refs`] picks it up.
    assert!(entities.contains(&"Friday"));
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
fn empty_input_yields_no_observations() {
    let scope = ScopeId::new_v4();
    assert!(ext().extract("", scope).is_empty());
}

#[test]
fn whitespace_only_input_yields_no_observations() {
    let scope = ScopeId::new_v4();
    assert!(ext().extract("   \t\n  ", scope).is_empty());
}

#[test]
fn leading_and_trailing_whitespace_is_handled() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("\n\n   The Migration ships Friday.\n\n", scope);
    assert!(obs.iter().any(|o| o.content == "Migration"));
}

#[test]
fn very_long_input_is_processed_without_panic() {
    let scope = ScopeId::new_v4();
    let mut text = String::new();
    for _ in 0..400 {
        text.push_str("The Project ships next Friday with Acme. ");
    }
    assert!(text.len() > 10_000);
    let obs = ext().extract(&text, scope);
    assert!(!obs.is_empty());
}

#[test]
fn input_with_only_urls_extracts_url_entities() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract(
        "https://example.com/path/to/doc and http://acme.io/post.",
        scope,
    );
    let urls: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .filter(|o| o.content.starts_with("http"))
        .map(|o| o.content.clone())
        .collect();
    assert!(urls.iter().any(|u| u.starts_with("https://example.com")));
    assert!(urls.iter().any(|u| u.starts_with("http://acme.io")));
}

#[test]
fn email_addresses_are_extracted_as_entities() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract(
        "Reach out to alice@example.com or bob.jones+foo@acme.co",
        scope,
    );
    let emails: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .filter(|o| o.content.contains('@') && !o.content.starts_with('@'))
        .map(|o| o.content.clone())
        .collect();
    assert!(emails.iter().any(|e| e == "alice@example.com"));
    assert!(emails.iter().any(|e| e == "bob.jones+foo@acme.co"));
}

#[test]
fn input_with_only_at_mentions_extracts_mention_entities() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("@alice @bob @charlie", scope);
    let mentions: Vec<_> = obs
        .iter()
        .filter(|o| o.content.starts_with('@'))
        .map(|o| o.content.clone())
        .collect();
    assert!(mentions.contains(&"@alice".to_string()));
    assert!(mentions.contains(&"@bob".to_string()));
    assert!(mentions.contains(&"@charlie".to_string()));
}

#[test]
fn date_time_references_are_picked_up() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("Ship by Friday or sometime in Q3 2026 next month.", scope);
    let dates: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .map(|o| o.content.as_str())
        .collect();
    assert!(dates.contains(&"Friday"));
    assert!(dates.iter().any(|d| d.starts_with("Q3")));
    assert!(dates.iter().any(|d| d.contains("month")));
}

#[test]
fn date_refs_handle_unicode_with_length_changing_lowercase() {
    // Regression test for the Unicode byte-offset bug surfaced in
    // PR #6 review: `İ` U+0130 lowercases to `i\u{307}` (2 → 3
    // bytes), so any extractor that lowercases the input and then
    // indexes the original `text` with byte offsets from the
    // lowercased view will either slice the wrong substring or
    // panic mid-codepoint. Both cases that appear in the
    // `extract_date_refs` paths — multi-word phrases and
    // single-token day / month names — are exercised here.
    let scope = ScopeId::new_v4();

    // Multi-word phrase, preceded by a length-changing Unicode
    // char.
    let obs = ext().extract("İstanbul team meets next week.", scope);
    let entities: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .map(|o| o.content.as_str())
        .collect();
    assert!(
        entities.iter().any(|e| e.eq_ignore_ascii_case("next week")),
        "got entities {entities:?}"
    );

    // Single-token day name preceded by a length-changing Unicode
    // char.
    let obs2 = ext().extract("İcebreaker on Friday with Acme.", scope);
    let entities2: Vec<_> = obs2
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .map(|o| o.content.as_str())
        .collect();
    assert!(entities2.contains(&"Friday"), "got entities {entities2:?}");

    // Q-pattern preceded by a length-changing Unicode char (the
    // `Q3 2026` pattern itself is ASCII so byte alignment is
    // straightforward, but the bug also affected the Q-pattern
    // when scanning the lowercased mirror).
    let obs3 = ext().extract("İ-team plan: Q3 2026 review.", scope);
    let entities3: Vec<_> = obs3
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .map(|o| o.content.as_str())
        .collect();
    assert!(
        entities3.iter().any(|d| d.starts_with("Q3")),
        "got entities {entities3:?}"
    );
}

#[test]
fn numeric_references_with_units_are_extracted() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("Need a $5M budget over 3 sprints; review 48h later.", scope);
    let numerics: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity)
        .map(|o| o.content.as_str())
        .collect();
    assert!(numerics.iter().any(|n| n.starts_with("$5")));
    assert!(numerics.iter().any(|n| n.contains("sprints")));
}

#[test]
fn questions_are_detected_via_question_mark() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("Who owns the API rollout?", scope);
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Question));
}

#[test]
fn questions_are_detected_via_interrogative_word() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("How do we ship the migration", scope);
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Question));
}

#[test]
fn multiline_input_emits_per_line_observations() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract(
        "TODO: draft the RFC\nWe approved the new policy\nWho owns the rollout?",
        scope,
    );
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Task));
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Decision));
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Question));
}

#[test]
fn unicode_and_emoji_input_does_not_panic() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract(
        "We agreed 🚀 to ship Friday.\nSchedule a review with Аня.",
        scope,
    );
    // We don't assert specifics — only that the extractor returns
    // without panicking and produces *some* observations.
    assert!(!obs.is_empty());
}

#[test]
fn mixed_case_keywords_still_match() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("WE DECIDED to ship. ToDo: update the docs.", scope);
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Decision));
    assert!(obs
        .iter()
        .any(|o| o.observation_type == ObservationType::Task));
}

#[test]
fn duplicate_entities_are_deduplicated_within_a_pass() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("Acme. Acme. Acme.", scope);
    let acme_count = obs
        .iter()
        .filter(|o| o.observation_type == ObservationType::Entity && o.content == "Acme")
        .count();
    assert_eq!(acme_count, 1);
}

#[test]
fn fresh_observations_are_candidate_state() {
    let scope = ScopeId::new_v4();
    let obs = ext().extract("Friday is the deadline for the migration.", scope);
    assert!(obs
        .iter()
        .all(|o| o.memory_state == memory_manager::MemoryState::Candidate));
}

/// Phase 1.10 end-to-end test: confirm the lexicon-telemetry
/// counters tick through the public extraction surface.
///
/// This is the cross-cutting "do the counters actually tick
/// through the public API?" check — calling
/// [`LexiconExtractor::extract`] on a non-trivial English body
/// must drive *some* lexicon-hit counter and *some* match-
/// strategy-fire counter (the exact tags / strategies depend on
/// which whatlang-detected sentence routes through which
/// keyword class — that's pinned by the unit tests in
/// `crates/observation_engine/src/lexicon_telemetry.rs`).  This
/// test pins the structural property: the public extractor IS
/// wired to the telemetry registry.
///
/// We use lower-bound (`>`) assertions because other tests in
/// the same binary touch the same process-singleton counters.
#[test]
fn lexicon_telemetry_counters_advance_through_public_extractor() {
    use observation_engine::lexicon_telemetry;
    let scope = ScopeId::new_v4();
    let before = lexicon_telemetry::snapshot();

    // Multi-sentence English body — guarantees the per-sentence
    // language detector resolves at least one sentence to a
    // lexicon (the `en` fallback at minimum) and exercises the
    // FirstToken strategy on the question class via "?".
    let _ = ext().extract(
        "Please review the deck before Friday. Can you sign off by EOD?",
        scope,
    );

    let after = lexicon_telemetry::snapshot();

    // *Some* lexicon hit must have been recorded.  We don't pin
    // a specific tag because that depends on whatlang's
    // per-sentence guess; the structural property is "the
    // public extractor is wired to record_lexicon_hit at least
    // once per non-empty extract() call".
    let total_lexicon_hits_before = before.hits_ar
        + before.hits_bo
        + before.hits_de
        + before.hits_en
        + before.hits_es
        + before.hits_fr
        + before.hits_he
        + before.hits_hi
        + before.hits_id
        + before.hits_it
        + before.hits_ja
        + before.hits_km
        + before.hits_ko
        + before.hits_lo
        + before.hits_ms
        + before.hits_my
        + before.hits_pt
        + before.hits_ru
        + before.hits_th
        + before.hits_vi
        + before.hits_zh;
    let total_lexicon_hits_after = after.hits_ar
        + after.hits_bo
        + after.hits_de
        + after.hits_en
        + after.hits_es
        + after.hits_fr
        + after.hits_he
        + after.hits_hi
        + after.hits_id
        + after.hits_it
        + after.hits_ja
        + after.hits_km
        + after.hits_ko
        + after.hits_lo
        + after.hits_ms
        + after.hits_my
        + after.hits_pt
        + after.hits_ru
        + after.hits_th
        + after.hits_vi
        + after.hits_zh;
    assert!(
        total_lexicon_hits_after > total_lexicon_hits_before,
        "no lexicon-hit counter advanced through the public extractor — the wire is broken"
    );

    // *Some* strategy fire must have been recorded too.
    let total_strategy_fires_before = before.strategy_first_token
        + before.strategy_first_bigram
        + before.strategy_substring
        + before.strategy_first_token_with_arabic_clitics
        + before.strategy_first_token_with_hebrew_clitics;
    let total_strategy_fires_after = after.strategy_first_token
        + after.strategy_first_bigram
        + after.strategy_substring
        + after.strategy_first_token_with_arabic_clitics
        + after.strategy_first_token_with_hebrew_clitics;
    assert!(
        total_strategy_fires_after > total_strategy_fires_before,
        "no match-strategy-fire counter advanced through the public extractor — the wire is broken"
    );
}
