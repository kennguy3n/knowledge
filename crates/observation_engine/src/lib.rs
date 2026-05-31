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

pub mod citation;
pub mod document;
pub mod error;
pub mod extractor;
pub mod interrogatives;
pub mod language;
pub mod pipeline;
pub mod promotion;
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
pub use pipeline::{default_pipeline, ObservationPipeline, PipelineRunOutput};
pub use promotion::{should_promote, ChannelPromotionPolicy, PromotionReason, PromotionResult};
pub use types::{Observation, ObservationType};
