//! Proposal lifecycle state machine.
//!
//! Per `PROPOSAL.md` §7.3, every agent proposal moves through this
//! state machine before it can affect canonical memory:
//!
//! ```text
//! Proposed ──► UnderReview ──► Promoted
//!                          └──► Rejected
//! ```
//!
//! `Proposed → UnderReview` is the manual or automatic admission of a
//! new proposal into the review pipeline. `UnderReview → Promoted`
//! happens either through an explicit human action or because a
//! tenant policy auto-promotes proposals matching specific criteria
//! (see [`AutoPromotionPolicy`]). `UnderReview → Rejected` is the
//! mirror transition for rejection.
//!
//! The lifecycle layer also produces the canonical artifact that the
//! substrate consumes once a proposal is promoted — see
//! [`CanonicalArtifact`] and [`ProposalStore::promote_to_canonical`].

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use evidence_store::ScopeId;
use memory_manager::SensitivityClass;

use crate::proposal::{
    AgentIdentity, AgentProposal, ConceptProposal, ObservationProposal, ProposalKind,
    RelationProposal, RelationType as ProposalRelationType, SummaryProposal,
};
use crate::schema::{validate_proposal, ProposalValidationError};

/// One state of a proposal in the lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    /// Newly submitted; not yet under review.
    Proposed,
    /// Admitted into the review pipeline.
    UnderReview,
    /// Promoted to canonical (terminal).
    Promoted,
    /// Rejected (terminal).
    Rejected,
}

impl ProposalState {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::UnderReview => "under_review",
            Self::Promoted => "promoted",
            Self::Rejected => "rejected",
        }
    }

    /// True iff `self` is a terminal state.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Promoted | Self::Rejected)
    }
}

/// Auto-promotion policy.
///
/// Criteria are AND-ed: a proposal must satisfy *all* fields to be
/// auto-promoted. `min_corroboration = 0` admits any proposal on the
/// corroboration axis. `require_human_for_critical = true` forces
/// manual review for any [`SensitivityClass::Critical`] proposal even
/// when every other criterion is satisfied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoPromotionPolicy {
    /// Minimum confidence (in `[0.0, 1.0]`) required for
    /// auto-promotion.
    pub min_confidence: f64,
    /// Minimum cross-source corroboration count required for
    /// auto-promotion.
    pub min_corroboration: u32,
    /// Strictest sensitivity class allowed under this policy. A
    /// proposal whose sensitivity is *more* sensitive than this is
    /// not auto-promotable.
    pub max_sensitivity: SensitivityClass,
    /// When `true`, [`SensitivityClass::Critical`] proposals always
    /// require explicit human review even if `max_sensitivity` would
    /// admit them.
    pub require_human_for_critical: bool,
}

impl Default for AutoPromotionPolicy {
    /// Default deny-by-default policy: nothing is auto-promotable.
    fn default() -> Self {
        Self {
            min_confidence: 1.1, // unreachable upper bound — default = no auto-promotion
            min_corroboration: u32::MAX,
            max_sensitivity: SensitivityClass::Noise,
            require_human_for_critical: true,
        }
    }
}

impl AutoPromotionPolicy {
    /// Construct a policy with the given thresholds.
    pub fn new(
        min_confidence: f64,
        min_corroboration: u32,
        max_sensitivity: SensitivityClass,
        require_human_for_critical: bool,
    ) -> Self {
        Self {
            min_confidence,
            min_corroboration,
            max_sensitivity,
            require_human_for_critical,
        }
    }

    /// Sensitivity rank — higher number = more sensitive.
    fn rank(class: SensitivityClass) -> u8 {
        match class {
            SensitivityClass::Noise => 0,
            SensitivityClass::Useful => 1,
            SensitivityClass::Important => 2,
            SensitivityClass::Critical => 3,
        }
    }

    /// True iff a proposal in `(confidence, corroboration_count,
    /// sensitivity)` matches this policy's criteria.
    pub fn matches(
        &self,
        confidence: f64,
        corroboration_count: u32,
        sensitivity: SensitivityClass,
    ) -> bool {
        if confidence < self.min_confidence {
            return false;
        }
        if corroboration_count < self.min_corroboration {
            return false;
        }
        if Self::rank(sensitivity) > Self::rank(self.max_sensitivity) {
            return false;
        }
        if self.require_human_for_critical && sensitivity == SensitivityClass::Critical {
            return false;
        }
        true
    }
}

/// Decision returned by [`ProposalStore::review`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalDecision {
    /// The proposal moved to [`ProposalState::UnderReview`] but does
    /// not match the policy — it needs an explicit human action.
    NeedsHumanReview,
    /// The proposal was auto-promoted to [`ProposalState::Promoted`].
    AutoPromoted,
}

/// Errors raised by the [`ProposalStore`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum LifecycleError {
    /// The proposal id was not found in the store.
    #[error("proposal {0} not found")]
    NotFound(Uuid),
    /// The proposal is in a state that does not allow the requested
    /// transition.
    #[error("proposal {id} in state {from} cannot transition to {to}")]
    InvalidTransition {
        /// The proposal id.
        id: Uuid,
        /// Current state.
        from: &'static str,
        /// Target state.
        to: &'static str,
    },
    /// The proposal failed schema validation on submission.
    #[error("proposal validation failed: {0}")]
    Validation(#[from] ProposalValidationError),
    /// The proposal had a TTL and the TTL has elapsed.
    #[error("proposal {0} has expired")]
    Expired(Uuid),
}

/// Type-erased proposal payload.
///
/// Stored inside [`StoredProposal`] so the lifecycle layer can hold a
/// uniform collection of proposals across the four payload types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum AnyPayload {
    /// Observation payload.
    Observation(ObservationProposal),
    /// Concept payload.
    Concept(ConceptProposal),
    /// Relation payload.
    Relation(RelationProposal),
    /// Summary payload.
    Summary(SummaryProposal),
}

impl AnyPayload {
    /// Return the [`ProposalKind`] tag for this payload.
    pub const fn kind(&self) -> ProposalKind {
        match self {
            Self::Observation(_) => ProposalKind::Observation,
            Self::Concept(_) => ProposalKind::Concept,
            Self::Relation(_) => ProposalKind::Relation,
            Self::Summary(_) => ProposalKind::Summary,
        }
    }
}

/// One proposal as it lives in the [`ProposalStore`].
///
/// Wraps the typed envelope's metadata into a single owned record so
/// the store can hold heterogeneous payloads in one
/// `HashMap<Uuid, _>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredProposal {
    /// Stable proposal id.
    pub id: Uuid,
    /// Current state in the lifecycle state machine.
    pub state: ProposalState,
    /// Substrate scope this proposal lives in.
    pub scope_id: ScopeId,
    /// Type-erased payload.
    pub payload: AnyPayload,
    /// Evidence rows backing the proposal.
    pub evidence_refs: Vec<crypto::EvidenceRef>,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Sensitivity class.
    pub sensitivity_class: SensitivityClass,
    /// Cross-source corroboration count — bumped via
    /// [`ProposalStore::record_corroboration`] when a separate
    /// evidence source supports the same claim.
    pub corroboration_count: u32,
    /// Optional TTL.
    pub ttl: Option<Duration>,
    /// Optional supersedes link.
    pub supersedes: Option<Uuid>,
    /// Optional contradicts link.
    pub contradicts: Option<Uuid>,
    /// Identity of the agent that produced this proposal.
    pub agent_identity: AgentIdentity,
    /// Submission time.
    pub submitted_at: DateTime<Utc>,
    /// Most-recent state-transition time.
    pub updated_at: DateTime<Utc>,
    /// Optional rejection reason once the proposal is in
    /// [`ProposalState::Rejected`].
    pub rejection_reason: Option<String>,
}

impl StoredProposal {
    /// Has this proposal expired against `now`?
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        let Some(ttl) = self.ttl else {
            return false;
        };
        let elapsed = now.signed_duration_since(self.submitted_at);
        let Ok(elapsed_std) = elapsed.to_std() else {
            // `now < submitted_at`; treat as not expired.
            return false;
        };
        elapsed_std >= ttl
    }
}

/// Canonical artifact produced by [`ProposalStore::promote_to_canonical`].
///
/// The substrate consumes one of these to actually insert a canonical
/// observation / concept / relation / summary into the relevant
/// downstream crate (`memory_manager`, `concept_graph`,
/// `synthesis_pipeline`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CanonicalArtifact {
    /// Canonical observation ready for the memory manager.
    Observation(CanonicalObservation),
    /// Canonical concept ready for the concept graph.
    Concept(CanonicalConcept),
    /// Canonical relation ready for the concept graph.
    Relation(CanonicalRelation),
    /// Canonical summary ready for the synthesis pipeline.
    Summary(CanonicalSummary),
}

impl CanonicalArtifact {
    /// Stable kind tag mirroring the variant.
    pub const fn kind(&self) -> ProposalKind {
        match self {
            Self::Observation(_) => ProposalKind::Observation,
            Self::Concept(_) => ProposalKind::Concept,
            Self::Relation(_) => ProposalKind::Relation,
            Self::Summary(_) => ProposalKind::Summary,
        }
    }

    /// Substrate id of the canonical object.
    pub fn id(&self) -> Uuid {
        match self {
            Self::Observation(o) => o.id,
            Self::Concept(c) => c.id,
            Self::Relation(r) => r.id,
            Self::Summary(s) => s.id,
        }
    }
}

/// Canonical observation ready for insertion into `memory_manager`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalObservation {
    /// Substrate id.
    pub id: Uuid,
    /// Scope.
    pub scope_id: ScopeId,
    /// Originating proposal id.
    pub proposal_id: Uuid,
    /// Observation claim.
    pub claim: String,
    /// Observation type tag.
    pub observation_type: String,
    /// Sensitivity class.
    pub sensitivity_class: SensitivityClass,
    /// Evidence rows backing this observation.
    pub evidence_refs: Vec<crypto::EvidenceRef>,
}

/// Canonical concept ready for insertion into `concept_graph`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalConcept {
    /// Substrate id.
    pub id: Uuid,
    /// Scope.
    pub scope_id: ScopeId,
    /// Originating proposal id.
    pub proposal_id: Uuid,
    /// Concept label.
    pub label: String,
    /// Concept definition.
    pub definition: String,
    /// Sensitivity class.
    pub sensitivity_class: SensitivityClass,
}

/// Canonical relation ready for insertion into `concept_graph`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalRelation {
    /// Substrate id.
    pub id: Uuid,
    /// Scope.
    pub scope_id: ScopeId,
    /// Originating proposal id.
    pub proposal_id: Uuid,
    /// Source object id.
    pub src: Uuid,
    /// Destination object id.
    pub dst: Uuid,
    /// Typed relation tag.
    pub relation: ProposalRelationType,
}

/// Canonical summary ready for insertion into `synthesis_pipeline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalSummary {
    /// Substrate id.
    pub id: Uuid,
    /// Scope.
    pub scope_id: ScopeId,
    /// Originating proposal id.
    pub proposal_id: Uuid,
    /// Summary body.
    pub text: String,
    /// Summary type tag.
    pub summary_type: String,
    /// Sensitivity class.
    pub sensitivity_class: SensitivityClass,
}

/// In-memory proposal store + lifecycle manager.
///
/// All `submit` / `review` / `promote` / `reject` operations are
/// state-machine validated. Mismatched transitions surface as
/// [`LifecycleError::InvalidTransition`].
#[derive(Debug, Default, Clone)]
pub struct ProposalStore {
    proposals: HashMap<Uuid, StoredProposal>,
}

impl ProposalStore {
    /// Construct a fresh empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of proposals currently in the store (any state).
    pub fn len(&self) -> usize {
        self.proposals.len()
    }

    /// True iff the store is empty.
    pub fn is_empty(&self) -> bool {
        self.proposals.is_empty()
    }

    /// Borrow a proposal by id.
    pub fn get(&self, id: Uuid) -> Option<&StoredProposal> {
        self.proposals.get(&id)
    }

    /// Submit an observation proposal. Returns the assigned id.
    pub fn submit_observation(
        &mut self,
        proposal: AgentProposal<ObservationProposal>,
    ) -> Result<Uuid, LifecycleError> {
        validate_proposal(&proposal)?;
        let id = proposal.id;
        let stored = stored_from_envelope(proposal, AnyPayload::Observation);
        self.proposals.insert(id, stored);
        Ok(id)
    }

    /// Submit a concept proposal. Returns the assigned id.
    pub fn submit_concept(
        &mut self,
        proposal: AgentProposal<ConceptProposal>,
    ) -> Result<Uuid, LifecycleError> {
        validate_proposal(&proposal)?;
        let id = proposal.id;
        let stored = stored_from_envelope(proposal, AnyPayload::Concept);
        self.proposals.insert(id, stored);
        Ok(id)
    }

    /// Submit a relation proposal. Returns the assigned id.
    pub fn submit_relation(
        &mut self,
        proposal: AgentProposal<RelationProposal>,
    ) -> Result<Uuid, LifecycleError> {
        validate_proposal(&proposal)?;
        let id = proposal.id;
        let stored = stored_from_envelope(proposal, AnyPayload::Relation);
        self.proposals.insert(id, stored);
        Ok(id)
    }

    /// Submit a summary proposal. Returns the assigned id.
    pub fn submit_summary(
        &mut self,
        proposal: AgentProposal<SummaryProposal>,
    ) -> Result<Uuid, LifecycleError> {
        validate_proposal(&proposal)?;
        let id = proposal.id;
        let stored = stored_from_envelope(proposal, AnyPayload::Summary);
        self.proposals.insert(id, stored);
        Ok(id)
    }

    /// Bump the cross-source corroboration count for `id`.
    ///
    /// Used by tests and by upstream connectors to record that an
    /// independent evidence source produced the same claim.
    pub fn record_corroboration(&mut self, id: Uuid) -> Result<(), LifecycleError> {
        let p = self
            .proposals
            .get_mut(&id)
            .ok_or(LifecycleError::NotFound(id))?;
        p.corroboration_count = p.corroboration_count.saturating_add(1);
        p.updated_at = Utc::now();
        Ok(())
    }

    /// Move a proposal from [`ProposalState::Proposed`] to
    /// [`ProposalState::UnderReview`] and run the auto-promotion
    /// policy. If the policy matches, the proposal is auto-promoted
    /// in the same call.
    pub fn review(
        &mut self,
        id: Uuid,
        policy: &AutoPromotionPolicy,
    ) -> Result<ProposalDecision, LifecycleError> {
        let now = Utc::now();
        let p = self
            .proposals
            .get_mut(&id)
            .ok_or(LifecycleError::NotFound(id))?;
        if p.is_expired(now) {
            // Expired proposals get auto-rejected.
            p.state = ProposalState::Rejected;
            p.updated_at = now;
            p.rejection_reason = Some("ttl_expired".into());
            return Err(LifecycleError::Expired(id));
        }
        if p.state != ProposalState::Proposed {
            return Err(LifecycleError::InvalidTransition {
                id,
                from: p.state.as_str(),
                to: ProposalState::UnderReview.as_str(),
            });
        }
        p.state = ProposalState::UnderReview;
        p.updated_at = now;
        let auto = policy.matches(p.confidence, p.corroboration_count, p.sensitivity_class);
        if auto {
            p.state = ProposalState::Promoted;
            p.updated_at = now;
            return Ok(ProposalDecision::AutoPromoted);
        }
        Ok(ProposalDecision::NeedsHumanReview)
    }

    /// Promote a proposal that is currently in
    /// [`ProposalState::UnderReview`] to
    /// [`ProposalState::Promoted`].
    pub fn promote(&mut self, id: Uuid) -> Result<(), LifecycleError> {
        let p = self
            .proposals
            .get_mut(&id)
            .ok_or(LifecycleError::NotFound(id))?;
        if p.state != ProposalState::UnderReview {
            return Err(LifecycleError::InvalidTransition {
                id,
                from: p.state.as_str(),
                to: ProposalState::Promoted.as_str(),
            });
        }
        p.state = ProposalState::Promoted;
        p.updated_at = Utc::now();
        Ok(())
    }

    /// Reject a proposal. Allowed from either
    /// [`ProposalState::Proposed`] or [`ProposalState::UnderReview`].
    pub fn reject(&mut self, id: Uuid, reason: impl Into<String>) -> Result<(), LifecycleError> {
        let p = self
            .proposals
            .get_mut(&id)
            .ok_or(LifecycleError::NotFound(id))?;
        if p.state.is_terminal() {
            return Err(LifecycleError::InvalidTransition {
                id,
                from: p.state.as_str(),
                to: ProposalState::Rejected.as_str(),
            });
        }
        p.state = ProposalState::Rejected;
        p.updated_at = Utc::now();
        p.rejection_reason = Some(reason.into());
        Ok(())
    }

    /// Render the canonical artifact for a proposal that is in
    /// [`ProposalState::Promoted`]. The returned artifact is ready to
    /// be inserted into the substrate's downstream crates.
    pub fn promote_to_canonical(&self, id: Uuid) -> Result<CanonicalArtifact, LifecycleError> {
        let p = self
            .proposals
            .get(&id)
            .ok_or(LifecycleError::NotFound(id))?;
        if p.state != ProposalState::Promoted {
            return Err(LifecycleError::InvalidTransition {
                id,
                from: p.state.as_str(),
                to: "canonical",
            });
        }
        Ok(match &p.payload {
            AnyPayload::Observation(o) => CanonicalArtifact::Observation(CanonicalObservation {
                id: Uuid::new_v4(),
                scope_id: p.scope_id,
                proposal_id: p.id,
                claim: o.claim.clone(),
                observation_type: o.observation_type.clone(),
                sensitivity_class: p.sensitivity_class,
                evidence_refs: p.evidence_refs.clone(),
            }),
            AnyPayload::Concept(c) => CanonicalArtifact::Concept(CanonicalConcept {
                id: Uuid::new_v4(),
                scope_id: p.scope_id,
                proposal_id: p.id,
                label: c.label.clone(),
                definition: c.definition.clone(),
                sensitivity_class: p.sensitivity_class,
            }),
            AnyPayload::Relation(r) => CanonicalArtifact::Relation(CanonicalRelation {
                id: Uuid::new_v4(),
                scope_id: p.scope_id,
                proposal_id: p.id,
                src: r.src,
                dst: r.dst,
                relation: r.relation.clone(),
            }),
            AnyPayload::Summary(s) => CanonicalArtifact::Summary(CanonicalSummary {
                id: Uuid::new_v4(),
                scope_id: p.scope_id,
                proposal_id: p.id,
                text: s.text.clone(),
                summary_type: s.summary_type.clone(),
                sensitivity_class: p.sensitivity_class,
            }),
        })
    }

    /// All proposal ids currently stored, in unspecified order.
    pub fn ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.proposals.keys().copied()
    }
}

fn stored_from_envelope<T, F: FnOnce(T) -> AnyPayload>(
    proposal: AgentProposal<T>,
    wrap: F,
) -> StoredProposal {
    StoredProposal {
        id: proposal.id,
        state: ProposalState::Proposed,
        scope_id: proposal.scope_id,
        payload: wrap(proposal.payload),
        evidence_refs: proposal.evidence_refs,
        confidence: proposal.confidence,
        sensitivity_class: proposal.sensitivity_class,
        corroboration_count: 0,
        ttl: proposal.ttl,
        supersedes: proposal.supersedes,
        contradicts: proposal.contradicts,
        agent_identity: proposal.agent_identity,
        submitted_at: proposal.created_at,
        updated_at: proposal.created_at,
        rejection_reason: None,
    }
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

    fn fixture_observation(
        confidence: f64,
        sensitivity: SensitivityClass,
    ) -> AgentProposal<ObservationProposal> {
        AgentProposal::new(
            ProposalKind::Observation,
            ScopeId::new_v4(),
            ObservationProposal::new("claim", "fact"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            confidence,
            sensitivity,
            fixture_identity(),
        )
    }

    fn permissive_policy() -> AutoPromotionPolicy {
        AutoPromotionPolicy::new(0.7, 0, SensitivityClass::Important, true)
    }

    #[test]
    fn submit_then_review_promotes_when_policy_matches() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.9, SensitivityClass::Useful))
            .expect("submit");
        let decision = store.review(id, &permissive_policy()).expect("review");
        assert_eq!(decision, ProposalDecision::AutoPromoted);
        assert_eq!(store.get(id).unwrap().state, ProposalState::Promoted);
    }

    #[test]
    fn submit_then_review_needs_human_when_below_confidence() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.5, SensitivityClass::Useful))
            .expect("submit");
        let decision = store.review(id, &permissive_policy()).expect("review");
        assert_eq!(decision, ProposalDecision::NeedsHumanReview);
        assert_eq!(store.get(id).unwrap().state, ProposalState::UnderReview);
    }

    #[test]
    fn auto_promotion_blocked_for_critical_when_required() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(1.0, SensitivityClass::Critical))
            .expect("submit");
        let policy = AutoPromotionPolicy::new(0.5, 0, SensitivityClass::Critical, true);
        let decision = store.review(id, &policy).expect("review");
        assert_eq!(decision, ProposalDecision::NeedsHumanReview);
    }

    #[test]
    fn auto_promotion_admits_critical_when_human_not_required() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(1.0, SensitivityClass::Critical))
            .expect("submit");
        let policy = AutoPromotionPolicy::new(0.5, 0, SensitivityClass::Critical, false);
        let decision = store.review(id, &policy).expect("review");
        assert_eq!(decision, ProposalDecision::AutoPromoted);
    }

    #[test]
    fn promote_requires_under_review() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.9, SensitivityClass::Useful))
            .expect("submit");
        // promote directly from Proposed should fail
        assert!(matches!(
            store.promote(id),
            Err(LifecycleError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn manual_promote_after_review() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.5, SensitivityClass::Useful))
            .expect("submit");
        store.review(id, &permissive_policy()).expect("review");
        store.promote(id).expect("promote");
        assert_eq!(store.get(id).unwrap().state, ProposalState::Promoted);
    }

    #[test]
    fn reject_from_proposed_or_under_review() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.9, SensitivityClass::Useful))
            .expect("submit");
        store.reject(id, "test").expect("reject");
        assert_eq!(store.get(id).unwrap().state, ProposalState::Rejected);

        let id2 = store
            .submit_observation(fixture_observation(0.5, SensitivityClass::Useful))
            .expect("submit");
        store.review(id2, &permissive_policy()).expect("review");
        store.reject(id2, "policy").expect("reject");
        assert_eq!(store.get(id2).unwrap().state, ProposalState::Rejected);
    }

    #[test]
    fn cannot_reject_promoted_proposal() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.9, SensitivityClass::Useful))
            .expect("submit");
        store.review(id, &permissive_policy()).expect("review");
        // Now Promoted — reject must fail
        assert!(matches!(
            store.reject(id, "test"),
            Err(LifecycleError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn promote_to_canonical_observation() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.9, SensitivityClass::Useful))
            .expect("submit");
        store.review(id, &permissive_policy()).expect("review");
        let artifact = store.promote_to_canonical(id).expect("canonical");
        match artifact {
            CanonicalArtifact::Observation(o) => {
                assert_eq!(o.claim, "claim");
                assert_eq!(o.proposal_id, id);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn promote_to_canonical_concept() {
        let mut store = ProposalStore::new();
        let p = AgentProposal::new(
            ProposalKind::Concept,
            ScopeId::new_v4(),
            ConceptProposal::new("Atlas", "Q3 launch"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.95,
            SensitivityClass::Important,
            fixture_identity(),
        );
        let id = store.submit_concept(p).expect("submit");
        store.review(id, &permissive_policy()).expect("review");
        let artifact = store.promote_to_canonical(id).expect("canonical");
        match artifact {
            CanonicalArtifact::Concept(c) => {
                assert_eq!(c.label, "Atlas");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn promote_to_canonical_relation() {
        let mut store = ProposalStore::new();
        let src = Uuid::new_v4();
        let dst = Uuid::new_v4();
        let p = AgentProposal::new(
            ProposalKind::Relation,
            ScopeId::new_v4(),
            RelationProposal::new(src, dst, RelationType::new("part_of")),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.9,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        let id = store.submit_relation(p).expect("submit");
        store.review(id, &permissive_policy()).expect("review");
        let artifact = store.promote_to_canonical(id).expect("canonical");
        match artifact {
            CanonicalArtifact::Relation(r) => {
                assert_eq!(r.src, src);
                assert_eq!(r.dst, dst);
                assert_eq!(r.relation.as_str(), "part_of");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn promote_to_canonical_summary() {
        let mut store = ProposalStore::new();
        let p = AgentProposal::new(
            ProposalKind::Summary,
            ScopeId::new_v4(),
            SummaryProposal::new("recap text", "channel"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            0.9,
            SensitivityClass::Useful,
            fixture_identity(),
        );
        let id = store.submit_summary(p).expect("submit");
        store.review(id, &permissive_policy()).expect("review");
        let artifact = store.promote_to_canonical(id).expect("canonical");
        match artifact {
            CanonicalArtifact::Summary(s) => {
                assert_eq!(s.text, "recap text");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn promote_to_canonical_requires_promoted() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.5, SensitivityClass::Useful))
            .expect("submit");
        // Still Proposed → must fail
        assert!(matches!(
            store.promote_to_canonical(id),
            Err(LifecycleError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn validation_failure_blocks_submission() {
        let mut store = ProposalStore::new();
        let mut p = fixture_observation(0.9, SensitivityClass::Useful);
        p.evidence_refs.clear();
        let err = store.submit_observation(p).unwrap_err();
        assert!(matches!(err, LifecycleError::Validation(_)));
    }

    #[test]
    fn expired_ttl_rejects_on_review() {
        let mut store = ProposalStore::new();
        let mut p = fixture_observation(0.9, SensitivityClass::Useful);
        p.ttl = Some(Duration::from_secs(1));
        // Pre-date the proposal so it has already expired by `now`.
        p.created_at = Utc::now() - chrono::Duration::seconds(60);
        let id = store.submit_observation(p).expect("submit");
        let err = store.review(id, &permissive_policy()).unwrap_err();
        assert_eq!(err, LifecycleError::Expired(id));
        assert_eq!(store.get(id).unwrap().state, ProposalState::Rejected);
    }

    #[test]
    fn corroboration_count_drives_auto_promotion() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.9, SensitivityClass::Useful))
            .expect("submit");
        let policy = AutoPromotionPolicy::new(0.7, 2, SensitivityClass::Important, true);
        // 0 corroboration → not enough
        let decision = store.review(id, &policy).expect("review");
        assert_eq!(decision, ProposalDecision::NeedsHumanReview);

        // Add corroboration and try a fresh proposal
        let id2 = store
            .submit_observation(fixture_observation(0.9, SensitivityClass::Useful))
            .expect("submit");
        store.record_corroboration(id2).expect("corroborate");
        store.record_corroboration(id2).expect("corroborate");
        let decision = store.review(id2, &policy).expect("review");
        assert_eq!(decision, ProposalDecision::AutoPromoted);
    }

    #[test]
    fn double_review_rejected() {
        let mut store = ProposalStore::new();
        let id = store
            .submit_observation(fixture_observation(0.5, SensitivityClass::Useful))
            .expect("submit");
        store.review(id, &permissive_policy()).expect("review");
        // Second review must fail (not in Proposed state)
        assert!(matches!(
            store.review(id, &permissive_policy()),
            Err(LifecycleError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn default_policy_denies_everything() {
        let policy = AutoPromotionPolicy::default();
        assert!(!policy.matches(1.0, u32::MAX, SensitivityClass::Noise));
    }

    #[test]
    fn proposal_state_terminal_check() {
        assert!(ProposalState::Promoted.is_terminal());
        assert!(ProposalState::Rejected.is_terminal());
        assert!(!ProposalState::Proposed.is_terminal());
        assert!(!ProposalState::UnderReview.is_terminal());
    }
}
