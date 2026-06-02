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

pub mod citation;
pub mod document;
pub mod error;
#[cfg(any(test, feature = "test-support"))]
pub mod eval;
pub mod extractor;
pub mod interrogatives;
pub mod language;
pub mod lexicon;
pub mod lexicon_telemetry;
pub mod persistent_telemetry;
pub mod pipeline;
pub mod promotion;
pub mod retrieval_telemetry;
pub mod types;

pub use citation::{
    Citation, CitationFormat, CitationRegistry, CitationRenderer, CitationSourceType,
};
pub use document::{
    default_document_pipeline, ChunkMetadata, DocumentChunk, DocumentChunker,
    DocumentExtractionResult, DocumentKind, DocumentObservationPipeline, DocumentRef,
    ObservationCitation, SlidingWindowChunker,
};
pub use error::{ObservationError, Result};
pub use extractor::{LexiconExtractor, ObservationExtractor};
pub use interrogatives::{
    interrogatives_for, matching_strategy_for, InterrogativeMatch, SUPPORTED_PRIMARY_TAGS,
};
pub use language::{detect_language, LanguageDetection, LanguageTag};
pub use lexicon::{
    default_registry, first_alphabetic_bigram, first_alphabetic_token,
    is_arabic_combining_or_tatweel, is_bidi_or_zwj_format, normalize_for_lookup, table_matches,
    KeywordClass, LanguageLexicon, LexiconRegistry, MatchStrategy, BUILTIN_LEXICONS,
    SUPPORTED_LEXICON_TAGS,
};
pub use lexicon_telemetry::{snapshot as lexicon_telemetry_snapshot, LexiconTelemetrySnapshot};
pub use pipeline::{default_pipeline, ObservationPipeline, PipelineRunOutput};
pub use promotion::{should_promote, ChannelPromotionPolicy, PromotionReason, PromotionResult};
pub use retrieval_telemetry::{snapshot as retrieval_metrics_snapshot, RetrievalMetricsSnapshot};
pub use types::{Observation, ObservationType};
