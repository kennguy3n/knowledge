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
use evidence_store::ScopeId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use memory_manager::SensitivityClass;

use crate::profile::{
    ApprovedConcept, ApprovedSummary, EvidencePack, ExportConstraint, ExportView, ExportViewContent,
};

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

    /// Fold the constraints attached to a profile into a stricter
    /// effective policy. Profile constraints can only *tighten* the
    /// policy — they never relax it — so an export emitted under the
    /// resulting policy is at least as restrictive as one emitted
    /// under the bare policy alone.
    ///
    /// Folding rules:
    ///
    /// * [`ExportConstraint::MaxConcepts(n)`] —
    ///   `effective.max_concepts = min(self.max_concepts, n)`.
    /// * [`ExportConstraint::MaxAge(d)`] — tightens
    ///   [`Self::time_window`]: `None` (no window) becomes
    ///   `Some(d)`, and `Some(existing)` becomes
    ///   `Some(min(existing, d))`.
    /// * [`ExportConstraint::ScopeRestriction(scopes)`] — tightens
    ///   [`Self::scope_whitelist`] by set intersection. `None` (any
    ///   scope) becomes `Some(scopes)`; `Some(existing)` becomes
    ///   the intersection of `existing` and `scopes`. An empty
    ///   intersection means "no scopes admitted".
    /// * [`ExportConstraint::SensitivityCeiling(class)`] — tightens
    ///   [`Self::sensitivity_ceiling`] to whichever of `self` and
    ///   `class` has the lower [`rank`] (lower rank = more
    ///   restrictive).
    ///
    /// Multiple constraints of the same kind on a single profile
    /// are folded left-to-right; each successive constraint
    /// tightens further. The original policy is consumed and the
    /// stricter copy is returned, so callers can chain
    /// `policy.with_constraints(&profile.constraints)` without
    /// cloning explicitly.
    pub fn with_constraints(mut self, constraints: &[ExportConstraint]) -> Self {
        for c in constraints {
            match c {
                ExportConstraint::MaxConcepts(n) => {
                    self.max_concepts = self.max_concepts.min(*n);
                }
                ExportConstraint::MaxAge(d) => {
                    self.time_window = Some(match self.time_window {
                        Some(existing) => existing.min(*d),
                        None => *d,
                    });
                }
                ExportConstraint::ScopeRestriction(scopes) => {
                    self.scope_whitelist = Some(match self.scope_whitelist.take() {
                        Some(existing) => existing
                            .into_iter()
                            .filter(|s| scopes.contains(s))
                            .collect(),
                        None => scopes.clone(),
                    });
                }
                ExportConstraint::SensitivityCeiling(class) => {
                    if rank(*class) < rank(self.sensitivity_ceiling) {
                        self.sensitivity_ceiling = *class;
                    }
                }
            }
        }
        self
    }
}

/// Content-variant selector for [`ExportView::from_decision`].
///
/// The variants mirror [`ExportViewContent`], but the concept set is
/// elided — the policy decision's `approved` list is the canonical
/// source of truth for which concepts the rendered view surfaces.
#[derive(Debug, Clone, Default)]
pub enum ExportViewRequest {
    /// Render only the engine-approved concepts.
    #[default]
    ConceptsOnly,
    /// Render concepts plus the supplied approved summaries.
    WithSummaries {
        /// Summaries to surface alongside the concept set.
        summaries: Vec<ApprovedSummary>,
    },
    /// Render concepts plus summaries plus a raw evidence pack.
    /// Allowed only when [`ExportDecision::allow_raw_evidence`] is
    /// `true`. The engine ANDs `policy.allow_raw_evidence` with the
    /// absence of any `Critical` concept in the approved set, so
    /// this guard prevents callers from rendering evidence the
    /// policy refused to permit.
    WithEvidencePack {
        /// Summaries to surface alongside the concept set.
        summaries: Vec<ApprovedSummary>,
        /// Raw evidence pack — only included when
        /// `decision.allow_raw_evidence` is true.
        evidence_pack: EvidencePack,
    },
}

/// Errors returned by [`ExportView::from_decision`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExportViewError {
    /// Caller asked for [`ExportViewRequest::WithEvidencePack`] but
    /// the supplied [`ExportDecision::allow_raw_evidence`] is
    /// `false` — either the policy refused raw evidence outright,
    /// or the engine suppressed it because the approved set
    /// contains a `Critical` concept.
    #[error("raw evidence requested but decision.allow_raw_evidence is false")]
    RawEvidenceNotAuthorised,
}

impl ExportView {
    /// Construct an [`ExportView`] from an [`ExportDecision`].
    ///
    /// This is the only public way to mint an [`ExportView`]. The
    /// raw [`ExportView::new`] constructor is `pub(crate)` so
    /// callers outside this crate cannot fabricate views whose
    /// concepts bypass [`PolicyEngine::evaluate`]. The view's
    /// concepts are always `decision.approved` — the engine's
    /// approved set is the canonical source of truth.
    ///
    /// A [`ExportViewRequest::WithEvidencePack`] request whose
    /// decision did not authorise raw evidence is rejected with
    /// [`ExportViewError::RawEvidenceNotAuthorised`].
    pub fn from_decision(
        decision: &ExportDecision,
        profile_id: Uuid,
        scope_id: ScopeId,
        request: ExportViewRequest,
    ) -> Result<Self, ExportViewError> {
        let concepts = decision.approved.clone();
        let content = match request {
            ExportViewRequest::ConceptsOnly => ExportViewContent::ConceptsOnly { concepts },
            ExportViewRequest::WithSummaries { summaries } => ExportViewContent::WithSummaries {
                concepts,
                summaries,
            },
            ExportViewRequest::WithEvidencePack {
                summaries,
                evidence_pack,
            } => {
                if !decision.allow_raw_evidence {
                    return Err(ExportViewError::RawEvidenceNotAuthorised);
                }
                ExportViewContent::WithEvidencePack {
                    concepts,
                    summaries,
                    evidence_pack,
                }
            }
        };
        Ok(Self::new(profile_id, scope_id, content))
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

    // F-7 — profile constraints become effective via
    // `ExportPolicy::with_constraints`.

    #[test]
    fn with_constraints_max_concepts_tightens() {
        let p = ExportPolicy {
            max_concepts: 10,
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[ExportConstraint::MaxConcepts(5)]);
        assert_eq!(folded.max_concepts, 5);
    }

    #[test]
    fn with_constraints_max_concepts_does_not_loosen() {
        let p = ExportPolicy {
            max_concepts: 5,
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[ExportConstraint::MaxConcepts(10)]);
        assert_eq!(folded.max_concepts, 5);
    }

    #[test]
    fn with_constraints_max_age_replaces_none() {
        let p = ExportPolicy {
            time_window: None,
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[ExportConstraint::MaxAge(Duration::from_secs(3600))]);
        assert_eq!(folded.time_window, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn with_constraints_max_age_tightens_existing() {
        let p = ExportPolicy {
            time_window: Some(Duration::from_secs(86_400)),
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[ExportConstraint::MaxAge(Duration::from_secs(3600))]);
        assert_eq!(folded.time_window, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn with_constraints_max_age_does_not_loosen_existing() {
        let p = ExportPolicy {
            time_window: Some(Duration::from_secs(3600)),
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[ExportConstraint::MaxAge(Duration::from_secs(86_400))]);
        assert_eq!(folded.time_window, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn with_constraints_scope_restriction_replaces_none() {
        let scope_a = Uuid::new_v4();
        let scope_b = Uuid::new_v4();
        let policy = ExportPolicy {
            scope_whitelist: None,
            ..ExportPolicy::default()
        };
        let folded =
            policy.with_constraints(&[ExportConstraint::ScopeRestriction(vec![scope_a, scope_b])]);
        assert_eq!(folded.scope_whitelist, Some(vec![scope_a, scope_b]));
    }

    #[test]
    fn with_constraints_scope_restriction_intersects_existing() {
        let scope_a = Uuid::new_v4();
        let scope_b = Uuid::new_v4();
        let scope_c = Uuid::new_v4();
        let scope_d = Uuid::new_v4();
        let policy = ExportPolicy {
            scope_whitelist: Some(vec![scope_a, scope_b, scope_c]),
            ..ExportPolicy::default()
        };
        let folded = policy.with_constraints(&[ExportConstraint::ScopeRestriction(vec![
            scope_b, scope_c, scope_d,
        ])]);
        // Order is preserved from `existing`; intersection is
        // `[scope_b, scope_c]`.
        assert_eq!(folded.scope_whitelist, Some(vec![scope_b, scope_c]));
    }

    #[test]
    fn with_constraints_scope_restriction_disjoint_yields_empty_whitelist() {
        let scope_a = Uuid::new_v4();
        let scope_b = Uuid::new_v4();
        let policy = ExportPolicy {
            scope_whitelist: Some(vec![scope_a]),
            ..ExportPolicy::default()
        };
        let folded = policy.with_constraints(&[ExportConstraint::ScopeRestriction(vec![scope_b])]);
        assert_eq!(folded.scope_whitelist, Some(Vec::<Uuid>::new()));
    }

    #[test]
    fn with_constraints_sensitivity_ceiling_tightens() {
        let p = ExportPolicy {
            sensitivity_ceiling: SensitivityClass::Important,
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[ExportConstraint::SensitivityCeiling(
            SensitivityClass::Useful,
        )]);
        assert_eq!(folded.sensitivity_ceiling, SensitivityClass::Useful);
    }

    #[test]
    fn with_constraints_sensitivity_ceiling_does_not_loosen() {
        let p = ExportPolicy {
            sensitivity_ceiling: SensitivityClass::Useful,
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[ExportConstraint::SensitivityCeiling(
            SensitivityClass::Important,
        )]);
        assert_eq!(folded.sensitivity_ceiling, SensitivityClass::Useful);
    }

    #[test]
    fn with_constraints_multiple_same_kind_fold_left_to_right() {
        let p = ExportPolicy {
            max_concepts: 100,
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[
            ExportConstraint::MaxConcepts(50),
            ExportConstraint::MaxConcepts(20),
            ExportConstraint::MaxConcepts(80),
        ]);
        // Each successive constraint can only tighten; the floor is
        // the strictest one seen so far.
        assert_eq!(folded.max_concepts, 20);
    }

    #[test]
    fn with_constraints_all_kinds_compose() {
        let scope = Uuid::new_v4();
        let p = ExportPolicy {
            max_concepts: 100,
            time_window: Some(Duration::from_secs(86_400)),
            sensitivity_ceiling: SensitivityClass::Critical,
            scope_whitelist: None,
            ..ExportPolicy::default()
        };
        let folded = p.with_constraints(&[
            ExportConstraint::MaxConcepts(10),
            ExportConstraint::MaxAge(Duration::from_secs(3600)),
            ExportConstraint::SensitivityCeiling(SensitivityClass::Useful),
            ExportConstraint::ScopeRestriction(vec![scope]),
        ]);
        assert_eq!(folded.max_concepts, 10);
        assert_eq!(folded.time_window, Some(Duration::from_secs(3600)));
        assert_eq!(folded.sensitivity_ceiling, SensitivityClass::Useful);
        assert_eq!(folded.scope_whitelist, Some(vec![scope]));
    }

    #[test]
    fn with_constraints_empty_list_is_identity() {
        let p = ExportPolicy {
            max_concepts: 7,
            time_window: Some(Duration::from_secs(900)),
            sensitivity_ceiling: SensitivityClass::Important,
            scope_whitelist: Some(vec![Uuid::new_v4()]),
            ..ExportPolicy::default()
        };
        let original = p.clone();
        let folded = p.with_constraints(&[]);
        assert_eq!(folded.max_concepts, original.max_concepts);
        assert_eq!(folded.time_window, original.time_window);
        assert_eq!(folded.sensitivity_ceiling, original.sensitivity_ceiling);
        assert_eq!(folded.scope_whitelist, original.scope_whitelist);
    }
}
