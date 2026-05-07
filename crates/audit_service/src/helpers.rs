//! Phase 5 audit-event helpers.
//!
//! These helpers wrap [`crate::AuditEntryBuilder`] for the common
//! Phase 5 / Phase 3 lifecycle events so callers don't have to
//! re-implement the same builder boilerplate at each call-site. They
//! return the [`crate::AuditEntryId`] of the appended entry so the
//! caller can correlate further actions back to the audit row.

use serde_json::json;
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::entry::{Actor, AuditActionType, AuditEntryBuilder, TargetRef, TargetType};
use crate::error::Result;
use crate::log::AuditLog;
use crate::AuditEntryId;

/// Append an [`AuditActionType::ExportRendered`] entry.
///
/// * `profile_id` — id of the [`crate::TargetType::ExportProfile`].
/// * `scope_id` — substrate scope the export was rendered in.
/// * `actor` — who rendered it (a human admin or an automated job).
/// * `concepts_exported` — the ids of the concepts that ended up in
///   the rendered view. Stored in the entry's `details` field as a
///   `concepts_exported` JSON array.
pub fn log_export(
    log: &mut AuditLog,
    profile_id: Uuid,
    scope_id: ScopeId,
    actor: Actor,
    concepts_exported: &[Uuid],
) -> Result<AuditEntryId> {
    let entry = AuditEntryBuilder::new()
        .actor(actor)
        .action(AuditActionType::ExportRendered)
        .target(TargetRef::new(TargetType::ExportProfile, profile_id))
        .scope(scope_id)
        .details(json!({
            "concepts_exported": concepts_exported,
            "count": concepts_exported.len(),
        }))
        .build()?;
    Ok(log.append(entry))
}

/// Append an [`AuditActionType::ExportSimulated`] entry.
///
/// Phase 5 produces an audit entry every time
/// [`PolicySimulator::simulate`](https://docs.rs/) is run so operators
/// can prove a simulation occurred without producing a real export.
pub fn log_export_simulated(
    log: &mut AuditLog,
    profile_id: Uuid,
    scope_id: ScopeId,
    actor: Actor,
    included_count: usize,
    excluded_count: usize,
) -> Result<AuditEntryId> {
    let entry = AuditEntryBuilder::new()
        .actor(actor)
        .action(AuditActionType::ExportSimulated)
        .target(TargetRef::new(TargetType::ExportProfile, profile_id))
        .scope(scope_id)
        .details(json!({
            "included_count": included_count,
            "excluded_count": excluded_count,
        }))
        .build()?;
    Ok(log.append(entry))
}

/// Append an [`AuditActionType::AgentProposalSubmitted`] entry.
///
/// Records that an agent submitted a new proposal awaiting review.
pub fn log_proposal_submitted(
    log: &mut AuditLog,
    proposal_id: Uuid,
    agent_id: Uuid,
    scope_id: ScopeId,
) -> Result<AuditEntryId> {
    let entry = AuditEntryBuilder::new()
        .actor(Actor::Agent(agent_id))
        .action(AuditActionType::AgentProposalSubmitted)
        .target(TargetRef::new(TargetType::MemoryObject, proposal_id))
        .scope(scope_id)
        .details(json!({
            "agent_id": agent_id,
        }))
        .build()?;
    Ok(log.append(entry))
}

/// Append an [`AuditActionType::AgentProposalPromoted`] entry.
///
/// Records that a previously-submitted proposal has been promoted to
/// canonical.
pub fn log_proposal_promoted(
    log: &mut AuditLog,
    proposal_id: Uuid,
    promoter_id: Uuid,
    scope_id: ScopeId,
) -> Result<AuditEntryId> {
    let entry = AuditEntryBuilder::new()
        .actor(Actor::User(promoter_id))
        .action(AuditActionType::AgentProposalPromoted)
        .target(TargetRef::new(TargetType::MemoryObject, proposal_id))
        .scope(scope_id)
        .details(json!({
            "promoter_id": promoter_id,
        }))
        .build()?;
    Ok(log.append(entry))
}

/// Append an [`AuditActionType::AgentProposalRejected`] entry.
///
/// `reason` is a free-form human-readable string captured into the
/// entry's `details` field.
pub fn log_proposal_rejected(
    log: &mut AuditLog,
    proposal_id: Uuid,
    rejector_id: Uuid,
    scope_id: ScopeId,
    reason: &str,
) -> Result<AuditEntryId> {
    let entry = AuditEntryBuilder::new()
        .actor(Actor::User(rejector_id))
        .action(AuditActionType::AgentProposalRejected)
        .target(TargetRef::new(TargetType::MemoryObject, proposal_id))
        .scope(scope_id)
        .details(json!({
            "rejector_id": rejector_id,
            "reason": reason,
        }))
        .build()?;
    Ok(log.append(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_scope() -> ScopeId {
        ScopeId::new_v4()
    }

    #[test]
    fn log_export_writes_export_rendered() {
        let mut log = AuditLog::new();
        let profile = Uuid::new_v4();
        let scope = fixture_scope();
        let actor = Actor::User(Uuid::new_v4());
        let concepts = vec![Uuid::new_v4(), Uuid::new_v4()];
        let id = log_export(&mut log, profile, scope, actor, &concepts).expect("log");
        let entry = log.get(id).expect("present");
        assert_eq!(entry.action_type, AuditActionType::ExportRendered);
        assert_eq!(entry.target.target_type, TargetType::ExportProfile);
        assert_eq!(entry.target.target_id, profile);
        assert_eq!(entry.scope_id, Some(scope));
        assert_eq!(
            entry
                .details
                .get("count")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn log_export_simulated_writes_export_simulated() {
        let mut log = AuditLog::new();
        let profile = Uuid::new_v4();
        let scope = fixture_scope();
        let id = log_export_simulated(&mut log, profile, scope, Actor::System, 3, 2).expect("log");
        let entry = log.get(id).expect("present");
        assert_eq!(entry.action_type, AuditActionType::ExportSimulated);
        assert_eq!(
            entry
                .details
                .get("included_count")
                .and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert_eq!(
            entry
                .details
                .get("excluded_count")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn log_proposal_submitted_records_agent_actor() {
        let mut log = AuditLog::new();
        let proposal = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let id = log_proposal_submitted(&mut log, proposal, agent, fixture_scope()).expect("log");
        let entry = log.get(id).expect("present");
        assert_eq!(entry.action_type, AuditActionType::AgentProposalSubmitted);
        match entry.actor {
            Actor::Agent(id) => assert_eq!(id, agent),
            other => panic!("expected Agent actor, got {other:?}"),
        }
    }

    #[test]
    fn log_proposal_promoted_records_user_actor() {
        let mut log = AuditLog::new();
        let proposal = Uuid::new_v4();
        let user = Uuid::new_v4();
        let id = log_proposal_promoted(&mut log, proposal, user, fixture_scope()).expect("log");
        let entry = log.get(id).expect("present");
        assert_eq!(entry.action_type, AuditActionType::AgentProposalPromoted);
        match entry.actor {
            Actor::User(id) => assert_eq!(id, user),
            other => panic!("expected User actor, got {other:?}"),
        }
    }

    #[test]
    fn log_proposal_rejected_records_reason() {
        let mut log = AuditLog::new();
        let proposal = Uuid::new_v4();
        let user = Uuid::new_v4();
        let id = log_proposal_rejected(&mut log, proposal, user, fixture_scope(), "duplicate")
            .expect("log");
        let entry = log.get(id).expect("present");
        assert_eq!(entry.action_type, AuditActionType::AgentProposalRejected);
        assert_eq!(
            entry
                .details
                .get("reason")
                .and_then(serde_json::Value::as_str),
            Some("duplicate")
        );
    }

    #[test]
    fn helpers_share_a_monotonic_sequence() {
        let mut log = AuditLog::new();
        let scope = fixture_scope();
        let _ = log_export(&mut log, Uuid::new_v4(), scope, Actor::System, &[]).expect("ok");
        let _ =
            log_proposal_submitted(&mut log, Uuid::new_v4(), Uuid::new_v4(), scope).expect("ok");
        let _ = log_proposal_promoted(&mut log, Uuid::new_v4(), Uuid::new_v4(), scope).expect("ok");
        let seqs: Vec<u64> = log.entries().iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }
}
