//! Channel Memory Object — the per-channel synthesis-output home.
//!
//! Per `docs/DESIGN.md` §6.2: "channel memory =
//! recap, decisions, open questions, active tasks". Each channel has
//! a single [`ChannelMemoryObject`] that the synthesis pipeline
//! updates on every window. Decisions, open questions, and active
//! tasks are themselves [`MemoryObject`]s — they decay on the same
//! state machine as personal-memory items, and the [`Self::decay_sweep`]
//! method archives completed tasks once the per-class TTL has
//! elapsed.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::error::{MemoryError, Result};
use crate::object::{MemoryObject, SensitivityClass};
use crate::state::MemoryState;
use crate::transitions::MemoryStateMachine;

/// Default TTL after which a *completed* active task is archived
/// from the channel memory's `active_tasks` list.
pub const DEFAULT_COMPLETED_TASK_TTL_DAYS: i64 = 30;

/// Default TTL after which a *resolved* open question is archived.
pub const DEFAULT_RESOLVED_QUESTION_TTL_DAYS: i64 = 30;

/// Canonical form used by the `*_dedup` insert helpers to decide
/// whether two surface texts refer to the same channel-memory item.
///
/// The synthesis SLM is non-deterministic on whitespace and casing —
/// the same decision may come back as `"Approved policy v3"` on one
/// run and `"approved  policy v3"` on the next. We collapse all
/// runs of ASCII whitespace, trim, and lowercase before comparing so
/// these collapse to one row.
fn normalise_surface_text(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Newtype wrapper so callers can distinguish a recorded **decision**
/// from a generic memory object.
///
/// A decision is just a [`MemoryObject`] in `Important` /
/// `Critical` sensitivity (per `docs/DESIGN.md` §4.3 — "decisions are
/// critical"). The wrapper buys two things:
///
/// 1. A type-level annotation that `add_decision` takes decisions,
///    not arbitrary memory objects.
/// 2. A short content surface (`text`) that mirrors the GBNF
///    `synth.summary.decisions[]` shape from `synthesis_pipeline`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// Underlying memory object.
    pub memory: MemoryObject,
    /// Surface text of the decision (`"Approved policy v3"`).
    pub text: String,
}

impl Decision {
    /// Construct a fresh `Important`-class decision. The underlying
    /// memory object starts in [`MemoryState::Candidate`]; callers
    /// promote through the standard memory-manager state machine
    /// (Reinforced → Consolidated → Canonical) as the decision
    /// accumulates corroboration.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Important),
            text: text.into(),
        }
    }
}

/// Open question — an inferred unresolved point captured during
/// synthesis (`"Who owns the API rollout?"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenQuestion {
    /// Underlying memory object.
    pub memory: MemoryObject,
    /// Surface text of the question.
    pub text: String,
    /// `Some(when)` once the question has been answered / resolved.
    pub resolved_at: Option<DateTime<Utc>>,
}

impl OpenQuestion {
    /// Construct a fresh `Useful`-class open question.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Useful),
            text: text.into(),
            resolved_at: None,
        }
    }

    /// Whether the question has been resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved_at.is_some()
    }
}

/// Active task — captured during synthesis (`"@Sara draft the RFC"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveTask {
    /// Underlying memory object.
    pub memory: MemoryObject,
    /// Surface text of the task.
    pub text: String,
    /// Optional assignee (free-form, typically an `@mention`).
    pub assignee: Option<String>,
    /// `Some(when)` once the task has been completed.
    pub completed_at: Option<DateTime<Utc>>,
}

impl ActiveTask {
    /// Construct a fresh `Useful`-class task.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Useful),
            text: text.into(),
            assignee: None,
            completed_at: None,
        }
    }

    /// Set the assignee.
    pub fn with_assignee(mut self, assignee: impl Into<String>) -> Self {
        self.assignee = Some(assignee.into());
        self
    }

    /// Whether the task has been completed.
    pub fn is_complete(&self) -> bool {
        self.completed_at.is_some()
    }
}

/// Channel memory object — the per-channel synthesis-output home.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelMemoryObject {
    /// Unique id (UUID v4).
    pub id: Uuid,
    /// Scope this channel memory belongs to.
    pub scope_id: ScopeId,
    /// Latest channel-recap text. Replaced on every synthesis window.
    pub recap: String,
    /// Decisions captured in this channel.
    pub decisions: Vec<Decision>,
    /// Open questions captured in this channel.
    pub open_questions: Vec<OpenQuestion>,
    /// Active tasks captured in this channel.
    pub active_tasks: Vec<ActiveTask>,
    /// Window id of the synthesis run that produced the current recap.
    /// `None` until the first synthesis.
    pub last_synthesis_window: Option<Uuid>,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock last update time.
    pub updated_at: DateTime<Utc>,
}

/// Counters returned by [`ChannelMemoryObject::decay_sweep`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelDecayReport {
    /// Number of completed tasks archived in this sweep.
    pub tasks_archived: usize,
    /// Number of resolved questions archived in this sweep.
    pub questions_archived: usize,
}

impl ChannelMemoryObject {
    /// Construct a fresh empty channel memory for `scope_id`.
    pub fn new(scope_id: ScopeId) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            scope_id,
            recap: String::new(),
            decisions: Vec::new(),
            open_questions: Vec::new(),
            active_tasks: Vec::new(),
            last_synthesis_window: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Replace the recap text with the latest synthesizer output.
    pub fn update_recap(&mut self, recap: impl Into<String>, synthesis_window: Option<Uuid>) {
        self.recap = recap.into();
        self.last_synthesis_window = synthesis_window;
        self.updated_at = Utc::now();
    }

    /// Append a decision and stamp `updated_at`. Returns the
    /// decision's memory id for callbacks.
    ///
    /// Unconditional append — for the synthesis-driven path that
    /// must not accumulate duplicates across windows, use
    /// [`Self::add_decision_dedup`] instead.
    pub fn add_decision(&mut self, decision: Decision) -> Uuid {
        let id = decision.memory.id;
        self.decisions.push(decision);
        self.updated_at = Utc::now();
        id
    }

    /// Append a task and stamp `updated_at`. Returns the task's
    /// memory id.
    ///
    /// Unconditional append — for the synthesis-driven path see
    /// [`Self::add_task_dedup`].
    pub fn add_task(&mut self, task: ActiveTask) -> Uuid {
        let id = task.memory.id;
        self.active_tasks.push(task);
        self.updated_at = Utc::now();
        id
    }

    /// Append an open question and stamp `updated_at`. Returns the
    /// question's memory id.
    ///
    /// Unconditional append — for the synthesis-driven path see
    /// [`Self::add_open_question_dedup`].
    pub fn add_open_question(&mut self, question: OpenQuestion) -> Uuid {
        let id = question.memory.id;
        self.open_questions.push(question);
        self.updated_at = Utc::now();
        id
    }

    /// Idempotent variant of [`Self::add_decision`] for the
    /// synthesis pipeline.
    ///
    /// Repeated `trigger_synthesis` calls feed overlapping evidence
    /// windows to the SLM, which re-emits the same decisions on
    /// every run. A naive `add_decision` would accumulate
    /// duplicates with each run. This variant compares the surface
    /// `text` against existing decisions and skips the insert if a
    /// case-insensitive whitespace-normalised match is found —
    /// preserving the original decision's memory state, lifecycle,
    /// and decay timing.
    ///
    /// Returns `Some(memory_id)` when the decision was newly
    /// inserted, `None` if it was deduplicated against an existing
    /// entry.
    pub fn add_decision_dedup(&mut self, decision: Decision) -> Option<Uuid> {
        let key = normalise_surface_text(&decision.text);
        if self
            .decisions
            .iter()
            .any(|d| normalise_surface_text(&d.text) == key)
        {
            return None;
        }
        Some(self.add_decision(decision))
    }

    /// Idempotent variant of [`Self::add_task`] for synthesis.
    /// Dedupes by `(text, assignee)` after surface normalisation so
    /// "@Sara draft the RFC" and "Draft the RFC" assigned to "@Sara"
    /// collapse to one row.
    ///
    /// # Dedup scope: the live `active_tasks` list only
    ///
    /// Dedup compares against the in-memory `active_tasks` vector,
    /// which holds **both** pending and completed-but-not-yet-archived
    /// tasks. [`Self::complete_task`] only flips `completed_at` — it
    /// does not remove the entry — so for the
    /// [`DEFAULT_COMPLETED_TASK_TTL_DAYS`] window (30 days by default)
    /// a completed task is still visible to dedup. Within that window,
    /// re-emitting the same `(text, assignee)` from a later synthesis
    /// run returns `None` and the completion stands; an explicit
    /// lifecycle event won't be undone by a stochastic SLM that
    /// happens to surface the same task text again.
    ///
    /// **After archival** (the next [`Self::decay_sweep`] past the
    /// TTL drops the entry from `active_tasks`), the same `(text,
    /// assignee)` may be re-added as a fresh `Candidate`. This is
    /// intentional: a year-old "draft the RFC" item that completed
    /// and aged out is, semantically, a different task from a new
    /// one the SLM surfaces today.
    ///
    /// Returns `Some(memory_id)` when the task was newly inserted,
    /// `None` when deduplicated against an existing
    /// (pending-or-completed-within-TTL) entry.
    pub fn add_task_dedup(&mut self, task: ActiveTask) -> Option<Uuid> {
        let key = normalise_surface_text(&task.text);
        let assignee_key = task
            .assignee
            .as_deref()
            .map(normalise_surface_text)
            .unwrap_or_default();
        if self.active_tasks.iter().any(|t| {
            normalise_surface_text(&t.text) == key
                && t.assignee
                    .as_deref()
                    .map(normalise_surface_text)
                    .unwrap_or_default()
                    == assignee_key
        }) {
            return None;
        }
        Some(self.add_task(task))
    }

    /// Idempotent variant of [`Self::add_open_question`] for
    /// synthesis. Dedupes by surface text. Already-resolved
    /// questions are *not* re-added: resolving is an explicit
    /// lifecycle event and re-adding would reopen the question.
    pub fn add_open_question_dedup(&mut self, question: OpenQuestion) -> Option<Uuid> {
        let key = normalise_surface_text(&question.text);
        if self
            .open_questions
            .iter()
            .any(|q| normalise_surface_text(&q.text) == key)
        {
            return None;
        }
        Some(self.add_open_question(question))
    }

    /// Mark `question_id` as resolved.
    pub fn resolve_question(&mut self, question_id: Uuid) -> Result<()> {
        let q = self
            .open_questions
            .iter_mut()
            .find(|q| q.memory.id == question_id)
            .ok_or(MemoryError::NotFound(question_id))?;
        q.resolved_at = Some(Utc::now());
        q.memory.last_accessed_at = Utc::now();
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Mark `task_id` as complete.
    pub fn complete_task(&mut self, task_id: Uuid) -> Result<()> {
        let t = self
            .active_tasks
            .iter_mut()
            .find(|t| t.memory.id == task_id)
            .ok_or(MemoryError::NotFound(task_id))?;
        t.completed_at = Some(Utc::now());
        t.memory.last_accessed_at = Utc::now();
        self.updated_at = Utc::now();
        Ok(())
    }

    /// All tasks that are not yet completed.
    pub fn list_active_tasks(&self) -> Vec<&ActiveTask> {
        self.active_tasks
            .iter()
            .filter(|t| !t.is_complete())
            .collect()
    }

    /// All questions that have not yet been resolved.
    pub fn list_open_questions(&self) -> Vec<&OpenQuestion> {
        self.open_questions
            .iter()
            .filter(|q| !q.is_resolved())
            .collect()
    }

    /// Archive completed tasks and resolved questions whose
    /// resolution / completion is older than the per-class TTL.
    /// Mirrors [`crate::decay::decay_sweep`] for the channel memory's
    /// short-lived items. Returns counters describing the sweep.
    pub fn decay_sweep(&mut self, now: DateTime<Utc>) -> ChannelDecayReport {
        self.decay_sweep_with(now,
            Duration::days(DEFAULT_COMPLETED_TASK_TTL_DAYS),
            Duration::days(DEFAULT_RESOLVED_QUESTION_TTL_DAYS),
        )
    }

    /// Lower-level [`Self::decay_sweep`] exposing the per-class TTLs.
    pub fn decay_sweep_with(&mut self,
        now: DateTime<Utc>,
        completed_task_ttl: Duration,
        resolved_question_ttl: Duration,
    ) -> ChannelDecayReport {
        let sm = MemoryStateMachine::new();
        let mut report = ChannelDecayReport::default();

        // Archive completed tasks past the TTL: drop them out of the
        // active list (they're preserved on the underlying
        // [`MemoryObject`] whose state we mark Archived).
        let mut keep_tasks = Vec::with_capacity(self.active_tasks.len());
        for mut t in std::mem::take(&mut self.active_tasks) {
            let archive = matches!(t.completed_at, Some(at) if (now - at) >= completed_task_ttl)
                && t.memory.state != MemoryState::Archived;
            if archive {
                if t.memory.state == MemoryState::Candidate {
                    let _ = sm.archive_candidate(&mut t.memory);
                } else {
                    t.memory.state = MemoryState::Archived;
                }
                report.tasks_archived = report.tasks_archived.saturating_add(1);
            } else {
                keep_tasks.push(t);
            }
        }
        self.active_tasks = keep_tasks;

        let mut keep_questions = Vec::with_capacity(self.open_questions.len());
        for mut q in std::mem::take(&mut self.open_questions) {
            let archive = matches!(q.resolved_at, Some(at) if (now - at) >= resolved_question_ttl)
                && q.memory.state != MemoryState::Archived;
            if archive {
                if q.memory.state == MemoryState::Candidate {
                    let _ = sm.archive_candidate(&mut q.memory);
                } else {
                    q.memory.state = MemoryState::Archived;
                }
                report.questions_archived = report.questions_archived.saturating_add(1);
            } else {
                keep_questions.push(q);
            }
        }
        self.open_questions = keep_questions;

        if report.tasks_archived > 0 || report.questions_archived > 0 {
            self.updated_at = now;
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_complete_task() {
        let scope = ScopeId::new_v4();
        let mut chan = ChannelMemoryObject::new(scope);
        let id = chan.add_task(ActiveTask::new(scope, "draft RFC"));
        assert_eq!(chan.list_active_tasks().len(), 1);
        chan.complete_task(id).unwrap();
        assert!(chan.list_active_tasks().is_empty());
    }

    #[test]
    fn resolve_unknown_question_errors() {
        let scope = ScopeId::new_v4();
        let mut chan = ChannelMemoryObject::new(scope);
        let err = chan.resolve_question(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }

    /// Pins the dedup contract for the synthesis pipeline: even
    /// if the SLM re-emits the same decision text on every run
    /// (case- and whitespace-insensitive), the channel memory
    /// records it exactly once. Regression test for the
    /// `synthesize_scope` duplicate-accumulation bug.
    #[test]
    fn add_decision_dedup_collapses_repeated_synthesis_runs() {
        let scope = ScopeId::new_v4();
        let mut chan = ChannelMemoryObject::new(scope);

        let first = chan.add_decision_dedup(Decision::new(scope, "Approved policy v3"));
        assert!(first.is_some(), "first insert must succeed");

        // Whitespace-normalised + case-insensitive match must be
        // treated as the same decision.
        let dup = chan.add_decision_dedup(Decision::new(scope, "  approved   POLICY v3  "));
        assert!(dup.is_none(), "second run must dedup");

        let new = chan.add_decision_dedup(Decision::new(scope, "Approved policy v4"));
        assert!(new.is_some(), "genuinely new decision must insert");

        assert_eq!(chan.decisions.len(), 2);
    }

    #[test]
    fn add_task_dedup_keys_on_text_and_assignee() {
        let scope = ScopeId::new_v4();
        let mut chan = ChannelMemoryObject::new(scope);

        let a = ActiveTask::new(scope, "Draft RFC").with_assignee("@sara");
        let b = ActiveTask::new(scope, " draft  rfc ").with_assignee("@SARA");
        let c = ActiveTask::new(scope, "Draft RFC").with_assignee("@bob");

        assert!(chan.add_task_dedup(a).is_some());
        assert!(chan.add_task_dedup(b).is_none(),
            "case/whitespace normalised match must dedup"
        );
        assert!(chan.add_task_dedup(c).is_some(),
            "different assignee is a different task"
        );
        assert_eq!(chan.active_tasks.len(), 2);
    }

    #[test]
    fn add_open_question_dedup_preserves_resolution_state() {
        let scope = ScopeId::new_v4();
        let mut chan = ChannelMemoryObject::new(scope);

        let id = chan
            .add_open_question_dedup(OpenQuestion::new(scope, "Who owns the API rollout?"))
            .expect("first insert");
        chan.resolve_question(id).unwrap();
        assert!(chan.open_questions[0].is_resolved());

        // SLM re-emits the same question on the next synthesis
        // window. Dedup must NOT reopen the resolved question by
        // inserting a fresh `OpenQuestion` row.
        let dup =
            chan.add_open_question_dedup(OpenQuestion::new(scope, "who   owns the api rollout?"));
        assert!(dup.is_none());
        assert_eq!(chan.open_questions.len(), 1);
        assert!(chan.open_questions[0].is_resolved());
    }
}
