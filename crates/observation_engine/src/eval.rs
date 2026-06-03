//! Observation quality evaluation framework.
//!
//! Provides a systematic way to measure extraction precision, recall,
//! and F1 per [`ObservationType`] against a curated golden dataset,
//! and to guard against regressions in CI by asserting minimum
//! thresholds.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use observation_engine::eval::{GoldenDataset, ExpectedObservation, run_eval};
//! use observation_engine::{LexiconExtractor, ObservationType};
//!
//! let dataset = GoldenDataset::new(vec![/* … test cases … */]);
//! let extractor = LexiconExtractor::default();
//! let report = run_eval(&extractor, &dataset);
//! println!("{report}");
//! ```
//!
//! See `docs/technical/extraction-quality.md` for a full guide on extending
//! the golden dataset.

use std::collections::HashMap;
use std::fmt;

use evidence_store::ScopeId;

use crate::extractor::ObservationExtractor;
use crate::types::ObservationType;

// ── Expected observation ────────────────────────────────────────────

/// A single expected observation in a golden-dataset test case.
///
/// Matching rules used by [`run_eval`]:
///
/// * `observation_type` must match exactly.
/// * `content_substring` must appear (case-insensitive) inside
///   the produced observation's `content`.
/// * When `min_confidence` / `max_confidence` are set the produced
///   observation's `confidence` must fall within the range
///   (inclusive on both ends).
#[derive(Debug, Clone)]
pub struct ExpectedObservation {
    /// The expected observation type.
    pub observation_type: ObservationType,
    /// A substring that must appear in the observation's `content`
    /// field (case-insensitive matching).
    pub content_substring: String,
    /// Optional lower bound on confidence (inclusive).
    pub min_confidence: Option<f64>,
    /// Optional upper bound on confidence (inclusive).
    pub max_confidence: Option<f64>,
}

impl ExpectedObservation {
    /// Convenience constructor without confidence bounds.
    pub fn new(observation_type: ObservationType, content_substring: impl Into<String>) -> Self {
        Self {
            observation_type,
            content_substring: content_substring.into(),
            min_confidence: None,
            max_confidence: None,
        }
    }

    /// Builder: set an inclusive confidence range.
    pub fn with_confidence_range(mut self, min: f64, max: f64) -> Self {
        self.min_confidence = Some(min);
        self.max_confidence = Some(max);
        self
    }
}

// ── Test case ───────────────────────────────────────────────────────

/// One `(input_text, expected_observations)` pair.
#[derive(Debug, Clone)]
pub struct TestCase {
    /// Human-readable label for diagnostics.
    pub label: String,
    /// The raw text fed to the extractor.
    pub input_text: String,
    /// All observations the extractor is expected to produce.
    pub expected: Vec<ExpectedObservation>,
}

impl TestCase {
    /// Convenience constructor.
    pub fn new(
        label: impl Into<String>,
        input_text: impl Into<String>,
        expected: Vec<ExpectedObservation>,
    ) -> Self {
        Self {
            label: label.into(),
            input_text: input_text.into(),
            expected,
        }
    }
}

// ── Golden dataset ──────────────────────────────────────────────────

/// A collection of [`TestCase`]s used for extraction evaluation.
#[derive(Debug, Clone)]
pub struct GoldenDataset {
    /// The test cases.
    pub cases: Vec<TestCase>,
}

impl GoldenDataset {
    /// Build a golden dataset from a vec of test cases.
    pub fn new(cases: Vec<TestCase>) -> Self {
        Self { cases }
    }

    /// Number of test cases.
    pub fn len(&self) -> usize {
        self.cases.len()
    }

    /// Whether the dataset is empty.
    pub fn is_empty(&self) -> bool {
        self.cases.is_empty()
    }
}

// ── Per-type metrics ────────────────────────────────────────────────

/// Precision / recall / F1 for one observation type.
#[derive(Debug, Clone, Copy)]
pub struct TypeMetrics {
    /// True positives.
    pub tp: usize,
    /// False positives.
    pub fp: usize,
    /// False negatives.
    pub fn_count: usize,
    /// Precision ∈ [0, 1].
    pub precision: f64,
    /// Recall ∈ [0, 1].
    pub recall: f64,
    /// F1 score ∈ [0, 1].
    pub f1: f64,
}

impl TypeMetrics {
    fn compute(tp: usize, fp: usize, fn_count: usize) -> Self {
        let precision = if tp + fp > 0 {
            tp as f64 / (tp + fp) as f64
        } else {
            0.0
        };
        let recall = if tp + fn_count > 0 {
            tp as f64 / (tp + fn_count) as f64
        } else {
            0.0
        };
        let f1 = if precision + recall > 0.0 {
            2.0 * precision * recall / (precision + recall)
        } else {
            0.0
        };
        Self {
            tp,
            fp,
            fn_count,
            precision,
            recall,
            f1,
        }
    }
}

impl fmt::Display for TypeMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "P={:.3} R={:.3} F1={:.3} (tp={} fp={} fn={})",
            self.precision, self.recall, self.f1, self.tp, self.fp, self.fn_count
        )
    }
}

// ── Eval report ─────────────────────────────────────────────────────

/// Full evaluation report returned by [`run_eval`].
#[derive(Debug, Clone)]
pub struct EvalReport {
    /// Per-type metrics.
    pub per_type: HashMap<ObservationType, TypeMetrics>,
    /// Total number of test cases evaluated.
    pub total_cases: usize,
}

impl EvalReport {
    /// Retrieve the F1 score for a given type, or `0.0` if absent.
    pub fn f1_for(&self, ty: ObservationType) -> f64 {
        self.per_type.get(&ty).map_or(0.0, |m| m.f1)
    }

    /// Macro-average F1 across all types present in the report.
    pub fn macro_f1(&self) -> f64 {
        if self.per_type.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.per_type.values().map(|m| m.f1).sum();
        sum / self.per_type.len() as f64
    }
}

impl fmt::Display for EvalReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Observation Eval Report ({} cases)", self.total_cases)?;
        writeln!(f, "{:-<60}", "")?;
        let mut types: Vec<_> = self.per_type.keys().collect();
        types.sort_by_key(|t| t.as_str());
        for ty in types {
            let m = &self.per_type[ty];
            writeln!(f, "  {:10}  {m}", ty.as_str())?;
        }
        writeln!(f, "{:-<60}", "")?;
        writeln!(f, "  macro-F1 = {:.3}", self.macro_f1())?;
        Ok(())
    }
}

// ── run_eval ────────────────────────────────────────────────────────

/// Run the evaluation: extract observations from every test case
/// in `dataset`, match them against the expected observations, and
/// compute per-type precision / recall / F1.
///
/// Matching algorithm per test case:
///
/// 1. Run the extractor on `input_text`.
/// 2. For each expected observation, find the first produced
///    observation that has the same `observation_type`, whose
///    `content` contains `content_substring` (case-insensitive),
///    and whose `confidence` is within the optional confidence
///    range. Mark it as a **true positive** and remove it from
///    the pool so it cannot match another expected observation.
/// 3. Unmatched expected observations → **false negatives**.
/// 4. Unmatched produced observations → **false positives**.
pub fn run_eval(extractor: &dyn ObservationExtractor, dataset: &GoldenDataset) -> EvalReport {
    let scope = ScopeId::new_v4();

    // Accumulators: (tp, fp, fn) per type.
    let mut counters: HashMap<ObservationType, (usize, usize, usize)> = HashMap::new();

    for case in &dataset.cases {
        let produced = extractor.extract(&case.input_text, scope);

        // Track which produced observations have been consumed.
        let mut consumed = vec![false; produced.len()];

        // Match expected → produced (greedy, first-match).
        for exp in &case.expected {
            let sub_lower = exp.content_substring.to_lowercase();
            let matched = produced.iter().enumerate().find(|(i, obs)| {
                if consumed[*i] {
                    return false;
                }
                if obs.observation_type != exp.observation_type {
                    return false;
                }
                if !obs.content.to_lowercase().contains(&sub_lower) {
                    return false;
                }
                if let Some(min) = exp.min_confidence {
                    if obs.confidence < min {
                        return false;
                    }
                }
                if let Some(max) = exp.max_confidence {
                    if obs.confidence > max {
                        return false;
                    }
                }
                true
            });

            let entry = counters.entry(exp.observation_type).or_insert((0, 0, 0));
            if let Some((idx, _)) = matched {
                consumed[idx] = true;
                entry.0 += 1; // TP
            } else {
                entry.2 += 1; // FN
            }
        }

        // Unmatched produced → FP, counted per their type.
        for (i, obs) in produced.iter().enumerate() {
            if !consumed[i] {
                let entry = counters.entry(obs.observation_type).or_insert((0, 0, 0));
                entry.1 += 1; // FP
            }
        }
    }

    let per_type = counters
        .into_iter()
        .map(|(ty, (tp, fp, fn_c))| (ty, TypeMetrics::compute(tp, fp, fn_c)))
        .collect();

    EvalReport {
        per_type,
        total_cases: dataset.cases.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dataset_produces_empty_report() {
        let ds = GoldenDataset::new(vec![]);
        let ext = crate::extractor::LexiconExtractor::default();
        let report = run_eval(&ext, &ds);
        assert_eq!(report.total_cases, 0);
        assert!(report.per_type.is_empty());
        assert!((report.macro_f1() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn single_case_perfect_match() {
        let ds = GoldenDataset::new(vec![TestCase::new(
            "simple-task",
            "TODO: review the pull request before Friday.",
            vec![ExpectedObservation::new(
                ObservationType::Task,
                "review the pull request",
            )],
        )]);
        let ext = crate::extractor::LexiconExtractor::default();
        let report = run_eval(&ext, &ds);
        let task_m = report.per_type.get(&ObservationType::Task).unwrap();
        assert!(task_m.tp >= 1);
    }

    #[test]
    fn confidence_range_filter() {
        let exp = ExpectedObservation::new(ObservationType::Task, "review")
            .with_confidence_range(0.0, 0.5);
        // Lexicon extractor uses fixed confidence ≥ 0.6 for tasks —
        // this should NOT match.
        let ds = GoldenDataset::new(vec![TestCase::new(
            "confidence-mismatch",
            "TODO: review the docs.",
            vec![exp],
        )]);
        let ext = crate::extractor::LexiconExtractor::default();
        let report = run_eval(&ext, &ds);
        let task_m = report.per_type.get(&ObservationType::Task).unwrap();
        assert_eq!(task_m.tp, 0);
        assert!(task_m.fn_count >= 1);
    }
}
