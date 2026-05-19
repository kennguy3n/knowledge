//! Contradiction detection and adjudication workflow.
//!
//! Per `docs/DESIGN.md` §11.1, the substrate
//! materialises *opposing claims* in the concept graph and runs a
//! lightweight adjudication state machine on top.
//!
//! `ContradictionDetector::scan` walks every pair of `Canonical`
//! nodes whose normalised content is *opposing* (a configurable
//! oracle, defaulting to a content-prefix-based heuristic for the
//! lexicon-only baseline) and returns a list of
//! [`ContradictionEdge`] candidates that the caller can persist
//! into the graph as `Contradicts` edges.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use concept_graph::{ConceptGraph, ConceptNode, NodeId, NodeState};
use evidence_store::EvidenceId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ReasoningError, Result};

/// One pair of nodes the detector flagged as contradictory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContradictionEdge {
    /// Stable id (UUID v4) for the contradiction record itself.
    pub id: Uuid,
    /// One side of the contradiction.
    pub left: NodeId,
    /// The opposing side.
    pub right: NodeId,
    /// Detection time.
    pub detected_at: DateTime<Utc>,
    /// Confidence in `0.0 ..= 1.0`. The lexicon baseline emits
    /// `0.6` for prefix matches; the SLM-backed detector is
    /// expected to emit calibrated probabilities.
    pub confidence: f64,
    /// Evidence rows on the `left` side.
    pub left_evidence: Vec<EvidenceId>,
    /// Evidence rows on the `right` side.
    pub right_evidence: Vec<EvidenceId>,
}

/// Pluggable oracle used by [`ContradictionDetector`] to decide
/// whether two nodes' content is *opposing*. Implementations can
/// be a lexicon, an embedding-based comparator, or an SLM call.
pub trait OpposingClaimOracle {
    /// Return `true` iff `left` and `right` are opposing claims.
    fn opposes(&self, left: &str, right: &str) -> bool;
}

/// Lexicon-only baseline — two contents oppose iff one is the
/// other prefixed with `"not "` (case-insensitive). Useful for
/// tests and for the bootstrap path where no SLM is available.
#[derive(Debug, Clone, Default)]
pub struct PrefixNegationOracle;

impl OpposingClaimOracle for PrefixNegationOracle {
    fn opposes(&self, left: &str, right: &str) -> bool {
        let l = left.trim().to_lowercase();
        let r = right.trim().to_lowercase();
        if l.is_empty() || r.is_empty() {
            return false;
        }
        l == format!("not {}", r) || r == format!("not {}", l)
    }
}

/// Scans a [`ConceptGraph`] for pairs of `Canonical` nodes whose
/// content opposes per the supplied [`OpposingClaimOracle`].
#[derive(Debug)]
pub struct ContradictionDetector<'o, O: OpposingClaimOracle> {
    /// Pluggable oracle.
    pub oracle: &'o O,
    /// Confidence to attach to flagged pairs.
    pub confidence: f64,
}

impl<'o, O: OpposingClaimOracle> ContradictionDetector<'o, O> {
    /// Construct a detector with the supplied oracle and a default
    /// confidence of `0.6`.
    pub fn new(oracle: &'o O) -> Self {
        Self {
            oracle,
            confidence: 0.6,
        }
    }

    /// Override the confidence emitted on every flagged pair.
    pub fn with_confidence(mut self, c: f64) -> Self {
        self.confidence = c;
        self
    }

    /// Scan the graph, returning every pair of canonical nodes
    /// the oracle flagged as opposing. Each pair is reported once
    /// with `left.id < right.id` so the output is deterministic.
    pub fn scan(&self, graph: &ConceptGraph) -> Vec<ContradictionEdge> {
        let canonical: Vec<&ConceptNode> = graph
            .iter_nodes()
            .filter(|n| n.state == NodeState::Canonical)
            .collect();
        let now = Utc::now();
        let mut out = Vec::new();
        for i in 0..canonical.len() {
            for j in (i + 1)..canonical.len() {
                let a = canonical[i];
                let b = canonical[j];
                if a.scope_id != b.scope_id {
                    continue;
                }
                if !self.oracle.opposes(&a.label, &b.label) {
                    continue;
                }
                let (left, right) = if a.id.0 <= b.id.0 { (a, b) } else { (b, a) };
                out.push(ContradictionEdge {
                    id: Uuid::new_v4(),
                    left: left.id,
                    right: right.id,
                    detected_at: now,
                    confidence: self.confidence,
                    left_evidence: Vec::new(),
                    right_evidence: Vec::new(),
                });
            }
        }
        out
    }
}

/// Lifecycle states for an adjudication record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjudicationState {
    /// Just detected — awaiting human / synthesizer review.
    Detected,
    /// Under review (e.g. surfaced in a synthesizer's queue).
    UnderReview,
    /// Resolved — see [`AdjudicationOutcome`].
    Resolved,
}

/// Outcome attached to a `Resolved` adjudication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdjudicationOutcome {
    /// One side won; the other was superseded.
    Winner {
        /// The side kept canonical.
        winner: NodeId,
        /// The side superseded.
        loser: NodeId,
    },
    /// Both sides remain valid in distinct contexts.
    BothValidInContext,
}

/// One adjudication record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdjudicationRecord {
    /// Contradiction this record adjudicates.
    pub contradiction_id: Uuid,
    /// Current state.
    pub state: AdjudicationState,
    /// Outcome (only `Some` once `state == Resolved`).
    pub outcome: Option<AdjudicationOutcome>,
    /// Record creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock of the most recent state transition.
    pub updated_at: DateTime<Utc>,
}

/// In-memory state machine for adjudication records.
#[derive(Debug, Clone, Default)]
pub struct AdjudicationWorkflow {
    records: HashMap<Uuid, AdjudicationRecord>,
}

impl AdjudicationWorkflow {
    /// Construct an empty workflow.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly-detected contradiction. Returns
    /// [`ReasoningError::InvalidAdjudicationTransition`] if a
    /// record already exists for the same contradiction id.
    pub fn detect(&mut self, contradiction_id: Uuid) -> Result<&AdjudicationRecord> {
        if self.records.contains_key(&contradiction_id) {
            return Err(ReasoningError::InvalidAdjudicationTransition);
        }
        let now = Utc::now();
        self.records.insert(
            contradiction_id,
            AdjudicationRecord {
                contradiction_id,
                state: AdjudicationState::Detected,
                outcome: None,
                created_at: now,
                updated_at: now,
            },
        );
        Ok(self.records.get(&contradiction_id).expect("just inserted"))
    }

    /// Move from `Detected` to `UnderReview`.
    pub fn mark_under_review(&mut self, contradiction_id: Uuid) -> Result<&AdjudicationRecord> {
        let rec = self
            .records
            .get_mut(&contradiction_id)
            .ok_or(ReasoningError::InvalidAdjudicationTransition)?;
        if rec.state != AdjudicationState::Detected {
            return Err(ReasoningError::InvalidAdjudicationTransition);
        }
        rec.state = AdjudicationState::UnderReview;
        rec.updated_at = Utc::now();
        Ok(rec)
    }

    /// Resolve the contradiction. Valid from any non-terminal
    /// state; resolving a terminal state errors.
    pub fn resolve(
        &mut self,
        contradiction_id: Uuid,
        outcome: AdjudicationOutcome,
    ) -> Result<&AdjudicationRecord> {
        let rec = self
            .records
            .get_mut(&contradiction_id)
            .ok_or(ReasoningError::InvalidAdjudicationTransition)?;
        if rec.state == AdjudicationState::Resolved {
            return Err(ReasoningError::InvalidAdjudicationTransition);
        }
        rec.state = AdjudicationState::Resolved;
        rec.outcome = Some(outcome);
        rec.updated_at = Utc::now();
        Ok(rec)
    }

    /// Look up an adjudication record by its contradiction id.
    pub fn get(&self, contradiction_id: Uuid) -> Option<&AdjudicationRecord> {
        self.records.get(&contradiction_id)
    }

    /// Number of records in flight.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True iff no records have been registered.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concept_graph::{ConceptGraph, ConceptNode, NodeState};
    use evidence_store::ScopeId;

    fn canonical_with_label(scope: ScopeId, label: &str) -> ConceptNode {
        let mut n = ConceptNode::new_candidate(label.to_string(), String::new(), scope);
        n.state = NodeState::Canonical;
        n
    }

    #[test]
    fn detector_flags_opposing_canonical_pairs() {
        let scope = ScopeId::new_v4();
        let mut g = ConceptGraph::new();
        let n1 = canonical_with_label(scope, "ship friday");
        let n2 = canonical_with_label(scope, "not ship friday");
        let id1 = n1.id;
        let id2 = n2.id;
        g.add_node(n1).unwrap();
        g.add_node(n2).unwrap();

        let oracle = PrefixNegationOracle;
        let detector = ContradictionDetector::new(&oracle);
        let edges = detector.scan(&g);
        assert_eq!(edges.len(), 1);
        let (lo, hi) = if id1.0 <= id2.0 {
            (id1, id2)
        } else {
            (id2, id1)
        };
        assert_eq!(edges[0].left, lo);
        assert_eq!(edges[0].right, hi);
        assert!((edges[0].confidence - 0.6).abs() < 1e-9);
    }

    #[test]
    fn detector_skips_candidates_and_cross_scope() {
        let scope_a = ScopeId::new_v4();
        let scope_b = ScopeId::new_v4();
        let mut g = ConceptGraph::new();

        // Same-scope but one is still a candidate.
        let mut n_cand =
            ConceptNode::new_candidate("ship friday".to_string(), String::new(), scope_a);
        n_cand.state = NodeState::Candidate;
        let n_can = canonical_with_label(scope_a, "not ship friday");
        g.add_node(n_cand).unwrap();
        g.add_node(n_can).unwrap();

        // Opposing canonicals but in different scopes.
        let n_x = canonical_with_label(scope_a, "approve change");
        let n_y = canonical_with_label(scope_b, "not approve change");
        g.add_node(n_x).unwrap();
        g.add_node(n_y).unwrap();

        let oracle = PrefixNegationOracle;
        let detector = ContradictionDetector::new(&oracle);
        let edges = detector.scan(&g);
        assert!(edges.is_empty());
    }

    #[test]
    fn workflow_runs_through_resolution() {
        let mut wf = AdjudicationWorkflow::new();
        let cid = Uuid::new_v4();
        wf.detect(cid).unwrap();
        assert_eq!(wf.get(cid).unwrap().state, AdjudicationState::Detected);
        wf.mark_under_review(cid).unwrap();
        assert_eq!(wf.get(cid).unwrap().state, AdjudicationState::UnderReview);
        let winner = NodeId(Uuid::new_v4());
        let loser = NodeId(Uuid::new_v4());
        wf.resolve(cid, AdjudicationOutcome::Winner { winner, loser })
            .unwrap();
        let rec = wf.get(cid).unwrap();
        assert_eq!(rec.state, AdjudicationState::Resolved);
        assert!(matches!(
            rec.outcome,
            Some(AdjudicationOutcome::Winner { .. })
        ));
    }

    #[test]
    fn workflow_rejects_resolving_resolved() {
        let mut wf = AdjudicationWorkflow::new();
        let cid = Uuid::new_v4();
        wf.detect(cid).unwrap();
        wf.resolve(cid, AdjudicationOutcome::BothValidInContext)
            .unwrap();
        let err = wf
            .resolve(cid, AdjudicationOutcome::BothValidInContext)
            .unwrap_err();
        assert_eq!(err, ReasoningError::InvalidAdjudicationTransition);
    }

    #[test]
    fn workflow_rejects_under_review_without_detect() {
        let mut wf = AdjudicationWorkflow::new();
        let err = wf.mark_under_review(Uuid::new_v4()).unwrap_err();
        assert_eq!(err, ReasoningError::InvalidAdjudicationTransition);
    }

    #[test]
    fn workflow_rejects_duplicate_detect() {
        let mut wf = AdjudicationWorkflow::new();
        let cid = Uuid::new_v4();
        wf.detect(cid).unwrap();
        let err = wf.detect(cid).unwrap_err();
        assert_eq!(err, ReasoningError::InvalidAdjudicationTransition);
    }
}
