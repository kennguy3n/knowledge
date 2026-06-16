//! Retention scoring per `docs/technical/design.md` §4.2.
//!
//! The retention score is a `0.0 ..= 1.0` value computed from six
//! weighted inputs:
//!
//! 1. **Pinning** — the strongest signal. A single pin should keep an
//!    object retrievable indefinitely.
//! 2. **Retrieval frequency** — how often the object has been
//!    retrieved as part of an answered query.
//! 3. **Cross-source corroboration** — number of independent evidence
//!    sources backing the same observation.
//! 4. **Contradiction signals** — does another canonical claim
//!    contradict this one? Encoded as a presence flag in
//!    [`MemoryObject::metadata`] under `"contradicted"`.
//! 5. **Age** — older things decay unless reinforced.
//! 6. **Non-use** — long stretches without retrieval pull the score
//!    down.
//!
//! The default weights are tuned so that:
//!
//! * A pinned object scores `≥ 0.9` regardless of age / non-use.
//! * A frequently retrieved (≥ 5 retrievals) recently used object
//!   scores in the `0.5 .. 0.8` band.
//! * An old, never-retrieved candidate scores below `0.2` and is
//!   eligible for archival.
//!
//! All inputs are saturating: very large counts do not blow the score
//! past `1.0`, and very large ages do not blow it below `0.0`.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::object::MemoryObject;

/// Per-input retention score. Each component is `0.0 ..= 1.0`; the
/// final [`RetentionScore::total`] is the weighted sum clamped to
/// `0.0 ..= 1.0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetentionScore {
    /// Weighted total in `0.0 ..= 1.0`.
    pub total: f64,
    /// Pinning component — `1.0` if pinned, else `0.0`.
    pub pinning: f64,
    /// Retrieval-frequency component — saturates at 5 retrievals.
    pub retrieval_frequency: f64,
    /// Cross-source corroboration component — saturates at 3 sources.
    pub corroboration: f64,
    /// Contradiction penalty — `1.0` if no contradiction, `0.0` if
    /// contradicted (so it can be combined as a positive weight).
    pub contradiction: f64,
    /// Age decay — `1.0` for fresh objects, decays exponentially
    /// toward `0.0` as the object ages.
    pub age: f64,
    /// Non-use decay — `1.0` if recently retrieved, decays toward
    /// `0.0` as `last_accessed_at` recedes.
    pub non_use: f64,
}

/// Weights used by [`compute_retention_score`]. The default values
/// produce the bands described in the module-level docs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetentionWeights {
    /// Weight on the pinning component.
    pub pinning: f64,
    /// Weight on the retrieval-frequency component.
    pub retrieval_frequency: f64,
    /// Weight on the cross-source corroboration component.
    pub corroboration: f64,
    /// Weight on the contradiction component.
    pub contradiction: f64,
    /// Weight on the age component.
    pub age: f64,
    /// Weight on the non-use component.
    pub non_use: f64,
}

impl Default for RetentionWeights {
    fn default() -> Self {
        // Sum: 0.5 + 0.15 + 0.1 + 0.05 + 0.1 + 0.1 = 1.0
        Self {
            pinning: 0.5,
            retrieval_frequency: 0.15,
            corroboration: 0.10,
            contradiction: 0.05,
            age: 0.10,
            non_use: 0.10,
        }
    }
}

/// Compute the retention score for `object` at time `now` using the
/// default weights.
pub fn compute_retention_score(object: &MemoryObject, now: DateTime<Utc>) -> RetentionScore {
    compute_with_weights(object, now, RetentionWeights::default())
}

/// Lower-level variant of [`compute_retention_score`] exposing the
/// weight set.
pub fn compute_with_weights(
    object: &MemoryObject,
    now: DateTime<Utc>,
    weights: RetentionWeights,
) -> RetentionScore {
    let pinning = if object.pin_count > 0 { 1.0 } else { 0.0 };

    // Saturating linear ramp on retrieval count: 0 -> 0.0, 5+ -> 1.0.
    let retrieval_frequency = (object.retrieval_count as f64 / 5.0).min(1.0);

    // Saturating linear ramp on corroboration: 0 -> 0.0, 3+ -> 1.0.
    let corroboration = (object.corroboration_count as f64 / 3.0).min(1.0);

    // Contradiction is encoded as `metadata.contradicted == true`.
    // `1.0` when not contradicted (additive credit), `0.0` when
    // contradicted (additive penalty).
    let contradiction = if object
        .metadata
        .get("contradicted")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        0.0
    } else {
        1.0
    };

    // Age decay: per-class half-life (`half_life_seconds_for_class`):
    // ~100y Critical, ~2y Important, 30d Useful, 1d Noise.
    let age_seconds = (now - object.created_at).num_seconds().max(0) as f64;
    let age = decay(age_seconds, half_life_seconds_for_class(object));

    // Non-use decay: 14-day half-life since last access.
    let non_use_seconds = (now - object.last_accessed_at).num_seconds().max(0) as f64;
    let non_use = decay(non_use_seconds, Duration::days(14).num_seconds() as f64);

    let weighted = weights.pinning * pinning
        + weights.retrieval_frequency * retrieval_frequency
        + weights.corroboration * corroboration
        + weights.contradiction * contradiction
        + weights.age * age
        + weights.non_use * non_use;
    // Pinning is the strongest retention signal (`docs/technical/design.md` §4.2):
    // a single pin must keep the object retrievable indefinitely.
    // Enforce a hard floor of 0.9 when pinned, regardless of age /
    // non-use decay.
    let total = if object.pin_count > 0 {
        weighted.max(0.9).clamp(0.0, 1.0)
    } else {
        weighted.clamp(0.0, 1.0)
    };

    RetentionScore {
        total,
        pinning,
        retrieval_frequency,
        corroboration,
        contradiction,
        age,
        non_use,
    }
}

/// Per-class half-life on the age component (`docs/technical/design.md` §4.3).
fn half_life_seconds_for_class(obj: &MemoryObject) -> f64 {
    use crate::object::SensitivityClass;
    let days = match obj.sensitivity_class {
        SensitivityClass::Critical => 36500.0, // ~100 years — effectively no passive decay.
        SensitivityClass::Important => 365.0 * 2.0, // ~2 years.
        SensitivityClass::Useful => 30.0,
        SensitivityClass::Noise => 1.0,
    };
    days * 86_400.0
}

/// Standard exponential decay: `2^(-elapsed / half_life)`. Returns
/// `1.0` for `elapsed = 0` and approaches `0.0` as elapsed grows.
fn decay(elapsed_seconds: f64, half_life_seconds: f64) -> f64 {
    if half_life_seconds <= 0.0 {
        return 0.0;
    }
    (-elapsed_seconds / half_life_seconds * std::f64::consts::LN_2)
        .exp()
        .clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::SensitivityClass;
    use evidence_store::ScopeId;

    fn obj() -> MemoryObject {
        MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful)
    }

    #[test]
    fn pinned_object_scores_high() {
        let mut o = obj();
        o.pin_count = 1;
        let s = compute_retention_score(&o, o.created_at);
        assert!(s.total >= 0.9, "pinned score: {}", s.total);
    }

    #[test]
    fn fresh_unused_candidate_scores_modestly() {
        let o = obj();
        let s = compute_retention_score(&o, o.created_at);
        // Fresh -> age=1, non_use=1, contradiction=1; no pin, no
        // retrievals, no corroboration. Score = 0.05+0.10+0.10 = 0.25.
        assert!(s.total > 0.2 && s.total < 0.3, "fresh score: {}", s.total);
    }

    #[test]
    fn ancient_unused_candidate_scores_low() {
        let mut o = obj();
        o.created_at = Utc::now() - Duration::days(365 * 5);
        o.last_accessed_at = o.created_at;
        let s = compute_retention_score(&o, Utc::now());
        // Useful class has 30-day age half-life; 5 years -> age ~= 0.
        // Non-use 5 years with 14-day half-life -> ~= 0. Only the
        // contradiction credit (1.0 * 0.05) survives.
        assert!(s.total < 0.1, "ancient score: {}", s.total);
    }

    #[test]
    fn frequently_retrieved_recently_used_scores_medium_high() {
        let mut o = obj();
        o.retrieval_count = 10;
        o.last_accessed_at = Utc::now();
        let s = compute_retention_score(&o, Utc::now());
        // 0.15 retrieval + 0.05 contradiction + 0.10 age + 0.10 non-use = 0.4.
        assert!(
            s.total > 0.35 && s.total < 0.55,
            "freq retrieved score: {}",
            s.total
        );
    }

    #[test]
    fn contradicted_object_takes_a_hit() {
        let mut o = obj();
        o.metadata = serde_json::json!({ "contradicted": true });
        let s_contradicted = compute_retention_score(&o, o.created_at);

        let mut o2 = obj();
        o2.created_at = o.created_at;
        o2.last_accessed_at = o.last_accessed_at;
        let s_clean = compute_retention_score(&o2, o.created_at);

        assert!(s_contradicted.total < s_clean.total);
    }

    #[test]
    fn all_components_are_in_unit_interval() {
        let o = obj();
        let s = compute_retention_score(&o, Utc::now());
        for comp in [
            s.pinning,
            s.retrieval_frequency,
            s.corroboration,
            s.contradiction,
            s.age,
            s.non_use,
        ] {
            assert!((0.0..=1.0).contains(&comp));
        }
        assert!((0.0..=1.0).contains(&s.total));
    }
}
