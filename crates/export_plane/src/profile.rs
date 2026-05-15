//! Portable concept profile data model.
//!
//! Per `docs/DESIGN.md` §3.5 the export plane "renders" curated knowledge
//! as a *portable concept profile* — a narrow, policy-gated, JSON-style
//! object that downstream tools can consume without ever touching raw
//! evidence. Every profile is anchored in a substrate scope, carries
//! an explicit set of constraints, and points back at a list of
//! [`ApprovedConcept`]s plus optional reasoning traces.
//!
//! An [`ExportView`] is a *rendered* version of a profile: the
//! concrete content the consumer ultimately sees, gated by the
//! [`crate::policy::ExportPolicy`] / [`crate::controls::ExportControlRegistry`]
//! pipeline. Raw evidence is only ever re-emitted as part of an
//! [`EvidencePack`] when the export policy explicitly opts in.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crypto::ProvenanceBundle;
use evidence_store::ScopeId;
use memory_manager::SensitivityClass;

/// A single concept that has been approved by the export-plane
/// approval workflow and is therefore *eligible* for inclusion in an
/// [`ExportView`].
///
/// Approval does *not* imply unconditional inclusion: every concept
/// is still re-evaluated by the [`crate::policy::PolicyEngine`] at
/// render time and may be filtered out by sensitivity, scope, or
/// freshness constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedConcept {
    /// Substrate concept node id (re-uses the id used by
    /// [`concept_graph::ConceptNode`]).
    pub concept_id: Uuid,
    /// Short label.
    pub label: String,
    /// Long-form definition.
    pub definition: String,
    /// Scope this concept lives in.
    pub scope_id: ScopeId,
    /// Provenance bundle re-used from
    /// [`crypto::provenance::ProvenanceBundle`]. Always populated for
    /// approved concepts — the export policy refuses concepts without
    /// provenance unless `require_provenance = false`.
    pub provenance: ProvenanceBundle,
    /// Sensitivity class.
    pub sensitivity_class: SensitivityClass,
    /// Wall-clock approval time.
    pub approved_at: DateTime<Utc>,
    /// Optional approval expiry. After this point the concept is no
    /// longer considered approved and will be filtered out by the
    /// policy engine even if it remains in the registry.
    pub expires_at: Option<DateTime<Utc>>,
}

impl ApprovedConcept {
    /// Construct a fresh [`ApprovedConcept`] with `approved_at = now`.
    pub fn new(
        concept_id: Uuid,
        label: impl Into<String>,
        definition: impl Into<String>,
        scope_id: ScopeId,
        provenance: ProvenanceBundle,
        sensitivity_class: SensitivityClass,
    ) -> Self {
        Self {
            concept_id,
            label: label.into(),
            definition: definition.into(),
            scope_id,
            provenance,
            sensitivity_class,
            approved_at: Utc::now(),
            expires_at: None,
        }
    }

    /// Attach an expiry instant.
    pub fn with_expiry(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// True iff the approval has expired against `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| now >= t)
    }
}

/// Reference to a reasoning trace surfaced alongside an exported
/// profile. The export plane does not own the trace — it only
/// surfaces a pointer back to whichever subsystem produced it (e.g.
/// the synthesis engine).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReasoningRef {
    /// Stable id of the trace.
    pub trace_id: Uuid,
    /// Human-readable summary tag (e.g. `"weekly-digest-recap"`).
    pub tag: String,
}

impl ReasoningRef {
    /// Construct a fresh reasoning trace pointer.
    pub fn new(trace_id: Uuid, tag: impl Into<String>) -> Self {
        Self {
            trace_id,
            tag: tag.into(),
        }
    }
}

/// Constraints applied at policy-evaluation time.
///
/// Each variant is independently checked. Multiple constraints
/// stacked on a single profile are AND-ed together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportConstraint {
    /// Maximum number of concepts the export may render.
    MaxConcepts(usize),
    /// Maximum approval age. Any concept whose `approved_at` is older
    /// than this is filtered out.
    MaxAge(Duration),
    /// Whitelist of allowed scopes. A concept whose scope is not in
    /// the list is filtered out.
    ScopeRestriction(Vec<Uuid>),
    /// Sensitivity ceiling. A concept whose sensitivity is *more*
    /// sensitive than this is filtered out.
    SensitivityCeiling(SensitivityClass),
}

/// Portable concept profile — the canonical export-plane object.
///
/// A profile is a static description of *what* the substrate is
/// willing to expose to a particular downstream tool; the rendered
/// payload is produced separately as an [`ExportView`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortableConceptProfile {
    /// Stable profile id.
    pub id: Uuid,
    /// Short profile name (e.g. `"sales-onboarding-q3"`).
    pub name: String,
    /// Long-form description.
    pub description: String,
    /// Target downstream tool (e.g. `"hubspot"`, `"copilot"`).
    pub target_tool: String,
    /// Approved concepts surfaced by this profile.
    pub concepts: Vec<ApprovedConcept>,
    /// Constraints applied by the policy engine at render time.
    pub constraints: Vec<ExportConstraint>,
    /// Reasoning traces surfaced alongside the profile.
    pub reasoning_traces: Vec<ReasoningRef>,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Optional profile expiry. Once expired,
    /// [`crate::simulator::PolicySimulator::simulate`] returns an
    /// empty preview (every concept lands in `excluded_concepts`
    /// with reason `"profile expired"`) and surfaces a warning.
    /// Profile-level expiry is *not* enforced by
    /// [`crate::policy::PolicyEngine::evaluate`] \u2014 the engine
    /// receives a flat `&[ApprovedConcept]` and has no profile
    /// context. Callers that bypass the simulator must check
    /// [`Self::is_expired`] themselves.
    pub expires_at: Option<DateTime<Utc>>,
    /// Substrate scope that owns this profile (used for audit-log
    /// scoping).
    pub scope_id: ScopeId,
}

impl PortableConceptProfile {
    /// Construct a fresh empty profile.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        target_tool: impl Into<String>,
        scope_id: ScopeId,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            description: description.into(),
            target_tool: target_tool.into(),
            concepts: Vec::new(),
            constraints: Vec::new(),
            reasoning_traces: Vec::new(),
            created_at: Utc::now(),
            expires_at: None,
            scope_id,
        }
    }

    /// Attach an expiry instant.
    pub fn with_expiry(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Append an approved concept.
    pub fn push_concept(&mut self, concept: ApprovedConcept) {
        self.concepts.push(concept);
    }

    /// Append a constraint.
    pub fn push_constraint(&mut self, constraint: ExportConstraint) {
        self.constraints.push(constraint);
    }

    /// Append a reasoning trace pointer.
    pub fn push_trace(&mut self, trace: ReasoningRef) {
        self.reasoning_traces.push(trace);
    }

    /// True iff the profile has expired against `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| now >= t)
    }
}

/// Evidence pack surfaced inside an export view.
///
/// Per `docs/DESIGN.md` §3.5 the export plane *never* emits raw evidence
/// unless the [`crate::policy::ExportPolicy::allow_raw_evidence`] flag
/// is explicitly true *and* every concept covered by the pack has a
/// sensitivity strictly below [`SensitivityClass::Critical`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePack {
    /// Evidence rows surfaced by the pack.
    pub evidence_refs: Vec<crypto::EvidenceRef>,
    /// Which concept ids this pack supports.
    pub concept_ids: Vec<Uuid>,
}

impl EvidencePack {
    /// Construct an empty evidence pack.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Variants of rendered content surfaced by an [`ExportView`].
///
/// `ConceptsOnly` is the default least-privilege option; the other
/// two options are gated by the [`crate::policy::ExportPolicy`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportViewContent {
    /// Only the concept set is surfaced.
    ConceptsOnly {
        /// Concepts surfaced.
        concepts: Vec<ApprovedConcept>,
    },
    /// Concepts plus a list of summaries (e.g. channel recap text).
    WithSummaries {
        /// Concepts surfaced.
        concepts: Vec<ApprovedConcept>,
        /// Approved summaries (id + body).
        summaries: Vec<ApprovedSummary>,
    },
    /// Concepts + summaries + an explicit evidence pack.
    WithEvidencePack {
        /// Concepts surfaced.
        concepts: Vec<ApprovedConcept>,
        /// Approved summaries (id + body).
        summaries: Vec<ApprovedSummary>,
        /// Evidence pack — only included when the export policy
        /// allows raw evidence and no concept is `Critical`.
        evidence_pack: EvidencePack,
    },
}

impl ExportViewContent {
    /// Borrow the concept list regardless of content variant.
    pub fn concepts(&self) -> &[ApprovedConcept] {
        match self {
            Self::ConceptsOnly { concepts }
            | Self::WithSummaries { concepts, .. }
            | Self::WithEvidencePack { concepts, .. } => concepts,
        }
    }

    /// Borrow the summary list (empty when not present).
    pub fn summaries(&self) -> &[ApprovedSummary] {
        match self {
            Self::ConceptsOnly { .. } => &[],
            Self::WithSummaries { summaries, .. } | Self::WithEvidencePack { summaries, .. } => {
                summaries
            }
        }
    }

    /// Borrow the evidence pack if present.
    pub fn evidence_pack(&self) -> Option<&EvidencePack> {
        if let Self::WithEvidencePack { evidence_pack, .. } = self {
            Some(evidence_pack)
        } else {
            None
        }
    }
}

/// One approved summary surfaced inside an [`ExportViewContent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedSummary {
    /// Substrate summary id.
    pub summary_id: Uuid,
    /// Substrate scope this summary belongs to. Mirrors
    /// [`ApprovedConcept::scope_id`] so consumers of the rendered
    /// [`ExportView`] can attribute each surfaced summary back to a
    /// scope without having to cross-reference the
    /// [`crate::controls::ExportControlRegistry`].
    pub scope_id: ScopeId,
    /// Summary body (post-redaction; see
    /// [`crate::controls::SummaryExportControl::redaction_level`]).
    pub body: String,
}

/// Rendered export view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportView {
    /// Stable view id.
    pub id: Uuid,
    /// Profile this view was rendered from.
    pub profile_id: Uuid,
    /// Wall-clock render time.
    pub rendered_at: DateTime<Utc>,
    /// Rendered content.
    pub content: ExportViewContent,
    /// Substrate scope that owns the rendered view.
    pub scope_id: ScopeId,
}

impl ExportView {
    /// Construct a fresh view stamped at `now`.
    ///
    /// This constructor is `pub(crate)` so callers outside the
    /// crate cannot bypass the policy engine. The supported public
    /// entry point is [`Self::from_decision`] in
    /// [`crate::policy`], which accepts an
    /// [`crate::policy::ExportDecision`] and uses *its* approved
    /// concept set as the canonical source of truth.
    pub(crate) fn new(profile_id: Uuid, scope_id: ScopeId, content: ExportViewContent) -> Self {
        Self {
            id: Uuid::new_v4(),
            profile_id,
            rendered_at: Utc::now(),
            content,
            scope_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::{EvidenceRef, ProvenanceAgent, SynthesisActivity};

    fn fixture_provenance() -> ProvenanceBundle {
        ProvenanceBundle::new(
            Uuid::new_v4(),
            SynthesisActivity::new("test-agent", "bonsai-1.7b@v1", "synth.test", Uuid::new_v4()),
            ProvenanceAgent::software("test"),
            vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        )
    }

    #[test]
    fn approved_concept_constructs_with_now() {
        let scope = ScopeId::new_v4();
        let concept = ApprovedConcept::new(
            Uuid::new_v4(),
            "Atlas",
            "Q3 launch",
            scope,
            fixture_provenance(),
            SensitivityClass::Useful,
        );
        assert_eq!(concept.label, "Atlas");
        assert!(concept.expires_at.is_none());
    }

    #[test]
    fn approved_concept_expiry_check() {
        let scope = ScopeId::new_v4();
        let concept = ApprovedConcept::new(
            Uuid::new_v4(),
            "Atlas",
            "Q3 launch",
            scope,
            fixture_provenance(),
            SensitivityClass::Useful,
        )
        .with_expiry(Utc::now() - chrono::Duration::seconds(60));
        assert!(concept.is_expired(Utc::now()));
    }

    #[test]
    fn portable_profile_builders() {
        let scope = ScopeId::new_v4();
        let mut p = PortableConceptProfile::new("name", "desc", "tool", scope);
        let concept = ApprovedConcept::new(
            Uuid::new_v4(),
            "Atlas",
            "Q3",
            scope,
            fixture_provenance(),
            SensitivityClass::Useful,
        );
        p.push_concept(concept);
        p.push_constraint(ExportConstraint::MaxConcepts(10));
        p.push_trace(ReasoningRef::new(Uuid::new_v4(), "trace"));
        assert_eq!(p.concepts.len(), 1);
        assert_eq!(p.constraints.len(), 1);
        assert_eq!(p.reasoning_traces.len(), 1);
    }

    #[test]
    fn export_view_helpers() {
        let scope = ScopeId::new_v4();
        let concept = ApprovedConcept::new(
            Uuid::new_v4(),
            "Atlas",
            "Q3",
            scope,
            fixture_provenance(),
            SensitivityClass::Useful,
        );
        let summary = ApprovedSummary {
            summary_id: Uuid::new_v4(),
            scope_id: scope,
            body: "body".into(),
        };
        let pack = EvidencePack {
            evidence_refs: vec![EvidenceRef::from_uuid(Uuid::new_v4())],
            concept_ids: vec![concept.concept_id],
        };
        let v = ExportView::new(
            Uuid::new_v4(),
            scope,
            ExportViewContent::WithEvidencePack {
                concepts: vec![concept.clone()],
                summaries: vec![summary.clone()],
                evidence_pack: pack.clone(),
            },
        );
        assert_eq!(v.content.concepts().len(), 1);
        assert_eq!(v.content.summaries().len(), 1);
        assert!(v.content.evidence_pack().is_some());
    }

    #[test]
    fn export_view_concepts_only_has_no_summaries_or_pack() {
        let scope = ScopeId::new_v4();
        let v = ExportView::new(
            Uuid::new_v4(),
            scope,
            ExportViewContent::ConceptsOnly { concepts: vec![] },
        );
        assert!(v.content.summaries().is_empty());
        assert!(v.content.evidence_pack().is_none());
    }
}
