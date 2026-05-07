//! Export policy + policy engine.
//!
//! Per `PROPOSAL.md` §3.5 every export emission is policy-gated. The
//! policy is a small declarative struct ([`ExportPolicy`]); the
//! engine ([`PolicyEngine`]) takes a list of candidate
//! [`crate::profile::ApprovedConcept`]s and returns an
//! [`ExportDecision`] containing the approved subset, the rejected
//! concepts (with reasons), and any non-fatal warnings.
//!
//! Least-privilege defaults:
//!
//! * `allow_raw_evidence: false` — raw evidence is **never** included
//!   in an export view unless this flag is explicitly true and every
//!   approved concept has sensitivity strictly below
//!   [`memory_manager::SensitivityClass::Critical`].
//! * `require_provenance: true` — concepts without a provenance
//!   bundle are filtered out.
//! * `sensitivity_ceiling: SensitivityClass::Useful` — the policy
//!   filters out anything more sensitive than this by default.

use std::time::Duration;

use chrono::{DateTime, Utc};
use crypto::ProvenanceBundle;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use memory_manager::SensitivityClass;

use crate::profile::ApprovedConcept;

/// Reason a candidate concept was rejected by the policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportRejectionReason {
    /// Concept sensitivity exceeded the policy ceiling.
    SensitivityExceeded {
        /// Concept's sensitivity.
        sensitivity: SensitivityClass,
        /// Policy ceiling.
        ceiling: SensitivityClass,
    },
    /// Concept's scope was not in the policy whitelist.
    ScopeNotWhitelisted {
        /// Concept's scope id.
        scope_id: Uuid,
    },
    /// Concept's provenance bundle does not satisfy the policy.
    ///
    /// Returned when `require_provenance` is true and the bundle's
    /// `entity_id` is nil or its `activity` is unpopulated (empty
    /// agent identity / empty model version). An empty `derivations`
    /// list is **not** by itself a rejection reason — the workflow
    /// that produced the bundle (e.g.
    /// `crate::approval::ConceptApprovalWorkflow`) is itself a
    /// synthesis activity, so administratively approved concepts
    /// have no upstream derivations.
    MissingProvenance,
    /// Concept approval has expired.
    Expired,
    /// Concept was older than the policy time window allows.
    OutsideTimeWindow,
    /// Hit the per-export concept cap.
    MaxConceptsReached,
}

/// One rejection in an [`ExportDecision`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRejection {
    /// Concept id that was rejected.
    pub concept_id: Uuid,
    /// Why the concept was rejected.
    pub reason: ExportRejectionReason,
}

/// Decision produced by [`PolicyEngine::evaluate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportDecision {
    /// Concepts the policy approved for export.
    pub approved: Vec<ApprovedConcept>,
    /// Concepts the policy rejected, with reasons.
    pub rejected: Vec<ExportRejection>,
    /// Non-fatal warnings (e.g. policy was about to hit max
    /// concepts, raw evidence was requested but blocked).
    pub warnings: Vec<String>,
    /// Whether raw evidence is allowed in the rendered view. The
    /// engine ANDs `policy.allow_raw_evidence` with the absence of
    /// any `Critical` concept in the approved set.
    pub allow_raw_evidence: bool,
}

/// Declarative export policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportPolicy {
    /// Whether raw evidence may be included in the rendered view.
    /// Default: `false`. Even when `true`, the engine will only
    /// emit raw evidence if the approved set contains *no* `Critical`
    /// concept.
    pub allow_raw_evidence: bool,
    /// Maximum number of concepts the policy will admit.
    pub max_concepts: usize,
    /// Maximum number of summaries the policy will admit (used by
    /// [`crate::simulator::PolicySimulator`]; the engine itself only
    /// gates concepts).
    pub max_summaries: usize,
    /// Sensitivity ceiling.
    pub sensitivity_ceiling: SensitivityClass,
    /// Optional explicit scope whitelist. `None` means "any scope".
    pub scope_whitelist: Option<Vec<Uuid>>,
    /// Optional time window. Concepts whose approval age is **strictly
    /// greater than** `time_window` (relative to the current wall
    /// clock) are filtered out — a concept whose approval age is
    /// exactly equal to `time_window` is still admitted. The boundary
    /// is therefore inclusive on the "still valid" side.
    pub time_window: Option<Duration>,
    /// Whether a populated `provenance` field is mandatory.
    pub require_provenance: bool,
}

impl Default for ExportPolicy {
    /// Default policy is the most restrictive sensible setting.
    fn default() -> Self {
        Self {
            allow_raw_evidence: false,
            max_concepts: 100,
            max_summaries: 25,
            sensitivity_ceiling: SensitivityClass::Useful,
            scope_whitelist: None,
            time_window: None,
            require_provenance: true,
        }
    }
}

impl ExportPolicy {
    /// Construct a fresh permissive policy with the given ceiling.
    pub fn permissive(ceiling: SensitivityClass) -> Self {
        Self {
            allow_raw_evidence: false,
            max_concepts: usize::MAX,
            max_summaries: usize::MAX,
            sensitivity_ceiling: ceiling,
            scope_whitelist: None,
            time_window: None,
            require_provenance: true,
        }
    }
}

/// Policy engine.
#[derive(Debug, Default, Clone)]
pub struct PolicyEngine;

impl PolicyEngine {
    /// Construct a fresh engine.
    pub fn new() -> Self {
        Self
    }

    /// Evaluate `candidates` against `policy` and return the
    /// resulting [`ExportDecision`]. The engine is pure / stateless;
    /// callers may invoke it from any thread.
    pub fn evaluate(
        &self,
        policy: &ExportPolicy,
        candidates: &[ApprovedConcept],
    ) -> ExportDecision {
        let now = Utc::now();
        let mut approved = Vec::new();
        let mut rejected = Vec::new();
        let mut warnings = Vec::new();

        for c in candidates {
            // Concept-expiry check.
            if c.is_expired(now) {
                rejected.push(ExportRejection {
                    concept_id: c.concept_id,
                    reason: ExportRejectionReason::Expired,
                });
                continue;
            }
            // Sensitivity ceiling.
            if rank(c.sensitivity_class) > rank(policy.sensitivity_ceiling) {
                rejected.push(ExportRejection {
                    concept_id: c.concept_id,
                    reason: ExportRejectionReason::SensitivityExceeded {
                        sensitivity: c.sensitivity_class,
                        ceiling: policy.sensitivity_ceiling,
                    },
                });
                continue;
            }
            // Scope whitelist.
            if let Some(whitelist) = &policy.scope_whitelist {
                if !whitelist.contains(&c.scope_id.0) {
                    rejected.push(ExportRejection {
                        concept_id: c.concept_id,
                        reason: ExportRejectionReason::ScopeNotWhitelisted {
                            scope_id: c.scope_id.0,
                        },
                    });
                    continue;
                }
            }
            // Provenance — gated by `require_provenance`. The
            // [`crypto::ProvenanceBundle`] type does not have a "null"
            // form, so the requirement is structural: the bundle must
            // identify the entity it is attached to (`entity_id` not
            // nil) and carry a populated synthesis activity (non-empty
            // `agent_identity` and `model_version`). An empty
            // `derivations` list is **legal** — administratively
            // approved concepts produced by
            // [`crate::approval::ConceptApprovalWorkflow`] are
            // themselves the synthesis activity and have no upstream
            // derivations.
            if policy.require_provenance && !is_provenance_populated(&c.provenance) {
                rejected.push(ExportRejection {
                    concept_id: c.concept_id,
                    reason: ExportRejectionReason::MissingProvenance,
                });
                continue;
            }
            // Time window.
            if let Some(window) = policy.time_window {
                if !approved_within(c.approved_at, window, now) {
                    rejected.push(ExportRejection {
                        concept_id: c.concept_id,
                        reason: ExportRejectionReason::OutsideTimeWindow,
                    });
                    continue;
                }
            }
            // Cap.
            if approved.len() >= policy.max_concepts {
                rejected.push(ExportRejection {
                    concept_id: c.concept_id,
                    reason: ExportRejectionReason::MaxConceptsReached,
                });
                continue;
            }
            approved.push(c.clone());
        }

        // Raw-evidence gate.
        let any_critical = approved
            .iter()
            .any(|c| c.sensitivity_class == SensitivityClass::Critical);
        let allow_raw_evidence = policy.allow_raw_evidence && !any_critical;
        if policy.allow_raw_evidence && any_critical {
            warnings.push(
                "raw evidence requested but suppressed because the approved set contains a Critical concept"
                    .into(),
            );
        }

        ExportDecision {
            approved,
            rejected,
            warnings,
            allow_raw_evidence,
        }
    }
}

/// True iff `bundle` carries enough structure to satisfy
/// `require_provenance: true`.
///
/// The check is intentionally narrow: we require the bundle to
/// identify the entity it is attached to (`entity_id` not nil) and
/// to carry a populated [`crypto::SynthesisActivity`] (non-empty
/// `agent_identity` and `model_version`). The
/// [`ProvenanceBundle::derivations`] field is **not** required to be
/// populated — administratively approved concepts produced by
/// [`crate::approval::ConceptApprovalWorkflow`] are themselves the
/// synthesis activity and have no upstream derivations.
fn is_provenance_populated(bundle: &ProvenanceBundle) -> bool {
    !bundle.entity_id.is_nil()
        && !bundle.activity.agent_identity.is_empty()
        && !bundle.activity.model_version.is_empty()
}

fn rank(class: SensitivityClass) -> u8 {
    match class {
        SensitivityClass::Noise => 0,
        SensitivityClass::Useful => 1,
        SensitivityClass::Important => 2,
        SensitivityClass::Critical => 3,
    }
}

fn approved_within(approved_at: DateTime<Utc>, window: Duration, now: DateTime<Utc>) -> bool {
    let elapsed = now.signed_duration_since(approved_at);
    let Ok(elapsed_std) = elapsed.to_std() else {
        // approved_at in the future — treat as in window.
        return true;
    };
    elapsed_std <= window
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::{EvidenceRef, ProvenanceAgent, ProvenanceBundle, SynthesisActivity};
    use evidence_store::ScopeId;

    fn provenance(empty: bool) -> ProvenanceBundle {
        let derivations = if empty {
            Vec::new()
        } else {
            vec![EvidenceRef::from_uuid(Uuid::new_v4())]
        };
        ProvenanceBundle::new(
            Uuid::new_v4(),
            SynthesisActivity::new("agent", "model", "prompt", Uuid::new_v4()),
            ProvenanceAgent::software("test"),
            derivations,
        )
    }

    fn fixture(scope: ScopeId, sensitivity: SensitivityClass) -> ApprovedConcept {
        ApprovedConcept::new(
            Uuid::new_v4(),
            "label",
            "definition",
            scope,
            provenance(false),
            sensitivity,
        )
    }

    #[test]
    fn default_policy_filters_above_ceiling() {
        let scope = ScopeId::new_v4();
        let candidates = vec![
            fixture(scope, SensitivityClass::Useful),
            fixture(scope, SensitivityClass::Important),
            fixture(scope, SensitivityClass::Critical),
        ];
        let decision = PolicyEngine::new().evaluate(&ExportPolicy::default(), &candidates);
        assert_eq!(decision.approved.len(), 1);
        assert_eq!(decision.rejected.len(), 2);
        assert!(decision
            .rejected
            .iter()
            .all(|r| matches!(r.reason, ExportRejectionReason::SensitivityExceeded { .. })));
    }

    /// Build a bundle whose `derivations` is empty but whose
    /// `entity_id` and `activity` are populated. This is the shape
    /// `ConceptApprovalWorkflow` produces and the policy engine must
    /// admit it under `require_provenance: true`.
    fn workflow_provenance() -> ProvenanceBundle {
        ProvenanceBundle::new(
            Uuid::new_v4(),
            SynthesisActivity::new("workflow", "model", "prompt", Uuid::new_v4()),
            ProvenanceAgent::software("workflow"),
            Vec::new(),
        )
    }

    #[test]
    fn empty_derivations_with_populated_activity_passes_provenance_check() {
        // The approval workflow attaches a bundle with empty
        // derivations because the workflow itself is the synthesis
        // activity. The default policy must admit it.
        let scope = ScopeId::new_v4();
        let mut c = fixture(scope, SensitivityClass::Useful);
        c.provenance = workflow_provenance();
        let decision = PolicyEngine::new().evaluate(&ExportPolicy::default(), &[c]);
        assert_eq!(decision.approved.len(), 1);
        assert!(decision.rejected.is_empty());
    }

    #[test]
    fn nil_entity_id_rejected_when_provenance_required() {
        let scope = ScopeId::new_v4();
        let mut c = fixture(scope, SensitivityClass::Useful);
        c.provenance = ProvenanceBundle::new(
            Uuid::nil(),
            SynthesisActivity::new("agent", "model", "prompt", Uuid::new_v4()),
            ProvenanceAgent::software("test"),
            Vec::new(),
        );
        let decision = PolicyEngine::new().evaluate(&ExportPolicy::default(), &[c]);
        assert_eq!(decision.approved.len(), 0);
        assert!(matches!(
            decision.rejected[0].reason,
            ExportRejectionReason::MissingProvenance
        ));
    }

    #[test]
    fn empty_activity_agent_rejected_when_provenance_required() {
        let scope = ScopeId::new_v4();
        let mut c = fixture(scope, SensitivityClass::Useful);
        c.provenance = ProvenanceBundle::new(
            Uuid::new_v4(),
            SynthesisActivity::new("", "model", "prompt", Uuid::new_v4()),
            ProvenanceAgent::software("test"),
            Vec::new(),
        );
        let decision = PolicyEngine::new().evaluate(&ExportPolicy::default(), &[c]);
        assert_eq!(decision.approved.len(), 0);
        assert!(matches!(
            decision.rejected[0].reason,
            ExportRejectionReason::MissingProvenance
        ));
    }

    #[test]
    fn empty_activity_model_rejected_when_provenance_required() {
        let scope = ScopeId::new_v4();
        let mut c = fixture(scope, SensitivityClass::Useful);
        c.provenance = ProvenanceBundle::new(
            Uuid::new_v4(),
            SynthesisActivity::new("agent", "", "prompt", Uuid::new_v4()),
            ProvenanceAgent::software("test"),
            Vec::new(),
        );
        let decision = PolicyEngine::new().evaluate(&ExportPolicy::default(), &[c]);
        assert_eq!(decision.approved.len(), 0);
        assert!(matches!(
            decision.rejected[0].reason,
            ExportRejectionReason::MissingProvenance
        ));
    }

    #[test]
    fn unpopulated_provenance_admitted_when_disabled() {
        // With `require_provenance: false`, even a structurally empty
        // bundle is admitted.
        let scope = ScopeId::new_v4();
        let mut c = fixture(scope, SensitivityClass::Useful);
        c.provenance = ProvenanceBundle::new(
            Uuid::nil(),
            SynthesisActivity::new("", "", "", Uuid::new_v4()),
            ProvenanceAgent::software(""),
            Vec::new(),
        );
        let p = ExportPolicy {
            require_provenance: false,
            ..ExportPolicy::default()
        };
        let decision = PolicyEngine::new().evaluate(&p, &[c]);
        assert_eq!(decision.approved.len(), 1);
    }

    #[test]
    fn populated_provenance_with_derivations_still_passes() {
        // Sanity: bundles with derivations remain admissible.
        let scope = ScopeId::new_v4();
        let mut c = fixture(scope, SensitivityClass::Useful);
        c.provenance = provenance(false);
        let decision = PolicyEngine::new().evaluate(&ExportPolicy::default(), &[c]);
        assert_eq!(decision.approved.len(), 1);
    }

    #[test]
    fn scope_whitelist_filters() {
        let scope_a = ScopeId::new_v4();
        let scope_b = ScopeId::new_v4();
        let candidates = vec![
            fixture(scope_a, SensitivityClass::Useful),
            fixture(scope_b, SensitivityClass::Useful),
        ];
        let p = ExportPolicy {
            scope_whitelist: Some(vec![scope_a.0]),
            ..ExportPolicy::default()
        };
        let decision = PolicyEngine::new().evaluate(&p, &candidates);
        assert_eq!(decision.approved.len(), 1);
        assert!(matches!(
            decision.rejected[0].reason,
            ExportRejectionReason::ScopeNotWhitelisted { .. }
        ));
    }

    #[test]
    fn max_concepts_caps_approvals() {
        let scope = ScopeId::new_v4();
        let candidates: Vec<_> = (0..5)
            .map(|_| fixture(scope, SensitivityClass::Useful))
            .collect();
        let p = ExportPolicy {
            max_concepts: 3,
            ..ExportPolicy::default()
        };
        let decision = PolicyEngine::new().evaluate(&p, &candidates);
        assert_eq!(decision.approved.len(), 3);
        assert_eq!(decision.rejected.len(), 2);
        assert!(decision
            .rejected
            .iter()
            .all(|r| matches!(r.reason, ExportRejectionReason::MaxConceptsReached)));
    }

    #[test]
    fn time_window_filters_old_concepts() {
        let scope = ScopeId::new_v4();
        let mut c = fixture(scope, SensitivityClass::Useful);
        c.approved_at = Utc::now() - chrono::Duration::seconds(7200);
        let p = ExportPolicy {
            time_window: Some(Duration::from_secs(3600)),
            ..ExportPolicy::default()
        };
        let decision = PolicyEngine::new().evaluate(&p, &[c]);
        assert_eq!(decision.approved.len(), 0);
        assert!(matches!(
            decision.rejected[0].reason,
            ExportRejectionReason::OutsideTimeWindow
        ));
    }

    #[test]
    fn expired_concept_rejected() {
        let scope = ScopeId::new_v4();
        let c = fixture(scope, SensitivityClass::Useful)
            .with_expiry(Utc::now() - chrono::Duration::seconds(60));
        let decision = PolicyEngine::new().evaluate(&ExportPolicy::default(), &[c]);
        assert_eq!(decision.approved.len(), 0);
        assert!(matches!(
            decision.rejected[0].reason,
            ExportRejectionReason::Expired
        ));
    }

    #[test]
    fn raw_evidence_blocked_for_critical_set_even_when_allowed() {
        let scope = ScopeId::new_v4();
        let c = fixture(scope, SensitivityClass::Critical);
        let p = ExportPolicy {
            allow_raw_evidence: true,
            ..ExportPolicy::permissive(SensitivityClass::Critical)
        };
        let decision = PolicyEngine::new().evaluate(&p, &[c]);
        assert!(!decision.allow_raw_evidence);
        assert!(decision.warnings.iter().any(|w| w.contains("raw evidence")));
    }

    #[test]
    fn raw_evidence_allowed_when_no_critical() {
        let scope = ScopeId::new_v4();
        let c = fixture(scope, SensitivityClass::Useful);
        let p = ExportPolicy {
            allow_raw_evidence: true,
            ..ExportPolicy::permissive(SensitivityClass::Important)
        };
        let decision = PolicyEngine::new().evaluate(&p, &[c]);
        assert!(decision.allow_raw_evidence);
    }

    #[test]
    fn raw_evidence_denied_when_policy_disabled() {
        let scope = ScopeId::new_v4();
        let c = fixture(scope, SensitivityClass::Useful);
        let p = ExportPolicy::default();
        let decision = PolicyEngine::new().evaluate(&p, &[c]);
        assert!(!decision.allow_raw_evidence);
    }
}
