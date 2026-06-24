//! Observation and observation type definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::{EvidenceId, ScopeId};
use memory_manager::MemoryState;

use crate::entity_types::{EntityType, IdentifierKind};
use crate::language::LanguageTag;

/// The five observation types that the substrate models.
///
/// Per `docs/technical/design.md` §3.2: "Normalized facts, claims, entities,
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
    /// A question — sentences ending in `?` or starting with
    /// interrogative words. Surfaces as channel-memory open
    /// questions.
    Question,
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
            Self::Question => "question",
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
    /// Confidence score in `0.0 ..= 1.0`. The lexicon extractor
    /// uses fixed per-type confidences (see
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
    /// BCP-47 primary language subtag for the source text the
    /// observation was extracted from. `None` when the
    /// upstream language detector either declined to classify the
    /// input or marked the result as unreliable. Downstream
    /// consumers (multilingual lexicon registry, per-locale FTS5
    /// tokenizer) MUST treat `None` as "unknown" rather than
    /// substitute a default — see
    /// [`crate::language::detect_language`] for the contract.
    #[serde(default)]
    pub language_tag: Option<LanguageTag>,
    /// Typed sub-classification when `observation_type == Entity`.
    /// `None` for non-Entity observation types, or for Entity
    /// observations that could not be classified into a more
    /// specific type (legacy / pre-G10 entities).
    ///
    /// See [`crate::entity_types::EntityType`] for the taxonomy.
    #[serde(default)]
    pub entity_type: Option<EntityType>,
    /// Industry-specific identifier sub-type when
    /// `entity_type == Some(EntityType::Identifier)`. `None`
    /// for all other entity types and non-Entity observations.
    ///
    /// See [`crate::entity_types::IdentifierKind`] for the
    /// full list of recognised identifier kinds.
    #[serde(default)]
    pub identifier_kind: Option<IdentifierKind>,
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
            language_tag: None,
            entity_type: None,
            identifier_kind: None,
        }
    }

    /// Builder-style helper: stamp the detected source language
    /// onto this observation. Called by
    /// [`crate::pipeline::ObservationPipeline::run`] after
    /// [`crate::language::detect_language`] runs over the raw
    /// input text, so every observation produced from a single
    /// ingestion shares the same language tag.
    pub fn with_language_tag(mut self, tag: Option<LanguageTag>) -> Self {
        self.language_tag = tag;
        self
    }

    /// Builder-style helper: stamp the typed entity sub-
    /// classification onto this observation. Only meaningful
    /// when `observation_type == Entity`.
    pub fn with_entity_type(mut self, entity_type: EntityType) -> Self {
        self.entity_type = Some(entity_type);
        self
    }

    /// Builder-style helper: stamp the identifier sub-kind onto
    /// this observation. Implies `entity_type == Identifier`.
    pub fn with_identifier_kind(mut self, kind: IdentifierKind) -> Self {
        self.entity_type = Some(EntityType::Identifier);
        self.identifier_kind = Some(kind);
        self
    }
}
