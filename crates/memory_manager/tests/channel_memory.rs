//! Integration tests for the Channel Memory Object.

use chrono::{Duration, Utc};
use evidence_store::ScopeId;
use memory_manager::{
    channel_memory::DEFAULT_COMPLETED_TASK_TTL_DAYS, ActiveTask, ChannelMemoryObject, Decision,
    MemoryError, MemoryState, OpenQuestion,
};
use uuid::Uuid;

#[test]
fn empty_channel_memory_has_no_decisions_or_tasks() {
    let scope = ScopeId::new_v4();
    let mem = ChannelMemoryObject::new(scope);
    assert_eq!(mem.scope_id, scope);
    assert!(mem.decisions.is_empty());
    assert!(mem.list_active_tasks().is_empty());
    assert!(mem.list_open_questions().is_empty());
    assert!(mem.last_synthesis_window.is_none());
}

#[test]
fn add_decision_records_text_and_id() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let id = mem.add_decision(Decision::new(scope, "Approved policy v3"));
    assert_eq!(mem.decisions.len(), 1);
    assert_eq!(mem.decisions[0].text, "Approved policy v3");
    assert_eq!(mem.decisions[0].memory.id, id);
}

#[test]
fn add_task_with_assignee() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let id = mem.add_task(ActiveTask::new(scope, "draft RFC").with_assignee("@Sara"));
    let active = mem.list_active_tasks();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].memory.id, id);
    assert_eq!(active[0].assignee.as_deref(), Some("@Sara"));
}

#[test]
fn complete_task_drops_it_from_active_list() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let id = mem.add_task(ActiveTask::new(scope, "draft RFC"));
    mem.add_task(ActiveTask::new(scope, "schedule review"));
    mem.complete_task(id).unwrap();
    let active = mem.list_active_tasks();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].text, "schedule review");
}

#[test]
fn complete_unknown_task_errors() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let err = mem.complete_task(Uuid::new_v4()).unwrap_err();
    assert!(matches!(err, MemoryError::NotFound(_)));
}

#[test]
fn add_and_resolve_open_question() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let id = mem.add_open_question(OpenQuestion::new(scope, "Who owns the API rollout?"));
    assert_eq!(mem.list_open_questions().len(), 1);
    mem.resolve_question(id).unwrap();
    assert!(mem.list_open_questions().is_empty());
    let resolved = &mem.open_questions[0];
    assert!(resolved.is_resolved());
}

#[test]
fn update_recap_records_window_id_and_bumps_timestamp() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let original = mem.updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));
    let window_id = Uuid::new_v4();
    mem.update_recap("Decisions made; tasks scheduled", Some(window_id));
    assert_eq!(mem.recap, "Decisions made; tasks scheduled");
    assert_eq!(mem.last_synthesis_window, Some(window_id));
    assert!(mem.updated_at > original);
}

#[test]
fn decay_sweep_archives_completed_tasks_past_ttl() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let stale = ActiveTask::new(scope, "ancient task");
    let stale_id = stale.memory.id;
    mem.active_tasks.push(stale);
    // Manually backdate the completion.
    mem.active_tasks[0].completed_at =
        Some(Utc::now() - Duration::days(DEFAULT_COMPLETED_TASK_TTL_DAYS + 1));
    // Add a fresh completed task that should NOT be archived.
    let recent = ActiveTask::new(scope, "recent task");
    let recent_id = recent.memory.id;
    mem.active_tasks.push(recent);
    mem.complete_task(recent_id).unwrap();
    let report = mem.decay_sweep(Utc::now());
    assert_eq!(report.tasks_archived, 1);
    assert!(mem.active_tasks.iter().all(|t| t.memory.id != stale_id));
    assert!(mem.active_tasks.iter().any(|t| t.memory.id == recent_id));
}

#[test]
fn decay_sweep_preserves_open_questions_until_resolution() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    let id = mem.add_open_question(OpenQuestion::new(scope, "Who owns the API?"));
    let report = mem.decay_sweep(Utc::now());
    assert_eq!(report.questions_archived, 0);
    assert_eq!(mem.list_open_questions().len(), 1);
    mem.resolve_question(id).unwrap();
    // Backdate resolution past the TTL, sweep again.
    mem.open_questions[0].resolved_at =
        Some(Utc::now() - Duration::days(memory_manager::DEFAULT_RESOLVED_QUESTION_TTL_DAYS + 1));
    let report = mem.decay_sweep(Utc::now());
    assert_eq!(report.questions_archived, 1);
}

#[test]
fn channel_memory_is_serde_round_trippable() {
    let scope = ScopeId::new_v4();
    let mut mem = ChannelMemoryObject::new(scope);
    mem.add_decision(Decision::new(scope, "ship Friday"));
    mem.add_task(ActiveTask::new(scope, "draft RFC").with_assignee("@Sara"));
    mem.add_open_question(OpenQuestion::new(scope, "Who owns the API?"));
    let bytes = serde_json::to_vec(&mem).unwrap();
    let back: ChannelMemoryObject = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, mem);
}

#[test]
fn decisions_carry_important_sensitivity() {
    let scope = ScopeId::new_v4();
    let d = Decision::new(scope, "approved policy");
    assert_eq!(
        d.memory.sensitivity_class,
        memory_manager::SensitivityClass::Important
    );
    assert_eq!(d.memory.state, MemoryState::Candidate);
}
