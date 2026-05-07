//! `observation_engine` — lexicon-first extraction of structured
//! observations from raw evidence text.
//!
//! Per `PROPOSAL.md` §3.2 / `ARCHITECTURE.md` §2.1 / `PHASES.md` Phase
//! 1, the observation engine turns raw text into a small number of
//! typed observations (entities, facts, tasks, decisions, claims).
//! The Phase 1 baseline is a **lexicon extractor** — regex / keyword
//! / capitalised-word heuristics that need no model — and is used as
//! the cheap first stage before XLM-R + SLM-assisted extraction in
//! later phases.
//!
//! Cross-references:
//!
//! * Lexicon-first approach: `PROPOSAL.md` §3.2 (cheap classifiers
//!   first, only candidates that clear the cheap classifier go to
//!   more expensive stages).
//! * Phase 1 deliverables: `PHASES.md` Phase 1.

#![deny(missing_docs)]

pub mod error;
pub mod extractor;
pub mod pipeline;
pub mod promotion;
pub mod types;

pub use error::{ObservationError, Result};
pub use extractor::{LexiconExtractor, ObservationExtractor};
pub use pipeline::ObservationPipeline;
pub use promotion::{should_promote, ChannelPromotionPolicy, PromotionReason, PromotionResult};
pub use types::{Observation, ObservationType};
