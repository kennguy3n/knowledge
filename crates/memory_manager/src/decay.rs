//! Decay sweep — iterate every memory object, recompute its retention
//! score, and drive the state machine forward when thresholds /
//! elapsed times are met.
//!
//! The sweep is intentionally cheap and side-effect free outside of
//! the supplied `objects` slice and the returned [`DecaySweepReport`].
//! The substrate calls it on a wall-clock cadence (Phase 1 will run
//! it on idle from the FFI surface).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::object::{MemoryObject, SensitivityClass};
use crate::retention::compute_retention_score;
use crate::state::MemoryState;
use crate::transitions::MemoryStateMachine;

/// Threshold below which a [`MemoryState::Candidate`] object is
/// archived during a decay sweep.
pub const DEFAULT_CANDIDATE_ARCHIVE_THRESHOLD: f64 = 0.15;

/// Default TTL after which a [`MemoryState::Superseded`] object is
/// archived. 90 days mirrors the substrate's "supersession preferred
/// over deletion" stance from `docs/DESIGN.md` §4.
pub const DEFAULT_SUPERSEDED_TTL_DAYS: i64 = 90;

/// Counters returned by [`decay_sweep`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecaySweepReport {
    /// Number of objects whose retention score was recomputed.
    pub scored: usize,
    /// Number of `Candidate -> Archived` transitions.
    pub candidates_archived: usize,
    /// Number of `Superseded -> Archived` transitions.
    pub superseded_archived: usize,
}

/// Run one decay sweep over `objects` at wall-clock `now`.
///
/// * Recomputes the retention score on every object.
/// * Transitions any `Candidate` whose score is below
///   [`DEFAULT_CANDIDATE_ARCHIVE_THRESHOLD`] to `Archived`.
/// * Transitions any `Superseded` whose time-since-supersession
///   exceeds [`DEFAULT_SUPERSEDED_TTL_DAYS`] to `Archived`. The
///   sweep uses `last_accessed_at` as the supersession reference
///   point; [`crate::transitions::MemoryStateMachine::supersede`]
///   stamps it on the transition specifically so this comparison
///   is correct.
///
/// Returns counters describing what changed. Objects in any other
/// state are left untouched (the rest of the state machine is
/// driven by explicit user / pipeline action).
pub fn decay_sweep(objects: &mut [MemoryObject], now: DateTime<Utc>) -> DecaySweepReport {
    decay_sweep_with(
        objects,
        now,
        DEFAULT_CANDIDATE_ARCHIVE_THRESHOLD,
        Duration::days(DEFAULT_SUPERSEDED_TTL_DAYS),
    )
}

/// Lower-level variant of [`decay_sweep`] exposing the archive
/// threshold and the supersession TTL.
pub fn decay_sweep_with(
    objects: &mut [MemoryObject],
    now: DateTime<Utc>,
    candidate_archive_threshold: f64,
    superseded_ttl: Duration,
) -> DecaySweepReport {
    let sm = MemoryStateMachine::new();
    let mut report = DecaySweepReport::default();
    for obj in objects.iter_mut() {
        let score = compute_retention_score(obj, now);
        obj.retention_score = score.total;
        report.scored += 1;

        // Match guards can't mutate the bound place, so we destructure
        // the state into local flags first and apply the transition
        // outside the match. Critical-class items are exempt from
        // passive decay per `docs/DESIGN.md` §4.3 — they only leave the
        // active set via explicit deprecation / supersession.
        let is_critical = obj.sensitivity_class == SensitivityClass::Critical;
        let (try_archive_candidate, try_archive_superseded) = match obj.state {
            MemoryState::Candidate => (
                !is_critical && score.total < candidate_archive_threshold,
                false,
            ),
            MemoryState::Superseded => (
                false,
                !is_critical && (now - obj.last_accessed_at) >= superseded_ttl,
            ),
            _ => (false, false),
        };
        if try_archive_candidate && sm.archive_candidate(obj).is_ok() {
            report.candidates_archived += 1;
        }
        if try_archive_superseded && sm.archive_superseded(obj).is_ok() {
            report.superseded_archived += 1;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::SensitivityClass;
    use evidence_store::ScopeId;

    fn fresh() -> MemoryObject {
        MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful)
    }

    #[test]
    fn ancient_candidate_is_archived() {
        let mut obj = fresh();
        obj.created_at = Utc::now() - Duration::days(365 * 5);
        obj.last_accessed_at = obj.created_at;
        let mut objs = vec![obj];
        let report = decay_sweep(&mut objs, Utc::now());
        assert_eq!(report.candidates_archived, 1);
        assert_eq!(objs[0].state, MemoryState::Archived);
    }

    #[test]
    fn pinned_candidate_survives_decay_sweep() {
        let mut obj = fresh();
        obj.pin_count = 1;
        obj.created_at = Utc::now() - Duration::days(365 * 5);
        obj.last_accessed_at = obj.created_at;
        let mut objs = vec![obj];
        let report = decay_sweep(&mut objs, Utc::now());
        assert_eq!(report.candidates_archived, 0);
        assert_eq!(objs[0].state, MemoryState::Candidate);
        assert!(objs[0].retention_score >= 0.5);
    }

    #[test]
    fn superseded_object_archives_after_ttl() {
        let mut obj = fresh();
        obj.state = MemoryState::Superseded;
        obj.last_accessed_at = Utc::now() - Duration::days(180);
        let mut objs = vec![obj];
        let report = decay_sweep(&mut objs, Utc::now());
        assert_eq!(report.superseded_archived, 1);
        assert_eq!(objs[0].state, MemoryState::Archived);
    }

    #[test]
    fn superseded_object_within_ttl_is_left_alone() {
        let mut obj = fresh();
        obj.state = MemoryState::Superseded;
        obj.last_accessed_at = Utc::now() - Duration::days(10);
        let mut objs = vec![obj];
        let report = decay_sweep(&mut objs, Utc::now());
        assert_eq!(report.superseded_archived, 0);
        assert_eq!(objs[0].state, MemoryState::Superseded);
    }

    /// Regression — a Canonical row that hadn't been read in a long
    /// time (>> the Superseded TTL) used to be archived on the very
    /// next sweep after supersession because the TTL was measured
    /// from `last_accessed_at` instead of from the supersession
    /// timestamp. `MemoryStateMachine::supersede` now stamps
    /// `last_accessed_at` on the transition so the 90-day grace
    /// period actually applies.
    #[test]
    fn supersede_resets_supersession_clock_so_ttl_grace_applies() {
        use crate::transitions::MemoryStateMachine;

        let mut obj = fresh();
        // Walk it to Canonical and backdate `last_accessed_at` by
        // far longer than the Superseded TTL.
        let sm = MemoryStateMachine::new();
        sm.reinforce(&mut obj).unwrap();
        sm.consolidate(&mut obj).unwrap();
        sm.canonicalize(&mut obj).unwrap();
        obj.last_accessed_at = Utc::now() - Duration::days(365);
        // Then supersede.
        sm.supersede(&mut obj, uuid::Uuid::new_v4()).unwrap();

        let mut objs = vec![obj];
        let report = decay_sweep(&mut objs, Utc::now());
        assert_eq!(
            report.superseded_archived, 0,
            "supersede() must stamp last_accessed_at so the Superseded TTL counts \
             from the supersession instant, not from the row's last read"
        );
        assert_eq!(objs[0].state, MemoryState::Superseded);
    }
}
