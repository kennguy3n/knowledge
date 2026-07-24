//! Multilingual NER extraction engine for the Knowledge substrate.
//!
//! This crate provides [`NerExtractor`], a unified extraction interface
//! that combines:
//!
//! - **XLM-V NER** (ONNX Runtime, gated behind the `onnx-runtime`
//!   feature): deterministic multilingual named entity recognition
//!   covering 100+ languages. XLM-V uses a 1M token vocabulary (vs
//!   XLM-R's 250k), providing better coverage for low-resource
//!   languages. Detects persons, organizations, locations, and
//!   miscellaneous entities using the CoNLL BIO-2 tagging scheme.
//! - **Lexicon extraction** (always available): the existing
//!   [`observation_engine::LexiconExtractor`] with its 22-language
//!   keyword tables for decisions, tasks, questions, and facts, plus
//!   baseline entity extraction (@-mentions, URLs, emails, dates,
//!   numerics).
//! - **Regex typed-entity extraction** (always available): the existing
//!   [`observation_engine::entity_extractors::extract_typed_entities`]
//!   for industry-specific identifiers (IBAN, ISIN, SWIFT/BIC, SKU,
//!   ICD-10, patent numbers, etc.).
//!
//! # Usage
//!
//! ```no_run
//! use ner_engine::NerExtractor;
//! use evidence_store::ScopeId;
//!
//! let extractor = NerExtractor::new();
//! let scope = ScopeId::new_v4();
//! let facts = extractor.extract_all(
//!     "John Smith decided to approve the budget.",
//!     scope,
//! );
//! assert!(!facts.entities.is_empty());
//! assert!(!facts.decisions.is_empty());
//! ```
//!
//! # Feature flags
//!
//! - `onnx-runtime` (default: off): enables the ONNX Runtime-backed
//!   XLM-RoBERTa NER model. When disabled, the extractor falls back to
//!   lexicon + regex extraction only, which still provides multilingual
//!   entity coverage via the built-in language lexicons.

pub mod extractor;
pub mod labels;
#[cfg(feature = "onnx-runtime")]
pub mod model;

pub use extractor::{EntitySource, ExtractedFacts, NerExtractor};
#[cfg(feature = "onnx-runtime")]
pub use model::NerModel;

/// Entity type taxonomy, aligned with the observation engine's
/// [`observation_engine::entity_types::EntityType`] but owned here so
/// the NER engine can be used independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    /// Named person.
    Person,
    /// Named organization / project / team.
    Organization,
    /// Named product / service / software system.
    Product,
    /// Geographic location.
    Location,
    /// Date or time reference.
    DateTime,
    /// Monetary amount.
    Currency,
    /// Structured identifier (IBAN, ISIN, SKU, etc.).
    Identifier,
    /// URL.
    Url,
    /// Email address.
    Email,
    /// Numeric quantity.
    Numeric,
    /// Measurement with units.
    Measurement,
    /// Named event.
    Event,
    /// Other / catch-all.
    Other,
}

/// One entity extracted by the NER engine.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExtractedEntity {
    /// Surface span of the entity (the extracted text).
    pub content: String,
    /// Typed entity classification.
    pub entity_type: EntityType,
    /// Extraction confidence in `0.0..=1.0`.
    pub confidence: f32,
    /// Which extraction pass produced this entity.
    pub source: EntitySource,
}
