//! Observation pipeline: lexicon extraction -> importance
//! classification -> candidate observations.

use evidence_store::{ImportanceClass, ImportanceClassifier, ScopeId};

use crate::error::{ObservationError, Result};
use crate::extractor::{LexiconExtractor, ObservationExtractor};
use crate::types::Observation;

/// One pass of the Phase-1 observation pipeline.
///
/// Per `PHASES.md` Phase 1: "Lexicon → XLM-R → SLM-assisted
/// observation pipeline". Phase 1 ships the lexicon stage; the
/// XLM-R + SLM stages stub through the [`ImportanceClassifier`]
/// (today the lexicon-only fallback in `evidence_store`) without
/// changing this surface.
pub struct ObservationPipeline<E, C>
where
    E: ObservationExtractor,
    C: ImportanceClassifier,
{
    extractor: E,
    classifier: C,
    /// Importance class threshold below which observations are
    /// dropped from the pipeline (defaults to
    /// [`ImportanceClass::Useful`]'s tag, so noise messages produce
    /// no observations).
    min_importance_tag: i32,
}

impl<E, C> ObservationPipeline<E, C>
where
    E: ObservationExtractor,
    C: ImportanceClassifier,
{
    /// Build a pipeline.
    pub fn new(extractor: E, classifier: C) -> Self {
        Self {
            extractor,
            classifier,
            min_importance_tag: ImportanceClass::Useful.as_tag(),
        }
    }

    /// Override the minimum importance class.
    pub fn with_min_importance(mut self, min: ImportanceClass) -> Self {
        self.min_importance_tag = min.as_tag();
        self
    }

    /// Run the pipeline.
    ///
    /// * Returns [`ObservationError::EmptyInput`] for empty input.
    /// * Returns no observations when the importance classifier
    ///   marks the text as below `min_importance_tag` (typically
    ///   noise).
    /// * Otherwise returns the lexicon extractor's output, with
    ///   `memory_state` already set to `Candidate` (per
    ///   [`Observation::new_candidate`]).
    pub fn run(&self, text: &str, scope: ScopeId) -> Result<Vec<Observation>> {
        if text.trim().is_empty() {
            return Err(ObservationError::EmptyInput);
        }
        let class = self.classifier.classify(text);
        if class.as_tag() < self.min_importance_tag {
            return Ok(Vec::new());
        }
        Ok(self.extractor.extract(text, scope))
    }
}

/// Convenience constructor — Phase-1 default pipeline (lexicon
/// extractor + lexicon-only importance classifier).
pub fn default_pipeline() -> ObservationPipeline<LexiconExtractor, evidence_store::LexiconClassifier>
{
    ObservationPipeline::new(
        LexiconExtractor::default(),
        evidence_store::LexiconClassifier::english_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_an_error() {
        let pipeline = default_pipeline();
        let scope = ScopeId::new_v4();
        let res = pipeline.run("   \n\t", scope);
        assert!(matches!(res, Err(ObservationError::EmptyInput)));
    }

    #[test]
    fn noise_input_yields_no_observations() {
        let pipeline = default_pipeline();
        let scope = ScopeId::new_v4();
        let obs = pipeline.run("hi", scope).unwrap();
        assert!(obs.is_empty());
    }

    #[test]
    fn substantive_input_yields_candidate_observations() {
        let pipeline = default_pipeline();
        let scope = ScopeId::new_v4();
        let obs = pipeline
            .run("Friday is the deadline for the migration.", scope)
            .unwrap();
        assert!(!obs.is_empty());
        assert!(obs
            .iter()
            .all(|o| o.memory_state == memory_manager::MemoryState::Candidate));
    }
}
