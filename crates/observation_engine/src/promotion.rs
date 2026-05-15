//! Channel-scoped promotion policy — should an extracted observation
//! actually be promoted into channel memory?
//!
//! Per `docs/DESIGN.md` §4 / §6.2 and `docs/internal/PHASES.md` Phase 2: not every
//! observation extracted from raw evidence belongs in channel
//! memory. The promotion policy gates promotion on:
//!
//! * a minimum [`ImportanceClass`],
//! * a minimum corroboration count (cross-source agreement),
//! * a maximum noise ratio (defensive: keep noisy extractions out
//!   even if a single high-importance observation slipped through).
//!
//! The Phase-2 surface here is a small struct + a pure
//! [`should_promote`] function so callers can swap in tenant-specific
//! policies.

use evidence_store::ImportanceClass;
use serde::{Deserialize, Serialize};

use crate::types::Observation;

/// Promotion policy for channel-memory promotion decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPromotionPolicy {
    /// Minimum [`ImportanceClass`] required for promotion.
    pub min_importance: ImportanceClass,
    /// Minimum cross-source corroboration count required.
    pub min_corroboration_count: u32,
    /// Maximum tolerated `noise / total` ratio across the
    /// observation batch the promotion is being scored against.
    /// `0.0` rejects any noise; `1.0` accepts arbitrary noise.
    pub max_noise_ratio: f64,
}

impl Default for ChannelPromotionPolicy {
    fn default() -> Self {
        Self {
            min_importance: ImportanceClass::Important,
            min_corroboration_count: 1,
            max_noise_ratio: 0.5,
        }
    }
}

/// The reason a [`should_promote`] decision was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionReason {
    /// All gates cleared; the observation should be promoted.
    Promoted,
    /// The observation's importance class is below
    /// [`ChannelPromotionPolicy::min_importance`].
    BelowImportanceFloor,
    /// Cross-source corroboration is below the policy threshold.
    InsufficientCorroboration,
    /// The surrounding batch is too noisy.
    BatchTooNoisy,
}

impl PromotionReason {
    /// Whether this reason indicates a "promote" decision.
    pub const fn is_promoted(self) -> bool {
        matches!(self, Self::Promoted)
    }
}

/// Decision returned by [`should_promote`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionResult {
    /// Final decision.
    pub promote: bool,
    /// The reason for the decision.
    pub reason: PromotionReason,
}

/// Decide whether `observation` should be promoted into channel
/// memory under `policy`.
///
/// `corroboration_count` is the number of independent evidence rows
/// that agree with `observation`. `batch_noise_ratio` is the
/// (`noise observations` / `total observations`) ratio in the
/// surrounding batch — typically computed once per pipeline run.
/// `importance` is the importance class assigned to `observation`
/// by the upstream classifier.
pub fn should_promote(
    observation: &Observation,
    importance: ImportanceClass,
    corroboration_count: u32,
    batch_noise_ratio: f64,
    policy: &ChannelPromotionPolicy,
) -> PromotionResult {
    let _ = observation;
    if importance.as_tag() < policy.min_importance.as_tag() {
        return PromotionResult {
            promote: false,
            reason: PromotionReason::BelowImportanceFloor,
        };
    }
    if corroboration_count < policy.min_corroboration_count {
        return PromotionResult {
            promote: false,
            reason: PromotionReason::InsufficientCorroboration,
        };
    }
    // Negative / NaN ratios are treated as "no noise" defensively.
    let noise = if batch_noise_ratio.is_finite() {
        batch_noise_ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if noise > policy.max_noise_ratio {
        return PromotionResult {
            promote: false,
            reason: PromotionReason::BatchTooNoisy,
        };
    }
    PromotionResult {
        promote: true,
        reason: PromotionReason::Promoted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ObservationType;
    use evidence_store::ScopeId;

    fn fixture_obs() -> Observation {
        Observation::new_candidate(
            ObservationType::Decision,
            "approved policy",
            ScopeId::new_v4(),
            0.9,
        )
    }

    #[test]
    fn promotes_with_default_policy() {
        let policy = ChannelPromotionPolicy::default();
        let obs = fixture_obs();
        let r = should_promote(&obs, ImportanceClass::Important, 1, 0.0, &policy);
        assert!(r.promote);
        assert_eq!(r.reason, PromotionReason::Promoted);
    }

    #[test]
    fn rejects_below_importance_floor() {
        let policy = ChannelPromotionPolicy::default();
        let obs = fixture_obs();
        let r = should_promote(&obs, ImportanceClass::Useful, 5, 0.0, &policy);
        assert!(!r.promote);
        assert_eq!(r.reason, PromotionReason::BelowImportanceFloor);
    }

    #[test]
    fn rejects_when_corroboration_below_threshold() {
        let policy = ChannelPromotionPolicy {
            min_corroboration_count: 2,
            ..ChannelPromotionPolicy::default()
        };
        let obs = fixture_obs();
        let r = should_promote(&obs, ImportanceClass::Important, 1, 0.0, &policy);
        assert!(!r.promote);
        assert_eq!(r.reason, PromotionReason::InsufficientCorroboration);
    }

    #[test]
    fn rejects_when_batch_is_too_noisy() {
        let policy = ChannelPromotionPolicy::default();
        let obs = fixture_obs();
        let r = should_promote(&obs, ImportanceClass::Important, 5, 0.95, &policy);
        assert!(!r.promote);
        assert_eq!(r.reason, PromotionReason::BatchTooNoisy);
    }
}
