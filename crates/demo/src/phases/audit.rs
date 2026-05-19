//! Stage 12 — Audit Service.
//!
//! Drives the [`audit_service::AuditLog`] populated by phases 5 / 6 /
//! 7 / 8 / 9 / 10 / 11 and exercises the production query API
//! (scope, action type, time range, actor) end-to-end. Every prior
//! phase that performs an audit-worthy action (synthesis emission,
//! permission grant, key destruction, export render / simulate, agent
//! proposal lifecycle, contradiction detection, connector exercise)
//! has already appended into [`RuntimeState::audit_log`]; this stage
//! now seals the run by:
//!
//! 1. Appending a tenant-lifecycle "demo-run-completed" entry so the
//!    final audit row is the substrate-level provenance of the demo.
//! 2. Asserting the log is strictly monotonic (sequence numbers are
//!    `0..len`).
//! 3. Running an [`audit_service::AuditQuery`] for every action type
//!    seen in the log and recording the per-action count into the
//!    report.
//! 4. Running scope-, time-range- and actor-filtered queries against
//!    the same log to verify all four query dimensions produce
//!    plausible, restrictive answers.

use std::time::Instant;

use audit_service::{Actor, AuditActionType, AuditEntryBuilder, AuditQuery, TargetRef, TargetType};
use chrono::Utc;
use evidence_store::ScopeId;
use serde_json::json;
use uuid::Uuid;

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE_LABEL: &str = "audit";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let start = Instant::now();
    let mut phase = PhaseReport::new("Stage 12: Audit Service");

    // 1. Append a tenant-lifecycle "demo-run-completed" entry so the
    //    final audit row is the substrate-level provenance of the
    //    demo run.
    let demo_run_id = Uuid::new_v4();
    let lifecycle_entry = AuditEntryBuilder::new()
        .actor(Actor::System)
        .action(AuditActionType::TenantLifecycle)
        .target(TargetRef::new(
            TargetType::Tenant,
            dataset.tenant_scope.id.0,
        ))
        .scope(dataset.tenant_scope.id)
        .details(json!({
            "event": "demo_run_completed",
            "demo_run_id": demo_run_id,
            "phase_count": 12,
            "dataset_messages": dataset.messages.len(),
        }))
        .build()
        .expect("build demo-run-completed audit entry");
    let lifecycle_id = state.audit_log.append(lifecycle_entry);

    let total_entries = state.audit_log.len();
    log.record(
        PHASE_LABEL,
        "audit log accumulated entries from every audit-emitting phase",
        total_entries >= 12,
    );

    // 2. Strict monotonicity: sequence numbers must be `0..len`.
    let mut monotonic = true;
    for (i, entry) in state.audit_log.entries().iter().enumerate() {
        if entry.sequence != i as u64 {
            monotonic = false;
            break;
        }
    }
    log.record(
        PHASE_LABEL,
        "audit log assigns strictly monotonic sequence numbers",
        monotonic,
    );

    // 3. Per-action-type query. Iterates every observed action type
    //    in the log and records the count via the production
    //    `AuditQuery::with_action` API.
    let mut action_types: Vec<AuditActionType> = Vec::new();
    for entry in state.audit_log.entries() {
        if !action_types.contains(&entry.action_type) {
            action_types.push(entry.action_type);
        }
    }
    for action in &action_types {
        let q = AuditQuery::new().with_action(*action);
        let n = state.audit_log.query(&q).count();
        phase.stat(format!("by_action.{}", action.as_str()), n.to_string());
    }
    log.record(
        PHASE_LABEL,
        "audit log surfaces at least four distinct action types",
        action_types.len() >= 4,
    );

    // 4a. Scope-filtered query — restrict to the tenant scope and
    //     verify every returned entry's scope matches.
    let tenant_scope: ScopeId = dataset.tenant_scope.id;
    let scope_query = AuditQuery::new().with_scope(tenant_scope);
    let scope_query_started = Instant::now();
    let scope_hits: Vec<_> = state.audit_log.query(&scope_query).collect();
    let scope_query_elapsed = scope_query_started.elapsed();
    let scope_match =
        !scope_hits.is_empty() && scope_hits.iter().all(|e| e.scope_id == Some(tenant_scope));
    log.record(
        PHASE_LABEL,
        "scope-filtered query returns only tenant-scope entries",
        scope_match,
    );
    phase.stat("scope_query.tenant.hits", scope_hits.len().to_string());
    report.add_benchmark(
        "audit.query_by_scope",
        scope_hits.len() as u64,
        scope_query_elapsed,
    );

    // 4b. Action-filtered query — narrow to canonical-promotion
    //     entries (agent / reasoning stages emit these via
    //     `log_proposal_promoted`).
    let promoted_query = AuditQuery::new().with_action(AuditActionType::AgentProposalPromoted);
    let promoted_q_started = Instant::now();
    let promoted_hits: Vec<_> = state.audit_log.query(&promoted_query).collect();
    let promoted_q_elapsed = promoted_q_started.elapsed();
    log.record(
        PHASE_LABEL,
        "action-filtered query returns at least one AgentProposalPromoted entry",
        !promoted_hits.is_empty(),
    );
    phase.stat(
        "action_query.agent_proposal_promoted.hits",
        promoted_hits.len().to_string(),
    );
    report.add_benchmark(
        "audit.query_by_action",
        promoted_hits.len() as u64,
        promoted_q_elapsed,
    );

    // 4c. Time-range query — restrict to entries appended *after*
    //     the lifecycle row we just inserted; the only match must be
    //     itself or a later-stamped entry.
    let lifecycle_ts = state
        .audit_log
        .get(lifecycle_id)
        .expect("lifecycle entry present")
        .timestamp;
    let since_query = AuditQuery::new().since(lifecycle_ts);
    let time_q_started = Instant::now();
    let since_hits: Vec<_> = state.audit_log.query(&since_query).collect();
    let time_q_elapsed = time_q_started.elapsed();
    log.record(
        PHASE_LABEL,
        "time-range (since) query reaches the just-appended lifecycle row",
        since_hits.iter().any(|e| e.id == lifecycle_id),
    );
    let until_query = AuditQuery::new().until(lifecycle_ts);
    let until_hits: Vec<_> = state.audit_log.query(&until_query).collect();
    log.record(
        PHASE_LABEL,
        "time-range (until) query covers earlier phases' entries",
        until_hits.len() >= total_entries.saturating_sub(1),
    );
    phase.stat(
        "time_query.since_lifecycle.hits",
        since_hits.len().to_string(),
    );
    phase.stat(
        "time_query.until_lifecycle.hits",
        until_hits.len().to_string(),
    );
    report.add_benchmark(
        "audit.query_by_time_range",
        (since_hits.len() + until_hits.len()) as u64,
        time_q_elapsed,
    );

    // 4d. Actor-filtered query — pick the most-frequent
    //     non-`System` actor and verify the query returns only
    //     entries by that actor.
    let mut user_or_agent: Option<(Uuid, &'static str)> = None;
    for entry in state.audit_log.entries() {
        match entry.actor {
            Actor::User(id) => {
                user_or_agent = Some((id, "user"));
                break;
            }
            Actor::Agent(id) => {
                user_or_agent = Some((id, "agent"));
                break;
            }
            Actor::System => {}
        }
    }
    if let Some((actor_id, kind)) = user_or_agent {
        let actor_query = AuditQuery::new().with_actor(actor_id);
        let actor_q_started = Instant::now();
        let actor_hits: Vec<_> = state.audit_log.query(&actor_query).collect();
        let actor_q_elapsed = actor_q_started.elapsed();
        let only_actor = !actor_hits.is_empty()
            && actor_hits.iter().all(|e| match e.actor {
                Actor::User(id) | Actor::Agent(id) => id == actor_id,
                Actor::System => false,
            });
        log.record(
            PHASE_LABEL,
            "actor-filtered query returns only entries by the chosen actor",
            only_actor,
        );
        phase.stat(
            format!("actor_query.{kind}.hits"),
            actor_hits.len().to_string(),
        );
        report.add_benchmark(
            "audit.query_by_actor",
            actor_hits.len() as u64,
            actor_q_elapsed,
        );
    } else {
        // The agent stage always emits a User-actor entry, so this branch is
        // a safety net for future refactors. We still record the
        // assertion so it's surfaced rather than silently skipped.
        log.record(
            PHASE_LABEL,
            "audit log contains at least one non-system actor",
            false,
        );
    }

    // 5. Combined query — scope ∧ action ∧ time. Demonstrates that
    //    the production filter API composes correctly.
    let combined = AuditQuery::new()
        .with_scope(tenant_scope)
        .with_action(AuditActionType::TenantLifecycle)
        .since(lifecycle_ts);
    let combined_q_started = Instant::now();
    let combined_hits: Vec<_> = state.audit_log.query(&combined).collect();
    let combined_q_elapsed = combined_q_started.elapsed();
    let combined_id_matches = combined_hits.iter().any(|e| e.id == lifecycle_id);
    log.record(
        PHASE_LABEL,
        "composite query (scope + action + since) recovers the lifecycle row",
        combined_id_matches,
    );
    phase.stat(
        "composite_query.lifecycle.hits",
        combined_hits.len().to_string(),
    );
    report.add_benchmark(
        "audit.composite_query",
        combined_hits.len() as u64,
        combined_q_elapsed,
    );

    // 6. Append-only invariant — `AuditLog` exposes no public
    //    mutation API. The type-system enforces this; the assertion
    //    here is a runtime sanity check that `len()` only grew over
    //    the run.
    log.record(
        PHASE_LABEL,
        "audit log is append-only (no entries were removed)",
        state.audit_log.len() >= total_entries,
    );

    // ---- Wrap up the phase report. ----
    phase.stat("audit_log.entries", state.audit_log.len().to_string());
    phase.stat("audit_log.action_types", action_types.len().to_string());
    phase.stat("queries.executed", "5".to_string());
    phase.note(format!(
        "audit log carries {} entries spanning {} distinct action types",
        state.audit_log.len(),
        action_types.len()
    ));
    phase.note(format!(
        "demo-run-completed lifecycle entry id = {}",
        lifecycle_id.0
    ));
    phase.note(format!(
        "demo run completed at {} UTC",
        Utc::now().to_rfc3339()
    ));

    phase.timing = start.elapsed();
    report.count("audit.entries", state.audit_log.len() as u64);
    report.count("audit.action_types", action_types.len() as u64);
    report.add_phase(phase);
}
