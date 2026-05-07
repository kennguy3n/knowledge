//! Integration tests for [`ChannelPromotionPolicy`] / [`should_promote`].

use evidence_store::{ImportanceClass, ScopeId};
use observation_engine::{
    should_promote, ChannelPromotionPolicy, Observation, ObservationType, PromotionReason,
};

fn obs() -> Observation {
    Observation::new_candidate(
        ObservationType::Decision,
        "approved policy v3",
        ScopeId::new_v4(),
        0.9,
    )
}

#[test]
fn default_policy_promotes_high_importance_with_corroboration() {
    let policy = ChannelPromotionPolicy::default();
    let r = should_promote(&obs(), ImportanceClass::Important, 1, 0.0, &policy);
    assert!(r.promote);
    assert_eq!(r.reason, PromotionReason::Promoted);
}

#[test]
fn critical_observations_clear_default_floor() {
    let policy = ChannelPromotionPolicy::default();
    let r = should_promote(&obs(), ImportanceClass::Critical, 1, 0.0, &policy);
    assert!(r.promote);
}

#[test]
fn useful_observations_are_rejected_under_default_floor() {
    let policy = ChannelPromotionPolicy::default();
    let r = should_promote(&obs(), ImportanceClass::Useful, 5, 0.0, &policy);
    assert!(!r.promote);
    assert_eq!(r.reason, PromotionReason::BelowImportanceFloor);
}

#[test]
fn noise_observations_are_rejected() {
    let policy = ChannelPromotionPolicy::default();
    let r = should_promote(&obs(), ImportanceClass::Noise, 100, 0.0, &policy);
    assert!(!r.promote);
    assert_eq!(r.reason, PromotionReason::BelowImportanceFloor);
}

#[test]
fn insufficient_corroboration_blocks_promotion() {
    let policy = ChannelPromotionPolicy {
        min_corroboration_count: 3,
        ..ChannelPromotionPolicy::default()
    };
    let r = should_promote(&obs(), ImportanceClass::Important, 1, 0.0, &policy);
    assert!(!r.promote);
    assert_eq!(r.reason, PromotionReason::InsufficientCorroboration);
}

#[test]
fn excessive_noise_ratio_blocks_promotion() {
    let policy = ChannelPromotionPolicy {
        max_noise_ratio: 0.25,
        ..ChannelPromotionPolicy::default()
    };
    let r = should_promote(&obs(), ImportanceClass::Important, 5, 0.5, &policy);
    assert!(!r.promote);
    assert_eq!(r.reason, PromotionReason::BatchTooNoisy);
}

#[test]
fn nan_noise_ratio_is_treated_as_zero() {
    let policy = ChannelPromotionPolicy::default();
    let r = should_promote(&obs(), ImportanceClass::Important, 1, f64::NAN, &policy);
    assert!(r.promote);
}

#[test]
fn zero_corroboration_threshold_accepts_no_corroboration() {
    let policy = ChannelPromotionPolicy {
        min_corroboration_count: 0,
        ..ChannelPromotionPolicy::default()
    };
    let r = should_promote(&obs(), ImportanceClass::Important, 0, 0.0, &policy);
    assert!(r.promote);
}

#[test]
fn policy_round_trips_through_serde() {
    let policy = ChannelPromotionPolicy {
        min_importance: ImportanceClass::Critical,
        min_corroboration_count: 2,
        max_noise_ratio: 0.1,
    };
    let bytes = serde_json::to_vec(&policy).unwrap();
    let back: ChannelPromotionPolicy = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, policy);
}

#[test]
fn promotion_reason_round_trips_through_serde() {
    let reasons = [
        PromotionReason::Promoted,
        PromotionReason::BelowImportanceFloor,
        PromotionReason::InsufficientCorroboration,
        PromotionReason::BatchTooNoisy,
    ];
    for r in reasons {
        let bytes = serde_json::to_vec(&r).unwrap();
        let back: PromotionReason = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, r);
    }
}

#[test]
fn promoted_helper_is_consistent_with_promote_flag() {
    let policy = ChannelPromotionPolicy::default();
    let promoted = should_promote(&obs(), ImportanceClass::Important, 1, 0.0, &policy);
    assert_eq!(promoted.promote, promoted.reason.is_promoted());
    let rejected = should_promote(&obs(), ImportanceClass::Useful, 1, 0.0, &policy);
    assert_eq!(rejected.promote, rejected.reason.is_promoted());
}
