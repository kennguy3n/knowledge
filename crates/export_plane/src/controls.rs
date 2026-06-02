//! Per-concept / per-summary / per-workflow export controls.
//!
//! Per `docs/DESIGN.md` §3.5, each substrate object that is *eligible*
//! for export carries an explicit, opt-in control row in this
//! registry. The registry enforces deny-by-default: a concept,
//! summary, or workflow that has no entry in the registry is *not*
//! exportable, regardless of how generous the export policy is.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use evidence_store::ScopeId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Per-concept export control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptExportControl {
    /// Substrate concept id.
    pub concept_id: Uuid,
    /// Master switch.
    pub exportable: bool,
    /// Whitelist of profile ids this concept may appear in. Empty
    /// means "any profile".
    pub allowed_profiles: Vec<Uuid>,
    /// Optional time-bound. After `created_at + time_bound` the
    /// control no longer authorises export.
    pub time_bound: Option<Duration>,
    /// Optional scope-bound. Empty means "any scope".
    pub scope_bound: Vec<Uuid>,
    /// Wall-clock creation time, used to evaluate `time_bound`.
    pub created_at: DateTime<Utc>,
}

impl ConceptExportControl {
    /// Construct a fresh control row, defaulted to `exportable = true`,
    /// no whitelist / scope-bound, no time-bound.
    pub fn new(concept_id: Uuid) -> Self {
        Self {
            concept_id,
            exportable: true,
            allowed_profiles: Vec::new(),
            time_bound: None,
            scope_bound: Vec::new(),
            created_at: Utc::now(),
        }
    }

    /// Whether this control still authorises export at `now`.
    ///
    /// When [`time_bound`] is set the control is active for
    /// `[created_at, created_at + time_bound)` — i.e. the boundary
    /// instant `created_at + time_bound` is **inactive** (the
    /// half-open interval). This matches
    /// [`crate::profile::ApprovedConcept::is_expired`], which treats
    /// `now == expires_at` as expired (boundary inclusive on the
    /// "no longer valid" side). Both `is_expired` / `is_active`
    /// therefore agree: the boundary instant is in the
    /// no-longer-valid bucket.
    ///
    /// [`time_bound`]: ConceptExportControl::time_bound
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        if !self.exportable {
            return false;
        }
        let Some(bound) = self.time_bound else {
            return true;
        };
        let Ok(elapsed) = now.signed_duration_since(self.created_at).to_std() else {
            return true;
        };
        elapsed < bound
    }

    /// Whether `profile_id` is permitted by this control's
    /// `allowed_profiles` whitelist (empty == any profile allowed).
    pub fn allows_profile(&self, profile_id: Uuid) -> bool {
        self.allowed_profiles.is_empty() || self.allowed_profiles.contains(&profile_id)
    }

    /// Whether `scope_id` is permitted by this control's
    /// `scope_bound` whitelist (empty == any scope allowed).
    pub fn allows_scope(&self, scope_id: Uuid) -> bool {
        self.scope_bound.is_empty() || self.scope_bound.contains(&scope_id)
    }
}

/// Redaction level for a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionLevel {
    /// Surface the full summary text.
    None,
    /// Surface a partially-redacted summary (PII / direct quotes
    /// stripped). The substrate-level redactor consumes this hint.
    Partial,
    /// Surface only metadata (id + tag) — no summary body at all.
    Full,
}

/// Per-summary export control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryExportControl {
    /// Substrate summary id.
    pub summary_id: Uuid,
    /// Substrate scope this summary lives in. The simulator filters
    /// summaries whose `scope_id` does not match the parent
    /// [`crate::profile::PortableConceptProfile::scope_id`] so a
    /// profile rooted in scope A cannot silently surface summaries
    /// from scope B. The field is mandatory for the same reason
    /// [`ConceptExportControl::scope_bound`] is consulted on the
    /// concept side: deny-by-default cross-scope leakage.
    pub scope_id: ScopeId,
    /// Master switch.
    pub exportable: bool,
    /// Required redaction level for the summary body.
    pub redaction_level: RedactionLevel,
}

impl SummaryExportControl {
    /// Construct a fresh control row.
    pub fn new(summary_id: Uuid, scope_id: ScopeId, redaction_level: RedactionLevel) -> Self {
        Self {
            summary_id,
            scope_id,
            exportable: true,
            redaction_level,
        }
    }
}

/// Per-workflow export control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowExportControl {
    /// Substrate workflow id.
    pub workflow_id: Uuid,
    /// Master switch.
    pub exportable: bool,
    /// Whitelist of agent ids that may consume this workflow when
    /// exported. Empty means "any agent".
    pub allowed_agents: Vec<Uuid>,
}

impl WorkflowExportControl {
    /// Construct a fresh control row.
    pub fn new(workflow_id: Uuid) -> Self {
        Self {
            workflow_id,
            exportable: true,
            allowed_agents: Vec::new(),
        }
    }

    /// Whether `agent_id` is permitted by this control's
    /// `allowed_agents` whitelist (empty == any agent allowed).
    pub fn allows_agent(&self, agent_id: Uuid) -> bool {
        self.allowed_agents.is_empty() || self.allowed_agents.contains(&agent_id)
    }
}

/// Errors raised by the [`ExportControlRegistry`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExportControlError {
    /// Tried to remove a control that does not exist.
    #[error("control not found: {0}")]
    NotFound(Uuid),
    /// Tried to insert a duplicate control.
    #[error("control already registered: {0}")]
    Duplicate(Uuid),
}

/// In-memory CRUD store for the three control types.
#[derive(Debug, Default, Clone)]
pub struct ExportControlRegistry {
    concepts: HashMap<Uuid, ConceptExportControl>,
    summaries: HashMap<Uuid, SummaryExportControl>,
    workflows: HashMap<Uuid, WorkflowExportControl>,
}

impl ExportControlRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    // ----- concepts -----

    /// Insert a new concept control. Errors on duplicate id.
    pub fn insert_concept(
        &mut self,
        control: ConceptExportControl,
    ) -> Result<(), ExportControlError> {
        if self.concepts.contains_key(&control.concept_id) {
            return Err(ExportControlError::Duplicate(control.concept_id));
        }
        self.concepts.insert(control.concept_id, control);
        Ok(())
    }

    /// Insert or overwrite a concept control.
    pub fn upsert_concept(&mut self, control: ConceptExportControl) {
        self.concepts.insert(control.concept_id, control);
    }

    /// Borrow a concept control.
    pub fn get_concept(&self, concept_id: Uuid) -> Option<&ConceptExportControl> {
        self.concepts.get(&concept_id)
    }

    /// Remove a concept control.
    pub fn remove_concept(&mut self, concept_id: Uuid) -> Result<(), ExportControlError> {
        self.concepts
            .remove(&concept_id)
            .map(|_| ())
            .ok_or(ExportControlError::NotFound(concept_id))
    }

    /// Iterate over every concept control.
    pub fn concepts(&self) -> impl Iterator<Item = &ConceptExportControl> {
        self.concepts.values()
    }

    /// **Deny-by-default** check: returns `true` iff there is an
    /// active concept control for `concept_id` whose `profile_id` /
    /// `scope_id` filters allow it.
    pub fn allows_concept(
        &self,
        concept_id: Uuid,
        profile_id: Uuid,
        scope_id: Uuid,
        now: DateTime<Utc>,
    ) -> bool {
        let Some(control) = self.concepts.get(&concept_id) else {
            return false;
        };
        if !control.is_active(now) {
            return false;
        }
        if !control.allows_profile(profile_id) {
            return false;
        }
        if !control.allows_scope(scope_id) {
            return false;
        }
        true
    }

    // ----- summaries -----

    /// Insert a summary control. Errors on duplicate.
    pub fn insert_summary(
        &mut self,
        control: SummaryExportControl,
    ) -> Result<(), ExportControlError> {
        if self.summaries.contains_key(&control.summary_id) {
            return Err(ExportControlError::Duplicate(control.summary_id));
        }
        self.summaries.insert(control.summary_id, control);
        Ok(())
    }

    /// Insert or overwrite a summary control.
    pub fn upsert_summary(&mut self, control: SummaryExportControl) {
        self.summaries.insert(control.summary_id, control);
    }

    /// Borrow a summary control.
    pub fn get_summary(&self, summary_id: Uuid) -> Option<&SummaryExportControl> {
        self.summaries.get(&summary_id)
    }

    /// Remove a summary control.
    pub fn remove_summary(&mut self, summary_id: Uuid) -> Result<(), ExportControlError> {
        self.summaries
            .remove(&summary_id)
            .map(|_| ())
            .ok_or(ExportControlError::NotFound(summary_id))
    }

    /// Iterate over every summary control.
    pub fn summaries(&self) -> impl Iterator<Item = &SummaryExportControl> {
        self.summaries.values()
    }

    /// Deny-by-default check for summaries.
    pub fn allows_summary(&self, summary_id: Uuid) -> bool {
        self.summaries
            .get(&summary_id)
            .is_some_and(|c| c.exportable)
    }

    // ----- workflows -----

    /// Insert a workflow control. Errors on duplicate.
    pub fn insert_workflow(
        &mut self,
        control: WorkflowExportControl,
    ) -> Result<(), ExportControlError> {
        if self.workflows.contains_key(&control.workflow_id) {
            return Err(ExportControlError::Duplicate(control.workflow_id));
        }
        self.workflows.insert(control.workflow_id, control);
        Ok(())
    }

    /// Insert or overwrite a workflow control.
    pub fn upsert_workflow(&mut self, control: WorkflowExportControl) {
        self.workflows.insert(control.workflow_id, control);
    }

    /// Borrow a workflow control.
    pub fn get_workflow(&self, workflow_id: Uuid) -> Option<&WorkflowExportControl> {
        self.workflows.get(&workflow_id)
    }

    /// Remove a workflow control.
    pub fn remove_workflow(&mut self, workflow_id: Uuid) -> Result<(), ExportControlError> {
        self.workflows
            .remove(&workflow_id)
            .map(|_| ())
            .ok_or(ExportControlError::NotFound(workflow_id))
    }

    /// Iterate over every workflow control.
    pub fn workflows(&self) -> impl Iterator<Item = &WorkflowExportControl> {
        self.workflows.values()
    }

    /// Deny-by-default check for workflows.
    pub fn allows_workflow(&self, workflow_id: Uuid, agent_id: Uuid) -> bool {
        let Some(control) = self.workflows.get(&workflow_id) else {
            return false;
        };
        control.exportable && control.allows_agent(agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_by_default_for_unregistered_concept() {
        let r = ExportControlRegistry::new();
        assert!(!r.allows_concept(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Utc::now()));
    }

    #[test]
    fn registered_concept_allowed() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        r.insert_concept(ConceptExportControl::new(id)).expect("ok");
        assert!(r.allows_concept(id, Uuid::new_v4(), Uuid::new_v4(), Utc::now()));
    }

    #[test]
    fn non_exportable_concept_blocked() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        let mut control = ConceptExportControl::new(id);
        control.exportable = false;
        r.insert_concept(control).expect("ok");
        assert!(!r.allows_concept(id, Uuid::new_v4(), Uuid::new_v4(), Utc::now()));
    }

    #[test]
    fn duplicate_insert_rejected() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        r.insert_concept(ConceptExportControl::new(id)).expect("ok");
        assert_eq!(
            r.insert_concept(ConceptExportControl::new(id)),
            Err(ExportControlError::Duplicate(id))
        );
    }

    #[test]
    fn upsert_overwrites() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        r.insert_concept(ConceptExportControl::new(id)).expect("ok");
        let mut updated = ConceptExportControl::new(id);
        updated.exportable = false;
        r.upsert_concept(updated);
        assert!(!r.allows_concept(id, Uuid::new_v4(), Uuid::new_v4(), Utc::now()));
    }

    #[test]
    fn time_bound_expires_concept() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        let mut control = ConceptExportControl::new(id);
        control.created_at = Utc::now() - chrono::Duration::seconds(120);
        control.time_bound = Some(Duration::from_secs(60));
        r.insert_concept(control).expect("ok");
        assert!(!r.allows_concept(id, Uuid::new_v4(), Uuid::new_v4(), Utc::now()));
    }

    #[test]
    fn allowed_profiles_filter() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        let allowed_profile = Uuid::new_v4();
        let mut control = ConceptExportControl::new(id);
        control.allowed_profiles = vec![allowed_profile];
        r.insert_concept(control).expect("ok");
        assert!(r.allows_concept(id, allowed_profile, Uuid::new_v4(), Utc::now()));
        assert!(!r.allows_concept(id, Uuid::new_v4(), Uuid::new_v4(), Utc::now()));
    }

    #[test]
    fn scope_bound_filter() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        let allowed_scope = Uuid::new_v4();
        let mut control = ConceptExportControl::new(id);
        control.scope_bound = vec![allowed_scope];
        r.insert_concept(control).expect("ok");
        assert!(r.allows_concept(id, Uuid::new_v4(), allowed_scope, Utc::now()));
        assert!(!r.allows_concept(id, Uuid::new_v4(), Uuid::new_v4(), Utc::now()));
    }

    #[test]
    fn summary_deny_by_default() {
        let r = ExportControlRegistry::new();
        assert!(!r.allows_summary(Uuid::new_v4()));
    }

    #[test]
    fn summary_registered_allowed() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        r.insert_summary(SummaryExportControl::new(
            id,
            ScopeId::new_v4(),
            RedactionLevel::None,
        ))
        .expect("ok");
        assert!(r.allows_summary(id));
    }

    #[test]
    fn summary_redaction_level_round_trip() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        r.insert_summary(SummaryExportControl::new(
            id,
            ScopeId::new_v4(),
            RedactionLevel::Partial,
        ))
        .expect("ok");
        assert_eq!(
            r.get_summary(id).unwrap().redaction_level,
            RedactionLevel::Partial
        );
    }

    #[test]
    fn workflow_deny_by_default() {
        let r = ExportControlRegistry::new();
        assert!(!r.allows_workflow(Uuid::new_v4(), Uuid::new_v4()));
    }

    #[test]
    fn workflow_allowed_agents_filter() {
        let mut r = ExportControlRegistry::new();
        let id = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let mut control = WorkflowExportControl::new(id);
        control.allowed_agents = vec![agent];
        r.insert_workflow(control).expect("ok");
        assert!(r.allows_workflow(id, agent));
        assert!(!r.allows_workflow(id, Uuid::new_v4()));
    }

    #[test]
    fn remove_returns_not_found_for_missing() {
        let mut r = ExportControlRegistry::new();
        assert!(matches!(
            r.remove_concept(Uuid::new_v4()),
            Err(ExportControlError::NotFound(_))
        ));
    }

    #[test]
    fn iter_visits_all_controls() {
        let mut r = ExportControlRegistry::new();
        for _ in 0..3 {
            r.insert_concept(ConceptExportControl::new(Uuid::new_v4()))
                .expect("ok");
        }
        assert_eq!(r.concepts().count(), 3);
    }
}
