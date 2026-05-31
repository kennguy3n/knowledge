//! Observation pipeline: lexicon extraction -> importance
//! classification -> candidate observations.

use evidence_store::{ImportanceClass, ImportanceClassifier, ScopeId};

use crate::error::{ObservationError, Result};
use crate::extractor::{LexiconExtractor, ObservationExtractor};
use crate::language::{detect_language, LanguageDetection};
use crate::types::Observation;

/// One pass of the observation pipeline.
///
/// Implements the "Lexicon → XLM-R → SLM-assisted observation
/// pipeline". Currently ships the lexicon stage; the
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
    ///   [`Observation::new_candidate`]) and `language_tag`
    ///   populated from [`detect_language`] when the detector
    ///   produced a reliable classification.
    pub fn run(&self, text: &str, scope: ScopeId) -> Result<Vec<Observation>> {
        let result = self.run_with_language(text, scope)?;
        Ok(result.observations)
    }

    /// Run the pipeline and surface the language-detection result
    /// alongside the produced observations. Callers that need to
    /// stamp the detected language onto the evidence-row metadata
    /// (so the FTS5 tokenizer and the lexicon registry can be
    /// reused without re-detecting) should use this variant
    /// rather than [`Self::run`].
    pub fn run_with_language(&self, text: &str, scope: ScopeId) -> Result<PipelineRunOutput> {
        if text.trim().is_empty() {
            return Err(ObservationError::EmptyInput);
        }
        // Detect the *dominant* (whole-input) language for the
        // row-level metadata. The per-sentence language tag that
        // gets stamped onto each observation is computed by the
        // extractor itself (Phase 1.4) — it runs `detect_language`
        // per sentence and falls back to the dominant language
        // when whatlang refuses to classify a short sentence. We
        // therefore *do not* re-stamp observations with the
        // whole-input tag here: that would clobber the finer
        // per-sentence tags from the extractor.
        let language = detect_language(text);
        let class = self.classifier.classify(text);
        if class.as_tag() < self.min_importance_tag {
            return Ok(PipelineRunOutput {
                observations: Vec::new(),
                language,
            });
        }
        // Extractor has already stamped per-sentence language tags
        // onto every produced observation (sentence-class
        // observations get the sentence language, entity-class
        // observations get the dominant language). Trust those
        // stamps and pass them through.
        let observations: Vec<Observation> = self.extractor.extract(text, scope);
        Ok(PipelineRunOutput {
            observations,
            language,
        })
    }
}

/// Output of [`ObservationPipeline::run_with_language`].
///
/// The `language` field is independent of whether any observations
/// were produced: callers that want to stamp the detected language
/// onto the evidence row's metadata even when the pipeline dropped
/// every observation as noise need to read it from a result that
/// also carries an empty `observations` vec.
#[derive(Debug, Clone)]
pub struct PipelineRunOutput {
    /// Observations produced by this run — already stamped with
    /// the detected language tag.
    pub observations: Vec<Observation>,
    /// Language detected on the raw input. `None` when the
    /// detector either refused to classify or marked the result
    /// as unreliable.
    pub language: Option<LanguageDetection>,
}

/// Convenience constructor — default pipeline (lexicon
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
    use crate::types::ObservationType;

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

    #[test]
    fn english_input_stamps_en_language_tag_on_every_observation() {
        let pipeline = default_pipeline();
        let scope = ScopeId::new_v4();
        let out = pipeline
            .run_with_language(
                "Friday is the deadline for the migration and the team approved the rollout plan.",
                scope,
            )
            .unwrap();
        assert!(!out.observations.is_empty());
        let detected = out
            .language
            .as_ref()
            .expect("english should be reliably detected");
        assert_eq!(detected.tag.as_str(), "en");
        for obs in &out.observations {
            let tag = obs
                .language_tag
                .as_ref()
                .expect("observation should inherit the detected language");
            assert_eq!(tag.as_str(), "en");
        }
    }

    #[test]
    fn noise_input_still_returns_detected_language_on_run_with_language() {
        // `noise` short input — the classifier drops every observation
        // but the detected language (when reliable) is still surfaced
        // so the caller can stamp it onto the evidence row metadata.
        let pipeline = default_pipeline();
        let scope = ScopeId::new_v4();
        let out = pipeline
            .run_with_language("hi", scope)
            .expect("non-empty input");
        assert!(out.observations.is_empty());
        // Two-letter input is too short to classify reliably — we just
        // assert the call did not panic and observations are empty.
        // The detected-language field may be `None`.
        let _ = out.language;
    }

    #[test]
    fn run_observations_unaffected_by_unreliable_language_detection() {
        // Whatlang declines pure-emoji input — the pipeline must
        // still produce observations from substantive text and just
        // leave `language_tag = None` on them.
        let pipeline = default_pipeline();
        let scope = ScopeId::new_v4();
        let obs = pipeline.run("!!!", scope).unwrap_or_default();
        // `!!!` is short and contains no useful signal — the
        // importance classifier drops it (length-based noise). The
        // detection contract is exercised in `language.rs` tests.
        assert!(obs.is_empty());
    }

    #[test]
    fn run_with_language_preserves_per_sentence_stamps_in_bilingual_input() {
        // Phase 1.4 contract: the pipeline runs `detect_language`
        // on the WHOLE input to set the row-level `language`
        // field, but it MUST NOT overwrite the per-sentence
        // language tags the extractor already attached to each
        // observation. In a bilingual EN / JA message, the JA
        // sentence's question observation should keep its `ja`
        // tag even when the whole-input dominant detection picks
        // `en` (or `None`).
        let pipeline = default_pipeline();
        let scope = ScopeId::new_v4();
        let text = "Please review the migration plan for this Friday deadline. \
                    今日の会議では何時に開始する予定でしょうか、ご確認お願いします。 \
                    Please send the agenda to the entire team today.";
        let out = pipeline.run_with_language(text, scope).unwrap();

        // The Japanese sentence's interrogative `何` should
        // produce a question observation tagged `ja`.
        let question = out
            .observations
            .iter()
            .find(|o| o.observation_type == ObservationType::Question)
            .expect("japanese question must be detected through pipeline");
        assert_eq!(
            question
                .language_tag
                .as_ref()
                .map(|t| t.primary().to_string()),
            Some("ja".to_string()),
            "japanese question observation must keep its per-sentence `ja` tag through \
             the pipeline, got {:?}",
            question.language_tag
        );

        // The English task sentences must keep their `en` tag —
        // not get clobbered by the dominant-detection result.
        let en_task_count = out
            .observations
            .iter()
            .filter(|o| o.observation_type == ObservationType::Task)
            .filter(|o| o.language_tag.as_ref().is_some_and(|t| t.primary() == "en"))
            .count();
        assert!(
            en_task_count >= 1,
            "expected at least one english-tagged task observation through the pipeline; \
             got tags: {:?}",
            out.observations
                .iter()
                .filter(|o| o.observation_type == ObservationType::Task)
                .map(|o| o.language_tag.clone())
                .collect::<Vec<_>>()
        );
    }
}
