//! Proposal data model for the agent write contract.
//!
//! Per `PROPOSAL.md` §7.3 every agent proposal carries:
//!
//! * Scope (user / channel / domain / tenant).
//! * PROV bundle (signed by the synthesiser key — re-used from
//!   [`crypto::ProvenanceBundle`]).
//! * Evidence refs — the rows the proposal was derived from.
//! * Confidence score in `[0.0, 1.0]`.
//! * Sensitivity class (re-used from
//!   [`memory_manager::SensitivityClass`]).
//! * Optional TTL.
//! * Optional `supersedes` / `contradicts` links.
//! * Agent identity + model version + skill / recipe id.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crypto::EvidenceRef;
use evidence_store::ScopeId;
use memory_manager::SensitivityClass;

/// Identity of the agent that produced a proposal.
///
/// Per `PROPOSAL.md` §7.3 every proposal carries the agent identity,
/// the model name + version, and the skill / recipe id. Skill /
/// recipe ids are optional because some integrations (e.g. raw
/// connector pipelines) do not have an associated skill or recipe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentIdentity {
    /// Stable agent id (UUID v4).
    pub agent_id: Uuid,
    /// Human-readable agent name (e.g. `"slack-connector"`,
    /// `"nina-pm"`).
    pub name: String,
    /// Model name (e.g. `"bonsai-1.7b"`).
    pub model_name: String,
    /// Model version (e.g. `"q1_0_g128-2026-04-01"`).
    pub model_version: String,
    /// Optional skill id from the prompt / skill catalog.
    pub skill_id: Option<String>,
    /// Optional recipe id from the recipe catalog.
    pub recipe_id: Option<String>,
}

impl AgentIdentity {
    /// Construct a fresh agent identity.
    pub fn new(
        agent_id: Uuid,
        name: impl Into<String>,
        model_name: impl Into<String>,
        model_version: impl Into<String>,
    ) -> Self {
        Self {
            agent_id,
            name: name.into(),
            model_name: model_name.into(),
            model_version: model_version.into(),
            skill_id: None,
            recipe_id: None,
        }
    }

    /// Attach a skill id and return the updated identity.
    pub fn with_skill(mut self, skill_id: impl Into<String>) -> Self {
        self.skill_id = Some(skill_id.into());
        self
    }

    /// Attach a recipe id and return the updated identity.
    pub fn with_recipe(mut self, recipe_id: impl Into<String>) -> Self {
        self.recipe_id = Some(recipe_id.into());
        self
    }
}

/// Typed kinds of proposal payload, used for routing and audit
/// tagging without having to inspect the generic payload type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    /// An [`ObservationProposal`].
    Observation,
    /// A [`ConceptProposal`].
    Concept,
    /// A [`RelationProposal`].
    Relation,
    /// A [`SummaryProposal`].
    Summary,
}

impl ProposalKind {
    /// Stable string tag used for serialisation / debugging.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Concept => "concept",
            Self::Relation => "relation",
            Self::Summary => "summary",
        }
    }
}

/// Generic proposal envelope wrapping a typed payload.
///
/// `T` is one of [`ObservationProposal`], [`ConceptProposal`],
/// [`RelationProposal`], or [`SummaryProposal`]. The envelope carries
/// the scope, evidence refs, confidence, sensitivity, TTL, supersession
/// / contradiction links, and the agent identity that wrote the
/// proposal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentProposal<T> {
    /// Stable proposal id assigned by the substrate when the proposal
    /// is submitted (UUID v4).
    pub id: Uuid,
    /// Kind of payload — convenience tag mirroring the type of `T`.
    pub kind: ProposalKind,
    /// Scope this proposal lives in (user / channel / domain /
    /// tenant).
    pub scope_id: ScopeId,
    /// Typed payload.
    pub payload: T,
    /// Evidence rows backing the proposal.
    pub evidence_refs: Vec<EvidenceRef>,
    /// Confidence score in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Sensitivity class — drives downstream decay / export gating.
    pub sensitivity_class: SensitivityClass,
    /// Optional TTL after which the proposal auto-expires if it has
    /// not been promoted or rejected.
    pub ttl: Option<Duration>,
    /// Optional id of an existing canonical object this proposal
    /// supersedes.
    pub supersedes: Option<Uuid>,
    /// Optional id of an existing canonical object this proposal
    /// contradicts.
    pub contradicts: Option<Uuid>,
    /// Identity of the agent that produced this proposal.
    pub agent_identity: AgentIdentity,
    /// Wall-clock submission time. Defaults to the moment the
    /// envelope was constructed; promoted by the [`crate::lifecycle`]
    /// layer when the proposal is submitted.
    pub created_at: DateTime<Utc>,
}

impl<T> AgentProposal<T> {
    /// Construct a fresh proposal with a generated id and `created_at`
    /// stamped to now.
    pub fn new(
        kind: ProposalKind,
        scope_id: ScopeId,
        payload: T,
        evidence_refs: Vec<EvidenceRef>,
        confidence: f64,
        sensitivity_class: SensitivityClass,
        agent_identity: AgentIdentity,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            scope_id,
            payload,
            evidence_refs,
            confidence,
            sensitivity_class,
            ttl: None,
            supersedes: None,
            contradicts: None,
            agent_identity,
            created_at: Utc::now(),
        }
    }

    /// Attach a TTL.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Attach a supersedes link.
    pub fn supersedes(mut self, target: Uuid) -> Self {
        self.supersedes = Some(target);
        self
    }

    /// Attach a contradicts link.
    pub fn contradicts(mut self, target: Uuid) -> Self {
        self.contradicts = Some(target);
        self
    }

    /// Map the payload to a different type, preserving every other
    /// field. Useful for dispatch in
    /// [`crate::lifecycle::ProposalStore`].
    pub fn map_payload<U, F: FnOnce(T) -> U>(self, f: F) -> AgentProposal<U> {
        AgentProposal {
            id: self.id,
            kind: self.kind,
            scope_id: self.scope_id,
            payload: f(self.payload),
            evidence_refs: self.evidence_refs,
            confidence: self.confidence,
            sensitivity_class: self.sensitivity_class,
            ttl: self.ttl,
            supersedes: self.supersedes,
            contradicts: self.contradicts,
            agent_identity: self.agent_identity,
            created_at: self.created_at,
        }
    }
}

/// Observation proposal payload — a structured factual claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationProposal {
    /// The observation text (e.g. `"Atlas launches Q3 2026"`).
    pub claim: String,
    /// Free-form observation tag — `fact` / `task` / `decision` /
    /// `entity` / …
    pub observation_type: String,
}

impl ObservationProposal {
    /// Construct a fresh observation proposal payload.
    pub fn new(claim: impl Into<String>, observation_type: impl Into<String>) -> Self {
        Self {
            claim: claim.into(),
            observation_type: observation_type.into(),
        }
    }
}

/// Concept proposal payload — a new node for the semantic plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptProposal {
    /// Short label (e.g. `"Project Atlas"`).
    pub label: String,
    /// Long-form definition.
    pub definition: String,
}

impl ConceptProposal {
    /// Construct a fresh concept proposal payload.
    pub fn new(label: impl Into<String>, definition: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            definition: definition.into(),
        }
    }
}

/// Relation type tag for [`RelationProposal`].
///
/// Mirrors the typed-edge taxonomy from `PROPOSAL.md` §3.3. Kept as
/// a free-form `String` is intentional — agents may propose
/// substrate-extending relation types and the eventual review step
/// validates against the substrate's accepted set.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelationType(pub String);

impl RelationType {
    /// Wrap a raw label.
    pub fn new(label: impl Into<String>) -> Self {
        Self(label.into())
    }

    /// Borrow the label.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Relation proposal payload — a directed typed edge between two
/// substrate ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationProposal {
    /// Source object id.
    pub src: Uuid,
    /// Destination object id.
    pub dst: Uuid,
    /// Typed relation tag (e.g. `"is_a"`, `"part_of"`, `"derived_from"`).
    pub relation: RelationType,
}

impl RelationProposal {
    /// Construct a fresh relation proposal payload.
    pub fn new(src: Uuid, dst: Uuid, relation: RelationType) -> Self {
        Self { src, dst, relation }
    }
}

/// Summary proposal payload — episodic / channel / domain summary
/// text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryProposal {
    /// Summary text body.
    pub text: String,
    /// Free-form summary tag — `episodic` / `channel` / `domain` /
    /// `tenant` / …
    pub summary_type: String,
}

impl SummaryProposal {
    /// Construct a fresh summary proposal payload.
    pub fn new(text: impl Into<String>, summary_type: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            summary_type: summary_type.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_identity() -> AgentIdentity {
        AgentIdentity::new(
            Uuid::new_v4(),
            "nina-pm",
            "bonsai-1.7b",
            "q1_0_g128-2026-04-01",
        )
        .with_skill("synth.summary.v1")
        .with_recipe("recipe.weekly_digest")
    }

    #[test]
    fn observation_proposal_round_trip() {
        let scope = ScopeId::new_v4();
        let p = AgentProposal::new(
            ProposalKind::Observation,
            scope,
            ObservationProposal::new("Atlas launches Q3 2026", "fact"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.85,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        assert_eq!(p.kind, ProposalKind::Observation);
        assert_eq!(p.scope_id, scope);
        assert_eq!(p.evidence_refs.len(), 1);
        assert_eq!(p.payload.claim, "Atlas launches Q3 2026");
    }

    #[test]
    fn proposal_kind_string_tags() {
        assert_eq!(ProposalKind::Observation.as_str(), "observation");
        assert_eq!(ProposalKind::Concept.as_str(), "concept");
        assert_eq!(ProposalKind::Relation.as_str(), "relation");
        assert_eq!(ProposalKind::Summary.as_str(), "summary");
    }

    #[test]
    fn agent_identity_carries_skill_and_recipe() {
        let id = fixture_identity();
        assert_eq!(id.skill_id.as_deref(), Some("synth.summary.v1"));
        assert_eq!(id.recipe_id.as_deref(), Some("recipe.weekly_digest"));
    }

    #[test]
    fn ttl_supersedes_contradicts_builders() {
        let scope = ScopeId::new_v4();
        let target = Uuid::new_v4();
        let p = AgentProposal::new(
            ProposalKind::Concept,
            scope,
            ConceptProposal::new("Atlas", "Project codename for Q3"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.9,
            SensitivityClass::Important,
            fixture_identity(),
        )
        .with_ttl(Duration::from_secs(3600))
        .supersedes(target)
        .contradicts(target);
        assert_eq!(p.ttl, Some(Duration::from_secs(3600)));
        assert_eq!(p.supersedes, Some(target));
        assert_eq!(p.contradicts, Some(target));
    }

    #[test]
    fn map_payload_preserves_envelope() {
        let scope = ScopeId::new_v4();
        let p = AgentProposal::new(
            ProposalKind::Summary,
            scope,
            SummaryProposal::new("hello world", "episodic"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.5,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        let id = p.id;
        let mapped = p.map_payload(|s| s.text.len());
        assert_eq!(mapped.id, id);
        assert_eq!(mapped.payload, "hello world".len());
    }
}
