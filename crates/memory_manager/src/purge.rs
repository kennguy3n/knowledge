//! TTL-based purge for Archived memory objects.
//!
//! The decay state machine transitions stale objects to
//! [`MemoryState::Archived`], but Archived objects remain in the
//! database indefinitely. This module implements the final step:
//! purging Archived objects that have exceeded a configurable
//! retention period, transitioning them to [`MemoryState::Deleted`].
//!
//! Per `docs/technical/design.md` §4.4, `Deleted` means the object's
//! scope DEK has been destroyed and the object is unrecoverable. The
//! purge here transitions the in-memory state to `Deleted`; the
//! actual ciphertext cleanup happens via cryptographic forgetting
//! (Gap 9: post-forgetting VACUUM + TRIM).
//!
//! Pinned objects are never purged — a pin is the strongest
//! retention signal and overrides the TTL.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::object::MemoryObject;
use crate::state::MemoryState;
use crate::transitions::MemoryStateMachine;

/// Default retention period for Archived objects before purge.
/// 365 days — mirrors the substrate's "long-term archival but not
/// forever" stance. Production deployments should tune per-tenant.
pub const DEFAULT_ARCHIVED_RETENTION_DAYS: i64 = 365;

/// Configuration for TTL-based purge of Archived objects.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PurgeConfig {
    /// How long an object can remain in `Archived` state before
    /// being purged to `Deleted`. Measured from `last_accessed_at`.
    pub retention: Duration,
    /// When `true`, pinned objects are also purged if they exceed
    /// the retention period. Defaults to `false` — pins override
    /// TTL purge as they do for decay.
    pub purge_pinned: bool,
}

impl Default for PurgeConfig {
    fn default() -> Self {
        Self {
            retention: Duration::days(DEFAULT_ARCHIVED_RETENTION_DAYS),
            purge_pinned: false,
        }
    }
}

impl PurgeConfig {
    /// Create a config with a custom retention period.
    pub fn with_retention_days(days: i64) -> Self {
        Self {
            retention: Duration::days(days),
            purge_pinned: false,
        }
    }

    /// Set whether pinned objects should be purged.
    pub fn purge_pinned(mut self, purge: bool) -> Self {
        self.purge_pinned = purge;
        self
    }
}

/// Counters returned by [`purge_archived`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReport {
    /// Number of objects examined (all Archived objects in the input).
    pub examined: usize,
    /// Number of objects transitioned from `Archived` to `Deleted`.
    pub purged: usize,
    /// Number of objects skipped because they are pinned and
    /// `purge_pinned` is `false`.
    pub skipped_pinned: usize,
    /// Number of objects skipped because they are within the
    /// retention period.
    pub skipped_within_ttl: usize,
}

/// Purge Archived objects that have exceeded the retention TTL.
///
/// Walks `objects` and transitions any [`MemoryState::Archived`]
/// object whose `last_accessed_at` is older than `config.retention`
/// to [`MemoryState::Deleted`]. Pinned objects are skipped unless
/// `config.purge_pinned` is `true`.
///
/// Objects in any state other than `Archived` are left untouched.
///
/// Returns a [`PurgeReport`] describing what happened.
pub fn purge_archived(
    objects: &mut [MemoryObject],
    now: DateTime<Utc>,
    config: PurgeConfig,
) -> PurgeReport {
    let sm = MemoryStateMachine::new();
    let mut report = PurgeReport::default();

    for obj in objects.iter_mut() {
        if obj.state != MemoryState::Archived {
            continue;
        }
        report.examined += 1;

        // Pinned objects are exempt unless explicitly overridden.
        if obj.pin_count > 0 && !config.purge_pinned {
            report.skipped_pinned += 1;
            continue;
        }

        // Check TTL: has the object been Archived longer than the
        // retention period? We measure from `last_accessed_at` which
        // is stamped when the object entered the Archived state.
        let age = now - obj.last_accessed_at;
        if age < config.retention {
            report.skipped_within_ttl += 1;
            continue;
        }

        // Transition to Deleted.
        if sm.delete_archived(obj).is_ok() {
            report.purged += 1;
        }
    }

    report
}

/// Convenience: purge with default config.
pub fn purge_archived_default(objects: &mut [MemoryObject], now: DateTime<Utc>) -> PurgeReport {
    purge_archived(objects, now, PurgeConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::SensitivityClass;
    use evidence_store::ScopeId;

    fn fresh_archived() -> MemoryObject {
        let mut obj = MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful);
        let sm = MemoryStateMachine::new();
        // Walk to Archived via the state machine.
        obj.retention_score = 0.0;
        obj.created_at = Utc::now() - Duration::days(400);
        obj.last_accessed_at = obj.created_at;
        sm.archive_candidate(&mut obj).ok();
        assert_eq!(obj.state, MemoryState::Archived);
        obj
    }

    #[test]
    fn archived_object_past_ttl_is_purged() {
        let obj = fresh_archived();
        // last_accessed_at is 400 days ago, default TTL is 365 days.
        let mut objs = vec![obj];
        let report = purge_archived_default(&mut objs, Utc::now());
        assert_eq!(report.examined, 1);
        assert_eq!(report.purged, 1);
        assert_eq!(objs[0].state, MemoryState::Deleted);
    }

    #[test]
    fn archived_object_within_ttl_is_skipped() {
        let mut obj = fresh_archived();
        obj.last_accessed_at = Utc::now() - Duration::days(10);
        let mut objs = vec![obj];
        let report = purge_archived_default(&mut objs, Utc::now());
        assert_eq!(report.examined, 1);
        assert_eq!(report.purged, 0);
        assert_eq!(report.skipped_within_ttl, 1);
        assert_eq!(objs[0].state, MemoryState::Archived);
    }

    #[test]
    fn pinned_archived_object_is_skipped() {
        let mut obj = fresh_archived();
        obj.pin_count = 1;
        obj.last_accessed_at = Utc::now() - Duration::days(500);
        let mut objs = vec![obj];
        let report = purge_archived_default(&mut objs, Utc::now());
        assert_eq!(report.examined, 1);
        assert_eq!(report.purged, 0);
        assert_eq!(report.skipped_pinned, 1);
        assert_eq!(objs[0].state, MemoryState::Archived);
    }

    #[test]
    fn pinned_archived_object_purged_when_override_enabled() {
        let mut obj = fresh_archived();
        obj.pin_count = 1;
        obj.last_accessed_at = Utc::now() - Duration::days(500);
        let mut objs = vec![obj];
        let config = PurgeConfig::default().purge_pinned(true);
        let report = purge_archived(&mut objs, Utc::now(), config);
        assert_eq!(report.examined, 1);
        assert_eq!(report.purged, 1);
        assert_eq!(objs[0].state, MemoryState::Deleted);
    }

    #[test]
    fn non_archived_objects_are_untouched() {
        let mut candidate = MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful);
        candidate.created_at = Utc::now() - Duration::days(500);
        candidate.last_accessed_at = candidate.created_at;

        let mut objs = vec![candidate];
        let report = purge_archived_default(&mut objs, Utc::now());
        assert_eq!(report.examined, 0);
        assert_eq!(report.purged, 0);
        assert_eq!(objs[0].state, MemoryState::Candidate);
    }

    #[test]
    fn custom_retention_period() {
        let mut obj = fresh_archived();
        obj.last_accessed_at = Utc::now() - Duration::days(30);
        let mut objs = vec![obj];

        // 10-day retention → 30-day-old object should be purged.
        let config = PurgeConfig::with_retention_days(10);
        let report = purge_archived(&mut objs, Utc::now(), config);
        assert_eq!(report.purged, 1);
        assert_eq!(objs[0].state, MemoryState::Deleted);
    }

    #[test]
    fn mixed_batch() {
        let mut objs = vec![
            // Old archived → purged.
            {
                let mut o = fresh_archived();
                o.last_accessed_at = Utc::now() - Duration::days(500);
                o
            },
            // Recent archived → skipped (within TTL).
            {
                let mut o = fresh_archived();
                o.last_accessed_at = Utc::now() - Duration::days(10);
                o
            },
            // Pinned archived → skipped.
            {
                let mut o = fresh_archived();
                o.pin_count = 1;
                o.last_accessed_at = Utc::now() - Duration::days(500);
                o
            },
            // Candidate → untouched.
            MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful),
        ];

        let report = purge_archived_default(&mut objs, Utc::now());
        assert_eq!(report.examined, 3);
        assert_eq!(report.purged, 1);
        assert_eq!(report.skipped_pinned, 1);
        assert_eq!(report.skipped_within_ttl, 1);
        assert_eq!(objs[0].state, MemoryState::Deleted);
        assert_eq!(objs[1].state, MemoryState::Archived);
        assert_eq!(objs[2].state, MemoryState::Archived);
        assert_eq!(objs[3].state, MemoryState::Candidate);
    }

    #[test]
    fn zero_retention_purges_all_archived() {
        let mut obj = fresh_archived();
        obj.last_accessed_at = Utc::now();
        let mut objs = vec![obj];
        let config = PurgeConfig::with_retention_days(0);
        let report = purge_archived(&mut objs, Utc::now(), config);
        assert_eq!(report.purged, 1);
        assert_eq!(objs[0].state, MemoryState::Deleted);
    }

    #[test]
    fn empty_batch_is_noop() {
        let mut objs: Vec<MemoryObject> = vec![];
        let report = purge_archived_default(&mut objs, Utc::now());
        assert_eq!(report, PurgeReport::default());
    }
}
