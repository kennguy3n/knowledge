//! Deterministic post-synthesis quality scoring and adaptive token
//! budgeting for the SLM-backed [`crate::LlamaCppSynthesizer`].
//!
//! # Why this exists
//!
//! A 2-bit-quantised on-device model (Bonsai-Ternary-1.7B) is prone to
//! a specific failure: instead of emitting the requested
//! [`SummaryBundle`] it prefaces (or replaces) the recap with
//! meta-commentary — `"The session highlights…"`, `"This summary
//! covers…"`. The GBNF grammar guarantees the *shape* of the JSON but
//! cannot police whether the `recap` is a faithful condensation or a
//! description of the task. This module supplies a **deterministic**
//! (no clock, no RNG, no allocation-order dependence) quality check the
//! synthesizer runs on the parsed bundle so it can detect that failure
//! and retry once with a larger budget, keeping whichever attempt
//! scores better by the very same function.
//!
//! Determinism matters: the same `(bundle, inputs)` must always yield
//! the same [`QualityReport`] so the verify-and-retry decision is itself
//! reproducible, matching the byte-reproducible sampling preset the
//! router now sends (see `inference_router::SamplingConfig`).

use crate::pipeline::SynthesisInputs;
use crate::schema::SummaryBundle;

/// Recap openers that mark meta-commentary rather than a factual
/// headline. Compared case-insensitively against the trimmed recap.
/// Kept ASCII-lowercase; the comparison lowercases the recap prefix so
/// `"The session"` and `"THE SESSION"` both trip.
pub const META_COMMENTARY_OPENERS: &[&str] = &[
    "the session",
    "the following",
    "this summary",
    "this session",
    "in summary",
    "this recap",
];

/// Minimum acceptable recap length in Unicode scalar values. A recap
/// shorter than this is almost always a truncated or empty emission
/// rather than a 2-4 sentence headline. Deliberately low so a terse but
/// legitimate one-line recap (`"Picked vendor X."`) is not flagged.
pub const MIN_RECAP_CHARS: usize = 12;

/// Minimum number of salient evidence terms before the optional
/// term-coverage check engages. Below this the evidence is too small for
/// coverage to be a meaningful signal (a two-row window can be faithfully
/// recapped without reusing any salient token), so the check is skipped
/// to avoid spurious retries.
pub const MIN_SALIENT_TERMS_FOR_COVERAGE: usize = 4;

/// Fraction of salient evidence terms the recap must mention before the
/// optional coverage check stops flagging it. Low on purpose — coverage
/// is a weak signal used only to catch a recap that ignores the evidence
/// entirely.
pub const MIN_TERM_COVERAGE: f64 = 0.10;

/// Minimum length, in Unicode scalar values, of a token that counts as
/// "salient" for the coverage signal. Filters stop-word-sized tokens
/// (`the`, `a`, `to`) without a language-specific stop list, keeping the
/// check multilingual-safe.
pub const MIN_SALIENT_TERM_LEN: usize = 4;

/// Outcome of scoring one [`SummaryBundle`] against its evidence.
///
/// `score` is a deterministic, signed integer where **higher is
/// better**; the synthesizer compares two attempts' scores to keep the
/// better bundle. The boolean flags explain *why* a bundle is
/// low-quality and drive the retry decision and the low-quality metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityReport {
    /// Deterministic quality score (higher is better). Combines recap
    /// length, structured-field richness, and salient-term coverage,
    /// minus penalties for each tripped flag.
    pub score: i64,
    /// Recap length in Unicode scalar values (the recap-length metric
    /// signal).
    pub recap_chars: usize,
    /// Recap opens with a known meta-commentary phrase.
    pub meta_commentary: bool,
    /// Recap is shorter than [`MIN_RECAP_CHARS`].
    pub too_short: bool,
    /// The coverage check engaged and the recap fell below
    /// [`MIN_TERM_COVERAGE`].
    pub low_coverage: bool,
}

impl QualityReport {
    /// `true` when any quality flag tripped — the bundle should trigger a
    /// verify-and-retry second attempt.
    #[must_use]
    pub const fn is_low_quality(&self) -> bool {
        self.meta_commentary || self.too_short || self.low_coverage
    }
}

/// Score a parsed [`SummaryBundle`] against the evidence it was
/// synthesised from. Pure and deterministic.
#[must_use]
pub fn score_bundle(bundle: &SummaryBundle, inputs: &SynthesisInputs) -> QualityReport {
    let recap = bundle.recap.trim();
    let recap_lower = recap.to_lowercase();
    let recap_chars = recap.chars().count();

    let meta_commentary = META_COMMENTARY_OPENERS
        .iter()
        .any(|opener| recap_lower.starts_with(opener));
    let too_short = recap_chars < MIN_RECAP_CHARS;

    let salient = salient_terms(inputs);
    let covered = salient
        .iter()
        .filter(|term| recap_lower.contains(term.as_str()))
        .count();
    let coverage_active = salient.len() >= MIN_SALIENT_TERMS_FOR_COVERAGE;
    // `f64::from(u32)` is lossless (no precision-loss cast); the counts
    // are tiny, so the `try_from` fallback never triggers in practice.
    let coverage = if salient.is_empty() {
        1.0
    } else {
        let covered_u = u32::try_from(covered).unwrap_or(u32::MAX);
        let total_u = u32::try_from(salient.len()).unwrap_or(u32::MAX);
        f64::from(covered_u) / f64::from(total_u)
    };
    let low_coverage = coverage_active && coverage < MIN_TERM_COVERAGE;

    // Reward signal: recap length (capped so a rambling recap cannot
    // out-score a tight one), populated structured fields, and covered
    // salient terms. Counts are capped before the (infallible after the
    // cap) `i64` conversion so no lossy cast is possible.
    let structured =
        bundle.decisions.len() + bundle.open_questions.len() + bundle.active_tasks.len();
    let recap_reward = i64::try_from(recap_chars.min(280)).unwrap_or(280);
    let structured_reward = i64::try_from(structured.min(1_000)).unwrap_or(1_000) * 8;
    let covered_reward = i64::try_from(covered.min(1_000)).unwrap_or(1_000) * 12;
    let mut score: i64 = recap_reward + structured_reward + covered_reward;

    // Penalties: heavy for meta-commentary (the dominant failure we are
    // hunting) so a clean retry always out-scores it; lighter for the
    // weaker length/coverage signals.
    if meta_commentary {
        score -= 500;
    }
    if too_short {
        score -= 200;
    }
    if low_coverage {
        score -= 50;
    }

    QualityReport {
        score,
        recap_chars,
        meta_commentary,
        too_short,
        low_coverage,
    }
}

/// Extract the deduplicated set of salient, lowercased terms from the
/// evidence (observation contents + recap seed). A salient term is an
/// alphanumeric token of at least [`MIN_SALIENT_TERM_LEN`] scalar
/// values. Order-independent and deterministic.
fn salient_terms(inputs: &SynthesisInputs) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let push_from = |text: &str, terms: &mut Vec<String>| {
        for raw in text.split(|c: char| !c.is_alphanumeric()) {
            if raw.chars().count() < MIN_SALIENT_TERM_LEN {
                continue;
            }
            let term = raw.to_lowercase();
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
    };
    for row in &inputs.observations {
        push_from(&row.content, &mut terms);
    }
    push_from(&inputs.recap_seed, &mut terms);
    terms
}

/// Floor / default `n_predict` token budget — never go below this so a
/// short window still gets a full headline. Matches the historical
/// `DEFAULT_N_PREDICT`.
pub const MIN_N_PREDICT: u32 = 512;

/// First-attempt ceiling. Bounded well under the substrate synthesis
/// deadline so a large window cannot run the generation long enough to
/// trip the gateway timeout (the prior cause of 502s).
pub const MAX_N_PREDICT: u32 = 1024;

/// Retry ceiling — strictly above [`MAX_N_PREDICT`] so the verify-and-
/// retry second attempt always gets a larger budget, while still staying
/// under the deadline.
pub const RETRY_N_PREDICT: u32 = 1536;

/// Extra token budget granted per observation row, added to
/// [`MIN_N_PREDICT`] and then clamped to [`MAX_N_PREDICT`].
pub const TOKENS_PER_ROW: u32 = 24;

/// Compute the adaptive first-attempt `n_predict` budget for a window
/// with `row_count` observation rows: `MIN + row_count * TOKENS_PER_ROW`,
/// clamped to `[MIN_N_PREDICT, MAX_N_PREDICT]`.
#[must_use]
pub fn adaptive_budget(row_count: usize) -> u32 {
    let rows = u32::try_from(row_count).unwrap_or(u32::MAX);
    let scaled = MIN_N_PREDICT.saturating_add(rows.saturating_mul(TOKENS_PER_ROW));
    scaled.clamp(MIN_N_PREDICT, MAX_N_PREDICT)
}

/// The retry budget for a window whose first attempt used `first_budget`.
/// Always strictly larger than the first attempt (a retry exists to give
/// the model more room) and capped at [`RETRY_N_PREDICT`].
#[must_use]
pub fn retry_budget(first_budget: u32) -> u32 {
    RETRY_N_PREDICT.max(first_budget.saturating_add(TOKENS_PER_ROW))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ImportanceTagClass, ObservationRow, ObservationRowKind};

    fn inputs_with(rows: &[(&str,)]) -> SynthesisInputs {
        SynthesisInputs {
            observations: rows
                .iter()
                .map(|(content,)| ObservationRow {
                    kind: ObservationRowKind::Fact,
                    content: (*content).to_string(),
                    importance: ImportanceTagClass::Important,
                    confidence: 0.9,
                })
                .collect(),
            recap_seed: String::new(),
        }
    }

    fn bundle(recap: &str) -> SummaryBundle {
        SummaryBundle {
            recap: recap.to_string(),
            ..SummaryBundle::default()
        }
    }

    #[test]
    fn detects_meta_commentary_openers() {
        for opener in META_COMMENTARY_OPENERS {
            let recap = format!("{opener} highlights several decisions and tasks.");
            let report = score_bundle(&bundle(&recap), &SynthesisInputs::default());
            assert!(
                report.meta_commentary,
                "opener `{opener}` must be flagged as meta-commentary"
            );
            assert!(report.is_low_quality());
        }
    }

    #[test]
    fn meta_commentary_is_case_insensitive() {
        let report = score_bundle(
            &bundle("THE SESSION covered the migration plan in detail."),
            &SynthesisInputs::default(),
        );
        assert!(report.meta_commentary);
    }

    #[test]
    fn factual_recap_is_not_flagged() {
        let report = score_bundle(
            &bundle("Adopted Postgres and scheduled the staging migration for Friday."),
            &SynthesisInputs::default(),
        );
        assert!(!report.is_low_quality(), "got {report:?}");
    }

    #[test]
    fn short_recap_is_flagged_too_short() {
        let report = score_bundle(&bundle("ok"), &SynthesisInputs::default());
        assert!(report.too_short);
        assert!(report.is_low_quality());
    }

    #[test]
    fn terse_one_liner_is_not_too_short() {
        let report = score_bundle(&bundle("Picked vendor X."), &SynthesisInputs::default());
        assert!(!report.too_short, "got {report:?}");
    }

    #[test]
    fn clean_retry_outscores_meta_commentary() {
        let inputs = inputs_with(&[("chose vendor X",), ("sign by Friday",)]);
        let meta = score_bundle(
            &bundle("The session highlights the vendor decision."),
            &inputs,
        );
        let clean = score_bundle(
            &bundle("Chose vendor X and committed to sign the contract by Friday."),
            &inputs,
        );
        assert!(meta.is_low_quality());
        assert!(!clean.is_low_quality());
        assert!(
            clean.score > meta.score,
            "clean ({}) must out-score meta ({})",
            clean.score,
            meta.score
        );
    }

    #[test]
    fn coverage_engages_only_with_enough_salient_terms() {
        // Four salient terms, recap mentions none -> low coverage.
        let inputs = inputs_with(&[("migration",), ("postgres",), ("billing",), ("staging",)]);
        let ignored = score_bundle(&bundle("Everyone agreed on the next steps."), &inputs);
        assert!(ignored.low_coverage, "got {ignored:?}");

        // Too few salient terms -> coverage check stays off.
        let tiny = inputs_with(&[("postgres",)]);
        let report = score_bundle(&bundle("Everyone agreed on the next steps."), &tiny);
        assert!(!report.low_coverage, "got {report:?}");
    }

    #[test]
    fn adaptive_budget_scales_and_clamps() {
        assert_eq!(adaptive_budget(0), MIN_N_PREDICT);
        assert_eq!(adaptive_budget(4), MIN_N_PREDICT + 4 * TOKENS_PER_ROW);
        // Large windows clamp at the ceiling rather than blowing the
        // synthesis deadline.
        assert_eq!(adaptive_budget(10_000), MAX_N_PREDICT);
    }

    #[test]
    fn retry_budget_always_exceeds_first_attempt() {
        for rows in [0usize, 4, 32, 10_000] {
            let first = adaptive_budget(rows);
            let retry = retry_budget(first);
            assert!(retry > first, "retry {retry} must exceed first {first}");
            assert!(retry <= RETRY_N_PREDICT);
        }
    }
}
