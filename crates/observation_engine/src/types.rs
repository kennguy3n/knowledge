//! Observation and observation type definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::{EvidenceId, ScopeId};
use memory_manager::MemoryState;

/// The five observation types that the substrate models in Phase 1.
///
/// Per `PROPOSAL.md` §3.2: "Normalized facts, claims, entities,
/// tasks, decisions extracted from evidence." The `Claim` type
/// covers structured assertions that are not yet corroborated; once
/// they are, the memory manager promotes them through `Reinforced ->
/// Consolidated -> Canonical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObservationType {
    /// A named entity (person, project, system).
    Entity,
    /// A declarative fact ("the migration ships Friday").
    Fact,
    /// An action item ("@Sara please draft the RFC").
    Task,
    /// An explicit decision ("approved the new policy").
    Decision,
    /// A claim that hasn't been corroborated yet.
    Claim,
}

impl ObservationType {
    /// Stable string tag used for serialisation and metadata
    /// matching.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Fact => "fact",
            Self::Task => "task",
            Self::Decision => "decision",
            Self::Claim => "claim",
        }
    }
}

/// One structured observation extracted from raw evidence text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Unique id (UUID v4).
    pub id: Uuid,
    /// What kind of observation this is.
    pub observation_type: ObservationType,
    /// Free-form content (the canonical surface form of the
    /// observation).
    pub content: String,
    /// Confidence score in `0.0 ..= 1.0`. The Phase 1 lexicon
    /// extractor uses fixed per-type confidences (see
    /// [`crate::extractor::LexiconExtractor`]).
    pub confidence: f64,
    /// Evidence rows this observation was extracted from.
    pub source_evidence_ids: Vec<EvidenceId>,
    /// Scope this observation belongs to.
    pub scope_id: ScopeId,
    /// Wall-clock extraction time.
    pub created_at: DateTime<Utc>,
    /// Memory state at the time of extraction. Always
    /// [`MemoryState::Candidate`] for fresh extractions.
    pub memory_state: MemoryState,
}

impl Observation {
    /// Construct a fresh candidate observation.
    pub fn new_candidate(
        observation_type: ObservationType,
        content: impl Into<String>,
        scope_id: ScopeId,
        confidence: f64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            observation_type,
            content: content.into(),
            confidence,
            source_evidence_ids: Vec::new(),
            scope_id,
            created_at: Utc::now(),
            memory_state: MemoryState::Candidate,
        }
    }
}
