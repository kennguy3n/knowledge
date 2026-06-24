//! `observation_engine` — lexicon-first extraction of structured
//! observations from raw evidence text.
//!
//! Per `docs/technical/design.md` §3.2 / `docs/technical/architecture.md` §2.1, the observation
//! engine turns raw text into a small number of typed observations
//! (entities, facts, tasks, decisions, claims). The baseline is a
//! **lexicon extractor** — regex / keyword / capitalised-word
//! heuristics that need no model — and is used as the cheap first
//! stage before XLM-R + SLM-assisted extraction.
//!
//! Cross-references:
//!
//! * Lexicon-first approach: `docs/technical/design.md` §3.2 (cheap classifiers
//!   first, only candidates that clear the cheap classifier go to
//!   more expensive stages).
//! * Observation deliverables: `docs/technical/design.md` §3.2.
//!
//! # Test-only types (`test-support` feature)
//!
//! `CONTRIBUTING.md` requires that test-only types be gated behind
//! `cfg(any(test, feature = "test-support"))` and documented here.
//! The `test-support` feature is declared in `Cargo.toml` as a
//! no-op feature flag; enabling it exposes the `eval` module which
//! contains `GoldenDataset`, `TestCase`, `ExpectedObservation`,
//! `EvalReport`, `TypeMetrics`, and `run_eval` — the observation
//! extraction quality evaluation framework.

#![deny(missing_docs)]

#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("test-support must not be enabled in release builds");

// STABLE
pub mod citation;
// STABLE
pub mod document;
// STABLE
pub mod error;
// UNSTABLE — test-only evaluation framework; gated behind `test-support`.
#[cfg(any(test, feature = "test-support"))]
pub mod eval;
// STABLE
pub mod cultural;
// STABLE
pub mod entity_extractors;
// STABLE
pub mod entity_types;
// STABLE
pub mod extractor;
// UNSTABLE — interrogative tables; internal heuristic.
#[doc(hidden)]
pub mod interrogatives;
// STABLE
pub mod language;
// STABLE
pub mod lexicon;
// UNSTABLE — internal telemetry.
#[doc(hidden)]
pub mod lexicon_telemetry;
// UNSTABLE — internal persistent telemetry.
#[doc(hidden)]
pub mod persistent_telemetry;
// STABLE
pub mod pipeline;
// STABLE
pub mod promotion;
// UNSTABLE — internal retrieval telemetry.
#[doc(hidden)]
pub mod retrieval_telemetry;
// STABLE
pub mod synonyms;
// STABLE
pub mod slm_refiner;
// STABLE
pub mod types;

// STABLE
pub use citation::{
    Citation, CitationFormat, CitationRegistry, CitationRenderer, CitationSourceType,
};
// STABLE
pub use document::{
    default_document_pipeline, ChunkMetadata, DocumentChunk, DocumentChunker,
    DocumentExtractionResult, DocumentKind, DocumentObservationPipeline, DocumentRef,
    ObservationCitation, SlidingWindowChunker,
};
// STABLE
pub use error::{ObservationError, Result};
// STABLE
pub use cultural::{
    convert_japanese_era, convert_thai_buddhist, convert_to_iso8601,
    detect_address_country, detect_calendar_system, enrich_entity, normalize_currency,
    normalize_person_name, CalendarSystem, ConvertedDate, CulturalMetadata, NameOrder,
    NormalizedCurrency, NormalizedName,
};
// STABLE
pub use entity_extractors::{
    extract_typed_entities, ExtractedEntity, EntityExtractionTier,
};
// STABLE
pub use entity_types::{EntityType, IdentifierDomain, IdentifierKind};
// STABLE
pub use extractor::{LexiconExtractor, ObservationExtractor};
// UNSTABLE — interrogative tables; internal heuristic.
#[doc(hidden)]
pub use interrogatives::{
    interrogatives_for, matching_strategy_for, InterrogativeMatch, SUPPORTED_PRIMARY_TAGS,
};
// STABLE
pub use language::{detect_language, LanguageDetection, LanguageTag};
// STABLE
pub use lexicon::{
    default_registry, first_alphabetic_bigram, first_alphabetic_token,
    is_arabic_combining_or_tatweel, is_bidi_or_zwj_format, normalize_for_lookup, table_matches,
    KeywordClass, LanguageLexicon, LexiconRegistry, MatchStrategy, BUILTIN_LEXICONS,
    SUPPORTED_LEXICON_TAGS,
};
// UNSTABLE — internal telemetry.
#[doc(hidden)]
pub use lexicon_telemetry::{snapshot as lexicon_telemetry_snapshot, LexiconTelemetrySnapshot};
// STABLE
pub use pipeline::{default_pipeline, ObservationPipeline, PipelineRunOutput};
// STABLE
pub use promotion::{should_promote, ChannelPromotionPolicy, PromotionReason, PromotionResult};
// STABLE
pub use synonyms::{are_synonyms, expand_fts_query, expand_query};
// STABLE
pub use slm_refiner::{
    apply_refinement, EntityRefiner, HeuristicRefiner, NoOpRefiner, RefinementCandidate,
    RefinementConfig, RefinementResult,
};
// UNSTABLE — internal telemetry.
#[doc(hidden)]
pub use retrieval_telemetry::{snapshot as retrieval_metrics_snapshot, RetrievalMetricsSnapshot};
// STABLE
pub use types::{Observation, ObservationType};
