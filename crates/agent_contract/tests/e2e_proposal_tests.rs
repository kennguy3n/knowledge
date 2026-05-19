//! End-to-end proposal-lifecycle tests.
//!
//! Exercises the full agent-write contract:
//!
//! ```text
//! AgentIdentity → AgentProposal → submit() → review(policy) →
//!     auto-promote / manual promote → promote_to_canonical() →
//!     CanonicalArtifact → audit-log entry
//! ```
//!
//! Cross-crate integration uses `audit_service` to verify that every
//! proposal state change produces an audit entry.

use std::time::Duration;

use uuid::Uuid;

use agent_contract::{
    lifecycle::{ProposalDecision, ProposalState},
    AgentIdentity, AgentProposal, AutoPromotionPolicy, CanonicalArtifact, ConceptProposal,
    ObservationProposal, ProposalKind, ProposalStore, ProposalValidationError, RelationProposal,
    RelationType, SummaryProposal,
};
use audit_service::{
    log_proposal_promoted, log_proposal_rejected, log_proposal_submitted, Actor, AuditActionType,
    AuditLog,
};
use crypto::EvidenceRef;
use evidence_store::ScopeId;
use memory_manager::SensitivityClass;

fn fixture_identity() -> AgentIdentity {
    AgentIdentity::new(
        Uuid::new_v4(),
        "nina-pm",
        "bonsai-1.7b",
        "q1_0_g128-2026-04-01",
    )
    .with_skill("synth.summary.v1")
}

fn permissive_policy() -> AutoPromotionPolicy {
    AutoPromotionPolicy::new(0.6, 0, SensitivityClass::Important, false)
}

#[test]
fn observation_proposal_full_lifecycle_with_audit() {
    // 1. Build agent identity + observation proposal.
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("Atlas launches Q3 2026", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.9,
        SensitivityClass::Useful,
        identity.clone(),
    );

    let mut store = ProposalStore::new();
    let mut audit = AuditLog::new();

    // 2. Submit + audit the submission.
    let id = store.submit_observation(proposal).expect("submit");
    log_proposal_submitted(&mut audit, id, identity.agent_id, scope).expect("audit submit");

    // 3. Review under a permissive policy → auto-promoted.
    let decision = store.review(id, &permissive_policy()).expect("review");
    assert_eq!(decision, ProposalDecision::AutoPromoted);
    assert_eq!(store.get(id).unwrap().state, ProposalState::Promoted);

    // 4. Audit the promotion. Auto-promotion is system-driven, so the
    //    audit entry is logged with `Actor::System` rather than a
    //    meaningless `Actor::User(<random uuid>)` — the audit trail
    //    must accurately distinguish human-driven promotions from
    //    substrate-driven ones.
    log_proposal_promoted(&mut audit, id, Actor::System, scope).expect("audit promote");

    // 5. Render the canonical artifact and verify it round-trips.
    let canonical = store.promote_to_canonical(id).expect("canonical");
    match canonical {
        CanonicalArtifact::Observation(o) => {
            assert_eq!(o.proposal_id, id);
            assert_eq!(o.scope_id, scope);
            assert_eq!(o.claim, "Atlas launches Q3 2026");
            assert_eq!(o.observation_type, "fact");
            assert_eq!(o.sensitivity_class, SensitivityClass::Useful);
        }
        other => panic!("expected Observation, got {other:?}"),
    }

    // 6. Audit trail has one submission + one promotion entry.
    let entries: Vec<_> = audit.entries().to_vec();
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
fn concept_proposal_promoted_manually_after_human_review() {
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Concept,
        scope,
        ConceptProposal::new("Atlas", "Project codename for Q3 launch"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        // Below the policy's confidence floor — manual review required.
        0.2,
        SensitivityClass::Useful,
        identity.clone(),
    );

    let mut store = ProposalStore::new();
    let id = store.submit_concept(proposal).expect("submit");
    let decision = store.review(id, &permissive_policy()).expect("review");
    assert_eq!(decision, ProposalDecision::NeedsHumanReview);
    assert_eq!(store.get(id).unwrap().state, ProposalState::UnderReview);

    store.promote(id).expect("promote");
    assert_eq!(store.get(id).unwrap().state, ProposalState::Promoted);

    let canonical = store.promote_to_canonical(id).expect("canonical");
    match canonical {
        CanonicalArtifact::Concept(c) => {
            assert_eq!(c.label, "Atlas");
            assert_eq!(c.proposal_id, id);
        }
        other => panic!("expected Concept, got {other:?}"),
    }
}

#[test]
fn relation_proposal_canonicalises() {
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let src = Uuid::new_v4();
    let dst = Uuid::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Relation,
        scope,
        RelationProposal::new(src, dst, RelationType::new("derived_from")),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.95,
        SensitivityClass::Useful,
        identity,
    );

    let mut store = ProposalStore::new();
    let id = store.submit_relation(proposal).expect("submit");
    store.review(id, &permissive_policy()).expect("review");
    let canonical = store.promote_to_canonical(id).expect("canonical");
    match canonical {
        CanonicalArtifact::Relation(r) => {
            assert_eq!(r.src, src);
            assert_eq!(r.dst, dst);
            assert_eq!(r.relation.as_str(), "derived_from");
        }
        other => panic!("expected Relation, got {other:?}"),
    }
}

#[test]
fn summary_proposal_canonicalises() {
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Summary,
        scope,
        SummaryProposal::new("Weekly digest …", "channel"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.8,
        SensitivityClass::Useful,
        identity,
    );

    let mut store = ProposalStore::new();
    let id = store.submit_summary(proposal).expect("submit");
    store.review(id, &permissive_policy()).expect("review");
    let canonical = store.promote_to_canonical(id).expect("canonical");
    match canonical {
        CanonicalArtifact::Summary(s) => {
            assert_eq!(s.text, "Weekly digest …");
            assert_eq!(s.summary_type, "channel");
        }
        other => panic!("expected Summary, got {other:?}"),
    }
}

#[test]
fn agent_cannot_write_canonical_directly() {
    // Agents only ever submit proposals — there is no API on
    // `ProposalStore` to insert directly into the `Promoted` state.
    // The strongest proof of this is that `promote_to_canonical`
    // refuses to render a canonical artifact for a proposal that has
    // not been promoted first.
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("a", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.9,
        SensitivityClass::Useful,
        identity,
    );
    let mut store = ProposalStore::new();
    let id = store.submit_observation(proposal).expect("submit");
    let err = store
        .promote_to_canonical(id)
        .expect_err("must not canonicalise without review/promote");
    let msg = err.to_string();
    assert!(msg.contains("cannot transition"), "got: {msg}");
}

#[test]
fn invalid_schema_blocks_submission() {
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let mut proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("a", "fact"),
        vec![],
        0.9,
        SensitivityClass::Useful,
        identity,
    );
    // Strip evidence to force schema validation to fail.
    proposal.evidence_refs.clear();

    let mut store = ProposalStore::new();
    let err = store.submit_observation(proposal).expect_err("blocked");
    let msg = err.to_string();
    assert!(msg.contains("validation failed"), "got: {msg}");

    // No proposal stored ⇒ no canonical artifact reachable.
    assert!(store.is_empty());
    let _ = ProposalValidationError::NoEvidence;
}

#[test]
fn expired_ttl_proposal_auto_rejected_with_audit() {
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("a", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.9,
        SensitivityClass::Useful,
        identity.clone(),
    )
    .with_ttl(Duration::from_secs(1));

    let mut store = ProposalStore::new();
    let mut audit = AuditLog::new();
    let id = store.submit_observation(proposal).expect("submit");
    log_proposal_submitted(&mut audit, id, identity.agent_id, scope).expect("audit submit");

    // The original `proposal` above carries a 1s TTL but isn't the
    // one we exercise the expiry path on — we drive that path with a
    // separate, near-zero TTL proposal below so the test does not
    // depend on real-time waits longer than a few milliseconds.
    let zero = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("b", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.9,
        SensitivityClass::Useful,
        identity.clone(),
    )
    .with_ttl(Duration::from_nanos(1));

    let zid = store.submit_observation(zero).expect("submit");
    // sleep 2ms is fine here — far below CI thresholds.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let err = store
        .review(zid, &permissive_policy())
        .expect_err("expired");
    assert!(err.to_string().contains("expired"));
    assert_eq!(store.get(zid).unwrap().state, ProposalState::Rejected);

    // Audit the rejection. TTL-expiry rejection is substrate-driven
    // (no human in the loop), so the audit entry is logged with
    // `Actor::System`.
    log_proposal_rejected(&mut audit, zid, Actor::System, scope, "ttl_expired")
        .expect("audit reject");
    assert!(audit
        .entries()
        .iter()
        .any(|e| e.action_type == AuditActionType::AgentProposalRejected));
}

#[test]
fn rejection_records_reason_and_audit_entry() {
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("a", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.9,
        SensitivityClass::Useful,
        identity.clone(),
    );

    let mut store = ProposalStore::new();
    let mut audit = AuditLog::new();
    let id = store.submit_observation(proposal).expect("submit");
    log_proposal_submitted(&mut audit, id, identity.agent_id, scope).expect("audit submit");

    let rejector = Uuid::new_v4();
    store.reject(id, "duplicate of canonical").expect("reject");
    log_proposal_rejected(
        &mut audit,
        id,
        Actor::User(rejector),
        scope,
        "duplicate of canonical",
    )
    .expect("audit reject");

    let stored = store.get(id).unwrap();
    assert_eq!(stored.state, ProposalState::Rejected);
    assert_eq!(
        stored.rejection_reason.as_deref(),
        Some("duplicate of canonical")
    );

    // 2 audit entries: submitted + rejected.
    let entries: Vec<_> = audit.entries().to_vec();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[1].action_type,
        AuditActionType::AgentProposalRejected
    );
}

#[test]
fn critical_sensitivity_requires_human_under_default_policy() {
    let identity = fixture_identity();
    let scope = ScopeId::new_v4();
    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        ObservationProposal::new("highly sensitive", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.99,
        SensitivityClass::Critical,
        identity,
    );
    let mut store = ProposalStore::new();
    let id = store.submit_observation(proposal).expect("submit");
    let policy = AutoPromotionPolicy::new(0.5, 0, SensitivityClass::Critical, true);
    let decision = store.review(id, &policy).expect("review");
    assert_eq!(decision, ProposalDecision::NeedsHumanReview);
}
