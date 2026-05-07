//! End-to-end Phase 5 export-plane tests.
//!
//! Exercises the full Phase 5 export pipeline:
//!
//! ```text
//! concept_graph.canonical → approval workflow → control registry →
//!     PortableConceptProfile → PolicySimulator → PolicyEngine →
//!     ExportView → audit-log entry
//! ```
//!
//! and the cross-crate integration with `agent_contract` (proposal →
//! canonical → approved → exported) and `audit_service` (every state
//! change produces an audit row).

use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use agent_contract::{
    AgentIdentity, AgentProposal, AutoPromotionPolicy, CanonicalArtifact, ConceptProposal,
    ProposalKind, ProposalStore,
};
use audit_service::{
    log_export, log_export_simulated, log_proposal_promoted, log_proposal_submitted, Actor,
    AuditActionType, AuditLog,
};
use concept_graph::{ConceptGraph, ConceptNode, NodeId, NodeState};
use crypto::{EvidenceRef, ProvenanceAgent, ProvenanceBundle, SynthesisActivity};
use evidence_store::ScopeId;
use export_plane::{
    ApprovalError, ApprovedConcept, ConceptApprovalWorkflow, ConceptExportControl,
    ExportConstraint, ExportControlRegistry, ExportPolicy, ExportRejectionReason, ExportView,
    ExportViewContent, PolicyEngine, PolicySimulator, PortableConceptProfile,
};
use memory_manager::SensitivityClass;

fn provenance_for(concept_id: Uuid) -> ProvenanceBundle {
    ProvenanceBundle::new(
        concept_id,
        SynthesisActivity::new(
            "export-plane-test",
            "test-model@v1",
            "synth.test.v1",
            Uuid::new_v4(),
        ),
        ProvenanceAgent::software("export-plane-test"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
    )
}

/// Build a fresh `ConceptGraph` containing one canonical concept and
/// register it in the export control registry. Returns the concept id
/// alongside the populated graph and registry.
fn graph_with_canonical_concept(
    label: &str,
    definition: &str,
    scope: ScopeId,
) -> (ConceptGraph, ExportControlRegistry, Uuid) {
    let mut graph = ConceptGraph::new();
    let mut node = ConceptNode::new_candidate(label, definition, scope);
    node.state = NodeState::Canonical;
    let id = node.id;
    graph.add_node(node).expect("add canonical node");

    let mut registry = ExportControlRegistry::new();
    registry
        .insert_concept(ConceptExportControl::new(id.0))
        .expect("insert control");

    (graph, registry, id.0)
}

#[test]
fn full_export_pipeline_emits_concepts_only_view_and_audit_trail() {
    // 1. Canonical concept in the graph.
    let scope = ScopeId::new_v4();
    let (graph, registry, concept_id) =
        graph_with_canonical_concept("Atlas", "Q3 launch program", scope);

    // 2. Approve for export.
    let mut workflow = ConceptApprovalWorkflow::new();
    let profile_id = Uuid::new_v4();
    let approved = workflow
        .approve_for_export(concept_id, scope, profile_id, &graph, &registry)
        .expect("approve");
    assert_eq!(approved.concept_id, concept_id);

    // 3. Build a portable concept profile.
    let mut profile = PortableConceptProfile::new(
        "atlas-launch-export",
        "Atlas launch concept profile for downstream tools",
        "downstream-tool",
        scope,
    );
    profile.id = profile_id;
    profile.push_concept(approved);
    profile.push_constraint(ExportConstraint::MaxConcepts(10));

    // 4. Simulate the export under the default (most-restrictive)
    //    policy. The approval workflow attaches its own populated
    //    provenance bundle attesting to the approval — the bundle's
    //    `derivations` list is empty by design (the workflow itself
    //    is the synthesis activity), and the policy engine treats
    //    that as legitimate provenance, so `require_provenance: true`
    //    (the default) is left in place.
    let policy = ExportPolicy::default();
    let mut audit = AuditLog::new();
    let admin = Uuid::new_v4();
    let sim = PolicySimulator::new(&policy, &registry);
    let result = sim.simulate(&profile);
    assert_eq!(result.included_concepts, vec![concept_id]);
    assert!(result.excluded_concepts.is_empty());
    assert!(!result.would_include_evidence);
    log_export_simulated(
        &mut audit,
        profile.id,
        scope,
        Actor::User(admin),
        result.included_concepts.len(),
        result.excluded_concepts.len(),
    )
    .expect("audit simulate");

    // 5. Render the export view via the engine.
    let decision = PolicyEngine::new().evaluate(&policy, &profile.concepts);
    assert_eq!(decision.approved.len(), 1);
    let view = ExportView::new(
        profile.id,
        scope,
        ExportViewContent::ConceptsOnly {
            concepts: decision.approved.clone(),
        },
    );

    // 6. Verify no raw evidence leaked into the rendered view.
    assert!(view.content.evidence_pack().is_none());
    assert!(view.content.summaries().is_empty());
    assert_eq!(view.content.concepts().len(), 1);
    assert!(!decision.allow_raw_evidence);

    // 7. Audit the actual render.
    log_export(
        &mut audit,
        profile.id,
        scope,
        Actor::User(admin),
        &decision
            .approved
            .iter()
            .map(|c| c.concept_id)
            .collect::<Vec<_>>(),
    )
    .expect("audit export");

    // 8. Audit trail has 2 entries: simulate + render.
    let entries = audit.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].action_type, AuditActionType::ExportSimulated);
    assert_eq!(entries[1].action_type, AuditActionType::ExportRendered);
}

#[test]
fn unapproved_concept_cannot_appear_in_export() {
    // Set up a profile that names a concept that has *not* been
    // approved. The control registry has no entry for it, so the
    // simulator must filter it out under deny-by-default.
    let scope = ScopeId::new_v4();
    let mut profile = PortableConceptProfile::new("p", "d", "tool", scope);
    let unapproved_concept_id = Uuid::new_v4();
    profile.push_concept(ApprovedConcept::new(
        unapproved_concept_id,
        "Phantom",
        "Not actually approved",
        scope,
        provenance_for(unapproved_concept_id),
        SensitivityClass::Useful,
    ));
    let policy = ExportPolicy::default();
    let registry = ExportControlRegistry::new();
    let result = PolicySimulator::new(&policy, &registry).simulate(&profile);
    assert!(result.included_concepts.is_empty());
    assert_eq!(result.excluded_concepts.len(), 1);
    assert!(result.excluded_concepts[0]
        .reason
        .contains("deny-by-default"));
}

#[test]
fn critical_sensitivity_concept_is_blocked_by_default_policy() {
    // The default policy's ceiling is `Useful`, so a `Critical`
    // approved concept must be filtered out by the engine.
    let scope = ScopeId::new_v4();
    let concept_id = Uuid::new_v4();
    let approved = ApprovedConcept::new(
        concept_id,
        "Crown Jewels",
        "Most-sensitive concept",
        scope,
        provenance_for(concept_id),
        SensitivityClass::Critical,
    );
    let mut profile = PortableConceptProfile::new("p", "d", "tool", scope);
    profile.push_concept(approved);

    let mut registry = ExportControlRegistry::new();
    registry
        .insert_concept(ConceptExportControl::new(concept_id))
        .expect("insert");

    let result = PolicySimulator::new(&ExportPolicy::default(), &registry).simulate(&profile);
    assert!(result.included_concepts.is_empty());
    assert_eq!(result.excluded_concepts.len(), 1);
    assert!(result.excluded_concepts[0]
        .reason
        .contains("exceeds policy ceiling"));
}

#[test]
fn raw_evidence_blocked_when_critical_concept_is_approved() {
    // Even with `allow_raw_evidence = true` and a permissive policy
    // ceiling, the engine *must* clear `allow_raw_evidence` if any
    // approved concept is `Critical`.
    let scope = ScopeId::new_v4();
    let concept_id = Uuid::new_v4();
    let approved = ApprovedConcept::new(
        concept_id,
        "Crown Jewels",
        "Most-sensitive concept",
        scope,
        provenance_for(concept_id),
        SensitivityClass::Critical,
    );

    let mut policy = ExportPolicy::permissive(SensitivityClass::Critical);
    policy.allow_raw_evidence = true;

    let decision = PolicyEngine::new().evaluate(&policy, &[approved]);
    assert_eq!(decision.approved.len(), 1);
    assert!(!decision.allow_raw_evidence);
    assert!(decision.warnings.iter().any(|w| w.contains("raw evidence")));
}

#[test]
fn concept_in_candidate_state_cannot_be_approved() {
    let scope = ScopeId::new_v4();
    let mut graph = ConceptGraph::new();
    let mut node = ConceptNode::new_candidate("label", "definition", scope);
    node.state = NodeState::Candidate;
    let id = node.id;
    graph.add_node(node).expect("add");
    let registry = {
        let mut r = ExportControlRegistry::new();
        r.insert_concept(ConceptExportControl::new(id.0))
            .expect("insert");
        r
    };
    let mut wf = ConceptApprovalWorkflow::new();
    let err = wf
        .approve_for_export(id.0, scope, Uuid::new_v4(), &graph, &registry)
        .expect_err("not canonical");
    assert_eq!(err, ApprovalError::NotCanonical(id.0));
}

#[test]
fn revocation_removes_concept_from_approved_set() {
    let scope = ScopeId::new_v4();
    let (graph, registry, concept_id) = graph_with_canonical_concept("Atlas", "definition", scope);
    let mut wf = ConceptApprovalWorkflow::new();
    let approved = wf
        .approve_for_export(concept_id, scope, Uuid::new_v4(), &graph, &registry)
        .expect("approve");
    assert_eq!(wf.list_approved(scope).len(), 1);
    wf.revoke_approval(approved.concept_id).expect("revoke");
    assert!(wf.list_approved(scope).is_empty());
}

#[test]
fn time_window_filters_old_concept_in_engine_pass() {
    let scope = ScopeId::new_v4();
    let concept_id = Uuid::new_v4();
    let mut approved = ApprovedConcept::new(
        concept_id,
        "Atlas",
        "definition",
        scope,
        provenance_for(concept_id),
        SensitivityClass::Useful,
    );
    approved.approved_at = Utc::now() - chrono::Duration::seconds(7200);

    let policy = ExportPolicy {
        time_window: Some(Duration::from_secs(3600)),
        ..ExportPolicy::default()
    };

    let decision = PolicyEngine::new().evaluate(&policy, &[approved]);
    assert!(decision.approved.is_empty());
    assert_eq!(decision.rejected.len(), 1);
    assert!(matches!(
        decision.rejected[0].reason,
        ExportRejectionReason::OutsideTimeWindow
    ));
}

#[test]
fn cross_crate_proposal_to_export_pipeline() {
    // Wires `agent_contract` + `concept_graph` + `export_plane` +
    // `audit_service` into one full pipeline:
    //
    //   1. Agent submits a ConceptProposal.
    //   2. ProposalStore promotes it (auto-promotion policy matches).
    //   3. The promoted CanonicalArtifact::Concept is hand-promoted
    //      into the concept_graph as Canonical (the substrate's job;
    //      we simulate it here).
    //   4. The approval workflow approves the canonical concept for
    //      export.
    //   5. The simulator + engine + view round-trip the concept into
    //      a ConceptsOnly export view.
    //   6. The audit log contains submit, promote, simulate, render
    //      events in that order.

    let scope = ScopeId::new_v4();
    let mut audit = AuditLog::new();

    // 1. Agent identity + concept proposal.
    let agent_id = Uuid::new_v4();
    let identity = AgentIdentity::new(agent_id, "ada", "bonsai-1.7b", "v1");
    let mut store = ProposalStore::new();
    let proposal = AgentProposal::new(
        ProposalKind::Concept,
        scope,
        ConceptProposal::new("Atlas", "Q3 launch program"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.95,
        SensitivityClass::Useful,
        identity,
    );
    let proposal_id = store.submit_concept(proposal).expect("submit");
    log_proposal_submitted(&mut audit, proposal_id, agent_id, scope).expect("audit submit");

    // 2. Auto-promote.
    let policy = AutoPromotionPolicy::new(0.5, 0, SensitivityClass::Important, false);
    store.review(proposal_id, &policy).expect("review");
    log_proposal_promoted(&mut audit, proposal_id, agent_id, scope).expect("audit promote");
    let canonical = store.promote_to_canonical(proposal_id).expect("canonical");

    // 3. Render canonical -> ConceptGraph (concept_id matches the
    //    canonical artifact's id).
    let CanonicalArtifact::Concept(canonical) = canonical else {
        panic!("expected concept");
    };
    let mut graph = ConceptGraph::new();
    let mut node = ConceptNode::new_candidate(&canonical.label, &canonical.definition, scope);
    node.id = NodeId::from_uuid(canonical.proposal_id);
    node.state = NodeState::Canonical;
    graph.add_node(node).expect("add canonical node");
    let canonical_concept_id = canonical.proposal_id;

    // 4. Approve for export.
    let mut registry = ExportControlRegistry::new();
    registry
        .insert_concept(ConceptExportControl::new(canonical_concept_id))
        .expect("insert");
    let mut wf = ConceptApprovalWorkflow::new();
    let profile_id = Uuid::new_v4();
    let approved = wf
        .approve_for_export(canonical_concept_id, scope, profile_id, &graph, &registry)
        .expect("approve");

    // 5. Build profile + simulate + render.
    let mut profile =
        PortableConceptProfile::new("atlas-export", "Atlas concept profile", "downstream", scope);
    profile.id = profile_id;
    profile.push_concept(approved);

    // The approval workflow attaches a populated provenance bundle
    // whose `derivations` list is empty (the workflow itself is the
    // synthesis activity). The policy engine treats that as
    // legitimate provenance, so the cross-crate pipeline runs under
    // the default `require_provenance: true` policy.
    let export_policy = ExportPolicy::default();
    let sim = PolicySimulator::new(&export_policy, &registry);
    let result = sim.simulate(&profile);
    assert_eq!(result.included_concepts, vec![canonical_concept_id]);
    log_export_simulated(
        &mut audit,
        profile.id,
        scope,
        Actor::User(agent_id),
        result.included_concepts.len(),
        result.excluded_concepts.len(),
    )
    .expect("audit simulate");

    let decision = PolicyEngine::new().evaluate(&export_policy, &profile.concepts);
    let view = ExportView::new(
        profile.id,
        scope,
        ExportViewContent::ConceptsOnly {
            concepts: decision.approved.clone(),
        },
    );
    assert!(view.content.evidence_pack().is_none());
    log_export(
        &mut audit,
        profile.id,
        scope,
        Actor::User(agent_id),
        &decision
            .approved
            .iter()
            .map(|c| c.concept_id)
            .collect::<Vec<_>>(),
    )
    .expect("audit export");

    // 6. Audit log has all 4 events.
    let entries = audit.entries();
    assert_eq!(entries.len(), 4);
    assert_eq!(
        entries[0].action_type,
        AuditActionType::AgentProposalSubmitted
    );
    assert_eq!(
        entries[1].action_type,
        AuditActionType::AgentProposalPromoted
    );
    assert_eq!(entries[2].action_type, AuditActionType::ExportSimulated);
    assert_eq!(entries[3].action_type, AuditActionType::ExportRendered);
}
