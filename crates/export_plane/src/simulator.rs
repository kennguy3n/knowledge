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
    pub fn simulate(&self, profile: &PortableConceptProfile) -> SimulationResult {
        let now = Utc::now();
        let mut included_concepts = Vec::new();
        let mut excluded_concepts = Vec::new();

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
        let decision = PolicyEngine::new().evaluate(self.policy, &policy_candidates);
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
        let included_summaries: Vec<Uuid> = self
            .controls
            .summaries()
            .filter(|c| c.exportable)
            .map(|c| c.summary_id)
            .collect();
        let excluded_summaries: Vec<SimulatedExclusion> = self
            .controls
            .summaries()
            .filter(|c| !c.exportable)
            .map(|c| SimulatedExclusion {
                entity_id: c.summary_id,
                reason: "summary control marked non-exportable".into(),
            })
            .collect();

        let would_include_evidence = decision.allow_raw_evidence;
        let total_export_size_estimate = estimate_size(profile, &included_concepts);

        SimulationResult {
            included_concepts,
            excluded_concepts,
            included_summaries,
            excluded_summaries,
            would_include_evidence,
            total_export_size_estimate,
            warnings: decision.warnings,
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
        let (profile, _scope) = fixture_profile(0);
        let policy = ExportPolicy::default();
        let mut registry = ExportControlRegistry::new();
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        registry
            .insert_summary(SummaryExportControl::new(s1, RedactionLevel::None))
            .expect("ok");
        let mut blocked = SummaryExportControl::new(s2, RedactionLevel::Full);
        blocked.exportable = false;
        registry.insert_summary(blocked).expect("ok");
        let sim = PolicySimulator::new(&policy, &registry);
        let result = sim.simulate(&profile);
        assert_eq!(result.included_summaries, vec![s1]);
        assert_eq!(result.excluded_summaries.len(), 1);
        assert_eq!(result.excluded_summaries[0].entity_id, s2);
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
