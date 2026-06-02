//! `observation_engine` — lexicon-first extraction of structured
//! observations from raw evidence text.
//!
//! Per `docs/DESIGN.md` §3.2 / `ARCHITECTURE.md` §2.1, the observation
//! engine turns raw text into a small number of typed observations
//! (entities, facts, tasks, decisions, claims). The baseline is a
//! **lexicon extractor** — regex / keyword / capitalised-word
//! heuristics that need no model — and is used as the cheap first
//! stage before XLM-R + SLM-assisted extraction.
//!
//! Cross-references:
//!
//! * Lexicon-first approach: `docs/DESIGN.md` §3.2 (cheap classifiers
//!   first, only candidates that clear the cheap classifier go to
//!   more expensive stages).
//! * Observation deliverables: `docs/DESIGN.md` §3.2.

#![deny(missing_docs)]

// STABLE
pub mod citation;
// STABLE
pub mod document;
// STABLE
pub mod error;
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
// UNSTABLE — internal telemetry.
#[doc(hidden)]
pub use retrieval_telemetry::{snapshot as retrieval_metrics_snapshot, RetrievalMetricsSnapshot};
// STABLE
pub use types::{Observation, ObservationType};
