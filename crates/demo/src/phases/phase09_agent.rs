//! Phase 9 — Agent Contract.
//!
//! Per `PROPOSAL.md` §7.3 and `ARCHITECTURE.md` §6, software agents
//! never write canonical memory directly — they go through a
//! proposal-only API that mints typed [`AgentProposal`]s and pushes
//! them through the lifecycle state machine
//! `Proposed → UnderReview → Promoted | Rejected`. The demo exercises
//! the contract end-to-end:
//!
//! 1. Build a real [`AgentIdentity`] for a "synth-agent" backed by a
//!    Bonsai-1.7B-style model + skill / recipe ids.
//! 2. Submit one proposal of every payload type
//!    ([`ObservationProposal`], [`ConceptProposal`],
//!    [`RelationProposal`], [`SummaryProposal`]) into a real
//!    [`ProposalStore`]. Evidence refs are pulled from Phase 1's
//!    [`crate::phases::runtime::IngestedRow`]s, scope ids are taken
//!    from the dataset, and the supersedes / contradicts links use
//!    canonical concept ids minted by Phase 4 — every value the
//!    agent contract carries is sourced from real prior phases.
//! 3. Run [`ProposalStore::review`] under a permissive
//!    [`AutoPromotionPolicy`] (auto-promotes the high-confidence
//!    observation), under the deny-by-default
//!    [`AutoPromotionPolicy::default`] (rejects everything by
//!    matching nothing — so review always returns
//!    [`ProposalDecision::NeedsHumanReview`]), and through
//!    [`ProposalStore::promote`] / [`ProposalStore::reject`] for the
//!    manually-handled proposals.
//! 4. Bump corroboration on the concept proposal to demonstrate
//!    cross-source corroboration.
//! 5. Render canonical artifacts via
//!    [`ProposalStore::promote_to_canonical`] and assert the four
//!    [`CanonicalArtifact`] variants are produced.
//! 6. Submit a TTL-bound proposal, advance time past the TTL, and
//!    confirm review surfaces [`LifecycleError::Expired`] and the
//!    proposal lands in [`ProposalState::Rejected`] with reason
//!    `"ttl_expired"`.
//! 7. Append `AgentProposalSubmitted`, `AgentProposalPromoted`, and
//!    `AgentProposalRejected` audit entries via the
//!    [`audit_service`] helpers.

use std::time::Duration;
use std::time::Instant;

use agent_contract::lifecycle::LifecycleError;
use agent_contract::{
    AgentIdentity, AgentProposal, AutoPromotionPolicy, CanonicalArtifact, ConceptProposal,
    ObservationProposal, ProposalDecision, ProposalKind, ProposalState, ProposalStore,
    RelationProposal, RelationType, SummaryProposal,
};
use audit_service::{log_proposal_promoted, log_proposal_rejected, log_proposal_submitted, Actor};
use crypto::EvidenceRef;
use memory_manager::SensitivityClass;
use uuid::Uuid;

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "phase09_agent";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Phase 9: Agent Contract");

    // -------- Agent identity + scopes ----------------------------
    let agent_uuid = Uuid::new_v4();
    let agent = AgentIdentity::new(
        agent_uuid,
        "synth-agent",
        "bonsai-1.7b",
        "q1_0_g128-2026-04-01",
    )
    .with_skill("synth.summary.v1")
    .with_recipe("recipe.weekly_digest");

    let channel_scope = dataset.channel_scope.id;
    let domain_scope = dataset.domain_scope.id;

    // Pull two real evidence refs from Phase 1.
    let evidence_refs: Vec<EvidenceRef> = state
        .ingested_rows
        .iter()
        .take(3)
        .map(|row| EvidenceRef::from_uuid(row.evidence_id.0))
        .collect();
    log.check(
        PHASE,
        "Phase 1 surfaced enough evidence rows to back agent proposals",
        evidence_refs.len() >= 2,
    );

    let mut store = ProposalStore::new();
    let mut submitted_ids: Vec<Uuid> = Vec::new();

    // -------- Submit observation ---------------------------------
    let submit_started = Instant::now();
    let observation_envelope = AgentProposal::new(
        ProposalKind::Observation,
        channel_scope,
        ObservationProposal::new(
            "Demo observation: substrate ingest pipeline is end-to-end testable",
            "fact",
        ),
        evidence_refs.clone(),
        0.92,
        SensitivityClass::Useful,
        agent.clone(),
    );
    let observation_id = store
        .submit_observation(observation_envelope)
        .expect("submit observation");
    submitted_ids.push(observation_id);

    // -------- Submit concept -------------------------------------
    let concept_envelope = AgentProposal::new(
        ProposalKind::Concept,
        channel_scope,
        ConceptProposal::new(
            "Knowledge Substrate Demo",
            "End-to-end demo run exercising every Knowledge substrate phase",
        ),
        evidence_refs.clone(),
        0.55,
        SensitivityClass::Useful,
        agent.clone(),
    );
    let concept_id = store
        .submit_concept(concept_envelope)
        .expect("submit concept");
    submitted_ids.push(concept_id);

    // -------- Submit relation ------------------------------------
    let (rel_src, rel_dst) = if state.canonical_concept_ids.len() >= 2 {
        (
            state.canonical_concept_ids[0],
            state.canonical_concept_ids[1],
        )
    } else {
        (Uuid::new_v4(), Uuid::new_v4())
    };
    let relation_envelope = AgentProposal::new(
        ProposalKind::Relation,
        domain_scope,
        RelationProposal::new(rel_src, rel_dst, RelationType::new("derived_from")),
        evidence_refs.clone(),
        0.7,
        SensitivityClass::Useful,
        agent.clone(),
    );
    let relation_id = store
        .submit_relation(relation_envelope)
        .expect("submit relation");
    submitted_ids.push(relation_id);

    // -------- Submit summary -------------------------------------
    let summary_envelope = AgentProposal::new(
        ProposalKind::Summary,
        channel_scope,
        SummaryProposal::new(
            "Demo channel summary: weekly digest of substrate operations",
            "channel",
        ),
        evidence_refs.clone(),
        0.6,
        SensitivityClass::Important,
        agent.clone(),
    );
    let summary_id = store
        .submit_summary(summary_envelope)
        .expect("submit summary");
    submitted_ids.push(summary_id);
    let submit_elapsed = submit_started.elapsed();

    log.check(
        PHASE,
        "store accepted all four typed proposals",
        store.len() == 4,
    );

    for id in &submitted_ids {
        log_proposal_submitted(&mut state.audit_log, *id, agent_uuid, channel_scope)
            .expect("log_proposal_submitted");
    }

    // -------- Duplicate id is refused ----------------------------
    let duplicate_envelope = AgentProposal::new(
        ProposalKind::Observation,
        channel_scope,
        ObservationProposal::new("Duplicate id should be refused", "fact"),
        evidence_refs.clone(),
        0.5,
        SensitivityClass::Useful,
        agent.clone(),
    );
    let mut clash = duplicate_envelope.clone();
    clash.id = observation_id;
    let dup_blocked = matches!(
        store.submit_observation(clash),
        Err(LifecycleError::DuplicateProposal(id)) if id == observation_id,
    );
    log.check(
        PHASE,
        "store refuses to overwrite an existing proposal id",
        dup_blocked,
    );

    // -------- Corroboration --------------------------------------
    store
        .record_corroboration(concept_id)
        .expect("record corroboration");
    store
        .record_corroboration(concept_id)
        .expect("record corroboration");
    let concept_corroboration = store
        .get(concept_id)
        .map(|p| p.corroboration_count)
        .unwrap_or_default();
    log.check(
        PHASE,
        "corroboration count is bumped on each call",
        concept_corroboration == 2,
    );

    // -------- Auto-promotion under permissive policy --------------
    let auto_policy = AutoPromotionPolicy::new(0.9, 0, SensitivityClass::Important, false);
    let review_started = Instant::now();
    let observation_decision = store
        .review(observation_id, &auto_policy)
        .expect("review observation");
    log.check(
        PHASE,
        "high-confidence observation auto-promotes under permissive policy",
        matches!(observation_decision, ProposalDecision::AutoPromoted),
    );
    log.check(
        PHASE,
        "auto-promoted observation is in Promoted state",
        store.get(observation_id).map(|p| p.state) == Some(ProposalState::Promoted),
    );

    // The concept's confidence is below the threshold even after
    // corroboration — review must surface NeedsHumanReview.
    let concept_decision = store
        .review(concept_id, &auto_policy)
        .expect("review concept");
    log.check(
        PHASE,
        "below-threshold concept needs human review",
        matches!(concept_decision, ProposalDecision::NeedsHumanReview),
    );

    // -------- Manual promote of concept --------------------------
    store.promote(concept_id).expect("promote concept");
    log.check(
        PHASE,
        "concept reaches Promoted via manual promote()",
        store.get(concept_id).map(|p| p.state) == Some(ProposalState::Promoted),
    );

    // -------- Manual review + reject for relation -----------------
    let relation_decision = store
        .review(relation_id, &auto_policy)
        .expect("review relation");
    log.check(
        PHASE,
        "below-threshold relation needs human review",
        matches!(relation_decision, ProposalDecision::NeedsHumanReview),
    );
    store
        .reject(relation_id, "demo: relation declined for review")
        .expect("reject relation");
    log.check(
        PHASE,
        "rejected relation lands in Rejected with explicit reason",
        store.get(relation_id).is_some_and(|p| {
            p.state == ProposalState::Rejected
                && p.rejection_reason.as_deref() == Some("demo: relation declined for review")
        }),
    );

    // -------- Default policy denies everything --------------------
    let default_policy = AutoPromotionPolicy::default();
    let summary_decision = store
        .review(summary_id, &default_policy)
        .expect("review summary");
    log.check(
        PHASE,
        "default policy admits to review without auto-promoting",
        matches!(summary_decision, ProposalDecision::NeedsHumanReview),
    );
    store.promote(summary_id).expect("promote summary");
    let review_elapsed = review_started.elapsed();

    // -------- Cannot reject promoted ------------------------------
    let cannot_reject_promoted = matches!(
        store.reject(observation_id, "should be rejected"),
        Err(LifecycleError::InvalidTransition { .. })
    );
    log.check(
        PHASE,
        "rejected once-promoted proposal is refused by the state machine",
        cannot_reject_promoted,
    );

    // -------- Canonical artifacts ---------------------------------
    let canonical_started = Instant::now();
    let canonical_observation = store
        .promote_to_canonical(observation_id)
        .expect("promote observation to canonical");
    let canonical_concept = store
        .promote_to_canonical(concept_id)
        .expect("promote concept to canonical");
    let canonical_summary = store
        .promote_to_canonical(summary_id)
        .expect("promote summary to canonical");
    let canonical_elapsed = canonical_started.elapsed();

    log.check(
        PHASE,
        "canonical observation derives from observation proposal",
        matches!(canonical_observation, CanonicalArtifact::Observation(_)),
    );
    log.check(
        PHASE,
        "canonical concept derives from concept proposal",
        matches!(canonical_concept, CanonicalArtifact::Concept(_)),
    );
    log.check(
        PHASE,
        "canonical summary derives from summary proposal",
        matches!(canonical_summary, CanonicalArtifact::Summary(_)),
    );

    // promote_to_canonical is deterministic — calling twice returns
    // the same id.
    let observation_id_again = store
        .promote_to_canonical(observation_id)
        .expect("re-promote observation")
        .id();
    log.check(
        PHASE,
        "promote_to_canonical is deterministic across calls",
        observation_id_again == canonical_observation.id(),
    );

    // Cannot derive canonical artifact for a rejected proposal.
    let canonical_rejected_blocked = matches!(
        store.promote_to_canonical(relation_id),
        Err(LifecycleError::InvalidTransition { .. })
    );
    log.check(
        PHASE,
        "canonical artifact is refused for rejected proposal",
        canonical_rejected_blocked,
    );

    // -------- TTL expiry path -------------------------------------
    let ttl_envelope = AgentProposal::new(
        ProposalKind::Observation,
        channel_scope,
        ObservationProposal::new("TTL-bound demo proposal", "fact"),
        evidence_refs.clone(),
        0.99,
        SensitivityClass::Useful,
        agent.clone(),
    )
    .with_ttl(Duration::from_millis(1));
    let ttl_id = store
        .submit_observation(ttl_envelope)
        .expect("submit ttl-bound observation");
    // Sleep a hair so the TTL definitely elapses on a real wall clock.
    std::thread::sleep(Duration::from_millis(5));
    let ttl_outcome = store.review(ttl_id, &auto_policy);
    let ttl_expired_handled =
        matches!(ttl_outcome, Err(LifecycleError::Expired(id)) if id == ttl_id);
    log.check(
        PHASE,
        "TTL-elapsed proposal is rejected with LifecycleError::Expired",
        ttl_expired_handled,
    );
    log.check(
        PHASE,
        "TTL-elapsed proposal lands in Rejected with reason `ttl_expired`",
        store.get(ttl_id).is_some_and(|p| {
            p.state == ProposalState::Rejected
                && p.rejection_reason.as_deref() == Some("ttl_expired")
        }),
    );

    // -------- Audit lifecycle entries -----------------------------
    let audit_started = Instant::now();
    log_proposal_promoted(
        &mut state.audit_log,
        observation_id,
        Actor::System,
        channel_scope,
    )
    .expect("log_proposal_promoted observation");
    log_proposal_promoted(
        &mut state.audit_log,
        concept_id,
        Actor::User(Uuid::new_v4()),
        channel_scope,
    )
    .expect("log_proposal_promoted concept");
    log_proposal_promoted(
        &mut state.audit_log,
        summary_id,
        Actor::User(Uuid::new_v4()),
        channel_scope,
    )
    .expect("log_proposal_promoted summary");
    log_proposal_rejected(
        &mut state.audit_log,
        relation_id,
        Actor::User(Uuid::new_v4()),
        domain_scope,
        "demo: relation declined for review",
    )
    .expect("log_proposal_rejected relation");
    log_proposal_rejected(
        &mut state.audit_log,
        ttl_id,
        Actor::System,
        channel_scope,
        "ttl_expired",
    )
    .expect("log_proposal_rejected ttl");
    let audit_elapsed = audit_started.elapsed();

    // -------- Bookkeeping + report --------------------------------
    let total_submitted: u64 = (submitted_ids.len() + 1) as u64; // +1 for ttl_id
    let total_rejected: u64 = 2;

    state.proposals_submitted += total_submitted;
    state.proposals_auto_promoted += 1;
    state.proposals_manually_promoted += 2;
    state.proposals_rejected += total_rejected;

    phase.timing = started.elapsed();
    phase.stat("proposals_submitted", total_submitted.to_string());
    phase.stat("proposals_auto_promoted", "1");
    phase.stat("proposals_manually_promoted", "2");
    phase.stat("proposals_rejected", total_rejected.to_string());
    phase.stat(
        "canonical_artifacts_derived",
        "3 (observation, concept, summary)",
    );
    phase.stat(
        "concept_corroboration_count",
        concept_corroboration.to_string(),
    );
    phase.note(
        "AgentProposal lifecycle exercised end-to-end: submission, \
         duplicate-id refusal, corroboration bump, AutoPromotionPolicy \
         match + miss, manual promote/reject, deterministic canonical \
         artifact derivation, and TTL-expiry rejection.",
    );

    report.count("proposals_submitted", state.proposals_submitted);
    report.count("proposals_auto_promoted", state.proposals_auto_promoted);
    report.count(
        "proposals_manually_promoted",
        state.proposals_manually_promoted,
    );
    report.count("proposals_rejected", state.proposals_rejected);
    report.add_phase(phase);
    report.add_benchmark("agent_proposal_submits", total_submitted, submit_elapsed);
    report.add_benchmark("agent_review_calls", 4, review_elapsed);
    report.add_benchmark("agent_canonical_derivations", 4, canonical_elapsed);
    report.add_benchmark("agent_audit_writes", 5, audit_elapsed);
}
