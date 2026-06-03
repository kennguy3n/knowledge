//! Drift detection for canonical concepts.
//!
//! Per `docs/technical/design.md` §11.1, *drift* fires when the evidence base
//! supporting a canonical claim changes — the underlying evidence
//! is superseded, removed, or weakened — even if no opposing
//! claim has been promoted yet. The detector is intentionally
//! cheap (no SLM calls) so it can run on every observation
//! commit. The output is a [`DriftMarker`] that the caller can
//! attach to a node to surface "evidence shifted" in the UI.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use concept_graph::{ConceptGraph, NodeId};
use evidence_store::EvidenceId;
use serde::{Deserialize, Serialize};

/// Why the evidence base for a canonical node has shifted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftReason {
    /// One or more pieces of evidence were superseded by newer
    /// evidence rows.
    EvidenceSuperseded,
    /// One or more pieces of evidence were removed (forgotten,
    /// purged, or quarantined).
    EvidenceRemoved,
    /// The evidence base shrank below the configured floor.
    EvidenceWeakened,
}

/// A flag attached to a node indicating its evidence base has
/// shifted. The marker is *informational* — the node remains
/// canonical until a synthesizer / human re-evaluates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftMarker {
    /// The node whose evidence shifted.
    pub node: NodeId,
    /// Reason for the marker.
    pub reason: DriftReason,
    /// Number of evidence rows present at promotion time.
    pub evidence_at_promotion: usize,
    /// Number of evidence rows still valid at detection time.
    pub evidence_remaining: usize,
    /// Wall-clock time the marker was emitted.
    pub detected_at: DateTime<Utc>,
}

/// Snapshot of a node's evidence base at promotion time, plus
/// the runtime view of which evidence rows are still valid.
///
/// In production this would be loaded from the
/// `concept_graph` / `evidence_store` join; here we accept it as
/// an explicit input so the detector stays storage-agnostic and
/// trivially testable.
#[derive(Debug, Clone)]
pub struct EvidenceSnapshot {
    /// The evidence rows the node was promoted on.
    pub baseline: Vec<EvidenceId>,
    /// The subset of `baseline` that are still valid (not
    /// superseded, not removed).
    pub still_valid: Vec<EvidenceId>,
    /// The evidence rows from `baseline` that have been
    /// superseded by newer rows.
    pub superseded: Vec<EvidenceId>,
    /// The evidence rows from `baseline` that have been removed
    /// outright.
    pub removed: Vec<EvidenceId>,
}

impl EvidenceSnapshot {
    /// Convenience constructor — partition a baseline into
    /// `still_valid` / `superseded` / `removed` given the
    /// surviving set and the superseded set. Anything in
    /// `baseline` not in either bucket is treated as removed.
    pub fn partition(
        baseline: Vec<EvidenceId>,
        still_valid: Vec<EvidenceId>,
        superseded: Vec<EvidenceId>,
    ) -> Self {
        let mut removed = Vec::new();
        for e in &baseline {
            if !still_valid.contains(e) && !superseded.contains(e) {
                removed.push(*e);
            }
        }
        Self {
            baseline,
            still_valid,
            superseded,
            removed,
        }
    }
}

/// Detector that emits [`DriftMarker`]s for canonical nodes
/// whose evidence base has shifted.
#[derive(Debug, Clone)]
pub struct DriftDetector {
    /// Minimum surviving-evidence ratio (in `[0.0, 1.0]`) below
    /// which a node is flagged as `EvidenceWeakened`. Defaults
    /// to `0.5`.
    pub weaken_ratio: f64,
}

impl Default for DriftDetector {
    fn default() -> Self {
        Self { weaken_ratio: 0.5 }
    }
}

impl DriftDetector {
    /// Construct a detector with the default weaken ratio.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the weaken ratio.
    pub fn with_weaken_ratio(mut self, ratio: f64) -> Self {
        self.weaken_ratio = ratio;
        self
    }

    /// Walk every canonical node in `graph` and return a
    /// [`DriftMarker`] for each whose snapshot in `snapshots`
    /// indicates drift. Nodes without a snapshot are skipped.
    pub fn scan(
        &self,
        graph: &ConceptGraph,
        snapshots: &HashMap<NodeId, EvidenceSnapshot>,
    ) -> Vec<DriftMarker> {
        let now = Utc::now();
        let mut out = Vec::new();
        for node in graph
            .iter_nodes()
            .filter(|n| n.state == concept_graph::NodeState::Canonical)
        {
            let Some(snap) = snapshots.get(&node.id) else {
                continue;
            };
            let baseline = snap.baseline.len();
            if baseline == 0 {
                continue;
            }
            let remaining = snap.still_valid.len();
            let reason = if !snap.removed.is_empty() {
                Some(DriftReason::EvidenceRemoved)
            } else if !snap.superseded.is_empty() {
                Some(DriftReason::EvidenceSuperseded)
            } else if (remaining as f64 / baseline as f64) < self.weaken_ratio {
                Some(DriftReason::EvidenceWeakened)
            } else {
                None
            };
            if let Some(reason) = reason {
                out.push(DriftMarker {
                    node: node.id,
                    reason,
                    evidence_at_promotion: baseline,
                    evidence_remaining: remaining,
                    detected_at: now,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concept_graph::{ConceptGraph, ConceptNode, NodeState};
    use evidence_store::ScopeId;

    fn canonical(scope: ScopeId, label: &str) -> ConceptNode {
        let mut n = ConceptNode::new_candidate(label.to_string(), String::new(), scope);
        n.state = NodeState::Canonical;
        n
    }

    #[test]
    fn flags_superseded_evidence() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let n = canonical(scope, "claim");
        let id = n.id;
        g.add_node(n).unwrap();
        let e1 = EvidenceId::new_v4();
        let e2 = EvidenceId::new_v4();
        let mut snaps = HashMap::new();
        snaps.insert(
            id,
            EvidenceSnapshot::partition(vec![e1, e2], vec![e2], vec![e1]),
        );
        let markers = DriftDetector::new().scan(&g, &snaps);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].reason, DriftReason::EvidenceSuperseded);
        assert_eq!(markers[0].evidence_at_promotion, 2);
        assert_eq!(markers[0].evidence_remaining, 1);
    }

    #[test]
    fn flags_removed_evidence() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let n = canonical(scope, "claim");
        let id = n.id;
        g.add_node(n).unwrap();
        let e1 = EvidenceId::new_v4();
        let e2 = EvidenceId::new_v4();
        let mut snaps = HashMap::new();
        snaps.insert(
            id,
            EvidenceSnapshot::partition(vec![e1, e2], vec![e2], vec![]),
        );
        let markers = DriftDetector::new().scan(&g, &snaps);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].reason, DriftReason::EvidenceRemoved);
    }

    #[test]
    fn flags_weakened_when_below_ratio() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let n = canonical(scope, "claim");
        let id = n.id;
        g.add_node(n).unwrap();
        // 4-row baseline; 1 still valid; 3 *neither* superseded
        // nor removed-from-baseline → exercise the explicit
        // "weakened" branch by using snapshots constructed
        // directly so the partition helper doesn't reclassify
        // the missing rows as `Removed`.
        let baseline: Vec<EvidenceId> = (0..4).map(|_| EvidenceId::new_v4()).collect();
        let snap = EvidenceSnapshot {
            baseline: baseline.clone(),
            still_valid: vec![baseline[0]],
            superseded: vec![],
            removed: vec![],
        };
        let mut snaps = HashMap::new();
        snaps.insert(id, snap);
        let markers = DriftDetector::new().with_weaken_ratio(0.5).scan(&g, &snaps);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].reason, DriftReason::EvidenceWeakened);
    }

    #[test]
    fn skips_nodes_without_snapshots() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        g.add_node(canonical(scope, "claim")).unwrap();
        let snaps = HashMap::new();
        let markers = DriftDetector::new().scan(&g, &snaps);
        assert!(markers.is_empty());
    }

    #[test]
    fn skips_unchanged_evidence() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let n = canonical(scope, "claim");
        let id = n.id;
        g.add_node(n).unwrap();
        let e1 = EvidenceId::new_v4();
        let e2 = EvidenceId::new_v4();
        let mut snaps = HashMap::new();
        snaps.insert(
            id,
            EvidenceSnapshot::partition(vec![e1, e2], vec![e1, e2], vec![]),
        );
        let markers = DriftDetector::new().scan(&g, &snaps);
        assert!(markers.is_empty());
    }

    #[test]
    fn skips_non_canonical_nodes() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let cand = ConceptNode::new_candidate("c", String::new(), scope);
        let id = cand.id;
        g.add_node(cand).unwrap();
        let mut snaps = HashMap::new();
        snaps.insert(
            id,
            EvidenceSnapshot::partition(
                vec![EvidenceId::new_v4()],
                vec![],
                vec![EvidenceId::new_v4()],
            ),
        );
        let markers = DriftDetector::new().scan(&g, &snaps);
        assert!(markers.is_empty());
    }
}
