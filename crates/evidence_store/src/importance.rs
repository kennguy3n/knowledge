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
        }
    }

    /// Builder-friendly default lexicon for English chat messages.
    /// Production deployments should override per-tenant.
    pub fn default_english() -> Self {
        Self::new(
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

    fn contains_whole_word(haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
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
                return true;
            }
            i = start + 1;
        }
        false
    }
}

impl ImportanceClassifier for LexiconClassifier {
    fn classify(&self, text: &str) -> ImportanceClass {
        let normalized = text.to_lowercase();

        if self.is_noise(&normalized) {
            return ImportanceClass::Noise;
        }
        if Self::matches_any(&normalized, &self.lexicon.critical_keywords) {
            return ImportanceClass::Critical;
        }
        if Self::matches_any(&normalized, &self.lexicon.important_keywords) {
            return ImportanceClass::Important;
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
}
