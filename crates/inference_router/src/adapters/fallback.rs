//! Encoder-only fallback adapter.
//!
//! [`FallbackAdapter`] satisfies classification tasks (`TagImportance`,
//! `ExtractEntities`, `PromoteObservation`) by running a real
//! lexicon-and-regex pipeline against the message body. The pipeline
//! is deliberately small and dependency-free — it's not as accurate as
//! a small encoder model, but it is meaningfully better than a
//! hardcoded constant and it produces deterministic, well-typed JSON
//! that exactly matches the grammars in [`crate::task`]. Synthesis
//! tasks are rejected with [`crate::RouterError::Unavailable`] so the
//! router signals the caller to fall back to a non-SLM strategy.
//!
//! ## Lexicons
//!
//! The lexicons below are intentionally small, lower-cased, and
//! whole-word-matched so they don't accidentally fire on substrings
//! (`"urgent"` matches but `"urgently"` would need its own entry —
//! we add common inflections explicitly). Adding a new lexicon entry
//! is the supported way to tune the classifier; do **not** push the
//! classification into the calling code.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::error::RouterError;
use crate::task::InferenceTask;

/// Encoder-only fallback adapter — always available, classification
/// only.
pub struct FallbackAdapter {
    available: AtomicBool,
}

impl FallbackAdapter {
    /// Construct a new fallback adapter.
    pub fn new() -> Self {
        Self {
            available: AtomicBool::new(true),
        }
    }
}

impl Default for FallbackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceAdapter for FallbackAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Fallback
    }

    fn probe(&self) -> ProbeResult {
        // Always available — encoder-only fallback runs everywhere.
        self.available.store(true, Ordering::SeqCst);
        ProbeResult::Available
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    fn supports(&self, task: InferenceTask) -> bool {
        task.is_classification()
    }

    fn generate(
        &self,
        task_tag: &str,
        prompt: &str,
        _grammar: &str,
    ) -> Result<String, RouterError> {
        let body = extract_body(prompt);
        match task_tag {
            "tag_importance" => Ok(classify_importance(body)),
            "extract_entities" => Ok(extract_entities(body)),
            "promote_observation" => Ok(promote_observation(body)),
            "synth_summary" | "synth_concept" | "adjudicate_contradiction" => {
                Err(RouterError::Unavailable {
                    task: stable_tag(task_tag),
                })
            }
            _ => Err(RouterError::Unavailable { task: "unknown" }),
        }
    }
}

fn stable_tag(task_tag: &str) -> &'static str {
    match task_tag {
        "synth_summary" => "synth_summary",
        "synth_concept" => "synth_concept",
        "adjudicate_contradiction" => "adjudicate_contradiction",
        _ => "unknown",
    }
}

/// Strip the static prompt scaffolding so the heuristics scan only
/// the user-visible message body.
///
/// The substrate's prompt templates (see [`crate::task::InferenceTask::prompt_template`])
/// end with one of `"\n\nMessage:\n"`, `"\n\nObservation:\n"`, or
/// `"\n\nObservations:\n"` followed by the body. We split on the
/// rightmost matching marker so a body that itself contains the
/// marker text (rare but possible) doesn't truncate the result.
fn extract_body(prompt: &str) -> &str {
    for marker in [
        "\n\nMessage:\n",
        "\n\nObservation:\n",
        "\n\nObservations:\n",
    ] {
        if let Some(idx) = prompt.rfind(marker) {
            return &prompt[idx + marker.len()..];
        }
    }
    prompt
}

// ─────────────────── tag_importance: lexicon scoring ────────────────────

/// Lexicon words that pull the message into the `Critical` class.
/// All entries are lower-case whole words; the matcher lowercases the
/// body once before scanning.
const CRITICAL_LEXICON: &[&str] = &[
    "outage",
    "critical",
    "p0",
    "p1",
    "incident",
    "emergency",
    "downtime",
    "asap",
    "breach",
    "compromised",
];

/// Lexicon words that pull the message into the `Important` class.
const IMPORTANT_LEXICON: &[&str] = &[
    "urgent",
    "important",
    "deadline",
    "blocker",
    "blocking",
    "action",
    "decision",
    "approve",
    "approval",
    "review",
    "release",
    "ship",
    "merge",
    "deploy",
    "rollback",
];

/// Lexicon words that pull the message into the `Useful` class.
const USEFUL_LEXICON: &[&str] = &[
    "todo",
    "task",
    "follow-up",
    "followup",
    "question",
    "investigate",
    "spec",
    "design",
    "rfc",
    "doc",
    "documentation",
    "note",
    "summary",
];

/// Lexicon words that bias the message toward `Noise`.
const NOISE_LEXICON: &[&str] = &[
    "lol",
    "haha",
    "👍",
    "🙏",
    "fyi",
    "thx",
    "thanks",
    "welcome",
    "weekend",
    "lunch",
    "coffee",
];

/// Score a body against `lexicon`, returning the number of whole-word
/// occurrences. Matching is case-insensitive (the caller pre-lowercases
/// the body) and word-bounded by ASCII non-alphanumeric characters,
/// which is good enough for English message bodies; the few false
/// negatives on inflected forms are why each lexicon ships several
/// related variants (`"approve"` + `"approval"`).
fn lexicon_count(lower_body: &str, lexicon: &[&str]) -> usize {
    let mut count = 0;
    for word in lexicon {
        // Token-aware containment scan: we want `"urgent"` to fire on
        // `"This is urgent."` but NOT on `"insurgent"`. The bytes of
        // the matched substring are tested for ASCII-alphanumeric
        // neighbours; non-alphanumeric (including the start / end of
        // the string) counts as a boundary.
        let mut search_from = 0;
        while let Some(rel_idx) = lower_body[search_from..].find(word) {
            let abs_idx = search_from + rel_idx;
            let before_ok = abs_idx == 0
                || !lower_body.as_bytes()[abs_idx - 1].is_ascii_alphanumeric();
            let end = abs_idx + word.len();
            let after_ok = end == lower_body.len()
                || !lower_body.as_bytes()[end].is_ascii_alphanumeric();
            if before_ok && after_ok {
                count += 1;
            }
            // Always advance past at least one byte to guarantee
            // forward progress even on zero-length matches (defence
            // against future lexicon entries that contain only
            // punctuation).
            search_from = abs_idx + word.len().max(1);
            if search_from >= lower_body.len() {
                break;
            }
        }
    }
    count
}

/// Run the importance classifier and emit the JSON shape demanded by
/// [`crate::task::GRAMMAR_TAG_IMPORTANCE`].
fn classify_importance(body: &str) -> String {
    if body.trim().is_empty() {
        return r#"{"class":"noise","confidence":0.50}"#.into();
    }
    let lower = body.to_ascii_lowercase();
    let critical = lexicon_count(&lower, CRITICAL_LEXICON);
    let important = lexicon_count(&lower, IMPORTANT_LEXICON);
    let useful = lexicon_count(&lower, USEFUL_LEXICON);
    let noise = lexicon_count(&lower, NOISE_LEXICON);

    // Additional signals: question marks bump us toward Useful (a
    // question is usually worth tracking); @-mentions count as Useful
    // (an addressed message is rarely Noise); ALL-CAPS words bump us
    // toward Important (shouting in chat).
    let question_marks = body.matches('?').count();
    let mentions = body.matches('@').count();
    let allcaps_words = body
        .split_whitespace()
        .filter(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_uppercase()))
        .count();

    let critical_score = critical * 3;
    let important_score = important * 2 + allcaps_words;
    let useful_score = useful + question_marks + mentions;
    let noise_score = noise;

    let total = critical_score + important_score + useful_score + noise_score;

    // Pick the highest-scoring class with deterministic ties broken
    // by severity order (Critical > Important > Useful > Noise) so
    // ambiguous bodies degrade conservatively (toward the more
    // attention-worthy class).
    let (class, confidence) = if total == 0 {
        ("noise", 0.50)
    } else if critical_score >= important_score
        && critical_score >= useful_score
        && critical_score >= noise_score
        && critical_score > 0
    {
        ("critical", confidence_from_density(critical_score, total))
    } else if important_score >= useful_score
        && important_score >= noise_score
        && important_score > 0
    {
        ("important", confidence_from_density(important_score, total))
    } else if useful_score >= noise_score && useful_score > 0 {
        ("useful", confidence_from_density(useful_score, total))
    } else {
        ("noise", confidence_from_density(noise_score, total))
    };

    format!(r#"{{"class":"{class}","confidence":{confidence:.2}}}"#)
}

/// Map a winning class's score to a `[0.55, 0.95]` confidence range.
/// Pure rule-based classifiers shouldn't claim 1.0 — leave headroom
/// for the SLM-driven adapter — but should beat 0.5 when there is
/// any signal at all.
fn confidence_from_density(score: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.50;
    }
    let ratio = score as f64 / total as f64;
    // Squash to [0.55, 0.95].
    0.55 + ratio * 0.40
}

// ──────────────────── extract_entities: regex-lite NER ────────────────────

/// Run a regex-free entity extractor over `body`. We avoid pulling in
/// the `regex` crate for one reason: the substrate keeps the
/// fallback adapter dependency-free so it can ship in size-constrained
/// builds (embedded, WASM). Hand-rolled scanners are fine for this
/// scope.
fn extract_entities(body: &str) -> String {
    if body.trim().is_empty() {
        return r#"{"entities":[]}"#.into();
    }
    let mut entities: Vec<(String, &'static str)> = Vec::new();

    extract_mentions(body, &mut entities);
    extract_urls(body, &mut entities);
    extract_iso_dates(body, &mut entities);
    extract_project_names(body, &mut entities);

    if entities.is_empty() {
        return r#"{"entities":[]}"#.into();
    }
    use std::fmt::Write as _;
    let mut out = String::from(r#"{"entities":["#);
    for (i, (name, kind)) in entities.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            r#"{{"name":"{name}","type":"{kind}"}}"#,
            name = json_escape(name),
        );
    }
    out.push_str("]}");
    out
}

/// Extract `@mention`-style tokens. Matches the conservative
/// definition `@` followed by one or more ASCII alphanumeric or
/// underscore characters, since that's what every chat platform we
/// target uses.
fn extract_mentions(body: &str, out: &mut Vec<(String, &'static str)>) {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
            {
                end += 1;
            }
            if end > start {
                let name = &body[start..end];
                out.push((format!("@{name}"), "person"));
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
}

/// Extract URLs. Conservative: looks for `http://` or `https://`
/// followed by a run of non-whitespace characters; trims trailing
/// punctuation (`.`, `,`, `)`, `]`) so a URL ending a sentence is
/// captured cleanly.
fn extract_urls(body: &str, out: &mut Vec<(String, &'static str)>) {
    for prefix in ["http://", "https://"] {
        let mut search_from = 0;
        while let Some(rel) = body[search_from..].find(prefix) {
            let start = search_from + rel;
            // Require a word boundary (start of string or non-alnum)
            // before the prefix so `"shttps://"` isn't matched.
            if start > 0
                && body.as_bytes()[start - 1].is_ascii_alphanumeric()
            {
                search_from = start + prefix.len();
                continue;
            }
            let tail = &body[start..];
            let end_rel = tail
                .find(|c: char| c.is_whitespace())
                .unwrap_or(tail.len());
            let mut url = &tail[..end_rel];
            while let Some(last) = url.chars().last() {
                if matches!(last, '.' | ',' | ')' | ']' | '!' | '?' | ';' | ':') {
                    url = &url[..url.len() - last.len_utf8()];
                } else {
                    break;
                }
            }
            if !url.is_empty() {
                out.push((url.to_string(), "url"));
            }
            search_from = start + end_rel.max(prefix.len());
        }
    }
}

/// Extract ISO-8601 date stamps in the `YYYY-MM-DD` shape. Anything
/// with separator characters other than `-`, or with fractional
/// seconds / time zones, is also accepted but only the date prefix is
/// captured.
fn extract_iso_dates(body: &str, out: &mut Vec<(String, &'static str)>) {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        let window = &bytes[i..i + 10];
        let is_date = window[0].is_ascii_digit()
            && window[1].is_ascii_digit()
            && window[2].is_ascii_digit()
            && window[3].is_ascii_digit()
            && window[4] == b'-'
            && window[5].is_ascii_digit()
            && window[6].is_ascii_digit()
            && window[7] == b'-'
            && window[8].is_ascii_digit()
            && window[9].is_ascii_digit();
        if is_date {
            // Reject if preceded by a digit (avoids matching the
            // tail half of a longer numeric token).
            let preceded_by_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            // Reject if followed by another digit (would mean we're
            // looking at e.g. "1234-56-7890" — not a real date).
            let followed_by_digit = i + 10 < bytes.len()
                && bytes[i + 10].is_ascii_digit();
            if !preceded_by_digit && !followed_by_digit {
                out.push((body[i..i + 10].to_string(), "date"));
            }
            i += 10;
        } else {
            i += 1;
        }
    }
}

/// Extract project / proper-noun phrases: sequences of two or more
/// capitalised whitespace-separated tokens (`"Knowledge Substrate"`,
/// `"OneDrive Connector"`). Skips well-known sentence starters that
/// are usually not entities.
fn extract_project_names(body: &str, out: &mut Vec<(String, &'static str)>) {
    /// Sentence-starter words that incidentally begin with a
    /// capital. Adding entries here trades recall for precision.
    const SENTENCE_STARTERS: &[&str] = &[
        "The", "This", "That", "We", "I", "You", "He", "She", "It", "They",
        "A", "An", "Our", "My", "Your", "His", "Her", "Their",
    ];
    let mut current: Vec<&str> = Vec::new();
    let emit = |buf: &mut Vec<&str>, out: &mut Vec<(String, &'static str)>| {
        if buf.len() >= 2 {
            let phrase = buf.join(" ");
            out.push((phrase, "project"));
        }
        buf.clear();
    };
    for token in body.split(|c: char| c.is_whitespace() || matches!(c, '.' | ',' | ':' | ';' | '!' | '?')) {
        // Strip trailing closing punctuation (e.g. `"design)"`).
        let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if token.len() < 2 {
            emit(&mut current, out);
            continue;
        }
        let first = token.as_bytes()[0];
        let starts_capital = first.is_ascii_uppercase();
        let rest_ok = token[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if starts_capital && rest_ok && !SENTENCE_STARTERS.contains(&token) {
            current.push(token);
        } else {
            emit(&mut current, out);
        }
    }
    emit(&mut current, out);
}

/// Minimal JSON string escape: handles backslash, quote, newline,
/// tab, and carriage return. Sufficient for the entity-name strings
/// the extractors emit (which are always ASCII-printable).
fn json_escape(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

// ───────────── promote_observation: decision / task heuristic ─────────────

/// Lexicon keywords that imply a decision or commitment worth
/// promoting to canonical knowledge.
const DECISION_LEXICON: &[&str] = &[
    "decided",
    "agreed",
    "resolved",
    "approved",
    "shipped",
    "merged",
    "deployed",
    "rolled out",
    "selected",
    "chose",
    "chosen",
    "concluded",
    "settled",
];

/// Lexicon keywords that imply a task assignment worth promoting.
const TASK_LEXICON: &[&str] = &[
    "assigned to",
    "action item",
    "todo:",
    "to-do:",
    "follow-up:",
    "followup:",
    "owner:",
    "will own",
    "will handle",
    "will drive",
    "will lead",
];

/// Lexicon keywords that imply a deadline worth promoting.
const DEADLINE_LEXICON: &[&str] = &[
    "by friday",
    "by monday",
    "by tuesday",
    "by wednesday",
    "by thursday",
    "by eod",
    "due:",
    "due by",
    "deadline:",
    "no later than",
    "before the end",
];

/// Heuristic promotion decision.
///
/// Returns the JSON shape demanded by [`crate::task::GRAMMAR_PROMOTE_OBSERVATION`].
fn promote_observation(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let decision_hits = lexicon_count(&lower, DECISION_LEXICON);
    // The task / deadline lexicons contain spaces, so the
    // word-boundary `lexicon_count` doesn't work cleanly — fall back
    // to a plain substring scan for those (substring is correct here
    // because every entry already starts at a word boundary in
    // English).
    let task_hits = TASK_LEXICON.iter().filter(|p| lower.contains(*p)).count();
    let deadline_hits = DEADLINE_LEXICON
        .iter()
        .filter(|p| lower.contains(*p))
        .count();

    let promote = decision_hits > 0 || task_hits > 0 || deadline_hits > 0;
    let reason = match (decision_hits > 0, task_hits > 0, deadline_hits > 0) {
        (true, _, _) => "contains a decision keyword",
        (false, true, _) => "contains a task assignment",
        (false, false, true) => "contains a deadline",
        (false, false, false) => "no decision, task, or deadline signal found",
    };
    format!(r#"{{"promote":{promote},"reason":"{reason}"}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_always_available() {
        let adapter = FallbackAdapter::new();
        assert_eq!(adapter.probe(), ProbeResult::Available);
        assert!(adapter.is_available());
    }

    #[test]
    fn fallback_supports_only_classification() {
        let adapter = FallbackAdapter::new();
        assert!(adapter.supports(InferenceTask::TagImportance));
        assert!(adapter.supports(InferenceTask::ExtractEntities));
        assert!(adapter.supports(InferenceTask::PromoteObservation));
        assert!(!adapter.supports(InferenceTask::SynthSummary));
        assert!(!adapter.supports(InferenceTask::SynthConcept));
        assert!(!adapter.supports(InferenceTask::AdjudicateContradiction));
    }

    #[test]
    fn fallback_classification_picks_critical_on_outage_word() {
        let adapter = FallbackAdapter::new();
        let prompt = format!(
            "{}\n\nMessage:\nProduction outage, page on-call now",
            InferenceTask::TagImportance.prompt_template()
        );
        let out = adapter
            .generate("tag_importance", &prompt, "")
            .expect("classification");
        assert!(out.contains("\"class\":\"critical\""), "got {out}");
    }

    #[test]
    fn fallback_classification_picks_important_on_deadline_word() {
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nMessage:\nWe need a deadline-driven decision today";
        let out = adapter.generate("tag_importance", prompt, "").unwrap();
        assert!(out.contains("\"class\":\"important\""), "got {out}");
    }

    #[test]
    fn fallback_classification_picks_noise_on_empty_body() {
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nMessage:\n   ";
        let out = adapter.generate("tag_importance", prompt, "").unwrap();
        assert!(out.contains("\"class\":\"noise\""), "got {out}");
    }

    #[test]
    fn fallback_classification_avoids_substring_false_positives() {
        // `"insurgent"` contains `"urgent"` but is a different word —
        // the word-boundary check must reject it.
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nMessage:\nThe insurgent attack continues";
        let out = adapter.generate("tag_importance", prompt, "").unwrap();
        assert!(!out.contains("\"class\":\"important\""), "got {out}");
    }

    #[test]
    fn fallback_classification_produces_well_formed_json() {
        let adapter = FallbackAdapter::new();
        let out = adapter.generate("tag_importance", "x", "").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(parsed["class"].is_string());
        assert!(parsed["confidence"].is_f64());
    }

    #[test]
    fn fallback_synthesis_returns_unavailable() {
        let adapter = FallbackAdapter::new();
        for tag in ["synth_summary", "synth_concept", "adjudicate_contradiction"] {
            let err = adapter.generate(tag, "", "").unwrap_err();
            assert!(matches!(err, RouterError::Unavailable { .. }));
            assert!(err.is_fallback());
        }
    }

    #[test]
    fn fallback_extract_entities_returns_empty_list_on_empty_body() {
        let adapter = FallbackAdapter::new();
        let out = adapter.generate("extract_entities", "", "").unwrap();
        assert_eq!(out, r#"{"entities":[]}"#);
    }

    #[test]
    fn fallback_extract_entities_pulls_mentions_urls_and_dates() {
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nMessage:\n@alice please review https://example.com by 2025-06-01";
        let out = adapter.generate("extract_entities", prompt, "").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let entities = parsed["entities"].as_array().expect("entities array");
        let names: Vec<&str> = entities
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"@alice"), "got {names:?}");
        assert!(names.contains(&"https://example.com"), "got {names:?}");
        assert!(names.contains(&"2025-06-01"), "got {names:?}");
    }

    #[test]
    fn fallback_extract_entities_pulls_project_names() {
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nMessage:\nThe Knowledge Substrate ships next week";
        let out = adapter.generate("extract_entities", prompt, "").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let names: Vec<String> = parsed["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("Knowledge Substrate")),
            "got {names:?}"
        );
    }

    #[test]
    fn fallback_promote_returns_true_on_decision_keyword() {
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nObservation:\nWe decided to ship v3";
        let out = adapter.generate("promote_observation", prompt, "").unwrap();
        assert!(out.contains("\"promote\":true"), "got {out}");
        assert!(out.contains("decision keyword"), "got {out}");
    }

    #[test]
    fn fallback_promote_returns_true_on_action_item() {
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nObservation:\nAction item: @bob owns the rollout";
        let out = adapter.generate("promote_observation", prompt, "").unwrap();
        assert!(out.contains("\"promote\":true"), "got {out}");
    }

    #[test]
    fn fallback_promote_returns_false_on_idle_chatter() {
        let adapter = FallbackAdapter::new();
        let prompt = "Stuff\n\nObservation:\nlol thanks";
        let out = adapter.generate("promote_observation", prompt, "").unwrap();
        assert!(out.contains("\"promote\":false"), "got {out}");
    }

    #[test]
    fn extract_body_strips_known_marker() {
        let prompt = "Some prompt\n\nMessage:\nthe actual body";
        assert_eq!(extract_body(prompt), "the actual body");
    }

    #[test]
    fn extract_body_returns_whole_prompt_when_no_marker() {
        assert_eq!(extract_body("nothing matches here"), "nothing matches here");
    }
}
