//! Integration test: agent proposal lifecycle.
//!
//! Agent proposes observation → verify recorded (not canonical) →
//! admin promotes → verify canonical → audit log records promotion.

use uuid::Uuid;

use agent_contract::{
    AgentIdentity, AgentProposal, AutoPromotionPolicy, CanonicalArtifact, ObservationProposal,
    ProposalDecision, ProposalKind, ProposalState, ProposalStore,
};
use audit_service::{
    log_proposal_promoted, log_proposal_submitted, Actor, AuditActionType, AuditLog, AuditQuery,
};
use crypto::EvidenceRef;
use integration_tests::test_helpers::ScopeId;
use memory_manager::SensitivityClass;

#[test]
fn agent_proposal_lifecycle_with_audit() {
    let scope = ScopeId::new_v4();
    let agent_id = Uuid::new_v4();
    let admin_id = Uuid::new_v4();

    let identity = AgentIdentity::new(agent_id, "test-agent", "bonsai-1.7b", "v0.1");
    let evidence_ref = EvidenceRef::from_uuid(Uuid::new_v4());

    let payload = ObservationProposal::new("Atlas launches Q3 2026", "fact");

    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        payload,
        vec![evidence_ref],
        0.85,
        SensitivityClass::Useful,
        identity,
    );
    let proposal_id = proposal.id;

    // 1. Submit proposal.
    let mut store = ProposalStore::new();
    let id = store.submit_observation(proposal).unwrap();
    assert_eq!(id, proposal_id);

    // Verify proposal is recorded but not yet canonical.
    let stored = store.get(id).unwrap();
    assert_eq!(stored.state, ProposalState::Proposed);

    // Audit: record submission.
    let mut audit = AuditLog::new();
    log_proposal_submitted(&mut audit, id, agent_id, scope).unwrap();

    // 2. Review proposal (deny-by-default policy → NeedsHumanReview).
    let policy = AutoPromotionPolicy::default();
    let decision = store.review(id, &policy).unwrap();
    assert_eq!(decision, ProposalDecision::NeedsHumanReview);

    let stored = store.get(id).unwrap();
    assert_eq!(stored.state, ProposalState::UnderReview);

    // 3. Admin promotes.
    store.promote(id).unwrap();
    let stored = store.get(id).unwrap();
    assert_eq!(stored.state, ProposalState::Promoted);

    // Audit: record promotion.
    log_proposal_promoted(&mut audit, id, Actor::User(admin_id), scope).unwrap();

    // 4. Produce canonical artifact.
    let artifact = store.promote_to_canonical(id).unwrap();
    assert!(
        matches!(artifact, CanonicalArtifact::Observation(_)),
        "canonical artifact should be an observation"
    );

    // 5. Verify audit log has both entries.
    let q = AuditQuery::new().with_scope(scope);
    let entries: Vec<_> = audit.query(&q).collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].action_type,
        AuditActionType::AgentProposalSubmitted
    );
    assert_eq!(
        entries[1].action_type,
        AuditActionType::AgentProposalPromoted
    );
}

#[test]
fn reject_proposal_is_terminal() {
    let scope = ScopeId::new_v4();
    let identity = AgentIdentity::new(Uuid::new_v4(), "bot", "model-x", "v1");
    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("claim", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.5,
        SensitivityClass::Useful,
        identity,
    );
    let id = proposal.id;

    let mut store = ProposalStore::new();
    store.submit_observation(proposal).unwrap();

    // Reject directly.
    store.reject(id, "duplicate").unwrap();
    let stored = store.get(id).unwrap();
    assert_eq!(stored.state, ProposalState::Rejected);

    // Re-promoting after rejection must fail.
    assert!(store.promote(id).is_err());
}

#[test]
fn duplicate_proposal_is_rejected() {
    let scope = ScopeId::new_v4();
    let identity = AgentIdentity::new(Uuid::new_v4(), "bot", "model-x", "v1");

    let mut proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("claim", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.5,
        SensitivityClass::Useful,
        identity.clone(),
    );
    let fixed_id = Uuid::new_v4();
    proposal.id = fixed_id;

    let mut store = ProposalStore::new();
    store.submit_observation(proposal.clone()).unwrap();

    // Second submission with same id should fail.
    assert!(store.submit_observation(proposal).is_err());
}
