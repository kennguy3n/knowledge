//! [`MemoryObject`] — the unit of decay-managed memory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::{EvidenceId, ScopeId};

use crate::state::MemoryState;

/// Sensitivity / criticality class from `PROPOSAL.md` §4.3 — drives
/// the per-object decay schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SensitivityClass {
    /// Tenant policy, regulatory rules, signed decisions. No passive
    /// decay; only explicit deprecation.
    Critical,
    /// Owners, project commitments, canonical concepts. Slow decay;
    /// supersession preferred.
    Important,
    /// Recurring tasks, channel recaps, workflows. Medium decay;
    /// archived if non-used.
    Useful,
    /// Greetings, social chatter, transient pings. Stays only in the
    /// raw evidence plane (ring buffer); never promoted.
    Noise,
}

impl SensitivityClass {
    /// Stable string tag used for serialisation / debugging.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Important => "important",
            Self::Useful => "useful",
            Self::Noise => "noise",
        }
    }
}

/// One memory object — a candidate / reinforced / consolidated /
/// canonical / superseded / archived / deleted observation living in
/// some scope.
///
/// Per `PROPOSAL.md` §4 and `ARCHITECTURE.md` §7. The fields are kept
/// deliberately small so `MemoryObject` is cheap to clone in tests and
/// in the in-memory [`crate::user_memory::UserMemoryObject`] CRUD layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryObject {
    /// Unique id (UUID v4).
    pub id: Uuid,
    /// Scope (channel / domain / tenant / personal).
    pub scope_id: ScopeId,
    /// Current state in the decay state machine.
    pub state: MemoryState,
    /// Sensitivity / criticality class — drives decay rate.
    pub sensitivity_class: SensitivityClass,
    /// Last computed retention score, in `0.0 ..= 1.0`. See
    /// [`crate::retention::compute_retention_score`].
    pub retention_score: f64,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock last access (read / pin / corroboration).
    pub last_accessed_at: DateTime<Utc>,
    /// Number of times this object has been retrieved as part of an
    /// answered query.
    pub retrieval_count: u32,
    /// Number of pins (user / admin). Pins are the strongest
    /// retention signal (`PROPOSAL.md` §4.2).
    pub pin_count: u32,
    /// Number of independent evidence sources backing the same
    /// observation (cross-source corroboration).
    pub corroboration_count: u32,
    /// Evidence rows this object was derived from. Used for
    /// provenance and supersession tracking.
    pub source_refs: Vec<EvidenceId>,
    /// If this object has been superseded by another, the id of the
    /// newer canonical claim.
    pub superseded_by: Option<Uuid>,
    /// Free-form metadata attached to the object — e.g. observation
    /// type, surface label, debug context. JSON-shaped so callers can
    /// extend without a schema migration.
    pub metadata: serde_json::Value,
}

impl MemoryObject {
    /// Construct a new candidate object with default counters and a
    /// retention score of `0.0` (it will be re-computed on the next
    /// decay sweep).
    pub fn new_candidate(scope_id: ScopeId, sensitivity_class: SensitivityClass) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            scope_id,
            state: MemoryState::Candidate,
            sensitivity_class,
            retention_score: 0.0,
            created_at: now,
            last_accessed_at: now,
            retrieval_count: 0,
            pin_count: 0,
            corroboration_count: 0,
            source_refs: Vec::new(),
            superseded_by: None,
            metadata: serde_json::Value::Null,
        }
    }

    /// Bump the retrieval counter and `last_accessed_at`.
    pub fn record_retrieval(&mut self, now: DateTime<Utc>) {
        self.retrieval_count = self.retrieval_count.saturating_add(1);
        self.last_accessed_at = now;
    }

    /// Bump the corroboration counter and `last_accessed_at`.
    pub fn record_corroboration(&mut self, now: DateTime<Utc>) {
        self.corroboration_count = self.corroboration_count.saturating_add(1);
        self.last_accessed_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_candidate_has_expected_defaults() {
        let scope = ScopeId::new_v4();
        let obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
        assert_eq!(obj.state, MemoryState::Candidate);
        assert_eq!(obj.sensitivity_class, SensitivityClass::Useful);
        assert_eq!(obj.retrieval_count, 0);
        assert_eq!(obj.pin_count, 0);
        assert_eq!(obj.corroboration_count, 0);
        assert!(obj.source_refs.is_empty());
        assert!(obj.superseded_by.is_none());
        assert_eq!(obj.scope_id, scope);
    }

    #[test]
    fn record_retrieval_bumps_counter_and_timestamp() {
        let mut obj = MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful);
        let later = obj.last_accessed_at + chrono::Duration::seconds(60);
        obj.record_retrieval(later);
        assert_eq!(obj.retrieval_count, 1);
        assert_eq!(obj.last_accessed_at, later);
    }
}
