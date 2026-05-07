//! Concept approval workflow.
//!
//! Bridges canonical concepts in [`concept_graph`] to
//! [`crate::profile::ApprovedConcept`]s on the export plane. The
//! workflow is the **only** way a canonical concept becomes
//! eligible for inclusion in a [`crate::profile::PortableConceptProfile`]
//! — every approval is gated by the [`crate::controls::ExportControlRegistry`].

use std::collections::HashMap;

use chrono::Utc;
use thiserror::Error;
use uuid::Uuid;

use concept_graph::{ConceptGraph, NodeId, NodeState};
use crypto::{ProvenanceAgent, ProvenanceBundle, SynthesisActivity};
use evidence_store::ScopeId;
use memory_manager::SensitivityClass;

use crate::controls::ExportControlRegistry;
use crate::profile::ApprovedConcept;

/// Errors raised by [`ConceptApprovalWorkflow`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApprovalError {
    /// Concept does not exist in the supplied [`ConceptGraph`].
    #[error("concept not found: {0}")]
    NotFound(Uuid),
    /// Concept is not in the [`NodeState::Canonical`] state.
    #[error("concept {0} is not canonical")]
    NotCanonical(Uuid),
    /// Concept has no [`crate::controls::ConceptExportControl`] entry,
    /// or the entry forbids export.
    #[error("concept {0} is not exportable per the control registry")]
    NotExportable(Uuid),
    /// Concept was already approved (used by `approve_for_export`).
    #[error("concept {0} is already approved")]
    AlreadyApproved(Uuid),
    /// Tried to revoke an approval that does not exist.
    #[error("concept {0} is not currently approved")]
    NotApproved(Uuid),
}

/// In-memory approval workflow.
#[derive(Debug, Default, Clone)]
pub struct ConceptApprovalWorkflow {
    approved: HashMap<Uuid, ApprovedConcept>,
}

impl ConceptApprovalWorkflow {
    /// Construct an empty workflow.
    pub fn new() -> Self {
        Self::default()
    }

    /// Approve a canonical concept for export.
    ///
    /// * The concept must exist in `graph`.
    /// * The concept must be in [`NodeState::Canonical`].
    /// * The concept must have an [`crate::controls::ConceptExportControl`]
    ///   in `controls` and that control must currently authorise
    ///   export (`exportable == true`, time-bound not exceeded).
    /// * `profile_id` is consulted against the control's
    ///   `allowed_profiles` whitelist.
    pub fn approve_for_export(
        &mut self,
        concept_id: Uuid,
        scope: ScopeId,
        profile_id: Uuid,
        graph: &ConceptGraph,
        controls: &ExportControlRegistry,
    ) -> Result<ApprovedConcept, ApprovalError> {
        let node_id = NodeId::from_uuid(concept_id);
        let node = graph
            .get_node(node_id)
            .ok_or(ApprovalError::NotFound(concept_id))?;
        if node.state != NodeState::Canonical {
            return Err(ApprovalError::NotCanonical(concept_id));
        }
        if !controls.allows_concept(concept_id, profile_id, scope.0, Utc::now()) {
            return Err(ApprovalError::NotExportable(concept_id));
        }
        if self.approved.contains_key(&concept_id) {
            return Err(ApprovalError::AlreadyApproved(concept_id));
        }

        // Build a provenance bundle attesting to the approval. The
        // approval workflow itself is a synthesis activity (it
        // *produces* the approved-concept entity).
        let provenance = ProvenanceBundle::new(
            concept_id,
            SynthesisActivity::new(
                "export_plane:approval_workflow",
                "export_plane@v1",
                "concept.approve.v1",
                Uuid::new_v4(),
            ),
            ProvenanceAgent::software("export_plane:approval_workflow"),
            Vec::new(),
        );

        let approved = ApprovedConcept::new(
            concept_id,
            node.label.clone(),
            node.definition.clone(),
            scope,
            provenance,
            sensitivity_for_concept(node),
        );
        self.approved.insert(concept_id, approved.clone());
        Ok(approved)
    }

    /// Revoke an existing approval.
    pub fn revoke_approval(&mut self, concept_id: Uuid) -> Result<(), ApprovalError> {
        self.approved
            .remove(&concept_id)
            .map(|_| ())
            .ok_or(ApprovalError::NotApproved(concept_id))
    }

    /// Borrow the approved-concept record for `concept_id`, if any.
    pub fn get(&self, concept_id: Uuid) -> Option<&ApprovedConcept> {
        self.approved.get(&concept_id)
    }

    /// List every concept currently approved within `scope`.
    pub fn list_approved(&self, scope: ScopeId) -> Vec<ApprovedConcept> {
        self.approved
            .values()
            .filter(|c| c.scope_id == scope)
            .cloned()
            .collect()
    }

    /// List every approved concept across every scope.
    pub fn list_all(&self) -> Vec<ApprovedConcept> {
        self.approved.values().cloned().collect()
    }
}

/// The substrate's [`concept_graph::ConceptNode`] does not (yet)
/// carry a `SensitivityClass` field — concept sensitivity is
/// inherited from the most-sensitive supporting evidence row.
/// Phase 5's export plane treats canonical concepts as `Useful`
/// by default; future phases will let callers override this when
/// they hand the concept to the approval workflow.
fn sensitivity_for_concept(_node: &concept_graph::ConceptNode) -> SensitivityClass {
    SensitivityClass::Useful
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::ConceptExportControl;
    use concept_graph::ConceptNode;

    fn graph_with_concept(state: NodeState) -> (ConceptGraph, Uuid) {
        let mut graph = ConceptGraph::new();
        let scope = ScopeId::new_v4();
        let mut node = ConceptNode::new_candidate("label", "definition", scope);
        node.state = state;
        let id = node.id;
        graph.add_node(node).expect("add");
        (graph, id.0)
    }

    fn registry_with(concept_id: Uuid) -> ExportControlRegistry {
        let mut r = ExportControlRegistry::new();
        r.insert_concept(ConceptExportControl::new(concept_id))
            .expect("insert");
        r
    }

    #[test]
    fn approve_canonical_concept_succeeds() {
        let (graph, id) = graph_with_concept(NodeState::Canonical);
        let registry = registry_with(id);
        let mut wf = ConceptApprovalWorkflow::new();
        let approved = wf
            .approve_for_export(id, ScopeId::new_v4(), Uuid::new_v4(), &graph, &registry)
            .expect("approve");
        assert_eq!(approved.concept_id, id);
        assert!(wf.get(id).is_some());
    }

    #[test]
    fn approve_unknown_concept_errors() {
        let graph = ConceptGraph::new();
        let registry = ExportControlRegistry::new();
        let mut wf = ConceptApprovalWorkflow::new();
        let id = Uuid::new_v4();
        let err = wf
            .approve_for_export(id, ScopeId::new_v4(), Uuid::new_v4(), &graph, &registry)
            .expect_err("unknown");
        assert_eq!(err, ApprovalError::NotFound(id));
    }

    #[test]
    fn approve_non_canonical_concept_errors() {
        let (graph, id) = graph_with_concept(NodeState::Candidate);
        let registry = registry_with(id);
        let mut wf = ConceptApprovalWorkflow::new();
        let err = wf
            .approve_for_export(id, ScopeId::new_v4(), Uuid::new_v4(), &graph, &registry)
            .expect_err("not canonical");
        assert_eq!(err, ApprovalError::NotCanonical(id));
    }

    #[test]
    fn approve_blocked_when_no_control_entry() {
        let (graph, id) = graph_with_concept(NodeState::Canonical);
        let registry = ExportControlRegistry::new();
        let mut wf = ConceptApprovalWorkflow::new();
        let err = wf
            .approve_for_export(id, ScopeId::new_v4(), Uuid::new_v4(), &graph, &registry)
            .expect_err("not exportable");
        assert_eq!(err, ApprovalError::NotExportable(id));
    }

    #[test]
    fn approve_blocked_when_control_disables_export() {
        let (graph, id) = graph_with_concept(NodeState::Canonical);
        let mut registry = ExportControlRegistry::new();
        let mut control = ConceptExportControl::new(id);
        control.exportable = false;
        registry.insert_concept(control).expect("insert");
        let mut wf = ConceptApprovalWorkflow::new();
        let err = wf
            .approve_for_export(id, ScopeId::new_v4(), Uuid::new_v4(), &graph, &registry)
            .expect_err("not exportable");
        assert_eq!(err, ApprovalError::NotExportable(id));
    }

    #[test]
    fn duplicate_approval_errors() {
        let (graph, id) = graph_with_concept(NodeState::Canonical);
        let registry = registry_with(id);
        let mut wf = ConceptApprovalWorkflow::new();
        let scope = ScopeId::new_v4();
        let profile = Uuid::new_v4();
        wf.approve_for_export(id, scope, profile, &graph, &registry)
            .expect("first");
        let err = wf
            .approve_for_export(id, scope, profile, &graph, &registry)
            .expect_err("duplicate");
        assert_eq!(err, ApprovalError::AlreadyApproved(id));
    }

    #[test]
    fn revoke_removes_approval() {
        let (graph, id) = graph_with_concept(NodeState::Canonical);
        let registry = registry_with(id);
        let mut wf = ConceptApprovalWorkflow::new();
        wf.approve_for_export(id, ScopeId::new_v4(), Uuid::new_v4(), &graph, &registry)
            .expect("approve");
        wf.revoke_approval(id).expect("revoke");
        assert!(wf.get(id).is_none());
    }

    #[test]
    fn revoke_unknown_errors() {
        let mut wf = ConceptApprovalWorkflow::new();
        let id = Uuid::new_v4();
        let err = wf.revoke_approval(id).expect_err("missing");
        assert_eq!(err, ApprovalError::NotApproved(id));
    }

    #[test]
    fn list_filters_by_scope() {
        let (graph_a, id_a) = graph_with_concept(NodeState::Canonical);
        let (graph_b, id_b) = graph_with_concept(NodeState::Canonical);
        let scope_a = ScopeId::new_v4();
        let scope_b = ScopeId::new_v4();

        let mut registry = ExportControlRegistry::new();
        registry
            .insert_concept(ConceptExportControl::new(id_a))
            .expect("a");
        registry
            .insert_concept(ConceptExportControl::new(id_b))
            .expect("b");

        let mut wf = ConceptApprovalWorkflow::new();
        wf.approve_for_export(id_a, scope_a, Uuid::new_v4(), &graph_a, &registry)
            .expect("a");
        wf.approve_for_export(id_b, scope_b, Uuid::new_v4(), &graph_b, &registry)
            .expect("b");

        let list_a = wf.list_approved(scope_a);
        assert_eq!(list_a.len(), 1);
        assert_eq!(list_a[0].concept_id, id_a);

        let list_b = wf.list_approved(scope_b);
        assert_eq!(list_b.len(), 1);
        assert_eq!(list_b[0].concept_id, id_b);

        assert_eq!(wf.list_all().len(), 2);
    }
}
