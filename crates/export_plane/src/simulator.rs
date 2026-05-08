//! Policy simulator — read-only preview of an export.
//!
//! Per `PROPOSAL.md` §3.5 the export plane MUST allow operators to
//! preview what an export *would* contain without rendering an
//! actual [`crate::profile::ExportView`]. The simulator combines a
//! [`crate::policy::ExportPolicy`], an
//! [`crate::controls::ExportControlRegistry`], and the candidate
//! [`crate::profile::PortableConceptProfile`] to produce a
//! [`SimulationResult`] containing what would be included, what
//! would be excluded (with reasons), and a rough byte-size
//! estimate.
//!
//! The simulator never produces an actual export — it only
//! previews. That guarantee is statically enforced: the simulator
//! has no `&mut` access to the approval workflow and never
//! constructs an [`crate::profile::ExportView`].

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::controls::ExportControlRegistry;
use crate::policy::{ExportPolicy, ExportRejectionReason, PolicyEngine};
use crate::profile::PortableConceptProfile;

/// One excluded entity in a [`SimulationResult`].
///
/// The struct is reused for both concept and summary exclusions —
/// `entity_id` therefore refers to whichever id is appropriate for
/// the field that holds the [`SimulatedExclusion`]:
///
/// * [`SimulationResult::excluded_concepts`] — `entity_id` is the
///   excluded concept's id.
/// * [`SimulationResult::excluded_summaries`] — `entity_id` is the
///   excluded summary's id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedExclusion {
    /// Id of the excluded concept or summary.
    pub entity_id: Uuid,
    /// Stable string reason. The simulator promotes
    /// [`ExportRejectionReason`] into a stable user-readable
    /// string so it can also surface deny-by-default rejections
    /// from the [`ExportControlRegistry`] under the same shape.
    pub reason: String,
}

/// Read-only summary of what an export would emit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationResult {
    /// Concepts that would be included.
    pub included_concepts: Vec<Uuid>,
    /// Concepts that would be excluded (with reasons).
    pub excluded_concepts: Vec<SimulatedExclusion>,
    /// Summaries that would be included.
    pub included_summaries: Vec<Uuid>,
    /// Summaries that would be excluded (with reasons).
    pub excluded_summaries: Vec<SimulatedExclusion>,
    /// Whether evidence would be included.
    pub would_include_evidence: bool,
    /// Rough byte-size estimate.
    pub total_export_size_estimate: usize,
    /// Non-fatal warnings.
    pub warnings: Vec<String>,
}

/// Simulator.
#[derive(Debug, Clone)]
pub struct PolicySimulator<'a> {
    policy: &'a ExportPolicy,
    controls: &'a ExportControlRegistry,
}

impl<'a> PolicySimulator<'a> {
    /// Construct a fresh simulator.
    pub fn new(policy: &'a ExportPolicy, controls: &'a ExportControlRegistry) -> Self {
        Self { policy, controls }
    }

    /// Simulate `profile` and return what the export would contain.
    ///
    /// Profile-level expiry is enforced here — the simulator owns
    /// the profile pointer, while the underlying [`PolicyEngine`]
    /// only sees a flat `&[ApprovedConcept]` and therefore cannot
    /// observe whether the parent profile is still active. If
    /// [`PortableConceptProfile::is_expired`] is true at the
    /// current wall clock, every concept is moved to
    /// `excluded_concepts` with a stable reason, no engine pass is
    /// performed, and a warning is surfaced on the result. Callers
    /// that bypass the simulator and invoke [`PolicyEngine::evaluate`]
    /// directly are responsible for performing the same check.
    pub fn simulate(&self, profile: &PortableConceptProfile) -> SimulationResult {
        let now = Utc::now();
        let mut included_concepts = Vec::new();
        let mut excluded_concepts = Vec::new();

        // Profile-expiry guard — honours the contract documented on
        // `PortableConceptProfile::expires_at`. An expired profile
        // produces an empty preview: every concept lands in
        // `excluded_concepts` and the simulator skips the engine
        // pass entirely so callers do not waste cycles evaluating
        // policy on a profile that is structurally inactive.
        if profile.is_expired(now) {
            for c in &profile.concepts {
                excluded_concepts.push(SimulatedExclusion {
                    entity_id: c.concept_id,
                    reason: "profile expired".into(),
                });
            }
            return SimulationResult {
                included_concepts,
                excluded_concepts,
                included_summaries: Vec::new(),
                excluded_summaries: Vec::new(),
                would_include_evidence: false,
                total_export_size_estimate: 0,
                warnings: vec!["profile expired \u{2014} simulator returning empty preview".into()],
            };
        }

        // Profile-constraint fold — F-7. Profile-attached
        // [`crate::profile::ExportConstraint`]s only become effective
        // if they are folded into the policy that the engine
        // actually evaluates. The simulator computes a stricter
        // copy via [`ExportPolicy::with_constraints`] and uses it
        // for both the engine pass and the `max_summaries` cap;
        // `self.policy` itself is not mutated.
        let effective_policy = self.policy.clone().with_constraints(&profile.constraints);

        // Pre-filter via the registry.
        let mut policy_candidates = Vec::new();
        for c in &profile.concepts {
            if !self
                .controls
                .allows_concept(c.concept_id, profile.id, profile.scope_id.0, now)
            {
                excluded_concepts.push(SimulatedExclusion {
                    entity_id: c.concept_id,
                    reason: "deny-by-default: concept not authorised by export control registry"
                        .into(),
                });
                continue;
            }
            policy_candidates.push(c.clone());
        }

        // Engine pass.
        let decision = PolicyEngine::new().evaluate(&effective_policy, &policy_candidates);
        for c in &decision.approved {
            included_concepts.push(c.concept_id);
        }
        for r in &decision.rejected {
            excluded_concepts.push(SimulatedExclusion {
                entity_id: r.concept_id,
                reason: rejection_reason_label(&r.reason),
            });
        }

        // Summary handling — Phase 5 surfaces summaries in the
        // simulation result so callers can wire them through, but
        // the policy engine itself only gates concepts. The
        // [`crate::controls::SummaryExportControl`] registry is
        // consulted directly; no separate engine pass is required.
        // Exportable summaries are capped at `policy.max_summaries`
        // so the preview faithfully reflects what an actual export
        // would emit; any summaries dropped by the cap are surfaced
        // both in `excluded_summaries` (with a stable reason) and as
        // a non-fatal warning on the `SimulationResult`.
        //
        // F-12 — summaries whose [`crate::controls::SummaryExportControl::scope_id`]
        // does not match the parent profile's `scope_id` are excluded
        // up-front with a stable reason. Without this filter a
        // profile rooted in scope A could silently surface summaries
        // registered in scope B (the registry is keyed only by
        // summary id), bypassing the substrate's scope boundary.
        // Cross-scope exclusions are reported regardless of the
        // summary's `exportable` flag so callers can spot misrouted
        // controls in the preview.
        //
        // The registry's underlying iterator is a `HashMap` iterator
        // and therefore non-deterministic. Two simulations of the
        // same profile under the same policy must surface the same
        // included/excluded summary ids, otherwise the preview is
        // unauditable. Both buckets are sorted by `summary_id` before
        // the cap is applied so the cut is stable across runs.
        let mut included_summaries: Vec<Uuid> = Vec::new();
        let mut excluded_summaries: Vec<SimulatedExclusion> = Vec::new();

        let mut cross_scope: Vec<&crate::controls::SummaryExportControl> = self
            .controls
            .summaries()
            .filter(|c| c.scope_id != profile.scope_id)
            .collect();
        cross_scope.sort_by_key(|c| c.summary_id);
        for c in cross_scope {
            excluded_summaries.push(SimulatedExclusion {
                entity_id: c.summary_id,
                reason: "summary scope does not match profile scope".into(),
            });
        }

        let mut exportable: Vec<&crate::controls::SummaryExportControl> = self
            .controls
            .summaries()
            .filter(|c| c.scope_id == profile.scope_id && c.exportable)
            .collect();
        exportable.sort_by_key(|c| c.summary_id);
        let mut blocked: Vec<&crate::controls::SummaryExportControl> = self
            .controls
            .summaries()
            .filter(|c| c.scope_id == profile.scope_id && !c.exportable)
            .collect();
        blocked.sort_by_key(|c| c.summary_id);

        for c in blocked {
            excluded_summaries.push(SimulatedExclusion {
                entity_id: c.summary_id,
                reason: "summary control marked non-exportable".into(),
            });
        }
        let mut warnings = decision.warnings;
        let mut capped = 0usize;
        for c in exportable {
            if included_summaries.len() < effective_policy.max_summaries {
                included_summaries.push(c.summary_id);
            } else {
                capped += 1;
                excluded_summaries.push(SimulatedExclusion {
                    entity_id: c.summary_id,
                    reason: "policy max_summaries cap reached".into(),
                });
            }
        }
        if capped > 0 {
            warnings.push(format!(
                "policy max_summaries cap reached: {capped} summary/-ies dropped from preview"
            ));
        }

        let would_include_evidence = decision.allow_raw_evidence;
        let total_export_size_estimate = estimate_size(profile, &included_concepts);

        SimulationResult {
            included_concepts,
            excluded_concepts,
            included_summaries,
            excluded_summaries,
            would_include_evidence,
            total_export_size_estimate,
            warnings,
        }
    }
}

fn rejection_reason_label(reason: &ExportRejectionReason) -> String {
    match reason {
        ExportRejectionReason::SensitivityExceeded {
            sensitivity,
            ceiling,
        } => format!("sensitivity {sensitivity:?} exceeds policy ceiling {ceiling:?}"),
        ExportRejectionReason::ScopeNotWhitelisted { scope_id } => {
            format!("scope {scope_id} not in policy whitelist")
        }
        ExportRejectionReason::MissingProvenance => "concept lacks provenance bundle".into(),
        ExportRejectionReason::Expired => "concept approval has expired".into(),
        ExportRejectionReason::OutsideTimeWindow => "concept older than policy time window".into(),
        ExportRejectionReason::MaxConceptsReached => "policy max_concepts cap reached".into(),
    }
}

fn estimate_size(profile: &PortableConceptProfile, included: &[Uuid]) -> usize {
    let mut sum = profile.name.len() + profile.description.len() + profile.target_tool.len();
    for c in &profile.concepts {
        if included.contains(&c.concept_id) {
            sum += c.label.len() + c.definition.len();
            sum += 64; // rough fixed cost for provenance + envelope
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::{ConceptExportControl, RedactionLevel, SummaryExportControl};
    use crate::policy::ExportPolicy;
    use crate::profile::{ApprovedConcept, PortableConceptProfile};
    use crypto::{EvidenceRef, ProvenanceAgent, ProvenanceBundle, SynthesisActivity};
    use evidence_store::ScopeId;
    use memory_manager::SensitivityClass;
    use std::time::Duration;

    fn provenance() -> ProvenanceBundle {
        ProvenanceBundle::new(
            Uuid::new_v4(),
            SynthesisActivity::new("agent", "model", "prompt", Uuid::new_v4()),
            ProvenanceAgent::software("test"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        )
    }

    fn fixture_profile(concept_count: usize) -> (PortableConceptProfile, ScopeId) {
        let scope = ScopeId::new_v4();
        let mut profile =
            PortableConceptProfile::new("demo", "demo profile", "downstream-tool", scope);
        for _ in 0..concept_count {
            profile.push_concept(ApprovedConcept::new(
                Uuid::new_v4(),
                "label",
                "definition",
                scope,
                provenance(),
                SensitivityClass::Useful,
            ));
        }
        (profile, scope)
    }

    #[test]
    fn simulator_excludes_unregistered_concepts() {
        let (profile, _scope) = fixture_profile(2);
        let policy = ExportPolicy::default();
        let registry = ExportControlRegistry::new();
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_concepts.len(), 0);
        assert_eq!(result.excluded_concepts.len(), 2);
        for ex in &result.excluded_concepts {
            assert!(ex.reason.contains("deny-by-default"));
        }
    }

    #[test]
    fn simulator_includes_when_registered_and_within_policy() {
        let (profile, _scope) = fixture_profile(2);
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_concepts.len(), 2);
        assert!(result.excluded_concepts.is_empty());
    }

    #[test]
    fn simulator_caps_via_policy() {
        let (profile, _scope) = fixture_profile(5);
        let policy = ExportPolicy {
            max_concepts: 2,
            ..ExportPolicy::default()
        };
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_concepts.len(), 2);
        assert_eq!(result.excluded_concepts.len(), 3);
        assert!(result
            .excluded_concepts
            .iter()
            .all(|e| e.reason.contains("max_concepts")));
    }

    #[test]
    fn simulator_blocks_evidence_when_policy_disallows() {
        let (profile, _scope) = fixture_profile(1);
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert!(!result.would_include_evidence);
    }

    #[test]
    fn simulator_size_estimate_grows_with_concepts() {
        let (profile, _scope) = fixture_profile(0);
        let policy = ExportPolicy::default();
        let registry = ExportControlRegistry::new();
        let baseline = PolicySimulator::new(&policy, &registry)
            .simulate(&profile)
            .total_export_size_estimate;

        let (profile, _scope) = fixture_profile(3);
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let with_concepts = PolicySimulator::new(&policy, &registry)
            .simulate(&profile)
            .total_export_size_estimate;
        assert!(with_concepts > baseline);
    }

    #[test]
    fn simulator_surfaces_summaries() {
        let (profile, scope) = fixture_profile(0);
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        registry
            .insert_summary(SummaryExportControl::new(s1, scope, RedactionLevel::None))
            .expect("ok");
        let mut blocked = SummaryExportControl::new(s2, scope, RedactionLevel::Full);
        blocked.exportable = false;
        registry.insert_summary(blocked).expect("ok");
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_summaries, vec![s1]);
        assert_eq!(result.excluded_summaries.len(), 1);
        assert_eq!(result.excluded_summaries[0].entity_id, s2);
    }

    #[test]
    fn simulator_caps_summaries_via_max_summaries_policy() {
        // Five exportable summaries are registered but the policy
        // permits only two — the simulator must report exactly two
        // included, three excluded with the `max_summaries` reason,
        // and surface a non-fatal warning describing the cap so the
        // preview matches the actual export shape.
        let (profile, scope) = fixture_profile(0);
        let policy = ExportPolicy {
            max_summaries: 2,
            ..ExportPolicy::default()
        };
        let mut registry = ExportControlRegistry::new();
        for _ in 0..5 {
            registry
                .insert_summary(SummaryExportControl::new(
                    Uuid::new_v4(),
                    scope,
                    RedactionLevel::None,
                ))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_summaries.len(), 2);
        assert_eq!(result.excluded_summaries.len(), 3);
        assert!(result
            .excluded_summaries
            .iter()
            .all(|e| e.reason.contains("max_summaries")));
        assert!(result.warnings.iter().any(|w| w.contains("max_summaries")));
    }

    #[test]
    fn simulator_does_not_warn_when_summary_count_under_cap() {
        // No cap hit means no warning — sanity check on the inverse
        // of `simulator_caps_summaries_via_max_summaries_policy`.
        let (profile, scope) = fixture_profile(0);
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        registry
            .insert_summary(SummaryExportControl::new(
                Uuid::new_v4(),
                scope,
                RedactionLevel::None,
            ))
            .expect("insert");
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_summaries.len(), 1);
        assert!(result.excluded_summaries.is_empty());
        assert!(!result.warnings.iter().any(|w| w.contains("max_summaries")));
    }

    #[test]
    fn simulator_excludes_summary_from_other_scope() {
        // F-12 regression — a profile rooted in scope A must not
        // surface a summary whose [`SummaryExportControl::scope_id`]
        // is scope B, even when the summary is otherwise exportable.
        // The mismatched summary lands in `excluded_summaries` with
        // a stable reason; the scope-A summary is included normally.
        let (profile, scope_a) = fixture_profile(0);
        let scope_b = ScopeId::new_v4();
        assert_ne!(scope_a, scope_b);

        let in_scope = Uuid::new_v4();
        let out_of_scope = Uuid::new_v4();
        let mut registry = ExportControlRegistry::new();
        registry
            .insert_summary(SummaryExportControl::new(
                in_scope,
                scope_a,
                RedactionLevel::None,
            ))
            .expect("insert in-scope");
        registry
            .insert_summary(SummaryExportControl::new(
                out_of_scope,
                scope_b,
                RedactionLevel::None,
            ))
            .expect("insert out-of-scope");

        let policy = ExportPolicy::default();
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);

        assert_eq!(result.included_summaries, vec![in_scope]);
        assert_eq!(result.excluded_summaries.len(), 1);
        assert_eq!(result.excluded_summaries[0].entity_id, out_of_scope);
        assert!(result.excluded_summaries[0]
            .reason
            .contains("summary scope does not match profile scope"));
    }

    #[test]
    fn simulator_cross_scope_summary_excluded_even_when_exportable() {
        // Cross-scope summaries must be filtered before the cap is
        // applied — otherwise the cap could be "used up" by
        // illegitimate cross-scope candidates and starve legitimate
        // in-scope summaries. We register `max_summaries` in-scope
        // entries plus one cross-scope entry; the in-scope ones must
        // all be included and the cross-scope one must be excluded
        // with the scope-mismatch reason (not the `max_summaries`
        // reason).
        let (profile, scope_a) = fixture_profile(0);
        let scope_b = ScopeId::new_v4();

        let policy = ExportPolicy {
            max_summaries: 2,
            ..ExportPolicy::default()
        };

        let mut registry = ExportControlRegistry::new();
        for _ in 0..2 {
            registry
                .insert_summary(SummaryExportControl::new(
                    Uuid::new_v4(),
                    scope_a,
                    RedactionLevel::None,
                ))
                .expect("insert in-scope");
        }
        let leaked = Uuid::new_v4();
        registry
            .insert_summary(SummaryExportControl::new(
                leaked,
                scope_b,
                RedactionLevel::None,
            ))
            .expect("insert leaked");

        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);

        assert_eq!(result.included_summaries.len(), 2);
        assert!(!result.included_summaries.contains(&leaked));
        let leaked_excl = result
            .excluded_summaries
            .iter()
            .find(|e| e.entity_id == leaked)
            .expect("leaked summary must be excluded");
        assert!(leaked_excl
            .reason
            .contains("summary scope does not match profile scope"));
    }

    #[test]
    fn simulator_returns_empty_preview_when_profile_expired() {
        // Regression for N-2: `PortableConceptProfile::expires_at` is
        // documented as enforced before the engine pass. The
        // simulator now honours that contract by short-circuiting
        // when the profile is expired — every concept lands in
        // `excluded_concepts` with a stable reason, no summaries
        // are surfaced, evidence is denied, and a warning is
        // recorded.
        let (mut profile, _scope) = fixture_profile(2);
        profile.expires_at = Some(Utc::now() - chrono::Duration::seconds(60));

        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }

        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);

        assert!(result.included_concepts.is_empty());
        assert_eq!(result.excluded_concepts.len(), profile.concepts.len());
        assert!(result
            .excluded_concepts
            .iter()
            .all(|e| e.reason == "profile expired"));
        assert!(result.included_summaries.is_empty());
        assert!(result.excluded_summaries.is_empty());
        assert!(!result.would_include_evidence);
        assert_eq!(result.total_export_size_estimate, 0);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("profile expired")));
    }

    // F-7 — profile-attached `ExportConstraint`s are folded into
    // the effective policy used for the engine pass and the
    // `max_summaries` cap. These integration tests exercise each
    // constraint variant end-to-end through the simulator.

    #[test]
    fn simulator_applies_profile_max_concepts_constraint() {
        // Bare policy admits 100 concepts; profile constraint cuts
        // that down to 2. Five concepts in the profile, three should
        // be rejected with `max_concepts` reason.
        let (mut profile, _scope) = fixture_profile(5);
        profile
            .constraints
            .push(crate::profile::ExportConstraint::MaxConcepts(2));
        let policy = ExportPolicy {
            max_concepts: 100,
            ..ExportPolicy::default()
        };
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_concepts.len(), 2);
        assert!(result
            .excluded_concepts
            .iter()
            .any(|e| e.reason.contains("max_concepts")));
    }

    #[test]
    fn simulator_applies_profile_max_age_constraint() {
        // Bare policy has no time window; profile constraint
        // restricts to last 60s. One concept approved 2h ago — must
        // be filtered with `time_window` reason.
        let scope = ScopeId::new_v4();
        let mut profile =
            PortableConceptProfile::new("demo", "demo profile", "downstream-tool", scope);
        let mut old = ApprovedConcept::new(
            Uuid::new_v4(),
            "label",
            "definition",
            scope,
            provenance(),
            SensitivityClass::Useful,
        );
        old.approved_at = Utc::now() - chrono::Duration::seconds(7200);
        profile.push_concept(old);
        profile
            .constraints
            .push(crate::profile::ExportConstraint::MaxAge(
                Duration::from_secs(60),
            ));
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert!(result.included_concepts.is_empty());
        assert!(result
            .excluded_concepts
            .iter()
            .any(|e| e.reason.contains("time window")));
    }

    #[test]
    fn simulator_applies_profile_sensitivity_ceiling_constraint() {
        // Bare policy ceiling is `Critical`; profile constraint
        // tightens to `Useful`. An `Important` concept must be
        // filtered with `sensitivity_ceiling` reason.
        let scope = ScopeId::new_v4();
        let mut profile =
            PortableConceptProfile::new("demo", "demo profile", "downstream-tool", scope);
        profile.push_concept(ApprovedConcept::new(
            Uuid::new_v4(),
            "label",
            "definition",
            scope,
            provenance(),
            SensitivityClass::Important,
        ));
        profile
            .constraints
            .push(crate::profile::ExportConstraint::SensitivityCeiling(
                SensitivityClass::Useful,
            ));
        let policy = ExportPolicy::permissive(SensitivityClass::Critical);
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert!(result.included_concepts.is_empty());
        assert!(result
            .excluded_concepts
            .iter()
            .any(|e| e.reason.contains("sensitivity") && e.reason.contains("ceiling")));
    }

    #[test]
    fn simulator_applies_profile_scope_restriction_constraint() {
        // Bare policy has no scope whitelist; profile constraint
        // pins to a single scope distinct from the concept's. The
        // concept must be rejected with `scope_not_whitelisted`.
        let scope = ScopeId::new_v4();
        let other_scope = ScopeId::new_v4();
        let mut profile =
            PortableConceptProfile::new("demo", "demo profile", "downstream-tool", scope);
        profile.push_concept(ApprovedConcept::new(
            Uuid::new_v4(),
            "label",
            "definition",
            scope,
            provenance(),
            SensitivityClass::Useful,
        ));
        profile
            .constraints
            .push(crate::profile::ExportConstraint::ScopeRestriction(vec![
                other_scope.0,
            ]));
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert!(result.included_concepts.is_empty());
        assert!(result
            .excluded_concepts
            .iter()
            .any(|e| e.reason.contains("whitelist")));
    }

    #[test]
    fn simulator_does_not_mutate_inputs() {
        // The simulator is read-only. We exercise that by running it
        // twice and asserting the output is identical.
        let (profile, _scope) = fixture_profile(2);
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        for c in &profile.concepts {
            registry
                .insert_concept(ConceptExportControl::new(c.concept_id))
                .expect("insert");
        }
        let sim = PolicySimulator::new(&policy, &registry);
        let a = sim.simulate(&profile);
        let b = sim.simulate(&profile);
        assert_eq!(a.included_concepts, b.included_concepts);
    }
}
