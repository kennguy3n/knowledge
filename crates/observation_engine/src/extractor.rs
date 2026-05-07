//! Lexicon-first observation extraction.
//!
//! The Phase-1 baseline. No model required; produces
//! [`crate::types::Observation`]s by scanning the input text for:
//!
//! * **Entities** — capitalised tokens (people / projects), `@`-mentions,
//!   `#`-tags.
//! * **Tasks** — sentences containing `TODO`, `ACTION`, `TASK`,
//!   `please`, or imperative-leading verbs.
//! * **Decisions** — sentences containing `decided`, `agreed`,
//!   `approved`, `ratified`, `signed off`.
//! * **Facts** — declarative-looking sentences that are not tasks,
//!   decisions, or pure entity mentions.
//!
//! Confidence values are intentionally fixed per type; the next
//! pipeline stage (XLM-R + SLM-assisted extraction in later phases)
//! refines them.

use evidence_store::ScopeId;

use crate::types::{Observation, ObservationType};

/// Extract structured observations from raw evidence text.
pub trait ObservationExtractor {
    /// Run the extractor over `text`, returning all observations
    /// found in the supplied `scope`.
    fn extract(&self, text: &str, scope: ScopeId) -> Vec<Observation>;
}

/// Phase-1 lexicon extractor (`PROPOSAL.md` §3.2 first pass).
#[derive(Debug, Clone)]
pub struct LexiconExtractor {
    decision_keywords: Vec<String>,
    task_keywords: Vec<String>,
    task_imperative_verbs: Vec<String>,
    /// English stop-words to strip from capitalised-token entity
    /// extraction (so we don't promote "The" / "This" / "Today" to
    /// entities).
    stop_words: Vec<String>,
}

impl Default for LexiconExtractor {
    fn default() -> Self {
        Self::english_default()
    }
}

impl LexiconExtractor {
    /// Build with explicit lexicons.
    pub fn new(
        decision_keywords: Vec<&str>,
        task_keywords: Vec<&str>,
        task_imperative_verbs: Vec<&str>,
        stop_words: Vec<&str>,
    ) -> Self {
        Self {
            decision_keywords: decision_keywords
                .into_iter()
                .map(str::to_lowercase)
                .collect(),
            task_keywords: task_keywords.into_iter().map(str::to_lowercase).collect(),
            task_imperative_verbs: task_imperative_verbs
                .into_iter()
                .map(str::to_lowercase)
                .collect(),
            stop_words: stop_words.into_iter().map(str::to_lowercase).collect(),
        }
    }

    /// Default English lexicon. Production deployments should
    /// override per-tenant.
    pub fn english_default() -> Self {
        Self::new(
            vec![
                "decided",
                "decision",
                "agreed",
                "approved",
                "ratified",
                "signed off",
                "sign-off",
                "go-live approved",
                "rejected",
            ],
            vec!["todo", "action", "task", "please", "fyi action"],
            vec![
                "draft",
                "send",
                "schedule",
                "review",
                "publish",
                "fix",
                "deploy",
                "ship",
                "investigate",
                "follow up",
                "follow-up",
                "prepare",
                "update",
                "merge",
            ],
            vec![
                "The",
                "This",
                "That",
                "These",
                "Those",
                "It",
                "Today",
                "Tomorrow",
                "Yesterday",
                "Friday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Saturday",
                "Sunday",
                "May",
                "June",
                "July",
            ],
        )
    }
}

fn split_sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'!' | b'?' | b'\n') {
            let s = text[start..i].trim();
            if !s.is_empty() {
                out.push(s);
            }
            start = i + 1;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn lower_contains_any(haystack_lower: &str, needles: &[String]) -> bool {
    needles.iter().any(|n| haystack_lower.contains(n.as_str()))
}

fn starts_with_imperative(haystack_lower: &str, verbs: &[String]) -> bool {
    let first = haystack_lower
        .split(|c: char| !c.is_alphabetic())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    verbs.iter().any(|v| v == first)
}

fn extract_at_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 1 {
                out.push(format!("@{}", &text[i + 1..j]));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn extract_capitalised_words(text: &str, stop_words: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphabetic() && c != '\'') {
        if raw.is_empty() {
            continue;
        }
        let mut chars = raw.chars();
        let first = chars.next().unwrap();
        if first.is_uppercase()
            && raw.chars().count() >= 2
            && !stop_words.iter().any(|s| s.eq_ignore_ascii_case(raw))
        {
            out.push(raw.to_string());
        }
    }
    out
}

impl ObservationExtractor for LexiconExtractor {
    fn extract(&self, text: &str, scope: ScopeId) -> Vec<Observation> {
        let mut out = Vec::new();

        // Entity extraction over the entire input.
        for mention in extract_at_mentions(text) {
            out.push(Observation::new_candidate(
                ObservationType::Entity,
                mention,
                scope,
                0.85,
            ));
        }
        let mut seen_entities = std::collections::HashSet::new();
        for word in extract_capitalised_words(text, &self.stop_words) {
            if seen_entities.insert(word.clone()) {
                out.push(Observation::new_candidate(
                    ObservationType::Entity,
                    word,
                    scope,
                    0.55,
                ));
            }
        }

        // Sentence-level extraction for tasks / decisions / facts.
        for sentence in split_sentences(text) {
            let lower = sentence.to_lowercase();
            if lower_contains_any(&lower, &self.decision_keywords) {
                out.push(Observation::new_candidate(
                    ObservationType::Decision,
                    sentence.to_string(),
                    scope,
                    0.75,
                ));
                continue;
            }
            if lower_contains_any(&lower, &self.task_keywords)
                || starts_with_imperative(&lower, &self.task_imperative_verbs)
            {
                out.push(Observation::new_candidate(
                    ObservationType::Task,
                    sentence.to_string(),
                    scope,
                    0.7,
                ));
                continue;
            }
            // Anything sentence-shaped (>= 6 chars, contains a space) and
            // not picked up as a task / decision is a Fact candidate.
            if sentence.len() >= 6 && sentence.contains(' ') {
                out.push(Observation::new_candidate(
                    ObservationType::Fact,
                    sentence.to_string(),
                    scope,
                    0.5,
                ));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_at_mentions_and_capitalised_entities() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract("@Sara please loop in Acme on the Migration", scope);
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Entity && o.content == "@Sara"));
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Entity && o.content == "Acme"));
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Entity && o.content == "Migration"));
    }

    #[test]
    fn detects_tasks_via_keywords_and_imperatives() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract(
            "TODO: draft the RFC. Please send it by Friday. Schedule a review.",
            scope,
        );
        let tasks: Vec<_> = obs
            .iter()
            .filter(|o| o.observation_type == ObservationType::Task)
            .collect();
        assert!(tasks.len() >= 2);
    }

    #[test]
    fn detects_decisions() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract("We decided to ship the launch on Friday.", scope);
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Decision));
    }

    #[test]
    fn declarative_sentences_become_facts() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract("The migration ships next Friday.", scope);
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Fact));
    }
}
