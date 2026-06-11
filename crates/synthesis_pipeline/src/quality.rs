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

use std::collections::HashSet;

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
///
/// Convenience wrapper over [`score_bundle_with_terms`] for callers that
/// hold a [`SynthesisInputs`]; it derives the salient-term set from the
/// inputs first. Callers that work with raw evidence text (e.g. the FFI
/// on-device path) should pre-compute terms with
/// [`salient_terms_from_texts`] and call [`score_bundle_with_terms`]
/// directly so the two synthesis paths share one scoring contract.
#[must_use]
pub fn score_bundle(bundle: &SummaryBundle, inputs: &SynthesisInputs) -> QualityReport {
    score_bundle_with_terms(bundle, &salient_terms(inputs))
}

/// Score a parsed [`SummaryBundle`] against a pre-computed set of
/// `salient` evidence terms (see [`salient_terms_from_texts`]). Pure and
/// deterministic — the same `(bundle, salient)` always yields the same
/// [`QualityReport`], so the verify-and-retry decision is reproducible.
#[must_use]
pub fn score_bundle_with_terms(bundle: &SummaryBundle, salient: &[String]) -> QualityReport {
    let recap = bundle.recap.trim();
    let recap_lower = recap.to_lowercase();
    let recap_chars = recap.chars().count();

    let meta_commentary = META_COMMENTARY_OPENERS
        .iter()
        .any(|opener| recap_lower.starts_with(opener));
    let too_short = recap_chars < MIN_RECAP_CHARS;

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

/// Extract the deduplicated set of salient, lowercased terms from an
/// arbitrary sequence of evidence texts. A salient term is an
/// alphanumeric token of at least [`MIN_SALIENT_TERM_LEN`] scalar
/// values. Deterministic: first-seen order is preserved and duplicates
/// are dropped, so the same texts always yield the same set.
///
/// This is the evidence-agnostic core shared by the
/// [`crate::LlamaCppSynthesizer`] (which feeds observation contents) and
/// the FFI on-device channel-recap path (which feeds decrypted evidence
/// bodies) so both score recaps against the same notion of "salient".
///
/// Tokenisation is deliberately language-agnostic (a split on
/// non-alphanumeric scalar values) so it carries no per-language word
/// list. For scripts without inter-word spaces (e.g. CJK) this yields
/// coarser, longer runs rather than individual words; coverage scoring
/// then matches them as substrings, which is acceptable because coverage
/// is only a weak, retry-nudging signal (see [`MIN_TERM_COVERAGE`]) and
/// the substring match still succeeds for matching recap text — it never
/// produces a false *negative*.
#[must_use]
pub fn salient_terms_from_texts<'a, I>(texts: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut terms: Vec<String> = Vec::new();
    // O(1)-average dedup index. The ordered `terms` Vec is the source of
    // truth for output order (first-seen preserved, per the reproducibility
    // contract); `seen` is only a membership set and is never iterated, so
    // its non-deterministic iteration order cannot leak into the result.
    let mut seen: HashSet<String> = HashSet::new();
    for text in texts {
        for raw in text.split(|c: char| !c.is_alphanumeric()) {
            if raw.chars().count() < MIN_SALIENT_TERM_LEN {
                continue;
            }
            let term = raw.to_lowercase();
            if seen.insert(term.clone()) {
                terms.push(term);
            }
        }
    }
    terms
}

/// Salient terms from a [`SynthesisInputs`]: every observation's content
/// followed by the recap seed.
fn salient_terms(inputs: &SynthesisInputs) -> Vec<String> {
    salient_terms_from_texts(
        inputs
            .observations
            .iter()
            .map(|row| row.content.as_str())
            .chain(std::iter::once(inputs.recap_seed.as_str())),
    )
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
/// under the deadline. A **hard** upper bound: [`retry_budget`] never
/// returns more than this for *any* input.
pub const RETRY_N_PREDICT: u32 = 1536;

/// Extra token budget granted per observation row, added to
/// [`MIN_N_PREDICT`] and then clamped to [`MAX_N_PREDICT`].
pub const TOKENS_PER_ROW: u32 = 24;

/// Headroom added to the first-attempt budget on a retry, before the
/// [`RETRY_N_PREDICT`] cap. Sized so the retry of a floor-budget first
/// attempt (`MIN_N_PREDICT = 512`) lands at `1024` and a ceiling-budget
/// first attempt (`MAX_N_PREDICT = 1024`) saturates the cap at `1536` —
/// the retry always gets strictly more room than the first attempt for
/// every budget [`adaptive_budget`] can produce, without ever exceeding
/// the deadline-safe ceiling.
pub const RETRY_BUDGET_BONUS: u32 = 512;

/// Compute the adaptive first-attempt `n_predict` budget for a window
/// with `row_count` observation rows: `MIN + row_count * TOKENS_PER_ROW`,
/// clamped to `[MIN_N_PREDICT, MAX_N_PREDICT]`.
///
/// This intentionally **owns** the synthesis token budget rather than
/// reading the host's `KNOWLEDGE_SLM_N_PREDICT` value (which still governs
/// the plain `dispatch()` classification/extraction tasks): synthesis
/// deadline safety — never running generation long enough to trip the
/// substrate synthesis deadline, the prior cause of 502s — must not be
/// defeated by an operator setting an arbitrarily large budget. The
/// `MIN_N_PREDICT` floor equals the env default (`512`), so a host that
/// left the default sees no change; only a host that raised the env var
/// above `512` and expected synthesis to inherit it is affected, which is
/// documented in `docs/technical/inference-routing.md`.
#[must_use]
pub fn adaptive_budget(row_count: usize) -> u32 {
    let rows = u32::try_from(row_count).unwrap_or(u32::MAX);
    let scaled = MIN_N_PREDICT.saturating_add(rows.saturating_mul(TOKENS_PER_ROW));
    scaled.clamp(MIN_N_PREDICT, MAX_N_PREDICT)
}

/// The retry budget for a window whose first attempt used `first_budget`:
/// `first_budget + RETRY_BUDGET_BONUS`, **capped** at [`RETRY_N_PREDICT`].
///
/// The cap is a hard upper bound — the result never exceeds
/// [`RETRY_N_PREDICT`] for any input, so a retry can never run the
/// generation past the deadline-safe ceiling (the prior cause of 502s).
/// For every first-attempt budget [`adaptive_budget`] can produce
/// (`[MIN_N_PREDICT, MAX_N_PREDICT]`) the bonus keeps the retry strictly
/// larger than the first attempt while staying under the cap.
#[must_use]
pub fn retry_budget(first_budget: u32) -> u32 {
    first_budget
        .saturating_add(RETRY_BUDGET_BONUS)
        .min(RETRY_N_PREDICT)
}

/// Suffix appended to the prompt on the verify-and-retry second attempt.
/// Reinforces the fact-only instruction after a first attempt that
/// drifted into meta-commentary. Shared by every synthesis path so the
/// retry prompt is identical on-device and server-side.
pub const RETRY_SUFFIX: &str = "\n\nSecond attempt — output only facts, no preface.";

/// One SLM attempt's parsed result: the bundle plus whether the strict
/// JSON parse failed and the salvage parser had to recover a usable
/// prefix (see [`truncated`](Self::truncated) for the precise semantics).
#[derive(Debug, Clone)]
pub struct Attempt {
    /// The parsed bundle (strict parse or salvaged prefix).
    pub bundle: SummaryBundle,
    /// `true` when the strict parse failed and the prefix-closing salvage
    /// (`SummaryBundle::from_slm_str_salvaged`) recovered a bundle.
    ///
    /// Strictly this flags *any* strict-parse failure that salvage could
    /// recover, not solely a token-cap truncation. But under the enforced
    /// GBNF grammar the only realistic cause of a salvageable strict-parse
    /// failure is the `n_predict` cap cutting the output off mid-emission;
    /// a non-truncation parse failure would require a server-side grammar
    /// bug. So it is the truncation metric signal in practice — feeding
    /// `synthesis_truncated_total` — with that rare edge as documented
    /// over-count rather than a silent miscount.
    pub truncated: bool,
}

/// Outcome of [`verify_and_retry`]: the kept bundle plus the metric
/// signals the caller records into its own telemetry backend (FFI
/// counters on-device, [`crate::SynthesisMetrics`] in the pipeline).
#[derive(Debug, Clone)]
pub struct VerifiedSynthesis {
    /// The bundle to persist — the better-scoring of the (up to) two
    /// attempts.
    pub bundle: SummaryBundle,
    /// Recap length, in scalar values, of the kept bundle.
    pub recap_chars: usize,
    /// The first attempt tripped a quality flag, so a retry was run.
    pub low_quality: bool,
    /// A second attempt was dispatched (always equals `low_quality`;
    /// retained as a distinct signal for the retry counter).
    pub retried: bool,
    /// The retry was dispatched but **errored**, so the first (mediocre
    /// but usable) bundle was kept rather than failing the synthesis.
    /// Surfaced as a distinct signal — rather than swallowed inside this
    /// pure function — so the caller can emit a diagnostic (a
    /// `tracing::warn!` and/or a metric) for a flaky adapter that fails
    /// only on the retry path. Always `false` unless `retried` is `true`.
    pub retry_failed: bool,
    /// How many of the dispatched attempts were salvaged from truncated
    /// output (0, 1, or 2) — added to the truncation counter.
    pub truncated_attempts: u8,
}

/// Run the deterministic verify-and-retry synthesis policy, sharing one
/// implementation across every synthesis path.
///
/// `run(prompt, n_predict)` performs a single SLM dispatch + parse and
/// returns an [`Attempt`]. This closure owns the transport (router
/// dispatch, salvage parsing); the policy here owns the *decision*:
///
/// 1. First attempt at [`adaptive_budget(row_count)`](adaptive_budget).
/// 2. Score it against `salient` ([`score_bundle_with_terms`]).
/// 3. If a quality flag tripped, retry **once** at
///    [`retry_budget`] with [`RETRY_SUFFIX`] appended, and keep whichever
///    attempt scores higher (ties keep the retry, which used the larger
///    budget). A retry that *errors* keeps the first bundle rather than
///    failing the whole synthesis.
///
/// Capped at one retry to bound latency. Pure given a deterministic
/// `run`, so the kept bundle is reproducible.
///
/// # Errors
///
/// Propagates the error from the **first** attempt only; a failed retry
/// is swallowed (the first bundle is kept).
pub fn verify_and_retry<E>(
    base_prompt: &str,
    row_count: usize,
    salient: &[String],
    mut run: impl FnMut(&str, u32) -> Result<Attempt, E>,
) -> Result<VerifiedSynthesis, E> {
    let first_budget = adaptive_budget(row_count);
    let first = run(base_prompt, first_budget)?;
    let mut truncated_attempts = u8::from(first.truncated);
    let first_report = score_bundle_with_terms(&first.bundle, salient);

    if !first_report.is_low_quality() {
        return Ok(VerifiedSynthesis {
            recap_chars: first_report.recap_chars,
            bundle: first.bundle,
            low_quality: false,
            retried: false,
            retry_failed: false,
            truncated_attempts,
        });
    }

    // Verify-and-retry: one larger-budget, fact-only second attempt.
    let retry_prompt = format!("{base_prompt}{RETRY_SUFFIX}");
    let verified = match run(&retry_prompt, retry_budget(first_budget)) {
        Ok(second) => {
            truncated_attempts += u8::from(second.truncated);
            let second_report = score_bundle_with_terms(&second.bundle, salient);
            // Ties keep the retry, which used the larger budget.
            if second_report.score >= first_report.score {
                VerifiedSynthesis {
                    recap_chars: second_report.recap_chars,
                    bundle: second.bundle,
                    low_quality: true,
                    retried: true,
                    retry_failed: false,
                    truncated_attempts,
                }
            } else {
                VerifiedSynthesis {
                    recap_chars: first_report.recap_chars,
                    bundle: first.bundle,
                    low_quality: true,
                    retried: true,
                    retry_failed: false,
                    truncated_attempts,
                }
            }
        }
        // A failed retry must not turn a usable (if mediocre) first
        // bundle into a hard failure — keep the first, but flag
        // `retry_failed` so the caller can emit a diagnostic.
        Err(_) => VerifiedSynthesis {
            recap_chars: first_report.recap_chars,
            bundle: first.bundle,
            low_quality: true,
            retried: true,
            retry_failed: true,
            truncated_attempts,
        },
    };
    Ok(verified)
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

    #[test]
    fn retry_budget_caps_at_ceiling_for_any_input() {
        // The ceiling is a HARD upper bound: even a (contractually
        // out-of-range) first budget far above the cap, and `u32::MAX`,
        // must never produce a retry budget over `RETRY_N_PREDICT` — a
        // retry can never blow the synthesis deadline.
        for first in [0, MIN_N_PREDICT, MAX_N_PREDICT, 1_512, 5_000, u32::MAX] {
            assert!(
                retry_budget(first) <= RETRY_N_PREDICT,
                "retry_budget({first}) = {} exceeded ceiling {RETRY_N_PREDICT}",
                retry_budget(first),
            );
        }
        // `first_budget` genuinely influences the result below the cap
        // (no dead-code branch): a larger first attempt yields a larger
        // retry until the cap saturates.
        assert_eq!(
            retry_budget(MIN_N_PREDICT),
            MIN_N_PREDICT + RETRY_BUDGET_BONUS
        );
        assert!(retry_budget(MIN_N_PREDICT) < retry_budget(MAX_N_PREDICT));
        assert_eq!(retry_budget(MAX_N_PREDICT), RETRY_N_PREDICT);
    }

    fn slm_bundle(recap: &str) -> Attempt {
        Attempt {
            bundle: bundle(recap),
            truncated: false,
        }
    }

    #[test]
    fn verify_and_retry_keeps_clean_first_without_retry() {
        let salient: Vec<String> = Vec::new();
        let mut calls = 0u32;
        let out = verify_and_retry("PROMPT", 2, &salient, |_prompt, _n| {
            calls += 1;
            Ok::<_, ()>(slm_bundle(
                "Adopted Postgres and scheduled the staging migration for Friday.",
            ))
        })
        .expect("clean first attempt");
        assert_eq!(calls, 1, "a clean first attempt must NOT retry");
        assert!(!out.retried);
        assert!(!out.low_quality);
        assert!(!out.retry_failed);
        assert_eq!(out.truncated_attempts, 0);
    }

    #[test]
    fn verify_and_retry_retries_and_keeps_better_bundle() {
        let salient: Vec<String> = Vec::new();
        let mut calls = 0u32;
        // First attempt trips meta-commentary; the retry is clean and
        // must win on score.
        let out = verify_and_retry("PROMPT", 3, &salient, |prompt, _n| {
            calls += 1;
            if prompt.contains(RETRY_SUFFIX) {
                Ok::<_, ()>(slm_bundle(
                    "Chose vendor X and committed to signing the contract by Friday.",
                ))
            } else {
                Ok::<_, ()>(slm_bundle("The session highlights the vendor decision."))
            }
        })
        .expect("retry path");
        assert_eq!(calls, 2, "a low-quality first attempt must retry once");
        assert!(out.retried);
        assert!(out.low_quality);
        assert!(
            !out.retry_failed,
            "a retry that succeeded must not set retry_failed"
        );
        assert!(
            !out.bundle.recap.to_lowercase().starts_with("the session"),
            "the clean retry must be kept, got `{}`",
            out.bundle.recap
        );
    }

    #[test]
    fn verify_and_retry_failed_retry_keeps_first_bundle() {
        let salient: Vec<String> = Vec::new();
        let mut calls = 0u32;
        let out = verify_and_retry("PROMPT", 1, &salient, |prompt, _n| {
            calls += 1;
            if prompt.contains(RETRY_SUFFIX) {
                Err("retry dispatch failed")
            } else {
                Ok(slm_bundle("The following summary covers the migration."))
            }
        })
        .expect("a failed retry must not fail the whole synthesis");
        assert_eq!(calls, 2);
        assert!(out.retried);
        assert!(
            out.retry_failed,
            "an errored retry must set retry_failed so the caller can diagnose it"
        );
        // The (mediocre) first bundle is preserved rather than erroring.
        assert!(out.bundle.recap.to_lowercase().starts_with("the following"));
    }

    #[test]
    fn verify_and_retry_propagates_first_attempt_error() {
        let salient: Vec<String> = Vec::new();
        let result = verify_and_retry::<&str>("PROMPT", 1, &salient, |_prompt, _n| {
            Err("first dispatch failed")
        });
        assert_eq!(result.err(), Some("first dispatch failed"));
    }

    #[test]
    fn verify_and_retry_counts_truncated_attempts() {
        let salient: Vec<String> = Vec::new();
        // Both attempts salvaged from truncated output; the first is
        // low-quality (too short) so a retry runs, and both truncations
        // are counted.
        let out = verify_and_retry("PROMPT", 1, &salient, |_prompt, _n| {
            Ok::<_, ()>(Attempt {
                bundle: bundle("ok"),
                truncated: true,
            })
        })
        .expect("truncation path");
        assert_eq!(out.truncated_attempts, 2);
    }

    #[test]
    fn salient_terms_from_texts_dedups_and_filters_short_tokens() {
        let terms = salient_terms_from_texts(["Postgres billing", "postgres the a"]);
        // `the`/`a` are below MIN_SALIENT_TERM_LEN; `postgres` dedups.
        assert_eq!(terms, vec!["postgres".to_string(), "billing".to_string()]);
    }

    #[test]
    fn score_bundle_with_terms_matches_score_bundle() {
        let inputs = inputs_with(&[("migration",), ("postgres",), ("billing",), ("staging",)]);
        let terms = salient_terms_from_texts(
            inputs
                .observations
                .iter()
                .map(|r| r.content.as_str())
                .chain(std::iter::once(inputs.recap_seed.as_str())),
        );
        let b = bundle("Everyone agreed on the next steps.");
        assert_eq!(
            score_bundle(&b, &inputs),
            score_bundle_with_terms(&b, &terms)
        );
    }
}
