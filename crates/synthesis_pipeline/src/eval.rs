//! Offline synthesis-quality evaluation primitives.
//!
//! This module is the in-crate counterpart of the offline eval harness in
//! `demos/synthesis-eval/`. The demo scores *recorded* model output (JSON on
//! disk) to publish the per-language report and gate CI; this module provides
//! the same three measurements as pure, deterministic library functions so the
//! synthesizer — and any future quality gate — can grade a recap *in process*,
//! against the very same definitions the demo and the docs use:
//!
//! * [`term_coverage`] — factual/term coverage of a recap against a labeled
//!   expected-terms set.
//! * [`ungrounded_recap_terms`] — faithfulness/grounding: salient recap terms
//!   absent from the session evidence (the recap analogue of
//!   [`crate::ungrounded_entry_count`], which grades the structured lists).
//! * [`recap_in_language`] — in-language correctness via a Unicode script
//!   detector. This closes a real gap: before this module the script detector
//!   lived only in the demo's Python (`demos/multilingual-rollup/run_rollup.py`),
//!   so the shipped pipeline could not tell that a 2-bit model had answered an
//!   Arabic session in English. It now can.
//!
//! Everything here is deterministic (no clock, no RNG, no allocation-order
//! dependence) and allocation-light, matching the determinism contract of
//! [`crate::quality`]. The salient-term notion is shared verbatim with
//! [`crate::salient_terms_from_texts`] so coverage and grounding mean the same
//! thing across the whole crate.

use std::collections::HashSet;

use crate::quality::salient_terms_from_texts;

/// Unicode script family a recap can be written in.
///
/// [`recap_in_language`] tallies characters with an *exhaustive* `match` over
/// these variants rather than by discriminant value, so adding or reordering a
/// variant is a compile error there — the classification can never silently
/// drift out of sync with this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Script {
    /// Latin script (English, French, German, Spanish, Vietnamese, …).
    Latin,
    /// CJK: Han ideographs plus Japanese kana.
    Cjk,
    /// Thai script (spaceless).
    Thai,
    /// Arabic script (right-to-left).
    Arabic,
    /// Devanagari (Hindi, Marathi, …).
    Devanagari,
    /// Any other alphabetic script we do not bucket explicitly.
    Other,
}

impl Script {
    /// Map a language *name* to its script family. Defaults to
    /// [`Script::Latin`] for any not-yet-classified language — the safe
    /// assumption that never over-claims a non-Latin stress case. Mirrors the
    /// `SCRIPTS` table in `demos/multilingual-rollup/run_rollup.py`.
    #[must_use]
    pub fn for_language(language: &str) -> Self {
        match language {
            "Japanese" | "Chinese" => Script::Cjk,
            "Thai" => Script::Thai,
            "Arabic" => Script::Arabic,
            "Hindi" => Script::Devanagari,
            // English / French / German / Spanish / Vietnamese / Indonesian /
            // Portuguese and any unknown language.
            _ => Script::Latin,
        }
    }

    /// `true` for Latin-script languages, which are held to the strict
    /// "no foreign-script letters at all" rule in [`recap_in_language`].
    #[must_use]
    pub const fn is_latin(self) -> bool {
        matches!(self, Script::Latin)
    }
}

/// Classify a single character into a [`Script`], or `None` for non-alphabetic
/// characters (digits, punctuation, whitespace) which carry no language signal.
///
/// Classification is by Unicode code-point block rather than by character name
/// (the std library exposes no name database), but the buckets are identical to
/// the demo's `unicodedata.name()`-based detector.
fn script_of_char(ch: char) -> Option<Script> {
    if !ch.is_alphabetic() {
        return None;
    }
    let c = ch as u32;
    // CJK: Hiragana, Katakana, Han (incl. Extension A), compatibility ideographs.
    if (0x3040..=0x30FF).contains(&c)
        || (0x3400..=0x4DBF).contains(&c)
        || (0x4E00..=0x9FFF).contains(&c)
        || (0xF900..=0xFAFF).contains(&c)
    {
        return Some(Script::Cjk);
    }
    if (0x0E00..=0x0E7F).contains(&c) {
        return Some(Script::Thai);
    }
    // Arabic: core block, supplement, and presentation forms A/B.
    if (0x0600..=0x06FF).contains(&c)
        || (0x0750..=0x077F).contains(&c)
        || (0xFB50..=0xFDFF).contains(&c)
        || (0xFE70..=0xFEFF).contains(&c)
    {
        return Some(Script::Arabic);
    }
    if (0x0900..=0x097F).contains(&c) {
        return Some(Script::Devanagari);
    }
    // Latin: Basic Latin letters, Latin-1 Supplement, Latin Extended-A/B, IPA
    // Extensions, Latin Extended Additional (Vietnamese diacritics), and the
    // Latin Extended-C/D/E blocks — matching the Python detector, whose
    // `unicodedata.name()` reports "LATIN …" across all of these.
    if (0x0041..=0x005A).contains(&c)
        || (0x0061..=0x007A).contains(&c)
        || (0x00C0..=0x02AF).contains(&c)
        || (0x1E00..=0x1EFF).contains(&c)
        || (0x2C60..=0x2C7F).contains(&c)
        || (0xA720..=0xA7FF).contains(&c)
        || (0xAB30..=0xAB6F).contains(&c)
    {
        return Some(Script::Latin);
    }
    Some(Script::Other)
}

/// Honest check that `recap` is written in `expected`'s script, not merely that
/// it is usable text.
///
/// Business tokens (`MySQL`, `Postgres`, `SKU-6310`, `VNPay`) are legitimately
/// Latin even inside a Thai/Arabic/CJK recap, so we compare *alphabetic*
/// character counts by script rather than demanding a pure block:
///
/// * Latin-script languages pass only when **zero** alphabetic characters of
///   another known script (CJK/Thai/Arabic/Devanagari) appear — deliberately
///   strict, so a single stray ideograph fails the recap.
/// * Non-Latin languages pass when the expected script is at least as prevalent
///   as Latin, tolerating embedded Latin product names while still failing a
///   recap that answered, say, an Arabic session in English.
///
/// An empty or placeholder (`"…"`) recap has no alphabetic characters and never
/// counts as in-language. Mirrors `in_language` in run_rollup.py and the demo
/// harness's `scorers.in_language`.
#[must_use]
pub fn recap_in_language(expected: Script, recap: &str) -> bool {
    let (mut latin, mut cjk, mut thai, mut arabic, mut devanagari, mut other) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for ch in recap.chars() {
        // Exhaustive match by variant (not by discriminant index): adding a
        // `Script` variant fails to compile here until it is tallied.
        match script_of_char(ch) {
            Some(Script::Latin) => latin += 1,
            Some(Script::Cjk) => cjk += 1,
            Some(Script::Thai) => thai += 1,
            Some(Script::Arabic) => arabic += 1,
            Some(Script::Devanagari) => devanagari += 1,
            Some(Script::Other) => other += 1,
            None => {}
        }
    }
    if latin + cjk + thai + arabic + devanagari + other == 0 {
        return false;
    }
    if expected.is_latin() {
        // Strict: no alphabetic characters of another *known* script.
        latin > 0 && (cjk + thai + arabic + devanagari) == 0
    } else {
        let expected_count = match expected {
            Script::Latin => latin, // unreachable: handled by is_latin() above
            Script::Cjk => cjk,
            Script::Thai => thai,
            Script::Arabic => arabic,
            Script::Devanagari => devanagari,
            Script::Other => other,
        };
        expected_count >= latin
    }
}

/// Coverage of a recap against a labeled expected-terms set: how many of the
/// `expected` terms the `recap` mentions (case-insensitive substring), out of
/// how many were expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCoverage {
    /// Number of expected terms found in the recap.
    pub matched: usize,
    /// Total number of expected terms.
    pub expected: usize,
}

impl TermCoverage {
    /// Matched fraction in `[0, 1]`. An empty expected set is vacuously fully
    /// covered (`1.0`), matching the demo harness.
    #[must_use]
    pub fn fraction(self) -> f64 {
        if self.expected == 0 {
            return 1.0;
        }
        // Lossless: counts are tiny, well within f64's integer range.
        let matched = u32::try_from(self.matched).unwrap_or(u32::MAX);
        let expected = u32::try_from(self.expected).unwrap_or(u32::MAX);
        f64::from(matched) / f64::from(expected)
    }
}

/// Score a recap's factual/term coverage against a labeled expected-terms set.
///
/// Case-insensitive substring match — identical to the persona demo's
/// `t.lower() in recap.lower()` and the harness's `scorers.term_coverage`, so a
/// recap scores the same here as in the published report.
#[must_use]
pub fn term_coverage(recap: &str, expected_terms: &[&str]) -> TermCoverage {
    let recap_lower = recap.to_lowercase();
    let matched = expected_terms
        .iter()
        .filter(|term| recap_lower.contains(&term.to_lowercase()))
        .count();
    TermCoverage {
        matched,
        expected: expected_terms.len(),
    }
}

/// Salient recap terms that do **not** appear anywhere in the session evidence
/// — the recap-level faithfulness signal.
///
/// "Salient" is the exact notion shared across the crate
/// ([`crate::salient_terms_from_texts`]): alphanumeric tokens of at least
/// [`crate::quality::MIN_SALIENT_TERM_LEN`] scalar values. A recap term is
/// grounded when it is one of the evidence's salient terms. Returned terms are
/// lowercased and de-duplicated in first-seen order, so the result is
/// deterministic.
///
/// This complements [`crate::ungrounded_entry_count`] (which grades the
/// structured `decisions`/`open_questions`/`active_tasks` lists): together they
/// vet the whole bundle — recap *and* lists — against the evidence.
///
/// When the evidence yields no salient terms, grounding cannot be assessed and
/// the result is empty (nothing is claimed ungrounded).
#[must_use]
pub fn ungrounded_recap_terms(recap: &str, evidence: &[&str]) -> Vec<String> {
    let evidence_terms = salient_terms_from_texts(evidence.iter().copied());
    if evidence_terms.is_empty() {
        return Vec::new();
    }
    let evidence_set: HashSet<&str> = evidence_terms.iter().map(String::as_str).collect();
    salient_terms_from_texts(std::iter::once(recap))
        .into_iter()
        .filter(|term| !evidence_set.contains(term.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_to_script() {
        assert_eq!(Script::for_language("French"), Script::Latin);
        assert_eq!(Script::for_language("Japanese"), Script::Cjk);
        assert_eq!(Script::for_language("Chinese"), Script::Cjk);
        assert_eq!(Script::for_language("Thai"), Script::Thai);
        assert_eq!(Script::for_language("Arabic"), Script::Arabic);
        assert_eq!(Script::for_language("Hindi"), Script::Devanagari);
        // Unknown -> Latin (safe default).
        assert_eq!(Script::for_language("Klingon"), Script::Latin);
    }

    #[test]
    fn char_classification() {
        assert_eq!(script_of_char('A'), Some(Script::Latin));
        assert_eq!(script_of_char('é'), Some(Script::Latin));
        assert_eq!(script_of_char('ầ'), Some(Script::Latin)); // Vietnamese
        assert_eq!(script_of_char('ɑ'), Some(Script::Latin)); // IPA Extensions U+0251
        assert_eq!(script_of_char('Ɫ'), Some(Script::Latin)); // Latin Extended-C U+2C62
        assert_eq!(script_of_char('ꜳ'), Some(Script::Latin)); // Latin Extended-D U+A733
        assert_eq!(script_of_char('決'), Some(Script::Cjk)); // Han
        assert_eq!(script_of_char('サ'), Some(Script::Cjk)); // Katakana
        assert_eq!(script_of_char('ก'), Some(Script::Thai));
        assert_eq!(script_of_char('ا'), Some(Script::Arabic));
        assert_eq!(script_of_char('क'), Some(Script::Devanagari));
        // Non-alphabetic carries no signal.
        assert_eq!(script_of_char('7'), None);
        assert_eq!(script_of_char('-'), None);
        assert_eq!(script_of_char(' '), None);
    }

    #[test]
    fn latin_in_language() {
        assert!(recap_in_language(
            Script::Latin,
            "Le litige CartoNord est résolu."
        ));
        // A French recap with stray CJK is not in-language.
        assert!(!recap_in_language(
            Script::Latin,
            "Le litige 決定 est résolu."
        ));
    }

    #[test]
    fn non_latin_in_language() {
        // Embedded Latin product names tolerated inside a CJK recap.
        assert!(recap_in_language(
            Script::Cjk,
            "AX-7サーボの過熱はファームウェアが原因である。"
        ));
        // An Arabic session answered in English fails.
        assert!(!recap_in_language(
            Script::Arabic,
            "The billing database migration from MySQL to Postgres."
        ));
        // …but a genuine Arabic recap passes despite the Latin brand names.
        assert!(recap_in_language(
            Script::Arabic,
            "ترحيل قاعدة بيانات الفوترة من MySQL إلى Postgres."
        ));
    }

    #[test]
    fn empty_recap_never_in_language() {
        assert!(!recap_in_language(Script::Latin, ""));
        assert!(!recap_in_language(Script::Cjk, "…"));
    }

    #[test]
    fn coverage_counts_and_fraction() {
        let cov = term_coverage(
            "The CARTONORD dispute over the avoir.",
            &["cartonord", "avoir", "humidité"],
        );
        assert_eq!(cov.matched, 2);
        assert_eq!(cov.expected, 3);
        assert!((cov.fraction() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn coverage_empty_expected_is_full() {
        assert!((term_coverage("anything", &[]).fraction() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn recap_grounding() {
        // Every salient recap term appears in the evidence.
        let ungrounded = ungrounded_recap_terms(
            "Migrate the billing database to Postgres next sprint.",
            &["Decision: migrate the billing database to Postgres next sprint."],
        );
        assert!(ungrounded.is_empty(), "unexpected: {ungrounded:?}");

        // "oracle" is not in the evidence -> flagged.
        let ungrounded = ungrounded_recap_terms(
            "Migrate the billing database to Oracle next sprint.",
            &["Decision: migrate the billing database to Postgres next sprint."],
        );
        assert_eq!(ungrounded, vec!["oracle".to_string()]);
    }

    #[test]
    fn grounding_without_evidence_is_empty() {
        assert!(ungrounded_recap_terms("anything at all here", &[]).is_empty());
    }
}
