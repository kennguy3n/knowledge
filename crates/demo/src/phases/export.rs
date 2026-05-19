//! Stage 8 — Export Plane.
//!
//! Per `docs/DESIGN.md` §3.5 the substrate exposes a *narrow*,
//! policy-gated interface for moving curated knowledge out of the
//! substrate into external surfaces. The demo wires up the full
//! export-plane pipeline against the canonical concepts produced by
//! the concept-graph stage:
//!
//! 1. Re-open the SQLCipher concept graph from the concept-graph stage so we operate
//!    on real canonical nodes (no fixtures, no fakes).
//! 2. Build a deny-by-default [`ExportControlRegistry`]. A curated
//!    majority of canonical concepts get [`ConceptExportControl`]
//!    rows; at least one canonical concept is intentionally omitted
//!    so the deny-by-default path is exercised on real data. A
//!    [`SummaryExportControl`] is also registered so the simulator
//!    surfaces summary handling.
//! 3. Run the [`ConceptApprovalWorkflow`] against the registered
//!    set, which mints `ApprovedConcept`s with real
//!    [`crypto::ProvenanceBundle`] envelopes.
//! 4. Build a [`PortableConceptProfile`] with a non-trivial set of
//!    [`ExportConstraint`]s and run [`PolicyEngine::evaluate`] on
//!    its concept list.
//! 5. Render two real [`ExportView`]s via
//!    [`ExportView::from_decision`]: `ConceptsOnly` and
//!    `WithSummaries`. A third call with
//!    [`ExportViewRequest::WithEvidencePack`] is issued to confirm
//!    the negative path returns [`ExportViewError::RawEvidenceNotAuthorised`].
//! 6. Run a [`PolicySimulator`] preview and assert the simulated
//!    inclusion set matches the engine's approved set.
//! 7. Append `AuditActionType::ExportRendered` and
//!    `AuditActionType::ExportSimulated` entries via
//!    [`audit_service::log_export`] / [`audit_service::log_export_simulated`].

use std::time::Instant;

use audit_service::{log_export, log_export_simulated, Actor};
use concept_graph::PersistentConceptGraph;
use export_plane::{
    ApprovedSummary, ConceptApprovalWorkflow, ConceptExportControl, EvidencePack, ExportConstraint,
    ExportControlRegistry, ExportPolicy, ExportView, ExportViewError, ExportViewRequest,
    PolicyEngine, PolicySimulator, PortableConceptProfile, RedactionLevel, SummaryExportControl,
};
use memory_manager::SensitivityClass;
use uuid::Uuid;

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "export";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Stage 8: Export Plane");

    // -------- Re-open the persistent concept graph -----------------
    //
    // The concept-graph stage persisted every substrate-level canonical
    // concept under the tenant scope (see the "Scope-cohesion contract"
    // docs in `concept_graph.rs`); reopening the SQLCipher database
    // does not rehydrate the in-memory graph automatically, so we
    // explicitly drive [`PersistentConceptGraph::load_scope`] to
    // restore the canonical set produced by the concept-graph stage.
    let db_path = state.graph_db_path.clone().expect(
        "concept-graph stage must run before export stage to materialise the concept graph",
    );
    let mut pgraph = PersistentConceptGraph::open(&db_path, &state.master_key)
        .expect("re-open the persistent concept graph");
    pgraph
        .load_scope(dataset.tenant_scope.id)
        .expect("rehydrate tenant scope from disk");
    let graph = pgraph.graph();

    // Resolve the canonical concept ids from in-memory state — concept-graph stage
    // populates them after promotion. They are real graph nodes, not
    // fixtures.
    let canonical_ids: Vec<Uuid> = state.canonical_concept_ids.clone();
    let canonical_total = canonical_ids.len();
    log.check(
        PHASE,
        "concept-graph stage surfaced at least 3 canonical concepts to export",
        canonical_total >= 3,
    );

    // -------- Build the export-control registry --------------------
    // Deny-by-default: register all but the *last* canonical id so the
    // simulator surfaces a real "deny-by-default" rejection on a
    // genuine canonical concept.
    let mut controls = ExportControlRegistry::new();
    let registered_ids: Vec<Uuid> = if canonical_total > 1 {
        canonical_ids[..canonical_total - 1].to_vec()
    } else {
        canonical_ids.clone()
    };
    let unregistered_id: Option<Uuid> = if canonical_total > 1 {
        Some(canonical_ids[canonical_total - 1])
    } else {
        None
    };
    let registry_started = Instant::now();
    for id in &registered_ids {
        controls
            .insert_concept(ConceptExportControl::new(*id))
            .expect("insert concept control");
    }
    let registry_elapsed = registry_started.elapsed();
    let registered_count = controls.concepts().count();

    log.check(
        PHASE,
        "registry contains a control row for every approved canonical concept",
        registered_count == registered_ids.len(),
    );
    log.check(
        PHASE,
        "deny-by-default: at least one canonical concept has no control row",
        unregistered_id.is_some(),
    );
    if let Some(id) = unregistered_id {
        log.check(
            PHASE,
            "registry rejects the un-registered canonical concept",
            !controls.allows_concept(id, Uuid::nil(), Uuid::nil(), chrono::Utc::now()),
        );
    }

    // Register a summary control so the simulator can surface a
    // non-empty summary path. The summary id is synthesised here
    // (memory-manager summaries aren't propagated through to the export stage
    // in the demo's simplified state); registering it lets the demo
    // exercise [`PolicySimulator::simulate`]'s summary code path
    // against a real registry.
    let summary_id = Uuid::new_v4();
    let blocked_summary_id = Uuid::new_v4();
    let scope_for_export = dataset.tenant_scope.id;
    controls
        .insert_summary(SummaryExportControl::new(
            summary_id,
            scope_for_export,
            RedactionLevel::Partial,
        ))
        .expect("insert summary control");
    let mut blocked_summary =
        SummaryExportControl::new(blocked_summary_id, scope_for_export, RedactionLevel::Full);
    blocked_summary.exportable = false;
    controls
        .insert_summary(blocked_summary)
        .expect("insert blocked summary control");

    // -------- Approval workflow ----------------------------------
    let mut workflow = ConceptApprovalWorkflow::new();
    let approval_started = Instant::now();
    let mut approved_concept_ids: Vec<Uuid> = Vec::new();

    // The profile id is needed up front so each
    // `ConceptExportControl`'s `allowed_profiles` whitelist can be
    // honoured (it's empty here, so any profile is allowed). We
    // generate a stable id for the profile so the workflow
    // call-sites can pass it consistently.
    let profile_id = Uuid::new_v4();

    // Sensitivity class is supplied by the caller (the workflow
    // explicitly refuses to invent a default — see the docstring on
    // `approve_for_export`). We assign `Useful` to most concepts and
    // `Important` to one so the policy engine's sensitivity ceiling
    // exercises a real upgrade. None are `Critical`, so the raw-
    // evidence path remains gated by the policy itself.
    for (idx, id) in registered_ids.iter().enumerate() {
        let sensitivity = if idx == 0 {
            SensitivityClass::Important
        } else {
            SensitivityClass::Useful
        };
        let approved = workflow
            .approve_for_export(
                *id,
                scope_for_export,
                profile_id,
                sensitivity,
                graph,
                &controls,
            )
            .expect("approve canonical concept");
        approved_concept_ids.push(approved.concept_id);
    }
    let approval_elapsed = approval_started.elapsed();
    let approval_count = approved_concept_ids.len();

    log.check(
        PHASE,
        "approval workflow accepted every registered canonical concept",
        approval_count == registered_ids.len(),
    );

    // Negative case: re-approving the same concept must fail.
    let duplicate_blocked = if let Some(first) = registered_ids.first() {
        workflow
            .approve_for_export(
                *first,
                scope_for_export,
                profile_id,
                SensitivityClass::Useful,
                graph,
                &controls,
            )
            .is_err()
    } else {
        false
    };
    log.check(
        PHASE,
        "duplicate approval is rejected by the workflow",
        duplicate_blocked,
    );

    // -------- Profile + constraints -------------------------------
    let mut profile = PortableConceptProfile::new(
        "demo-export-profile",
        "Export demo profile rendering canonical concepts to a downstream tool",
        "demo-downstream-tool",
        scope_for_export,
    );
    // Force the profile id so the registry's `allowed_profiles`
    // whitelist (empty here) and the audit trail line up across the
    // approval / render / simulate calls.
    profile.id = profile_id;

    for approved in workflow.list_approved(scope_for_export) {
        profile.push_concept(approved);
    }
    // The cap is set well above the substrate-level canonical
    // concept count produced by the concept-graph stage so the demo policy admits
    // every approved concept; the cap-tightening contract itself
    // is exercised by `with_constraints_max_concepts_tightens` in
    // `crates/export_plane/src/policy.rs`.
    profile.push_constraint(ExportConstraint::MaxConcepts(64));
    profile.push_constraint(ExportConstraint::SensitivityCeiling(
        SensitivityClass::Important,
    ));
    profile.push_constraint(ExportConstraint::ScopeRestriction(vec![scope_for_export.0]));
    let profile_concept_count = profile.concepts.len() as u64;

    log.check(
        PHASE,
        "profile carries every approved concept",
        profile.concepts.len() == approval_count,
    );
    log.check(
        PHASE,
        "profile carries a non-empty constraint set",
        !profile.constraints.is_empty(),
    );

    // -------- Policy engine ---------------------------------------
    let policy = ExportPolicy {
        sensitivity_ceiling: SensitivityClass::Important,
        ..ExportPolicy::default()
    };
    let effective_policy = policy.clone().with_constraints(&profile.constraints);
    let evaluate_started = Instant::now();
    let decision = PolicyEngine::new().evaluate(&effective_policy, &profile.concepts);
    let evaluate_elapsed = evaluate_started.elapsed();
    let approved_by_engine = decision.approved.len() as u64;
    let rejected_by_engine = decision.rejected.len() as u64;

    log.check(
        PHASE,
        "policy engine approves every concept under the demo policy",
        approved_by_engine == profile_concept_count,
    );
    log.check(
        PHASE,
        "policy engine rejects no concept under the demo policy",
        rejected_by_engine == 0,
    );
    log.check(
        PHASE,
        "policy engine refuses raw evidence by default",
        !decision.allow_raw_evidence,
    );

    // -------- Render: ConceptsOnly --------------------------------
    let render_started = Instant::now();
    let concepts_only = ExportView::from_decision(
        &decision,
        profile.id,
        scope_for_export,
        ExportViewRequest::ConceptsOnly,
    )
    .expect("render ConceptsOnly view");
    let concepts_only_elapsed = render_started.elapsed();
    log.check(
        PHASE,
        "ConceptsOnly view surfaces every approved concept",
        concepts_only.content.concepts().len() as u64 == approved_by_engine,
    );
    log.check(
        PHASE,
        "ConceptsOnly view exposes no summaries or evidence pack",
        concepts_only.content.summaries().is_empty()
            && concepts_only.content.evidence_pack().is_none(),
    );

    // -------- Render: WithSummaries -------------------------------
    let with_summaries_started = Instant::now();
    let summary_payload = ApprovedSummary {
        summary_id,
        scope_id: scope_for_export,
        body: "Demo channel summary surfaced via the export plane.".into(),
    };
    let with_summaries = ExportView::from_decision(
        &decision,
        profile.id,
        scope_for_export,
        ExportViewRequest::WithSummaries {
            summaries: vec![summary_payload.clone()],
        },
    )
    .expect("render WithSummaries view");
    let with_summaries_elapsed = with_summaries_started.elapsed();
    log.check(
        PHASE,
        "WithSummaries view exposes the supplied summary",
        with_summaries.content.summaries().len() == 1,
    );
    log.check(
        PHASE,
        "WithSummaries view still surfaces the full approved concept set",
        with_summaries.content.concepts().len() as u64 == approved_by_engine,
    );

    // -------- Negative path: WithEvidencePack must be rejected -----
    let evidence_pack = EvidencePack {
        evidence_refs: state
            .ingested_rows
            .iter()
            .take(2)
            .map(|row| crypto::EvidenceRef::from_uuid(row.evidence_id.0))
            .collect(),
        concept_ids: approved_concept_ids.clone(),
    };
    let evidence_request = ExportViewRequest::WithEvidencePack {
        summaries: vec![summary_payload.clone()],
        evidence_pack,
    };
    let evidence_attempt =
        ExportView::from_decision(&decision, profile.id, scope_for_export, evidence_request);
    let evidence_blocked = matches!(
        evidence_attempt,
        Err(ExportViewError::RawEvidenceNotAuthorised)
    );
    log.check(
        PHASE,
        "WithEvidencePack is rejected when policy disallows raw evidence",
        evidence_blocked,
    );

    // -------- Policy simulator ------------------------------------
    let simulate_started = Instant::now();
    let simulator = PolicySimulator::new(&policy, &controls);
    let simulation = simulator.simulate(&profile);
    let simulate_elapsed = simulate_started.elapsed();
    log.check(
        PHASE,
        "simulator's included-concept set matches the engine's approved set",
        simulation.included_concepts.len() as u64 == approved_by_engine,
    );
    log.check(
        PHASE,
        "simulator surfaces the registered exportable summary",
        simulation.included_summaries.contains(&summary_id),
    );
    log.check(
        PHASE,
        "simulator excludes the non-exportable summary",
        simulation
            .excluded_summaries
            .iter()
            .any(|ex| ex.entity_id == blocked_summary_id),
    );
    log.check(
        PHASE,
        "simulator estimate is non-trivial when concepts are included",
        simulation.total_export_size_estimate > 0,
    );
    log.check(
        PHASE,
        "simulator does not authorise raw-evidence emission under the demo policy",
        !simulation.would_include_evidence,
    );

    // -------- Audit ------------------------------------------------
    let audit_started = Instant::now();
    log_export(
        &mut state.audit_log,
        profile.id,
        scope_for_export,
        Actor::System,
        &decision
            .approved
            .iter()
            .map(|c| c.concept_id)
            .collect::<Vec<_>>(),
    )
    .expect("log_export must succeed");
    log_export_simulated(
        &mut state.audit_log,
        profile.id,
        scope_for_export,
        Actor::System,
        simulation.included_concepts.len(),
        simulation.excluded_concepts.len(),
    )
    .expect("log_export_simulated must succeed");
    let audit_elapsed = audit_started.elapsed();

    // -------- Bookkeeping + report --------------------------------
    state.export_profiles_created += 1;
    state.export_concepts_approved += approval_count as u64;
    state.export_views_rendered += 2; // ConceptsOnly + WithSummaries
    state.export_simulations_run += 1;

    phase.timing = started.elapsed();
    phase.stat("canonical_concepts_total", canonical_total.to_string());
    phase.stat("controls_registered", registered_count.to_string());
    phase.stat("approvals_minted", approval_count.to_string());
    phase.stat("engine_approved_concepts", approved_by_engine.to_string());
    phase.stat("engine_rejected_concepts", rejected_by_engine.to_string());
    phase.stat(
        "view_concepts_only_concepts",
        concepts_only.content.concepts().len().to_string(),
    );
    phase.stat(
        "view_with_summaries_summaries",
        with_summaries.content.summaries().len().to_string(),
    );
    phase.stat(
        "simulator_included_concepts",
        simulation.included_concepts.len().to_string(),
    );
    phase.stat(
        "simulator_excluded_concepts",
        simulation.excluded_concepts.len().to_string(),
    );
    phase.stat(
        "simulator_included_summaries",
        simulation.included_summaries.len().to_string(),
    );
    phase.stat(
        "simulator_size_estimate_bytes",
        simulation.total_export_size_estimate.to_string(),
    );
    phase.note(
        "Deny-by-default ExportControlRegistry + PolicyEngine + PolicySimulator + \
         ConceptApprovalWorkflow + ExportView render pipeline driven by the concept-graph stage's \
         canonical concepts.",
    );

    report.count("export_profiles_created", state.export_profiles_created);
    report.count("export_concepts_approved", state.export_concepts_approved);
    report.count("export_views_rendered", state.export_views_rendered);
    report.count("export_simulations_run", state.export_simulations_run);
    report.add_phase(phase);
    report.add_benchmark(
        "export_registry_inserts",
        registered_count as u64,
        registry_elapsed,
    );
    report.add_benchmark(
        "export_concept_approvals",
        approval_count as u64,
        approval_elapsed,
    );
    report.add_benchmark("export_engine_evaluate", 1, evaluate_elapsed);
    report.add_benchmark("export_render_concepts_only", 1, concepts_only_elapsed);
    report.add_benchmark("export_render_with_summaries", 1, with_summaries_elapsed);
    report.add_benchmark("export_policy_simulate", 1, simulate_elapsed);
    report.add_benchmark("export_audit_writes", 2, audit_elapsed);

    // Drop the persistent graph handle now that we are done; later
    // stages that need the graph open it again from disk.
    drop(pgraph);
}
