//! Lexicon-only importance classifier (baseline fallback).
//!
//! Per `docs/technical/design.md` §4.3 and `docs/technical/architecture.md` §3.3 the substrate has
//! four importance classes: **Critical**, **Important**, **Useful**,
//! **Noise**. The full pipeline classifies via XLM-R + Bonsai-1.7B; this
//! module provides the lexicon-only fallback that runs even when the
//! SLM and the encoder are not available (low-tier devices, bootstrap,
//! and as a safety net for cold-start).
//!
//! The classifier is exposed through the [`ImportanceClassifier`]
//! trait so the SLM-based and encoder-based classifiers can implement
//! the same interface without rewriting downstream callers.
//!
//! # Negation awareness
//!
//! Since v1.3 the lexicon classifier is **negation-aware**: when a
//! keyword match is found, the classifier scans backwards within a
//! configurable character window for a negation term (e.g. "not",
//! "cancelled", "rejected"). If a negation term precedes the keyword
//! within the window, the classification is downgraded by one level
//! (Critical → Important, Important → Useful). This prevents
//! false-positive high-importance classifications for messages like
//! "We decided NOT to go with Vendor X" or "The launch was cancelled".
//!
//! Negation detection is heuristic and intentionally conservative:
//! it only downgrades by one level (never to Noise), and the window is
//! tight (default 30 characters) to avoid false negation triggers from
//! unrelated negative words earlier in the text. The SLM-backed
//! classifier handles nuanced negation semantics when available; this
//! heuristic is the offline fallback.

use serde::{Deserialize, Serialize};

/// The four importance classes (per `docs/technical/design.md` §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImportanceClass {
    /// Tenant policy, regulatory rules, signed decisions. No passive
    /// decay; only explicit deprecation.
    Critical,
    /// Owners, project commitments, canonical concepts. Slow decay;
    /// supersession preferred.
    Important,
    /// Recurring tasks, channel recaps, workflows. Medium decay;
    /// archived if non-used.
    Useful,
    /// Greetings, social chatter, transient pings. Stays only in the
    /// raw evidence plane (ring buffer); never promoted.
    Noise,
}

impl ImportanceClass {
    /// Stable integer tag used for SQL storage. Order is intentionally
    /// chosen so that higher tags = higher importance.
    pub const fn as_tag(self) -> i32 {
        match self {
            Self::Noise => 0,
            Self::Useful => 1,
            Self::Important => 2,
            Self::Critical => 3,
        }
    }

    /// Inverse of [`Self::as_tag`].
    pub fn from_tag(tag: i32) -> Option<Self> {
        match tag {
            0 => Some(Self::Noise),
            1 => Some(Self::Useful),
            2 => Some(Self::Important),
            3 => Some(Self::Critical),
            _ => None,
        }
    }
}

/// Trait that every importance classifier — lexicon, encoder, SLM —
/// must implement.
pub trait ImportanceClassifier {
    /// Classify `text` into one of the four [`ImportanceClass`]
    /// variants.
    fn classify(&self, text: &str) -> ImportanceClass;
}

/// Configurable lexicon of words and phrases used by the
/// [`LexiconClassifier`].
///
/// Lexicons are intentionally not hard-coded: callers can build a
/// per-tenant lexicon (regulated industries care about different
/// "critical" terms than general consumers do).
#[derive(Debug, Clone)]
pub struct Lexicon {
    /// Lower-cased exact-match tokens that flag a message as **Noise**.
    /// Matching is whole-text or whole-token (after lower-casing and
    /// trimming).
    pub noise_tokens: Vec<String>,
    /// Lower-cased substrings that flag a message as **Noise** when
    /// they are the *entire* message (e.g. "thanks!", "+1", emoji
    /// reactions).
    pub noise_phrases: Vec<String>,
    /// Lower-cased substring keywords that mark a message as
    /// **Critical** (e.g. "policy", "regulatory").
    pub critical_keywords: Vec<String>,
    /// Lower-cased substring keywords that mark a message as
    /// **Important** (e.g. "deadline", "milestone").
    pub important_keywords: Vec<String>,
    /// Minimum non-numeric body length (in characters) below which a
    /// message is automatically classified as **Noise**.
    pub min_signal_chars: usize,
    /// Lower-cased substrings that, when found within
    /// [`Self::negation_window`] characters *before* a critical or
    /// important keyword, downgrade the classification by one level.
    ///
    /// Examples: "not", "no longer", "cancelled", "rejected",
    /// "denied", "revoked", "rescinded", "withdrawn".
    pub negation_terms: Vec<String>,
    /// Maximum number of characters to scan backwards from a keyword
    /// match position when looking for a preceding negation term.
    /// Default: 30 characters — tight enough to avoid false triggers
    /// from unrelated negative words, loose enough to catch
    /// "not ... approved" and "cancelled ... launch".
    pub negation_window: usize,
}

impl Lexicon {
    /// Build a fresh lexicon from explicit lists. All inputs are
    /// lower-cased.
    pub fn new(
        noise_tokens: Vec<&str>,
        noise_phrases: Vec<&str>,
        critical_keywords: Vec<&str>,
        important_keywords: Vec<&str>,
        min_signal_chars: usize,
    ) -> Self {
        Self::with_negation(
            noise_tokens,
            noise_phrases,
            critical_keywords,
            important_keywords,
            min_signal_chars,
            Vec::new(),
            30,
        )
    }

    /// Full constructor including negation terms and window.
    /// All inputs are lower-cased.
    pub fn with_negation(
        noise_tokens: Vec<&str>,
        noise_phrases: Vec<&str>,
        critical_keywords: Vec<&str>,
        important_keywords: Vec<&str>,
        min_signal_chars: usize,
        negation_terms: Vec<&str>,
        negation_window: usize,
    ) -> Self {
        Self {
            noise_tokens: noise_tokens.into_iter().map(str::to_lowercase).collect(),
            noise_phrases: noise_phrases.into_iter().map(str::to_lowercase).collect(),
            critical_keywords: critical_keywords
                .into_iter()
                .map(str::to_lowercase)
                .collect(),
            important_keywords: important_keywords
                .into_iter()
                .map(str::to_lowercase)
                .collect(),
            min_signal_chars,
            negation_terms: negation_terms.into_iter().map(str::to_lowercase).collect(),
            negation_window,
        }
    }

    /// Builder-friendly default lexicon for English chat messages.
    /// Production deployments should override per-tenant.
    pub fn default_english() -> Self {
        Self::with_negation(
            // noise_tokens — short reaction-style tokens
            vec![
                "hi", "hello", "hey", "yo", "sup", "ok", "okay", "k", "kk", "thanks", "thx", "ty",
                "+1", "-1", "lol", "lmao", "rofl", "nice", "great", "cool", "yes", "no", "yep",
                "nope", "sure", "fine", "👍", "🎉", "❤️", "🚀", "✅", "💯",
            ],
            // noise_phrases — the *entire* body equals one of these
            vec![
                "good morning",
                "good afternoon",
                "good evening",
                "good night",
                "have a good one",
                "have a great day",
                "thanks!",
                "thank you",
                "thank you!",
                "you're welcome",
            ],
            // critical_keywords — substrate-level high-stakes signals
            vec![
                "policy",
                "regulatory",
                "compliance",
                "approved",
                "signed",
                "budget confirmed",
                "executed",
                "ratified",
                "authorised",
                "authorized",
                "legal hold",
                "incident",
                "outage",
                "breach",
            ],
            // important_keywords — substrate-level commitments / decisions
            vec![
                "deadline",
                "owner",
                "decision",
                "launch",
                "milestone",
                "assigned to",
                "blocker",
                "release",
                "ship",
                "approval needed",
                "kickoff",
                "go-live",
                "commit",
            ],
            5,
            // negation_terms — words/phrases that, when preceding a
            // critical or important keyword within the negation window,
            // downgrade the classification by one level.
            vec![
                "not",
                "no longer",
                "cancelled",
                "canceled",
                "rejected",
                "denied",
                "revoked",
                "rescinded",
                "withdrawn",
                "postponed",
                "deferred",
                "reverted",
                "rolled back",
                "on hold",
                "blocked",
                "declined",
                "refused",
                "abandoned",
                "scrapped",
                "voided",
            ],
            30,
        )
    }
}

impl Default for Lexicon {
    fn default() -> Self {
        Self::default_english()
    }
}

/// Lexicon-only classifier.
///
/// The classifier never panics, never allocates per call beyond the
/// usual lower-casing buffer, and is safe to call concurrently from
/// multiple threads — `&self` only, no internal state.
#[derive(Debug, Clone, Default)]
pub struct LexiconClassifier {
    lexicon: Lexicon,
}

impl LexiconClassifier {
    /// Build a classifier from an explicit lexicon.
    pub fn new(lexicon: Lexicon) -> Self {
        Self { lexicon }
    }

    /// Build a classifier with the substrate's default English lexicon.
    pub fn english_default() -> Self {
        Self::new(Lexicon::default_english())
    }

    fn is_noise(&self, normalized: &str) -> bool {
        if normalized.is_empty() {
            return true;
        }

        // Pure numeric / date-like content shouldn't be auto-noise even
        // if short — it's often a signal ("$1.2M", "2026-05-07").
        let has_digit = normalized.chars().any(|c| c.is_ascii_digit());
        let trimmed = normalized.trim();
        if !has_digit && trimmed.chars().count() < self.lexicon.min_signal_chars {
            return true;
        }

        if self.lexicon.noise_phrases.iter().any(|p| p == trimmed) {
            return true;
        }
        if self.lexicon.noise_tokens.iter().any(|t| t == trimmed) {
            return true;
        }

        false
    }

    /// Whole-word / whole-phrase substring match.
    ///
    /// `haystack` must already be lower-cased. A `needle` matches if it
    /// appears in `haystack` bounded on both sides by either the
    /// string boundary or a non-alphanumeric character. This avoids
    /// false positives such as `"signed"` matching `"assigned"`.
    fn matches_any(haystack: &str, needles: &[String]) -> bool {
        needles
            .iter()
            .any(|n| Self::contains_whole_word(haystack, n.as_str()))
    }

    /// Find the byte offset of the first whole-word match of any
    /// `needle` in `haystack`. Returns `(start, end)` byte offsets, or
    /// `None` if no match is found. Used by the negation-aware
    /// classification path to know *where* the keyword matched so it
    /// can scan backwards for a negation term.
    fn find_first_match(haystack: &str, needles: &[String]) -> Option<(usize, usize)> {
        needles
            .iter()
            .filter_map(|n| Self::find_whole_word(haystack, n.as_str()))
            .min_by_key(|(start, _)| *start)
    }

    fn contains_whole_word(haystack: &str, needle: &str) -> bool {
        Self::find_whole_word(haystack, needle).is_some()
    }

    /// Find the byte offsets of the first whole-word match of `needle`
    /// in `haystack`.
    fn find_whole_word(haystack: &str, needle: &str) -> Option<(usize, usize)> {
        if needle.is_empty() {
            return None;
        }
        let bytes = haystack.as_bytes();
        let nlen = needle.len();
        let mut i = 0usize;
        while let Some(pos) = haystack[i..].find(needle) {
            let start = i + pos;
            let end = start + nlen;
            let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
            let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return Some((start, end));
            }
            i = start + 1;
        }
        None
    }

    /// Check whether any negation term appears within `negation_window`
    /// characters of `keyword_start` in `haystack` — looking both
    /// **backwards** (before the keyword) and **forwards** (after the
    /// keyword).
    ///
    /// Backwards scan catches: "not approved", "rejected compliance".
    /// Forwards scan catches: "launch was cancelled", "policy was revoked".
    fn has_negation_nearby(&self, haystack: &str, keyword_start: usize, keyword_end: usize) -> bool {
        if self.lexicon.negation_terms.is_empty() || self.lexicon.negation_window == 0 {
            return false;
        }
        // Backwards: scan the substring ending at keyword_start.
        let back_start = keyword_start.saturating_sub(self.lexicon.negation_window);
        let preceding = &haystack[back_start..keyword_start];
        if Self::matches_any(preceding, &self.lexicon.negation_terms) {
            return true;
        }
        // Forwards: scan the substring starting at keyword_end.
        let fwd_end = (keyword_end + self.lexicon.negation_window).min(haystack.len());
        let following = &haystack[keyword_end..fwd_end];
        Self::matches_any(following, &self.lexicon.negation_terms)
    }

    /// Downgrade an [`ImportanceClass`] by one level if a negation term
    /// is found near the keyword match (before or after). Never
    /// downgrades below [`ImportanceClass::Useful`] — Noise is reserved
    /// for the explicit noise-detection path.
    fn apply_negation(
        &self,
        class: ImportanceClass,
        haystack: &str,
        keyword_start: usize,
        keyword_end: usize,
    ) -> ImportanceClass {
        if !self.has_negation_nearby(haystack, keyword_start, keyword_end) {
            return class;
        }
        match class {
            ImportanceClass::Critical => ImportanceClass::Important,
            ImportanceClass::Important => ImportanceClass::Useful,
            other => other,
        }
    }
}

impl ImportanceClassifier for LexiconClassifier {
    fn classify(&self, text: &str) -> ImportanceClass {
        let normalized = text.to_lowercase();

        if self.is_noise(&normalized) {
            return ImportanceClass::Noise;
        }

        // Negation-aware critical keyword check: find the first match,
        // then scan backwards and forwards for a negation term. If
        // negated, downgrade Critical → Important (not to Useful/Noise —
        // a negated critical event is still important, just not critical).
        if let Some((start, end)) = Self::find_first_match(&normalized, &self.lexicon.critical_keywords) {
            let class = ImportanceClass::Critical;
            return self.apply_negation(class, &normalized, start, end);
        }

        // Negation-aware important keyword check: same approach,
        // downgrades Important → Useful.
        if let Some((start, end)) = Self::find_first_match(&normalized, &self.lexicon.important_keywords) {
            let class = ImportanceClass::Important;
            return self.apply_negation(class, &normalized, start, end);
        }

        ImportanceClass::Useful
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_detection_on_greetings_and_reactions() {
        let c = LexiconClassifier::english_default();
        assert_eq!(c.classify("hi"), ImportanceClass::Noise);
        assert_eq!(c.classify("hello"), ImportanceClass::Noise);
        assert_eq!(c.classify("good morning"), ImportanceClass::Noise);
        assert_eq!(c.classify("thanks!"), ImportanceClass::Noise);
        assert_eq!(c.classify("+1"), ImportanceClass::Noise);
        assert_eq!(c.classify("👍"), ImportanceClass::Noise);
    }

    #[test]
    fn noise_detection_on_short_strings() {
        let c = LexiconClassifier::english_default();
        assert_eq!(c.classify("ok"), ImportanceClass::Noise);
        assert_eq!(c.classify(""), ImportanceClass::Noise);
        assert_eq!(c.classify("  "), ImportanceClass::Noise);
    }

    #[test]
    fn short_numeric_content_is_not_auto_noise() {
        let c = LexiconClassifier::english_default();
        // Has a digit, and isn't in the noise lists — should be Useful.
        assert_eq!(c.classify("$1.2M"), ImportanceClass::Useful);
    }

    #[test]
    fn critical_detection() {
        let c = LexiconClassifier::english_default();
        assert_eq!(
            c.classify("This change is required by our compliance policy."),
            ImportanceClass::Critical
        );
        assert_eq!(
            c.classify("Budget confirmed for FY27 expansion."),
            ImportanceClass::Critical
        );
        assert_eq!(
            c.classify("Legal hold issued on the marketing channel."),
            ImportanceClass::Critical
        );
    }

    #[test]
    fn important_detection() {
        let c = LexiconClassifier::english_default();
        assert_eq!(
            c.classify("Friday is the deadline for the migration."),
            ImportanceClass::Important
        );
        assert_eq!(
            c.classify("This task is assigned to Anna."),
            ImportanceClass::Important
        );
        assert_eq!(
            c.classify("Launch is set for next quarter."),
            ImportanceClass::Important
        );
    }

    #[test]
    fn default_useful_for_normal_text() {
        let c = LexiconClassifier::english_default();
        assert_eq!(
            c.classify("Let's revisit the dashboard design tomorrow."),
            ImportanceClass::Useful
        );
    }

    #[test]
    fn custom_lexicon_overrides() {
        // Verify the lexicon really is configurable: a tenant lexicon
        // with no critical keywords should never produce Critical.
        let lexicon = Lexicon::new(vec!["noise"], vec!["just noise"], vec![], vec!["urgent"], 5);
        let c = LexiconClassifier::new(lexicon);
        assert_eq!(
            c.classify("This change is required by our compliance policy."),
            ImportanceClass::Useful
        );
        assert_eq!(
            c.classify("This is urgent please."),
            ImportanceClass::Important
        );
        assert_eq!(c.classify("noise"), ImportanceClass::Noise);
        assert_eq!(c.classify("just noise"), ImportanceClass::Noise);
    }

    #[test]
    fn tag_roundtrip() {
        for class in [
            ImportanceClass::Critical,
            ImportanceClass::Important,
            ImportanceClass::Useful,
            ImportanceClass::Noise,
        ] {
            assert_eq!(ImportanceClass::from_tag(class.as_tag()), Some(class));
        }
        assert_eq!(ImportanceClass::from_tag(99), None);
    }

    // ── Negation-aware classification tests ──

    #[test]
    fn negation_downgrades_critical_to_important() {
        let c = LexiconClassifier::english_default();
        // "not approved" — "approved" is a critical keyword, "not" is a
        // negation term within the 30-char window (backwards scan).
        assert_eq!(
            c.classify("The budget was not approved for this quarter."),
            ImportanceClass::Important
        );
        // "rejected" after "compliance" — critical keyword negated
        // by forward scan.
        assert_eq!(
            c.classify("The compliance request was rejected by the board."),
            ImportanceClass::Important
        );
    }

    #[test]
    fn negation_downgrades_important_to_useful() {
        let c = LexiconClassifier::english_default();
        // "not" before "decision" — important keyword negated.
        assert_eq!(
            c.classify("We made a decision not to go with Vendor X."),
            ImportanceClass::Useful
        );
        // "cancelled" after "launch" — important keyword negated
        // by forward scan.
        assert_eq!(
            c.classify("The launch was cancelled due to budget cuts."),
            ImportanceClass::Useful
        );
        // "no longer" before "assigned to" — important keyword negated.
        assert_eq!(
            c.classify("Sarah is no longer assigned to this project."),
            ImportanceClass::Useful
        );
    }

    #[test]
    fn negation_does_not_affect_non_negated_keywords() {
        let c = LexiconClassifier::english_default();
        // "not" is far from "deadline" — outside the 30-char window.
        assert_eq!(
            c.classify("Noted. The deadline for the migration is next Friday, please ensure all teams are ready."),
            ImportanceClass::Important
        );
        // No negation term at all.
        assert_eq!(
            c.classify("The deadline is tomorrow."),
            ImportanceClass::Important
        );
        // "approved" without any negation.
        assert_eq!(
            c.classify("The budget was approved unanimously."),
            ImportanceClass::Critical
        );
    }

    #[test]
    fn negation_window_is_respected() {
        let c = LexiconClassifier::english_default();
        // "not" is far from "deadline" — outside the 30-char window.
        // Construct a sentence where "not" is >30 chars before "deadline".
        let text = "not relevant anymore, moving on to other things, the deadline is tomorrow";
        // "not" at pos 0, "deadline" at pos 56 — well outside 30-char window.
        assert_eq!(
            c.classify(text),
            ImportanceClass::Important
        );
        // "not" close to "deadline" — within window.
        let text_close = "not the deadline we wanted";
        assert_eq!(
            c.classify(text_close),
            ImportanceClass::Useful
        );
    }

    #[test]
    fn negation_never_downgrades_below_useful() {
        let c = LexiconClassifier::english_default();
        // Even with negation, Important → Useful (not Noise).
        assert_eq!(
            c.classify("The launch was cancelled."),
            ImportanceClass::Useful
        );
        // Critical → Important (not Useful or Noise).
        assert_eq!(
            c.classify("The policy was revoked."),
            ImportanceClass::Important
        );
    }

    #[test]
    fn negation_with_custom_lexicon() {
        let lexicon = Lexicon::with_negation(
            vec!["noise"],
            vec!["just noise"],
            vec!["approved"],
            vec!["deadline"],
            5,
            vec!["not", "cancelled", "否决"],
            25,
        );
        let c = LexiconClassifier::new(lexicon);
        // Custom negation works — "not" before "approved" (backwards).
        assert_eq!(
            c.classify("The proposal was not approved."),
            ImportanceClass::Important
        );
        // "cancelled" after "deadline" (forwards).
        assert_eq!(
            c.classify("The deadline was cancelled by the PM."),
            ImportanceClass::Useful
        );
        // CJK negation term before "approved".
        assert_eq!(
            c.classify("否决 the approved budget"),
            ImportanceClass::Important
        );
    }

    #[test]
    fn negation_terms_empty_means_no_negation() {
        // Lexicon::new (without with_negation) has empty negation_terms.
        let lexicon = Lexicon::new(
            vec!["noise"], vec!["just noise"],
            vec!["approved"], vec!["deadline"], 5,
        );
        let c = LexiconClassifier::new(lexicon);
        // "not approved" should still be Critical — no negation detection.
        assert_eq!(
            c.classify("The budget was not approved."),
            ImportanceClass::Critical
        );
    }

    #[test]
    fn negation_detects_multiple_negation_terms() {
        let c = LexiconClassifier::english_default();
        // "cancelled" before "launch".
        assert_eq!(
            c.classify("The product launch was cancelled yesterday."),
            ImportanceClass::Useful
        );
        // "denied" before "approved".
        assert_eq!(
            c.classify("The board denied the approved budget request."),
            ImportanceClass::Important  // "approved" is critical, negated → Important
        );
        // "on hold" before "decision".
        assert_eq!(
            c.classify("The decision is on hold pending review."),
            ImportanceClass::Useful
        );
        // "postponed" before "launch".
        assert_eq!(
            c.classify("The launch was postponed to next quarter."),
            ImportanceClass::Useful
        );
        // "scrapped" before "release".
        assert_eq!(
            c.classify("The release was scrapped by the team."),
            ImportanceClass::Useful
        );
    }

    #[test]
    fn negation_preserves_noise_classification() {
        let c = LexiconClassifier::english_default();
        // Noise detection runs before keyword/negation checks.
        assert_eq!(c.classify("hi"), ImportanceClass::Noise);
        assert_eq!(c.classify("+1"), ImportanceClass::Noise);
        assert_eq!(c.classify("ok"), ImportanceClass::Noise);
    }

    #[test]
    fn negation_with_critical_and_important_both_present() {
        let c = LexiconClassifier::english_default();
        // Both "approved" (critical) and "deadline" (important) present.
        // "approved" is checked first; if not negated, returns Critical.
        assert_eq!(
            c.classify("The deadline is set, and the budget was approved."),
            ImportanceClass::Critical
        );
        // "not" negates "approved" (critical → important), but "deadline"
        // is also present and not negated → important. Since critical
        // path is checked first and negated to Important, that's the
        // result.
        assert_eq!(
            c.classify("The deadline is set, but the budget was not approved."),
            ImportanceClass::Important
        );
    }

    #[test]
    fn negation_window_zero_disables_negation() {
        let lexicon = Lexicon::with_negation(
            vec!["noise"], vec!["just noise"],
            vec!["approved"], vec!["deadline"],
            5,
            vec!["not"],
            0,  // zero window → negation disabled
        );
        let c = LexiconClassifier::new(lexicon);
        assert_eq!(
            c.classify("The budget was not approved."),
            ImportanceClass::Critical
        );
    }
}
