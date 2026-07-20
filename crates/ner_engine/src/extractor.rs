//! [`NerExtractor`] — multilingual NER extraction combining XLM-RoBERTa
//! ONNX inference with the existing lexicon + regex extractors.
//!
//! The extractor runs up to three extraction passes and merges the
//! results:
//!
//! 1. **XLM-R NER** (when the `onnx-runtime` feature is enabled and a
//!    model is loaded): tokenization → ONNX inference → argmax → BIO
//!    span decoding. Catches persons, organizations, locations, and
//!    miscellaneous named entities across 100+ languages.
//! 2. **Lexicon extraction** (always): the existing
//!    [`observation_engine::LexiconExtractor`] runs its 16-language
//!    keyword tables for decisions, tasks, questions, and facts, plus
//!    baseline entity extraction (@-mentions, URLs, emails, dates,
//!    numerics).
//! 3. **Regex typed-entity extraction** (always): the existing
//!    [`observation_engine::entity_extractors::extract_typed_entities`]
//!    runs pattern-based identifier extraction (IBAN, ISIN, SWIFT/BIC,
//!    SKU, ICD-10, patent numbers, etc.).
//!
//! Results from all three passes are merged and deduplicated by
//! `(content, entity_type)` pair so the same entity surfaced by both
//! NER and the lexicon extractor appears only once.

use std::collections::HashSet;

use evidence_store::ScopeId;
use observation_engine::entity_extractors::{extract_typed_entities, EntityExtractionTier};
use observation_engine::entity_types::EntityType as ObsEntityType;
use observation_engine::extractor::{LexiconExtractor, ObservationExtractor};
use observation_engine::language::detect_language;
use observation_engine::types::ObservationType;

use crate::{EntityType, ExtractedEntity};

/// Source of an extracted entity — used for provenance tracking and
/// deduplication decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitySource {
    /// XLM-RoBERTa NER model.
    Ner,
    /// Lexicon extractor (capitalised tokens, @-mentions, etc.).
    Lexicon,
    /// Regex pattern extractor (IBAN, SKU, etc.).
    Regex,
}

/// Multilingual NER extractor combining XLM-RoBERTa ONNX, lexicon, and
/// regex extraction.
///
/// When the `onnx-runtime` feature is enabled and a model is loaded
/// via [`NerExtractor::with_model`], the extractor runs XLM-R NER
/// alongside the lexicon and regex extractors. When no model is loaded
/// (or the feature is disabled), it falls back to lexicon + regex
/// only — still providing multilingual entity coverage via the 22
/// built-in language lexicons.
#[derive(Clone)]
pub struct NerExtractor {
    #[cfg(feature = "onnx-runtime")]
    model: Option<crate::model::NerModel>,
    lexicon: LexiconExtractor,
    extraction_tier: EntityExtractionTier,
}

impl Default for NerExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl NerExtractor {
    /// Build a new extractor with the default lexicon registry and
    /// mid-tier entity extraction. No ONNX model is loaded; call
    /// [`Self::with_model`] to enable XLM-R NER.
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "onnx-runtime")]
            model: None,
            lexicon: LexiconExtractor::english_default(),
            extraction_tier: EntityExtractionTier::Mid,
        }
    }

    /// Build an extractor with a specific lexicon registry.
    pub fn with_lexicon(lexicon: LexiconExtractor) -> Self {
        Self {
            #[cfg(feature = "onnx-runtime")]
            model: None,
            lexicon,
            extraction_tier: EntityExtractionTier::Mid,
        }
    }

    /// Attach an ONNX NER model (when the `onnx-runtime` feature is
    /// enabled). The model must have been loaded via
    /// [`crate::model::NerModel::load`].
    #[cfg(feature = "onnx-runtime")]
    #[must_use]
    pub fn with_model(mut self, model: crate::model::NerModel) -> Self {
        self.model = Some(model);
        self
    }

    /// Set the entity extraction tier (controls whether regex
    /// pattern-based identifier extraction runs).
    #[must_use]
    pub fn with_extraction_tier(mut self, tier: EntityExtractionTier) -> Self {
        self.extraction_tier = tier;
        self
    }

    /// Extract entities from `text`, returning deduplicated
    /// [`ExtractedEntity`] values from all available extraction passes.
    ///
    /// The `scope` parameter is carried through to the lexicon
    /// extractor for observation stamping.
    pub fn extract_entities(&self, text: &str, scope: ScopeId) -> Vec<ExtractedEntity> {
        let mut entities = Vec::new();
        let mut seen: HashSet<(String, EntityType)> = HashSet::new();

        // ── Pass 1: XLM-R NER (when available) ──────────────────────
        #[cfg(feature = "onnx-runtime")]
        if let Some(ref model) = self.model {
            if let Ok(labels) = model.predict_labels(text) {
                for entity in crate::model::NerModel::decode_spans(&labels) {
                    let key = (entity.content.clone(), entity.entity_type);
                    if seen.insert(key) {
                        entities.push(entity);
                    }
                }
            }
        }

        // ── Pass 2: Lexicon extraction ──────────────────────────────
        // The lexicon extractor produces `Observation` values, which
        // we convert to `ExtractedEntity` for the unified merge. We
        // only carry over entity-class observations (not decisions /
        // tasks / questions / facts — those are handled separately by
        // the hybrid synthesizer via the lexicon's `do_extract`).
        let dominant_language = detect_language(text).map(|d| d.tag);
        let observations = self
            .lexicon
            .extract_with_dominant_language(text, scope, dominant_language.as_ref());

        for obs in &observations {
            if obs.observation_type == ObservationType::Entity {
                let entity_type = map_observation_entity_type(obs.entity_type);
                let key = (obs.content.clone(), entity_type);
                if seen.insert(key) {
                    entities.push(ExtractedEntity {
                        content: obs.content.clone(),
                        entity_type,
                        confidence: obs.confidence as f32,
                        source: EntitySource::Lexicon,
                    });
                }
            }
        }

        // ── Pass 3: Regex typed-entity extraction ───────────────────
        for extracted in extract_typed_entities(text, self.extraction_tier) {
            let entity_type = map_typed_entity_type(extracted.entity_type);
            let key = (extracted.content.clone(), entity_type);
            if seen.insert(key) {
                entities.push(ExtractedEntity {
                    content: extracted.content,
                    entity_type,
                    confidence: extracted.confidence as f32,
                    source: EntitySource::Regex,
                });
            }
        }

        entities
    }

    /// Extract all observations (entities + decisions + tasks + questions
    /// + facts) from `text`, returning the full lexicon extractor output
    /// augmented with NER entities.
    ///
    /// This is the primary entry point for the hybrid synthesizer's
    /// Stage 1 extraction: it produces the full set of extracted facts
    /// (entities, decisions, tasks, questions, facts) that Stage 2
    /// rephrases into a summary bundle.
    pub fn extract_all(&self, text: &str, scope: ScopeId) -> ExtractedFacts {
        let dominant_language = detect_language(text).map(|d| d.tag);
        let observations = self
            .lexicon
            .extract_with_dominant_language(text, scope, dominant_language.as_ref());

        let mut entities: Vec<ExtractedEntity> = Vec::new();
        let mut decisions: Vec<String> = Vec::new();
        let mut tasks: Vec<String> = Vec::new();
        let mut questions: Vec<String> = Vec::new();
        let mut facts: Vec<String> = Vec::new();
        let mut seen_entities: HashSet<(String, EntityType)> = HashSet::new();

        // NER entities (when available)
        #[cfg(feature = "onnx-runtime")]
        if let Some(ref model) = self.model {
            if let Ok(labels) = model.predict_labels(text) {
                for entity in crate::model::NerModel::decode_spans(&labels) {
                    let key = (entity.content.clone(), entity.entity_type);
                    if seen_entities.insert(key) {
                        entities.push(entity);
                    }
                }
            }
        }

        // Lexicon observations
        for obs in &observations {
            match obs.observation_type {
                ObservationType::Entity => {
                    let entity_type = map_observation_entity_type(obs.entity_type);
                    let key = (obs.content.clone(), entity_type);
                    if seen_entities.insert(key) {
                        entities.push(ExtractedEntity {
                            content: obs.content.clone(),
                            entity_type,
                            confidence: obs.confidence as f32,
                            source: EntitySource::Lexicon,
                        });
                    }
                }
                ObservationType::Decision => {
                    if !decisions.contains(&obs.content) {
                        decisions.push(obs.content.clone());
                    }
                }
                ObservationType::Task => {
                    if !tasks.contains(&obs.content) {
                        tasks.push(obs.content.clone());
                    }
                }
                ObservationType::Question => {
                    if !questions.contains(&obs.content) {
                        questions.push(obs.content.clone());
                    }
                }
                ObservationType::Fact => {
                    if !facts.contains(&obs.content) {
                        facts.push(obs.content.clone());
                    }
                }
                _ => {}
            }
        }

        // Regex typed entities
        for extracted in extract_typed_entities(text, self.extraction_tier) {
            let entity_type = map_typed_entity_type(extracted.entity_type);
            let key = (extracted.content.clone(), entity_type);
            if seen_entities.insert(key) {
                entities.push(ExtractedEntity {
                    content: extracted.content,
                    entity_type,
                    confidence: extracted.confidence as f32,
                    source: EntitySource::Regex,
                });
            }
        }

        ExtractedFacts {
            entities,
            decisions,
            tasks,
            questions,
            facts,
            dominant_language: dominant_language.map(|t| t.as_str().to_string()),
        }
    }

    /// Whether the ONNX NER model is loaded.
    pub fn has_ner_model(&self) -> bool {
        #[cfg(feature = "onnx-runtime")]
        {
            self.model.is_some()
        }
        #[cfg(not(feature = "onnx-runtime"))]
        {
            false
        }
    }
}

/// All facts extracted from a session's messages by Stage 1 of the
/// hybrid synthesis pipeline.
///
/// This struct is the input to Stage 2 (SLM rephrasing). The hybrid
/// synthesizer builds a rephrase prompt from these facts and dispatches
/// it to the SLM, which connects them into fluent prose.
#[derive(Debug, Clone, Default)]
pub struct ExtractedFacts {
    /// Named entities (persons, organizations, locations, identifiers, etc.).
    pub entities: Vec<ExtractedEntity>,
    /// Decision sentences extracted by the lexicon.
    pub decisions: Vec<String>,
    /// Task sentences extracted by the lexicon.
    pub tasks: Vec<String>,
    /// Question sentences extracted by the lexicon.
    pub questions: Vec<String>,
    /// Factual declarative sentences extracted by the lexicon.
    pub facts: Vec<String>,
    /// Dominant detected language of the source text (BCP-47 primary
    /// subtag, e.g. `"en"`, `"ja"`, `"ar"`).
    pub dominant_language: Option<String>,
}

impl ExtractedFacts {
    /// Whether all fact categories are empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
            && self.decisions.is_empty()
            && self.tasks.is_empty()
            && self.questions.is_empty()
            && self.facts.is_empty()
    }

    /// Total count of all extracted facts across all categories.
    pub fn total_count(&self) -> usize {
        self.entities.len()
            + self.decisions.len()
            + self.tasks.len()
            + self.questions.len()
            + self.facts.len()
    }

    /// Format extracted facts into a text body for the
    /// `SynthSummaryRephrase` prompt template's `{body}` placeholder.
    ///
    /// Produces a structured text listing of entities, decisions,
    /// tasks, questions, and facts. This is the shared formatting
    /// function used by the `HybridSynthesizer`, the FFI
    /// `synthesize_scope` path, and the `SlmSummarizer` hybrid path,
    /// ensuring all three produce identical prompts for the same
    /// extracted facts.
    pub fn format_rephrase_body(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();

        if !self.entities.is_empty() {
            let _ = writeln!(&mut out, "\nEntities:");
            for entity in &self.entities {
                let _ = writeln!(
                    &mut out,
                    "- {} ({:?})",
                    entity.content, entity.entity_type
                );
            }
        }

        if !self.decisions.is_empty() {
            let _ = writeln!(&mut out, "\nDecisions:");
            for d in &self.decisions {
                let _ = writeln!(&mut out, "- {}", d);
            }
        }

        if !self.tasks.is_empty() {
            let _ = writeln!(&mut out, "\nTasks:");
            for t in &self.tasks {
                let _ = writeln!(&mut out, "- {}", t);
            }
        }

        if !self.questions.is_empty() {
            let _ = writeln!(&mut out, "\nQuestions:");
            for q in &self.questions {
                let _ = writeln!(&mut out, "- {}", q);
            }
        }

        if !self.facts.is_empty() {
            let _ = writeln!(&mut out, "\nFacts:");
            for f in &self.facts {
                let _ = writeln!(&mut out, "- {}", f);
            }
        }

        out
    }
}

/// Map the observation engine's `EntityType` to the NER engine's
/// [`EntityType`].
fn map_typed_entity_type(
    obs_type: observation_engine::entity_types::EntityType,
) -> EntityType {
    use observation_engine::entity_types::EntityType as OE;
    match obs_type {
        OE::Person => EntityType::Person,
        OE::Organization => EntityType::Organization,
        OE::Product => EntityType::Product,
        OE::Location => EntityType::Location,
        OE::Date => EntityType::DateTime,
        OE::Currency => EntityType::Currency,
        OE::Identifier => EntityType::Identifier,
        OE::Url => EntityType::Url,
        OE::Email => EntityType::Email,
        OE::Numeric => EntityType::Numeric,
        OE::Measurement => EntityType::Measurement,
        OE::Event => EntityType::Event,
        OE::Unknown => EntityType::Other,
    }
}

/// Map an observation's entity type (from the observation engine's
/// typed entity field) to the NER engine's [`EntityType`].
fn map_observation_entity_type(obs_type: Option<ObsEntityType>) -> EntityType {
    match obs_type {
        Some(ObsEntityType::Person) => EntityType::Person,
        Some(ObsEntityType::Organization) => EntityType::Organization,
        Some(ObsEntityType::Product) => EntityType::Product,
        Some(ObsEntityType::Location) => EntityType::Location,
        Some(ObsEntityType::Date) => EntityType::DateTime,
        Some(ObsEntityType::Currency) => EntityType::Currency,
        Some(ObsEntityType::Identifier) => EntityType::Identifier,
        Some(ObsEntityType::Url) => EntityType::Url,
        Some(ObsEntityType::Email) => EntityType::Email,
        Some(ObsEntityType::Numeric) => EntityType::Numeric,
        Some(ObsEntityType::Measurement) => EntityType::Measurement,
        Some(ObsEntityType::Event) => EntityType::Event,
        Some(ObsEntityType::Unknown) | None => EntityType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_all_finds_entities_and_decisions() {
        let extractor = NerExtractor::new();
        let scope = ScopeId::new_v4();
        let facts = extractor.extract_all(
            "John Smith decided to approve the budget. Please send the invoice to Acme Corp.",
            scope,
        );
        assert!(!facts.entities.is_empty());
        assert!(!facts.decisions.is_empty());
        assert!(!facts.tasks.is_empty());
    }

    #[test]
    fn extract_all_deduplicates_entities() {
        let extractor = NerExtractor::new();
        let scope = ScopeId::new_v4();
        let facts = extractor.extract_all(
            "Contact john@example.com or john@example.com for details.",
            scope,
        );
        let email_count = facts
            .entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Email)
            .count();
        assert_eq!(email_count, 1, "duplicate emails should be deduplicated");
    }

    #[test]
    fn extract_all_detects_questions() {
        let extractor = NerExtractor::new();
        let scope = ScopeId::new_v4();
        let facts = extractor.extract_all(
            "What is the deadline for the Q3 report?",
            scope,
        );
        assert!(!facts.questions.is_empty());
    }

    #[test]
    fn extract_all_finds_typed_identifiers() {
        let extractor = NerExtractor::new();
        let scope = ScopeId::new_v4();
        let facts = extractor.extract_all(
            "The IBAN is GB82WEST12345698765432 and the SKU is ABC-123.",
            scope,
        );
        assert!(facts.entities.iter().any(|e| e.entity_type == EntityType::Identifier));
    }

    #[test]
    fn extract_all_empty_for_blank_input() {
        let extractor = NerExtractor::new();
        let scope = ScopeId::new_v4();
        let facts = extractor.extract_all("", scope);
        assert!(facts.is_empty());
    }

    #[test]
    fn extracted_facts_total_count() {
        let facts = ExtractedFacts {
            entities: vec![ExtractedEntity {
                content: "test".into(),
                entity_type: EntityType::Person,
                confidence: 0.9,
                source: EntitySource::Ner,
            }],
            decisions: vec!["decided".into()],
            tasks: vec!["do thing".into()],
            questions: vec!["why?".into()],
            facts: vec!["it is".into()],
            dominant_language: Some("en".into()),
        };
        assert_eq!(facts.total_count(), 5);
        assert!(!facts.is_empty());
    }
}
