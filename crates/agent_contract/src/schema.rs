//! Schema validation for agent proposals.
//!
//! Per `docs/technical/design.md` §7.3 every proposal must:
//!
//! * Carry a non-nil [`evidence_store::ScopeId`].
//! * Have at least one evidence reference.
//! * Carry a confidence score in `[0.0, 1.0]`.
//! * Carry a non-empty agent identity (id, name, model name, model
//!   version are mandatory; skill / recipe ids are optional but,
//!   when present, must be non-empty).
//! * Have `TTL > 0` if a TTL is supplied.
//!
//! Validation runs *before* a proposal enters the
//! [`crate::lifecycle::ProposalStore`] — the store rejects any
//! invalid proposal up front so the lifecycle state machine never has
//! to consider a malformed envelope.

use thiserror::Error;
use uuid::Uuid;

use crate::proposal::{
    AgentProposal, ConceptProposal, ObservationProposal, RelationProposal, SummaryProposal,
};

/// Validation failure for an agent proposal.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ProposalValidationError {
    /// Confidence is not in `[0.0, 1.0]` or is NaN.
    #[error("confidence {0} is not in [0.0, 1.0]")]
    ConfidenceOutOfRange(f64),
    /// Confidence is NaN.
    #[error("confidence is NaN")]
    ConfidenceNaN,
    /// Proposal carried no evidence references.
    #[error("proposal must carry at least one evidence_ref")]
    NoEvidence,
    /// Proposal carried a nil scope id.
    #[error("scope_id must not be nil")]
    NilScope,
    /// `agent_identity.agent_id` was nil.
    #[error("agent_identity.agent_id must not be nil")]
    NilAgentId,
    /// `agent_identity.name` was empty.
    #[error("agent_identity.name must not be empty")]
    EmptyAgentName,
    /// `agent_identity.model_name` was empty.
    #[error("agent_identity.model_name must not be empty")]
    EmptyModelName,
    /// `agent_identity.model_version` was empty.
    #[error("agent_identity.model_version must not be empty")]
    EmptyModelVersion,
    /// `agent_identity.skill_id` was supplied but empty.
    #[error("agent_identity.skill_id must not be empty when present")]
    EmptySkillId,
    /// `agent_identity.recipe_id` was supplied but empty.
    #[error("agent_identity.recipe_id must not be empty when present")]
    EmptyRecipeId,
    /// TTL was supplied but was zero.
    #[error("ttl must be > 0 when present")]
    ZeroTtl,
    /// A required payload field was empty.
    #[error("payload field `{0}` must not be empty")]
    EmptyPayloadField(&'static str),
    /// A relation proposal had `src == dst`.
    #[error("relation proposal must not have src == dst")]
    SelfRelation,
    /// A relation proposal carried a nil endpoint id.
    #[error("relation proposal endpoint must not be nil")]
    NilRelationEndpoint,
}

/// Trait implemented by every payload type that can be carried by an
/// [`AgentProposal`]. Each payload knows how to validate its own
/// type-specific fields; the generic envelope validates the shared
/// metadata.
pub trait ValidatePayload {
    /// Validate type-specific payload fields.
    fn validate(&self) -> Result<(), ProposalValidationError>;
}

impl ValidatePayload for ObservationProposal {
    fn validate(&self) -> Result<(), ProposalValidationError> {
        if self.claim.trim().is_empty() {
            return Err(ProposalValidationError::EmptyPayloadField("claim"));
        }
        if self.observation_type.trim().is_empty() {
            return Err(ProposalValidationError::EmptyPayloadField(
                "observation_type",
            ));
        }
        Ok(())
    }
}

impl ValidatePayload for ConceptProposal {
    fn validate(&self) -> Result<(), ProposalValidationError> {
        if self.label.trim().is_empty() {
            return Err(ProposalValidationError::EmptyPayloadField("label"));
        }
        if self.definition.trim().is_empty() {
            return Err(ProposalValidationError::EmptyPayloadField("definition"));
        }
        Ok(())
    }
}

impl ValidatePayload for RelationProposal {
    fn validate(&self) -> Result<(), ProposalValidationError> {
        if self.src == Uuid::nil() || self.dst == Uuid::nil() {
            return Err(ProposalValidationError::NilRelationEndpoint);
        }
        if self.src == self.dst {
            return Err(ProposalValidationError::SelfRelation);
        }
        if self.relation.as_str().trim().is_empty() {
            return Err(ProposalValidationError::EmptyPayloadField("relation"));
        }
        Ok(())
    }
}

impl ValidatePayload for SummaryProposal {
    fn validate(&self) -> Result<(), ProposalValidationError> {
        if self.text.trim().is_empty() {
            return Err(ProposalValidationError::EmptyPayloadField("text"));
        }
        if self.summary_type.trim().is_empty() {
            return Err(ProposalValidationError::EmptyPayloadField("summary_type"));
        }
        Ok(())
    }
}

/// Validate the envelope-level fields of a proposal plus the
/// type-specific payload.
///
/// # Errors
///
/// Returns the first violated invariant as a
/// [`ProposalValidationError`].
pub fn validate_proposal<T: ValidatePayload>(
    proposal: &AgentProposal<T>,
) -> Result<(), ProposalValidationError> {
    if proposal.confidence.is_nan() {
        return Err(ProposalValidationError::ConfidenceNaN);
    }
    if !(0.0..=1.0).contains(&proposal.confidence) {
        return Err(ProposalValidationError::ConfidenceOutOfRange(
            proposal.confidence,
        ));
    }
    if proposal.evidence_refs.is_empty() {
        return Err(ProposalValidationError::NoEvidence);
    }
    if proposal.scope_id.0 == Uuid::nil() {
        return Err(ProposalValidationError::NilScope);
    }
    if proposal.agent_identity.agent_id == Uuid::nil() {
        return Err(ProposalValidationError::NilAgentId);
    }
    if proposal.agent_identity.name.trim().is_empty() {
        return Err(ProposalValidationError::EmptyAgentName);
    }
    if proposal.agent_identity.model_name.trim().is_empty() {
        return Err(ProposalValidationError::EmptyModelName);
    }
    if proposal.agent_identity.model_version.trim().is_empty() {
        return Err(ProposalValidationError::EmptyModelVersion);
    }
    if let Some(skill) = &proposal.agent_identity.skill_id {
        if skill.trim().is_empty() {
            return Err(ProposalValidationError::EmptySkillId);
        }
    }
    if let Some(recipe) = &proposal.agent_identity.recipe_id {
        if recipe.trim().is_empty() {
            return Err(ProposalValidationError::EmptyRecipeId);
        }
    }
    if let Some(ttl) = proposal.ttl {
        if ttl.is_zero() {
            return Err(ProposalValidationError::ZeroTtl);
        }
    }
    proposal.payload.validate()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crypto::EvidenceRef;
    use evidence_store::ScopeId;
    use memory_manager::SensitivityClass;

    use super::*;
    use crate::proposal::{
        AgentIdentity, AgentProposal, ConceptProposal, ObservationProposal, ProposalKind,
        RelationProposal, RelationType, SummaryProposal,
    };

    fn fixture_identity() -> AgentIdentity {
        AgentIdentity::new(Uuid::new_v4(), "agent", "bonsai", "v1")
    }

    fn fixture_observation() -> AgentProposal<ObservationProposal> {
        AgentProposal::new(
            ProposalKind::Observation,
            ScopeId::new_v4(),
            ObservationProposal::new("claim", "fact"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.7,
            SensitivityClass::Useful,
            fixture_identity(),
        )
    }

    #[test]
    fn valid_observation_passes() {
        validate_proposal(&fixture_observation()).expect("ok");
    }

    #[test]
    fn confidence_out_of_range_low() {
        let mut p = fixture_observation();
        p.confidence = -0.1;
        assert!(matches!(
            validate_proposal(&p),
            Err(ProposalValidationError::ConfidenceOutOfRange(_))
        ));
    }

    #[test]
    fn confidence_out_of_range_high() {
        let mut p = fixture_observation();
        p.confidence = 1.1;
        assert!(matches!(
            validate_proposal(&p),
            Err(ProposalValidationError::ConfidenceOutOfRange(_))
        ));
    }

    #[test]
    fn confidence_nan_rejected() {
        let mut p = fixture_observation();
        p.confidence = f64::NAN;
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::ConfidenceNaN)
        );
    }

    #[test]
    fn no_evidence_rejected() {
        let mut p = fixture_observation();
        p.evidence_refs.clear();
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::NoEvidence)
        );
    }

    #[test]
    fn nil_scope_rejected() {
        let mut p = fixture_observation();
        p.scope_id = ScopeId(Uuid::nil());
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::NilScope)
        );
    }

    #[test]
    fn nil_agent_id_rejected() {
        let mut p = fixture_observation();
        p.agent_identity.agent_id = Uuid::nil();
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::NilAgentId)
        );
    }

    #[test]
    fn empty_agent_name_rejected() {
        let mut p = fixture_observation();
        p.agent_identity.name = String::new();
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::EmptyAgentName)
        );
    }

    #[test]
    fn empty_model_fields_rejected() {
        let mut p = fixture_observation();
        p.agent_identity.model_name = String::new();
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::EmptyModelName)
        );
        let mut q = fixture_observation();
        q.agent_identity.model_version = String::new();
        assert_eq!(
            validate_proposal(&q),
            Err(ProposalValidationError::EmptyModelVersion)
        );
    }

    #[test]
    fn empty_skill_recipe_rejected() {
        let mut p = fixture_observation();
        p.agent_identity.skill_id = Some(String::new());
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::EmptySkillId)
        );
        let mut q = fixture_observation();
        q.agent_identity.recipe_id = Some(String::new());
        assert_eq!(
            validate_proposal(&q),
            Err(ProposalValidationError::EmptyRecipeId)
        );
    }

    #[test]
    fn zero_ttl_rejected() {
        let mut p = fixture_observation();
        p.ttl = Some(Duration::from_secs(0));
        assert_eq!(validate_proposal(&p), Err(ProposalValidationError::ZeroTtl));
    }

    #[test]
    fn empty_payload_observation_rejected() {
        let mut p = fixture_observation();
        p.payload.claim = String::new();
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::EmptyPayloadField("claim"))
        );
    }

    #[test]
    fn empty_payload_concept_rejected() {
        let p = AgentProposal::new(
            ProposalKind::Concept,
            ScopeId::new_v4(),
            ConceptProposal::new("", "definition"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.5,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::EmptyPayloadField("label"))
        );
    }

    #[test]
    fn relation_self_loop_rejected() {
        let same = Uuid::new_v4();
        let p = AgentProposal::new(
            ProposalKind::Relation,
            ScopeId::new_v4(),
            RelationProposal::new(same, same, RelationType::new("is_a")),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.5,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::SelfRelation)
        );
    }

    #[test]
    fn relation_nil_endpoint_rejected() {
        let p = AgentProposal::new(
            ProposalKind::Relation,
            ScopeId::new_v4(),
            RelationProposal::new(Uuid::nil(), Uuid::new_v4(), RelationType::new("is_a")),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.5,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::NilRelationEndpoint)
        );
    }

    #[test]
    fn empty_payload_summary_rejected() {
        let p = AgentProposal::new(
            ProposalKind::Summary,
            ScopeId::new_v4(),
            SummaryProposal::new("", "episodic"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.5,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        assert_eq!(
            validate_proposal(&p),
            Err(ProposalValidationError::EmptyPayloadField("text"))
        );
    }
}
