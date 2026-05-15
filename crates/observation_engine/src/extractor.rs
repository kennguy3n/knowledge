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

/// Phase-1 lexicon extractor (`docs/DESIGN.md` §3.2 first pass).
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
            // Multi-word entries belong here (lower_contains_any does
            // substring matching) — `starts_with_imperative` only
            // compares the first alphabetic-only token of a sentence
            // against the imperative-verb list, so multi-word verbs
            // would otherwise be unreachable.
            vec![
                "todo",
                "action",
                "task",
                "please",
                "fyi action",
                "follow up",
                "follow-up",
            ],
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

/// One sentence and the punctuation that ended it (`'.'`, `'!'`,
/// `'?'`, or `'\n'`).
#[derive(Debug, Clone, Copy)]
struct SentenceSlice<'a> {
    text: &'a str,
    terminator: Option<u8>,
}

fn split_sentences_with_terminator(text: &str) -> Vec<SentenceSlice<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'!' | b'?' | b'\n') {
            let s = text[start..i].trim();
            if !s.is_empty() {
                out.push(SentenceSlice {
                    text: s,
                    terminator: Some(*b),
                });
            }
            start = i + 1;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(SentenceSlice {
            text: tail,
            terminator: None,
        });
    }
    out
}

/// Interrogative words that mark a sentence as a question even
/// without a `?` terminator.
const INTERROGATIVES: &[&str] = &[
    "who", "what", "when", "where", "why", "how", "which", "whose", "whom",
];

fn looks_like_question(sentence: &str, terminator: Option<u8>) -> bool {
    if terminator == Some(b'?') {
        return true;
    }
    let lower = sentence.trim().to_lowercase();
    let first = lower
        .split(|c: char| !c.is_alphabetic())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    INTERROGATIVES.contains(&first)
}

fn extract_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < text.len() {
        let rest = &text[idx..];
        let candidate_offset = ["http://", "https://"]
            .iter()
            .filter_map(|prefix| rest.find(prefix).map(|p| (p, prefix.len())))
            .min_by_key(|(p, _)| *p);
        let Some((rel, _prefix_len)) = candidate_offset else {
            break;
        };
        let abs_start = idx + rel;
        let after = &text[abs_start..];
        let url_end = after
            .find(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ')' || c == ']')
            .map_or(after.len(), |e| e);
        let mut url = &after[..url_end];
        // Trim trailing punctuation that is unlikely to be part of
        // the URL (`.` / `!` / `?` / `\"` / `'`).
        while let Some(last) = url.chars().last() {
            if matches!(last, '.' | '!' | '?' | '"' | '\'') {
                url = &url[..url.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        if url.len() > 8 {
            out.push(url.to_string());
        }
        idx = abs_start + url_end;
    }
    out
}

fn extract_emails(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in
        text.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '<' || c == '>')
    {
        let token =
            token.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | '"' | '\'' | '(' | ')'));
        let Some(at) = token.find('@') else { continue };
        if at == 0 || at == token.len() - 1 {
            continue;
        }
        let local = &token[..at];
        let domain = &token[at + 1..];
        if !local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        {
            continue;
        }
        if !domain.contains('.') {
            continue;
        }
        if !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            continue;
        }
        out.push(token.to_string());
    }
    out
}

const MONTH_TOKENS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
];

const DAY_TOKENS: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "today",
    "tomorrow",
    "yesterday",
];

const RELATIVE_DATE_PHRASES: &[&str] = &[
    "next week",
    "last week",
    "next month",
    "last month",
    "next quarter",
];

/// Case-insensitive (ASCII-fold) substring search returning the
/// `(start_byte, end_byte)` span of the first match in `haystack`,
/// or `None`. Operates over `char_indices` so the returned byte
/// offsets are always at valid UTF-8 boundaries even when
/// lowercasing would otherwise change byte length (e.g. Turkish
/// `İ` U+0130 → `i\u{307}`).
fn ci_find(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((0, 0));
    }
    let needle_chars: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
    let h_chars: Vec<(usize, char)> = haystack.char_indices().collect();
    if needle_chars.len() > h_chars.len() {
        return None;
    }
    for start in 0..=(h_chars.len() - needle_chars.len()) {
        let hits = needle_chars
            .iter()
            .enumerate()
            .all(|(i, &n)| h_chars[start + i].1.to_ascii_lowercase() == n);
        if hits {
            let start_byte = h_chars[start].0;
            let end_byte = if start + needle_chars.len() < h_chars.len() {
                h_chars[start + needle_chars.len()].0
            } else {
                haystack.len()
            };
            return Some((start_byte, end_byte));
        }
    }
    None
}

fn extract_date_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();

    // Multi-word relative phrases. We scan `text` directly with
    // [`ci_find`] so the matched byte span aligns with valid UTF-8
    // boundaries even when the input contains characters whose
    // lowercase form has a different byte length than the original.
    for phrase in RELATIVE_DATE_PHRASES {
        if let Some((start, end)) = ci_find(text, phrase) {
            out.push(text[start..end].to_string());
        }
    }

    // Day / month tokens — walk the *original* text by char so the
    // emitted span preserves casing without ever indexing through a
    // length-changed lowercase view.
    let mut start: Option<usize> = None;
    let mut last_end = 0usize;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            let token = &text[s..i];
            let lower = token.to_ascii_lowercase();
            if DAY_TOKENS.contains(&lower.as_str()) || MONTH_TOKENS.contains(&lower.as_str()) {
                out.push(token.to_string());
            }
        }
        last_end = i + c.len_utf8();
    }
    if let Some(s) = start.take() {
        let token = &text[s..last_end];
        let lower = token.to_ascii_lowercase();
        if DAY_TOKENS.contains(&lower.as_str()) || MONTH_TOKENS.contains(&lower.as_str()) {
            out.push(token.to_string());
        }
    }

    // `Q3 2026` / `Q1 2027` style. The pattern is pure ASCII —
    // `q` / `Q`, an ASCII digit, optional ASCII whitespace, four
    // ASCII digits — so byte indices are always char boundaries.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        if bytes[i].eq_ignore_ascii_case(&b'q') && bytes[i + 1].is_ascii_digit() {
            let q_digit = bytes[i + 1];
            if matches!(q_digit, b'1' | b'2' | b'3' | b'4') {
                let mut j = i + 2;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k - j == 4 {
                    out.push(text[i..k].to_string());
                    i = k;
                    continue;
                }
            }
        }
        i += 1;
    }

    out.sort();
    out.dedup();
    out
}

fn extract_numeric_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `$5M`, `$50,000`, `$2.5B`.
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit() || bytes[j] == b',' || bytes[j] == b'.')
            {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b'k' | b'K' | b'm' | b'M' | b'b' | b'B') {
                j += 1;
            }
            if j > i + 1 {
                out.push(text[i..j].to_string());
            }
            i = j;
            continue;
        }
        // Numeric + unit (`3 sprints`, `2 weeks`, `48h`).
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit() || bytes[j] == b',' || bytes[j] == b'.')
            {
                j += 1;
            }
            // Optional whitespace then a unit word.
            let after_num = j;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && bytes[k].is_ascii_alphabetic() {
                k += 1;
            }
            if k > j && (k - j) >= 1 {
                let unit = text[j..k].to_lowercase();
                let units = [
                    "sprint", "sprints", "week", "weeks", "day", "days", "month", "months",
                    "quarter", "quarters", "hour", "hours", "minute", "minutes", "h", "min",
                    "users", "people", "tickets", "issues", "pr", "prs", "%", "percent",
                ];
                if units.iter().any(|u| u == &unit.as_str()) {
                    out.push(text[i..k].to_string());
                    i = k;
                    continue;
                }
            }
            // Lone number like `42`. Skip — too noisy.
            i = after_num;
            continue;
        }
        i += 1;
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
        let mut seen_entities: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Entity extraction over the entire input.
        for mention in extract_at_mentions(text) {
            if seen_entities.insert(mention.clone()) {
                out.push(Observation::new_candidate(
                    ObservationType::Entity,
                    mention,
                    scope,
                    0.85,
                ));
            }
        }
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
        for url in extract_urls(text) {
            if seen_entities.insert(url.clone()) {
                out.push(Observation::new_candidate(
                    ObservationType::Entity,
                    url,
                    scope,
                    0.9,
                ));
            }
        }
        for email in extract_emails(text) {
            if seen_entities.insert(email.clone()) {
                out.push(Observation::new_candidate(
                    ObservationType::Entity,
                    email,
                    scope,
                    0.9,
                ));
            }
        }
        for date_ref in extract_date_refs(text) {
            if seen_entities.insert(date_ref.clone()) {
                out.push(Observation::new_candidate(
                    ObservationType::Entity,
                    date_ref,
                    scope,
                    0.6,
                ));
            }
        }
        for numeric in extract_numeric_refs(text) {
            if seen_entities.insert(numeric.clone()) {
                out.push(Observation::new_candidate(
                    ObservationType::Entity,
                    numeric,
                    scope,
                    0.7,
                ));
            }
        }

        // Sentence-level extraction for tasks / decisions / questions
        // / facts.
        for slice in split_sentences_with_terminator(text) {
            let sentence = slice.text;
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
            if looks_like_question(sentence, slice.terminator) {
                out.push(Observation::new_candidate(
                    ObservationType::Question,
                    sentence.to_string(),
                    scope,
                    0.7,
                ));
                continue;
            }
            // Anything sentence-shaped (>= 6 chars, contains a space) and
            // not picked up as a task / decision / question is a Fact
            // candidate.
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
