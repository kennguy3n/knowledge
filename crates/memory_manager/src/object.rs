//! [`MemoryObject`] — the unit of decay-managed memory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::{EvidenceId, ScopeId};

use crate::state::MemoryState;

/// Sensitivity / criticality class from `docs/technical/design.md` §4.3 — drives
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
/// Per `docs/technical/design.md` §4 and `docs/technical/architecture.md` §7. The fields are kept
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
    /// retention signal (`docs/technical/design.md` §4.2).
    pub pin_count: u32,
    /// Number of independent evidence sources backing the same
    /// observation (cross-source corroboration). This count is
    /// **source-deduplicated**: the same author posting 3 times in
    /// the same channel counts as 1 source, not 3. Deduplication
    /// is keyed by [`Self::corroboration_sources`].
    pub corroboration_count: u32,
    /// Evidence rows this object was derived from. Used for
    /// provenance and supersession tracking.
    pub source_refs: Vec<EvidenceId>,
    /// Fingerprints of sources that have already corroborated this
    /// object, used to prevent double-counting the same source.
    /// A source fingerprint is derived from the connector kind +
    /// author/sender identity (e.g. `"slack:U12345"`,
    /// `"email:user@example.com"`). When empty, corroboration
    /// counting falls back to the legacy un-deduplicated behaviour
    /// for backward compatibility.
    #[serde(default)]
    pub corroboration_sources: Vec<String>,
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
            corroboration_sources: Vec::new(),
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
    ///
    /// **Legacy path**: no source fingerprint — always increments.
    /// Use [`Self::record_corroboration_from_source`] for
    /// source-deduplicated counting.
    pub fn record_corroboration(&mut self, now: DateTime<Utc>) {
        self.corroboration_count = self.corroboration_count.saturating_add(1);
        self.last_accessed_at = now;
    }

    /// Record corroboration from a specific source, deduplicating
    /// by source fingerprint. If this source has already
    /// corroborated this object, the counter is **not** incremented
    /// — only `last_accessed_at` is refreshed.
    ///
    /// The source fingerprint should be a stable identifier for
    /// the *author* of the corroborating evidence, not the evidence
    /// row itself. Good fingerprints:
    /// - `"slack:U12345"` (Slack user ID)
    /// - `"email:alice@example.com"` (email sender)
    /// - `"github:octocat"` (GitHub login)
    /// - `"jira:admin"` (Jira reporter)
    ///
    /// Bad fingerprints:
    /// - Evidence row IDs (unique per row, never deduplicates)
    /// - Channel IDs (too coarse, deduplicates different people)
    pub fn record_corroboration_from_source(
        &mut self,
        source_fingerprint: &str,
        now: DateTime<Utc>,
    ) -> bool {
        if self.corroboration_sources.iter().any(|s| s == source_fingerprint) {
            self.last_accessed_at = now;
            return false;
        }
        self.corroboration_sources.push(source_fingerprint.to_string());
        self.corroboration_count = self.corroboration_count.saturating_add(1);
        self.last_accessed_at = now;
        true
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

    #[test]
    fn record_corroboration_from_source_deduplicates() {
        let mut obj = MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Important);
        let now = Utc::now();

        // First corroboration from slack:U123 — should increment.
        assert!(obj.record_corroboration_from_source("slack:U123", now));
        assert_eq!(obj.corroboration_count, 1);
        assert_eq!(obj.corroboration_sources.len(), 1);

        // Same source again — should NOT increment.
        assert!(!obj.record_corroboration_from_source("slack:U123", now));
        assert_eq!(obj.corroboration_count, 1);
        assert_eq!(obj.corroboration_sources.len(), 1);

        // Different source — should increment.
        assert!(obj.record_corroboration_from_source("email:alice@example.com", now));
        assert_eq!(obj.corroboration_count, 2);
        assert_eq!(obj.corroboration_sources.len(), 2);

        // Third unique source.
        assert!(obj.record_corroboration_from_source("github:octocat", now));
        assert_eq!(obj.corroboration_count, 3);
        assert_eq!(obj.corroboration_sources.len(), 3);

        // Second repeat — no increment.
        assert!(!obj.record_corroboration_from_source("email:alice@example.com", now));
        assert_eq!(obj.corroboration_count, 3);
        assert_eq!(obj.corroboration_sources.len(), 3);
    }

    #[test]
    fn record_corroboration_from_source_updates_last_accessed() {
        let mut obj = MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful);
        let now = Utc::now();
        let later = now + chrono::Duration::seconds(120);

        // Even when deduplicating (no increment), last_accessed_at is refreshed.
        obj.record_corroboration_from_source("slack:U999", now);
        assert_eq!(obj.last_accessed_at, now);

        obj.record_corroboration_from_source("slack:U999", later);
        assert_eq!(obj.last_accessed_at, later);
        assert_eq!(obj.corroboration_count, 1); // still 1
    }

    #[test]
    fn legacy_record_corroboration_still_works() {
        let mut obj = MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful);
        let now = Utc::now();
        obj.record_corroboration(now);
        obj.record_corroboration(now);
        assert_eq!(obj.corroboration_count, 2);
        // Legacy path doesn't populate corroboration_sources.
        assert!(obj.corroboration_sources.is_empty());
    }
}
