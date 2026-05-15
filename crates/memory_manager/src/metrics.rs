//! Memory-quality metrics (Phase 7).
//!
//! `docs/DESIGN.md` §11 calls for substrate-level memory quality
//! tracking. This module supplies three primary metrics, an
//! aggregate report, and a [`MetricsCollector`] that the decay
//! sweep / retrieval paths can hook into:
//!
//! 1. **Retention precision** — given a window of "retained"
//!    objects (state ∈ {Canonical, Important, Critical}), the
//!    fraction whose `retrieval_count` is non-zero. Loosely:
//!    "of the things we kept, how many ever paid back".
//! 2. **Contradiction detection rate** — given a set of
//!    contradicting fact pairs and the detector's positive set,
//!    `|detected ∩ truth| / |truth|`.
//! 3. **Decay-tuning metrics** — counts of objects promoted /
//!    archived / deleted across one decay sweep window. Folded
//!    over time, these tune the candidate-archive threshold and
//!    superseded TTL.
//!
//! The metrics are deliberately deterministic functions of their
//! inputs — no hidden randomness, no I/O — so the audit log can
//! attach them to a sweep cycle and the caller can compare runs.
//!
//! Cross-references:
//!
//! * Phase 7 deliverables: `docs/internal/PHASES.md` Phase 7.
//! * Decay sweep: [`crate::decay::decay_sweep`].
//! * Retention scoring: [`crate::retention`].
//! * Module map: `ARCHITECTURE.md` §2.1 (`memory_manager`).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::decay::DecaySweepReport;
use crate::object::{MemoryObject, SensitivityClass};
use crate::state::MemoryState;

/// Retention-precision metric.
///
/// A retained object is one whose state is `Canonical` (or whose
/// sensitivity class is `Critical`, which is exempt from passive
/// decay). The metric is `retrieved / retained` — i.e. of the
/// objects the substrate decided to keep, how many were ever
/// touched by a retrieval. A high value means we're keeping the
/// right things; a low value means we are over-retaining.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetentionPrecision {
    /// Number of objects considered "retained".
    pub retained: u64,
    /// Subset of `retained` whose `retrieval_count > 0`.
    pub retrieved: u64,
    /// `retrieved / retained` clamped to `0.0 ..= 1.0`. `0.0` when
    /// no objects qualify (vacuous case).
    pub precision: f64,
}

impl RetentionPrecision {
    /// Build a [`RetentionPrecision`] from raw counts.
    pub fn from_counts(retrieved: u64, retained: u64) -> Self {
        let precision = if retained == 0 {
            0.0
        } else {
            (retrieved as f64 / retained as f64).clamp(0.0, 1.0)
        };
        Self {
            retained,
            retrieved,
            precision,
        }
    }
}

/// Contradiction-detection rate.
///
/// Recall of the contradiction detector against a ground-truth set
/// of contradicting pairs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContradictionDetectionRate {
    /// Total contradicting pairs in the ground-truth set.
    pub ground_truth_total: u64,
    /// Subset of `ground_truth_total` the detector flagged.
    pub detected: u64,
    /// `detected / ground_truth_total` clamped to `0.0 ..= 1.0`.
    pub rate: f64,
}

impl ContradictionDetectionRate {
    /// Build a [`ContradictionDetectionRate`] from raw counts.
    pub fn from_counts(detected: u64, ground_truth_total: u64) -> Self {
        let rate = if ground_truth_total == 0 {
            0.0
        } else {
            (detected as f64 / ground_truth_total as f64).clamp(0.0, 1.0)
        };
        Self {
            ground_truth_total,
            detected,
            rate,
        }
    }
}

/// Outcome counts for one decay sweep window.
///
/// Folds the [`DecaySweepReport`] (counts produced by one sweep)
/// with explicit promotion / deletion counts the caller threads in
/// from upstream state machine activity (e.g. canonicalization
/// promotions in the same window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DecayTuningMetrics {
    /// Number of `Candidate -> {Reinforced, Consolidated, Canonical}`
    /// promotions observed in the window.
    pub promoted: u64,
    /// Number of `* -> Archived` transitions observed in the window.
    pub archived: u64,
    /// Number of `Archived -> {gone}` deletions observed in the
    /// window.
    pub deleted: u64,
    /// Total objects scored by the sweep.
    pub scored: u64,
}

impl DecayTuningMetrics {
    /// Build [`DecayTuningMetrics`] from a sweep report and
    /// out-of-band promotion / deletion counts.
    pub fn from_sweep(report: DecaySweepReport, promoted: u64, deleted: u64) -> Self {
        let archived = report.candidates_archived as u64 + report.superseded_archived as u64;
        Self {
            promoted,
            archived,
            deleted,
            scored: report.scored as u64,
        }
    }

    /// Convenience: sum of two metric windows.
    pub fn merge(self, other: Self) -> Self {
        Self {
            promoted: self.promoted + other.promoted,
            archived: self.archived + other.archived,
            deleted: self.deleted + other.deleted,
            scored: self.scored + other.scored,
        }
    }
}

/// Aggregate report covering all three metrics. The audit log
/// attaches a [`MemoryQualityReport`] to every sweep cycle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MemoryQualityReport {
    /// Wall-clock time the report was generated.
    pub generated_at: DateTime<Utc>,
    /// Retention-precision component.
    pub retention_precision: RetentionPrecision,
    /// Contradiction-detection-rate component.
    pub contradiction_rate: ContradictionDetectionRate,
    /// Decay-tuning component.
    pub decay_tuning: DecayTuningMetrics,
}

/// Compute the retention-precision metric for a slice of
/// [`MemoryObject`]s. An object is "retained" if its state is
/// [`MemoryState::Canonical`] or its sensitivity class is
/// [`SensitivityClass::Critical`]. It is "retrieved" if its
/// `retrieval_count` is greater than zero.
pub fn compute_retention_precision(objects: &[MemoryObject]) -> RetentionPrecision {
    let mut retained = 0u64;
    let mut retrieved = 0u64;
    for o in objects {
        let kept = matches!(o.state, MemoryState::Canonical)
            || o.sensitivity_class == SensitivityClass::Critical;
        if kept {
            retained += 1;
            if o.retrieval_count > 0 {
                retrieved += 1;
            }
        }
    }
    RetentionPrecision::from_counts(retrieved, retained)
}

/// One ground-truth contradicting pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContradictionPair {
    /// Id of the first object.
    pub a: Uuid,
    /// Id of the second object.
    pub b: Uuid,
}

impl ContradictionPair {
    /// Construct a [`ContradictionPair`]. The pair is canonicalised
    /// so that `(a, b)` and `(b, a)` compare equal — order does not
    /// matter for the contradiction relation.
    pub fn new(a: Uuid, b: Uuid) -> Self {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        Self { a: lo, b: hi }
    }
}

/// Compute the contradiction-detection rate.
pub fn compute_contradiction_rate(
    detected: &[ContradictionPair],
    ground_truth: &[ContradictionPair],
) -> ContradictionDetectionRate {
    if ground_truth.is_empty() {
        return ContradictionDetectionRate::from_counts(0, 0);
    }
    let truth: HashSet<ContradictionPair> = ground_truth.iter().copied().collect();
    let detected_set: HashSet<ContradictionPair> = detected.iter().copied().collect();
    let hits = truth.intersection(&detected_set).count() as u64;
    ContradictionDetectionRate::from_counts(hits, ground_truth.len() as u64)
}

/// Build a [`DecayTuningMetrics`] from a sweep report and out-of-band
/// promotion / deletion counts. Convenience wrapper around
/// [`DecayTuningMetrics::from_sweep`].
pub fn decay_sweep_report(
    report: DecaySweepReport,
    promoted: u64,
    deleted: u64,
) -> DecayTuningMetrics {
    DecayTuningMetrics::from_sweep(report, promoted, deleted)
}

/// Stateful collector that accumulates metric counters across
/// sweeps. Intended to be embedded in the decay loop:
///
/// 1. Before the sweep, call [`Self::record_promotion`] /
///    [`Self::record_deletion`] for any state-machine activity that
///    happened in the window (the state machine itself is the
///    authority — the collector just accepts the counts).
/// 2. After the sweep, call [`Self::record_sweep`] with the
///    [`DecaySweepReport`] returned by [`crate::decay::decay_sweep`].
/// 3. Periodically, call [`Self::generate_report`] to publish a
///    [`MemoryQualityReport`] and reset the in-window counters.
#[derive(Debug, Clone, Default)]
pub struct MetricsCollector {
    /// Window-local tally of promotions seen since the last
    /// [`Self::generate_report`] call.
    promotions: u64,
    /// Window-local tally of deletions seen since the last
    /// [`Self::generate_report`] call.
    deletions: u64,
    /// Last sweep report, if one has been recorded.
    last_sweep: Option<DecaySweepReport>,
    /// Detected contradictions accumulated since the last report.
    detected_contradictions: HashSet<ContradictionPair>,
    /// Latest known retrieval counts indexed by object id. Lets the
    /// collector deduplicate "retrieval observed" events when a
    /// caller invokes [`Self::record_retrieval`] multiple times for
    /// the same object id.
    retrieval_counts: HashMap<Uuid, u64>,
}

impl MetricsCollector {
    /// Construct an empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one state-machine promotion (e.g. `Candidate ->
    /// Reinforced`, `Reinforced -> Consolidated`, `Consolidated ->
    /// Canonical`).
    pub fn record_promotion(&mut self) {
        self.promotions = self.promotions.saturating_add(1);
    }

    /// Record one deletion (e.g. `Archived -> gone`).
    pub fn record_deletion(&mut self) {
        self.deletions = self.deletions.saturating_add(1);
    }

    /// Record a single retrieval observation against `object_id` /
    /// `current_count`. Only the highest known count per id is
    /// retained, so re-emitting the same retrieval observation is
    /// idempotent.
    pub fn record_retrieval(&mut self, object_id: Uuid, current_count: u64) {
        let entry = self.retrieval_counts.entry(object_id).or_default();
        if current_count > *entry {
            *entry = current_count;
        }
    }

    /// Record one detected contradiction pair.
    pub fn record_detected_contradiction(&mut self, pair: ContradictionPair) {
        self.detected_contradictions.insert(pair);
    }

    /// Stash the most recent [`DecaySweepReport`] for inclusion in
    /// the next [`Self::generate_report`] call.
    pub fn record_sweep(&mut self, report: DecaySweepReport) {
        self.last_sweep = Some(report);
    }

    /// Number of currently-recorded retrieval observations.
    pub fn retrieval_observations(&self) -> usize {
        self.retrieval_counts.len()
    }

    /// Number of currently-recorded contradictions.
    pub fn detected_contradictions_len(&self) -> usize {
        self.detected_contradictions.len()
    }

    /// Generate a [`MemoryQualityReport`] from the current state
    /// and reset the in-window counters. The contradiction-rate
    /// component is computed against `ground_truth`, the
    /// retention-precision component against `objects`.
    pub fn generate_report(
        &mut self,
        now: DateTime<Utc>,
        objects: &[MemoryObject],
        ground_truth: &[ContradictionPair],
    ) -> MemoryQualityReport {
        let retention_precision = compute_retention_precision(objects);
        let detected: Vec<ContradictionPair> =
            self.detected_contradictions.iter().copied().collect();
        let contradiction_rate = compute_contradiction_rate(&detected, ground_truth);
        let sweep_report = self.last_sweep.take().unwrap_or_default();
        let decay_tuning =
            DecayTuningMetrics::from_sweep(sweep_report, self.promotions, self.deletions);

        // Reset the window counters but keep the cumulative
        // retrieval-observation map — clients may call
        // `compute_retention_precision` against a longer-lived
        // object slice.
        self.promotions = 0;
        self.deletions = 0;
        self.detected_contradictions.clear();

        MemoryQualityReport {
            generated_at: now,
            retention_precision,
            contradiction_rate,
            decay_tuning,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transitions::MemoryStateMachine;
    use evidence_store::ScopeId;

    fn obj(
        state: MemoryState,
        retrieval_count: u32,
        sensitivity: SensitivityClass,
    ) -> MemoryObject {
        let mut o = MemoryObject::new_candidate(ScopeId::new_v4(), sensitivity);
        // Walk the state machine forward by mutating directly — the
        // tests only care about reaching a target state.
        o.state = state;
        o.retrieval_count = retrieval_count;
        o
    }

    #[test]
    fn precision_is_zero_when_no_retained_objects() {
        let p = compute_retention_precision(&[]);
        assert_eq!(p.retained, 0);
        assert_eq!(p.retrieved, 0);
        assert_eq!(p.precision, 0.0);
    }

    #[test]
    fn precision_handles_mixed_states() {
        let objects = vec![
            obj(MemoryState::Canonical, 5, SensitivityClass::Useful),
            obj(MemoryState::Canonical, 0, SensitivityClass::Useful),
            obj(MemoryState::Candidate, 100, SensitivityClass::Useful),
            obj(MemoryState::Archived, 5, SensitivityClass::Useful),
            obj(MemoryState::Candidate, 0, SensitivityClass::Critical),
        ];
        let p = compute_retention_precision(&objects);
        // Retained = 2 canonical + 1 critical (non-canonical) = 3.
        // Retrieved among retained = canonical(5) only = 1.
        assert_eq!(p.retained, 3);
        assert_eq!(p.retrieved, 1);
        assert!((p.precision - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn precision_is_one_when_every_retained_is_retrieved() {
        let objects = vec![
            obj(MemoryState::Canonical, 1, SensitivityClass::Useful),
            obj(MemoryState::Canonical, 2, SensitivityClass::Useful),
        ];
        let p = compute_retention_precision(&objects);
        assert_eq!(p.precision, 1.0);
    }

    #[test]
    fn contradiction_rate_canonicalises_pair_order() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let p1 = ContradictionPair::new(a, b);
        let p2 = ContradictionPair::new(b, a);
        assert_eq!(p1, p2);
    }

    #[test]
    fn contradiction_rate_zero_when_no_truth() {
        let r = compute_contradiction_rate(&[], &[]);
        assert_eq!(r.ground_truth_total, 0);
        assert_eq!(r.detected, 0);
        assert_eq!(r.rate, 0.0);
    }

    #[test]
    fn contradiction_rate_full_recall() {
        let p1 = ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4());
        let p2 = ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4());
        let r = compute_contradiction_rate(&[p1, p2], &[p1, p2]);
        assert_eq!(r.detected, 2);
        assert_eq!(r.ground_truth_total, 2);
        assert_eq!(r.rate, 1.0);
    }

    #[test]
    fn contradiction_rate_partial_recall() {
        let p1 = ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4());
        let p2 = ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4());
        let p3 = ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4());
        let r = compute_contradiction_rate(&[p1], &[p1, p2, p3]);
        assert!((r.rate - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn contradiction_rate_ignores_false_positives() {
        let truth = ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4());
        let bogus = ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4());
        let r = compute_contradiction_rate(&[bogus], &[truth]);
        assert_eq!(r.detected, 0);
        assert_eq!(r.rate, 0.0);
    }

    #[test]
    fn decay_tuning_metrics_from_sweep_sums_archives() {
        let report = DecaySweepReport {
            scored: 10,
            candidates_archived: 3,
            superseded_archived: 2,
        };
        let m = decay_sweep_report(report, 4, 1);
        assert_eq!(m.scored, 10);
        assert_eq!(m.archived, 5);
        assert_eq!(m.promoted, 4);
        assert_eq!(m.deleted, 1);
    }

    #[test]
    fn decay_tuning_merge_sums_components() {
        let a = DecayTuningMetrics {
            promoted: 2,
            archived: 3,
            deleted: 1,
            scored: 10,
        };
        let b = DecayTuningMetrics {
            promoted: 5,
            archived: 1,
            deleted: 0,
            scored: 7,
        };
        let m = a.merge(b);
        assert_eq!(m.promoted, 7);
        assert_eq!(m.archived, 4);
        assert_eq!(m.deleted, 1);
        assert_eq!(m.scored, 17);
    }

    #[test]
    fn metrics_collector_aggregates_promotions_and_deletions() {
        let mut mc = MetricsCollector::new();
        mc.record_promotion();
        mc.record_promotion();
        mc.record_deletion();

        let report = DecaySweepReport {
            scored: 5,
            candidates_archived: 1,
            superseded_archived: 1,
        };
        mc.record_sweep(report);

        let now = Utc::now();
        let objects = vec![obj(MemoryState::Canonical, 1, SensitivityClass::Useful)];
        let r = mc.generate_report(now, &objects, &[]);
        assert_eq!(r.decay_tuning.promoted, 2);
        assert_eq!(r.decay_tuning.deleted, 1);
        assert_eq!(r.decay_tuning.archived, 2);
        assert_eq!(r.decay_tuning.scored, 5);
        assert_eq!(r.retention_precision.precision, 1.0);
    }

    #[test]
    fn metrics_collector_resets_window_counters_after_report() {
        let mut mc = MetricsCollector::new();
        mc.record_promotion();
        mc.record_deletion();
        mc.record_detected_contradiction(ContradictionPair::new(Uuid::new_v4(), Uuid::new_v4()));
        let _ = mc.generate_report(Utc::now(), &[], &[]);
        // After a report, the in-window counters reset; the next
        // report starts from zero.
        let r2 = mc.generate_report(Utc::now(), &[], &[]);
        assert_eq!(r2.decay_tuning.promoted, 0);
        assert_eq!(r2.decay_tuning.deleted, 0);
        assert_eq!(r2.decay_tuning.archived, 0);
        assert_eq!(r2.decay_tuning.scored, 0);
        assert_eq!(r2.contradiction_rate.detected, 0);
    }

    #[test]
    fn metrics_collector_record_retrieval_is_idempotent() {
        let mut mc = MetricsCollector::new();
        let id = Uuid::new_v4();
        mc.record_retrieval(id, 1);
        mc.record_retrieval(id, 3);
        mc.record_retrieval(id, 2); // smaller — must be ignored.
        assert_eq!(mc.retrieval_observations(), 1);
    }

    #[test]
    fn report_round_trips_through_serde() {
        let report = MemoryQualityReport {
            generated_at: Utc::now(),
            retention_precision: RetentionPrecision::from_counts(2, 3),
            contradiction_rate: ContradictionDetectionRate::from_counts(1, 2),
            decay_tuning: DecayTuningMetrics::from_sweep(
                DecaySweepReport {
                    scored: 1,
                    candidates_archived: 1,
                    superseded_archived: 0,
                },
                0,
                0,
            ),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: MemoryQualityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn end_to_end_metrics_against_state_machine() {
        // Drive a real state-machine cycle and confirm the
        // collector's counters line up with the resulting object
        // state.
        let sm = MemoryStateMachine::new();
        let mut o = MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful);
        let mut mc = MetricsCollector::new();

        sm.reinforce(&mut o).unwrap();
        mc.record_promotion();
        sm.consolidate(&mut o).unwrap();
        mc.record_promotion();
        sm.canonicalize(&mut o).unwrap();
        mc.record_promotion();

        // Pretend the object got retrieved twice in the window.
        o.retrieval_count = 2;
        mc.record_retrieval(o.id, 2);

        let r = mc.generate_report(Utc::now(), std::slice::from_ref(&o), &[]);
        assert_eq!(r.decay_tuning.promoted, 3);
        assert_eq!(r.retention_precision.retained, 1);
        assert_eq!(r.retention_precision.retrieved, 1);
        assert_eq!(r.retention_precision.precision, 1.0);
    }
}
