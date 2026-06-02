//! Domain Memory Object — the per-domain synthesis-output home.
//!
//! Per `docs/DESIGN.md` §6.2: the Domain Memory
//! Object captures **cross-channel workstreams, dependencies, risks,
//! procedures** for a logical work area within a B2B tenant. It is
//! the second tier of the synthesis hierarchy:
//!
//! 1. Channel synthesis consumes raw messages → [`ChannelMemoryObject`].
//! 2. **Domain synthesis consumes channel outputs** → [`DomainMemoryObject`].
//! 3. Tenant synthesis consumes domain outputs + approved official
//!    docs → [`crate::tenant_memory::TenantMemoryObject`].
//!
//! Like the channel memory object, the domain memory object reuses
//! [`MemoryObject`] for its individual items so that the same decay
//! state machine and retention scoring drive the lifecycle of
//! workstreams, dependencies, risks, and procedures. The
//! [`Self::decay_sweep`] method archives completed workstreams /
//! resolved risks once a per-class TTL has elapsed; long-lived
//! procedures and dependencies sit at higher sensitivity classes
//! and are archived under the supersession path instead.
//!
//! [`ChannelMemoryObject`]: crate::channel_memory::ChannelMemoryObject

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::error::{MemoryError, Result};
use crate::object::{MemoryObject, SensitivityClass};
use crate::state::MemoryState;
use crate::transitions::MemoryStateMachine;

/// Default TTL after which a *completed* workstream is archived from
/// the domain memory's `workstreams` list.
pub const DEFAULT_COMPLETED_WORKSTREAM_TTL_DAYS: i64 = 60;

/// Default TTL after which a *resolved* risk is archived.
pub const DEFAULT_RESOLVED_RISK_TTL_DAYS: i64 = 60;

/// Cross-channel workstream — a tracked unit of work that spans
/// multiple channels (e.g. `"Q3 launch readiness"`). Workstreams sit
/// at `Important` sensitivity by default; once a human / policy
/// promotes them to canonical they move under the supersession path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workstream {
    /// Underlying memory object.
    pub memory: MemoryObject,
    /// Surface text describing the workstream.
    pub text: String,
    /// Optional owner reference (free-form, typically an `@mention`).
    pub owner: Option<String>,
    /// `Some(when)` once the workstream has been completed.
    pub completed_at: Option<DateTime<Utc>>,
}

impl Workstream {
    /// Construct a fresh `Important`-class workstream.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Important),
            text: text.into(),
            owner: None,
            completed_at: None,
        }
    }

    /// Set the owner.
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    /// Whether the workstream has been completed.
    pub fn is_complete(&self) -> bool {
        self.completed_at.is_some()
    }
}

/// Cross-channel dependency — a directed `from -> on` reference where
/// `on` blocks `from`. Dependencies sit at `Important` sensitivity
/// because they encode the structural shape of the work, not transient
/// chatter; supersession is preferred over deletion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    /// Underlying memory object.
    pub memory: MemoryObject,
    /// Surface text describing the dependency
    /// (e.g. `"API contract → SDK release"`).
    pub text: String,
    /// `Some(when)` once the dependency has been resolved.
    pub resolved_at: Option<DateTime<Utc>>,
}

impl Dependency {
    /// Construct a fresh `Important`-class dependency.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Important),
            text: text.into(),
            resolved_at: None,
        }
    }

    /// Whether the dependency has been resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved_at.is_some()
    }
}

/// Risk — a tracked threat to a workstream
/// (e.g. `"vendor outage on Region A"`). Risks default to `Important`
/// sensitivity; on resolution they archive after the per-class TTL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Risk {
    /// Underlying memory object.
    pub memory: MemoryObject,
    /// Surface text describing the risk.
    pub text: String,
    /// `Some(when)` once the risk has been resolved (closed / mitigated).
    pub resolved_at: Option<DateTime<Utc>>,
}

impl Risk {
    /// Construct a fresh `Important`-class risk.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Important),
            text: text.into(),
            resolved_at: None,
        }
    }

    /// Whether the risk has been resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved_at.is_some()
    }
}

/// Procedure — a stable cross-channel "how we do X" pattern
/// (e.g. `"deploy on green CI + sign-off from on-call"`). Procedures
/// are durable, so they default to `Critical` sensitivity (no passive
/// decay; only explicit deprecation), mirroring tenant memory's
/// retention rule for canonical policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Procedure {
    /// Underlying memory object.
    pub memory: MemoryObject,
    /// Surface text describing the procedure.
    pub text: String,
    /// `Some(when)` once the procedure has been deprecated. Procedures
    /// never decay passively — deprecation is the only path off the
    /// list.
    pub deprecated_at: Option<DateTime<Utc>>,
}

impl Procedure {
    /// Construct a fresh `Critical`-class procedure.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Critical),
            text: text.into(),
            deprecated_at: None,
        }
    }

    /// Whether the procedure has been deprecated.
    pub fn is_deprecated(&self) -> bool {
        self.deprecated_at.is_some()
    }
}

/// Domain memory object — the per-domain synthesis-output home.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainMemoryObject {
    /// Unique id (UUID v4).
    pub id: Uuid,
    /// Scope this domain memory belongs to.
    pub scope_id: ScopeId,
    /// Latest domain-level recap text. Replaced on every synthesis
    /// window.
    pub recap: String,
    /// Cross-channel workstreams tracked in this domain.
    pub workstreams: Vec<Workstream>,
    /// Cross-channel dependencies tracked in this domain.
    pub dependencies: Vec<Dependency>,
    /// Risks tracked in this domain.
    pub risks: Vec<Risk>,
    /// Stable cross-channel procedures.
    pub procedures: Vec<Procedure>,
    /// Channel scopes whose synthesis outputs feed this domain
    /// memory. Domain synthesis consumes [`ChannelMemoryObject`]
    /// outputs; this field records the channels in scope.
    ///
    /// [`ChannelMemoryObject`]: crate::channel_memory::ChannelMemoryObject
    pub channel_scopes: Vec<ScopeId>,
    /// Window id of the synthesis run that produced the current recap.
    /// `None` until the first synthesis.
    pub last_synthesis_window: Option<Uuid>,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock last update time.
    pub updated_at: DateTime<Utc>,
}

/// Counters returned by [`DomainMemoryObject::decay_sweep`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainDecayReport {
    /// Number of completed workstreams archived in this sweep.
    pub workstreams_archived: usize,
    /// Number of resolved risks archived in this sweep.
    pub risks_archived: usize,
    /// Number of resolved dependencies archived in this sweep.
    pub dependencies_archived: usize,
}

impl DomainMemoryObject {
    /// Construct a fresh empty domain memory for `scope_id`.
    pub fn new(scope_id: ScopeId) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            scope_id,
            recap: String::new(),
            workstreams: Vec::new(),
            dependencies: Vec::new(),
            risks: Vec::new(),
            procedures: Vec::new(),
            channel_scopes: Vec::new(),
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

    /// Register a channel scope that feeds into this domain. The
    /// substrate's hierarchy rule (`docs/DESIGN.md` §6.3) is that domain
    /// synthesis only consumes channel outputs, so the registered
    /// channels enumerate the legal sources.
    pub fn attach_channel_scope(&mut self, channel: ScopeId) {
        if !self.channel_scopes.contains(&channel) {
            self.channel_scopes.push(channel);
            self.updated_at = Utc::now();
        }
    }

    /// Append a workstream. Returns the underlying memory id.
    pub fn add_workstream(&mut self, workstream: Workstream) -> Uuid {
        let id = workstream.memory.id;
        self.workstreams.push(workstream);
        self.updated_at = Utc::now();
        id
    }

    /// Append a dependency. Returns the underlying memory id.
    pub fn add_dependency(&mut self, dependency: Dependency) -> Uuid {
        let id = dependency.memory.id;
        self.dependencies.push(dependency);
        self.updated_at = Utc::now();
        id
    }

    /// Append a risk. Returns the underlying memory id.
    pub fn add_risk(&mut self, risk: Risk) -> Uuid {
        let id = risk.memory.id;
        self.risks.push(risk);
        self.updated_at = Utc::now();
        id
    }

    /// Append a procedure. Returns the underlying memory id.
    pub fn add_procedure(&mut self, procedure: Procedure) -> Uuid {
        let id = procedure.memory.id;
        self.procedures.push(procedure);
        self.updated_at = Utc::now();
        id
    }

    /// Mark `workstream_id` as complete.
    pub fn complete_workstream(&mut self, workstream_id: Uuid) -> Result<()> {
        let w = self
            .workstreams
            .iter_mut()
            .find(|w| w.memory.id == workstream_id)
            .ok_or(MemoryError::NotFound(workstream_id))?;
        w.completed_at = Some(Utc::now());
        w.memory.last_accessed_at = Utc::now();
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Mark `dependency_id` as resolved.
    pub fn resolve_dependency(&mut self, dependency_id: Uuid) -> Result<()> {
        let d = self
            .dependencies
            .iter_mut()
            .find(|d| d.memory.id == dependency_id)
            .ok_or(MemoryError::NotFound(dependency_id))?;
        d.resolved_at = Some(Utc::now());
        d.memory.last_accessed_at = Utc::now();
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Mark `risk_id` as resolved.
    pub fn resolve_risk(&mut self, risk_id: Uuid) -> Result<()> {
        let r = self
            .risks
            .iter_mut()
            .find(|r| r.memory.id == risk_id)
            .ok_or(MemoryError::NotFound(risk_id))?;
        r.resolved_at = Some(Utc::now());
        r.memory.last_accessed_at = Utc::now();
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Mark `procedure_id` as deprecated. Procedures default to
    /// `Critical` sensitivity and never decay passively; deprecation
    /// is the only path off the list (mirrors the tenant-memory
    /// "explicit deprecation only" rule).
    pub fn deprecate_procedure(&mut self, procedure_id: Uuid) -> Result<()> {
        let p = self
            .procedures
            .iter_mut()
            .find(|p| p.memory.id == procedure_id)
            .ok_or(MemoryError::NotFound(procedure_id))?;
        p.deprecated_at = Some(Utc::now());
        p.memory.last_accessed_at = Utc::now();
        self.updated_at = Utc::now();
        Ok(())
    }

    /// All workstreams that are not yet completed.
    pub fn list_active_workstreams(&self) -> Vec<&Workstream> {
        self.workstreams
            .iter()
            .filter(|w| !w.is_complete())
            .collect()
    }

    /// All dependencies that are not yet resolved.
    pub fn list_open_dependencies(&self) -> Vec<&Dependency> {
        self.dependencies
            .iter()
            .filter(|d| !d.is_resolved())
            .collect()
    }

    /// All risks that are not yet resolved.
    pub fn list_open_risks(&self) -> Vec<&Risk> {
        self.risks.iter().filter(|r| !r.is_resolved()).collect()
    }

    /// All procedures that have not been deprecated.
    pub fn list_active_procedures(&self) -> Vec<&Procedure> {
        self.procedures
            .iter()
            .filter(|p| !p.is_deprecated())
            .collect()
    }

    /// Archive completed workstreams, resolved dependencies, and
    /// resolved risks whose resolution / completion is older than the
    /// per-class TTL. Procedures are *never* archived by this sweep —
    /// they only leave the list via [`Self::deprecate_procedure`],
    /// matching the `Critical`-class "no passive decay" rule from
    /// `docs/DESIGN.md` §4.3.
    pub fn decay_sweep(&mut self, now: DateTime<Utc>) -> DomainDecayReport {
        self.decay_sweep_with(now,
            Duration::days(DEFAULT_COMPLETED_WORKSTREAM_TTL_DAYS),
            Duration::days(DEFAULT_RESOLVED_RISK_TTL_DAYS),
            Duration::days(DEFAULT_RESOLVED_RISK_TTL_DAYS),
        )
    }

    /// Lower-level [`Self::decay_sweep`] exposing the per-class TTLs.
    pub fn decay_sweep_with(&mut self,
        now: DateTime<Utc>,
        completed_workstream_ttl: Duration,
        resolved_risk_ttl: Duration,
        resolved_dependency_ttl: Duration,
    ) -> DomainDecayReport {
        let sm = MemoryStateMachine::new();
        let mut report = DomainDecayReport::default();

        let mut keep_workstreams = Vec::with_capacity(self.workstreams.len());
        for mut w in std::mem::take(&mut self.workstreams) {
            let archive = matches!(w.completed_at, Some(at) if (now - at) >= completed_workstream_ttl)
                && w.memory.state != MemoryState::Archived;
            if archive {
                if w.memory.state == MemoryState::Candidate {
                    let _ = sm.archive_candidate(&mut w.memory);
                } else {
                    w.memory.state = MemoryState::Archived;
                }
                report.workstreams_archived = report.workstreams_archived.saturating_add(1);
            } else {
                keep_workstreams.push(w);
            }
        }
        self.workstreams = keep_workstreams;

        let mut keep_risks = Vec::with_capacity(self.risks.len());
        for mut r in std::mem::take(&mut self.risks) {
            let archive = matches!(r.resolved_at, Some(at) if (now - at) >= resolved_risk_ttl)
                && r.memory.state != MemoryState::Archived;
            if archive {
                if r.memory.state == MemoryState::Candidate {
                    let _ = sm.archive_candidate(&mut r.memory);
                } else {
                    r.memory.state = MemoryState::Archived;
                }
                report.risks_archived = report.risks_archived.saturating_add(1);
            } else {
                keep_risks.push(r);
            }
        }
        self.risks = keep_risks;

        let mut keep_dependencies = Vec::with_capacity(self.dependencies.len());
        for mut d in std::mem::take(&mut self.dependencies) {
            let archive = matches!(d.resolved_at, Some(at) if (now - at) >= resolved_dependency_ttl)
                && d.memory.state != MemoryState::Archived;
            if archive {
                if d.memory.state == MemoryState::Candidate {
                    let _ = sm.archive_candidate(&mut d.memory);
                } else {
                    d.memory.state = MemoryState::Archived;
                }
                report.dependencies_archived = report.dependencies_archived.saturating_add(1);
            } else {
                keep_dependencies.push(d);
            }
        }
        self.dependencies = keep_dependencies;

        if report.workstreams_archived > 0
            || report.risks_archived > 0
            || report.dependencies_archived > 0
        {
            self.updated_at = now;
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_complete_workstream() {
        let scope = ScopeId::new_v4();
        let mut dom = DomainMemoryObject::new(scope);
        let id = dom.add_workstream(Workstream::new(scope, "Q3 launch readiness"));
        assert_eq!(dom.list_active_workstreams().len(), 1);
        dom.complete_workstream(id).unwrap();
        assert!(dom.list_active_workstreams().is_empty());
    }

    #[test]
    fn procedure_defaults_to_critical_sensitivity() {
        let scope = ScopeId::new_v4();
        let proc_ = Procedure::new(scope, "deploy on green CI");
        assert_eq!(proc_.memory.sensitivity_class, SensitivityClass::Critical);
    }

    #[test]
    fn unknown_workstream_returns_not_found() {
        let scope = ScopeId::new_v4();
        let mut dom = DomainMemoryObject::new(scope);
        let err = dom.complete_workstream(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }
}
