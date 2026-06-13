//! Reasoning-plane FFI entry points.
//!
//! Surfaces the three reasoning queries the product needs to answer
//! *"what changed / what contradicts / why this answer"* over the FFI
//! boundary, so the substrate server → Go gateway → UI read path can
//! render them without re-implementing any reasoning logic host-side:
//!
//! * [`reasoning_contradictions`] — opposing canonical claims in a
//!   scope (the *"what contradicts"* surface).
//! * [`reasoning_drift`] — canonical claims whose evidence base has
//!   shifted (the *"what changed"* surface).
//! * [`reasoning_explain_query`] — the query planner's rationale for a
//!   retrieval, i.e. how the substrate would route a question to the
//!   cheapest satisfying retrieval mode (the *"why this answer"*
//!   surface).
//!
//! # Scope isolation
//!
//! Both graph-derived queries project the concept graph from **only**
//! the requested scope's live user-memory observations (the same
//! projection [`crate::get_concept_graph`] uses) and run the detector
//! over that single-scope graph. A cryptographically-forgotten scope —
//! or one with no memory — yields an empty result, never an error and
//! never another scope's data. [`reasoning_explain_query`] reads no
//! scope data at all (the plan is a pure function of the query text);
//! it still validates the `scope_id` so the gateway's per-scope
//! authorisation envelope is uniform across all three routes.
//!
//! # Cost bound
//!
//! Contradiction detection is pairwise over canonical nodes, so the
//! projected graph is capped at [`REASONING_MAX_NODES`] highest-
//! retention observations. This bounds the worst-case work a single
//! pathological scope can impose on the shared substrate across the SME
//! fleet, mirroring the render budget [`crate::get_concept_graph`]
//! applies.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use concept_graph::{project_memory_graph, ConceptGraph, NodeId};
use evidence_store::EvidenceId;
use memory_manager::{MemoryFilter, MemoryObject, MemoryState};
use reasoning_engine::{
    ContradictionDetector, DriftDetector, DriftReason, EvidenceSnapshot, NegationOracle,
    QueryClass, QueryPlanner, RetrievalMode,
};
use serde::{Deserialize, Serialize};

use crate::error::FfiResult;
use crate::metrics;
use crate::runtime::{with_runtime, RuntimeHandle};
use crate::{memory_object_to_projection, parse_scope_id};

/// Upper bound on the number of observations a single reasoning scan
/// projects into the concept graph. Pairwise contradiction detection is
/// `O(canonical²)`, so this caps the worst-case CPU a single scope can
/// impose on the shared substrate. The highest-retention observations
/// are kept when a scope exceeds the budget.
pub const REASONING_MAX_NODES: usize = 256;

/// An opposing pair of canonical claims in a scope, enriched with the
/// human-readable labels of each side so the UI can render the
/// contradiction without a second round-trip to fetch node text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContradictionView {
    /// Stable id of the contradiction record itself.
    pub id: String,
    /// Node id of the left-hand claim.
    pub left_id: String,
    /// Human-readable label of the left-hand claim.
    pub left_label: String,
    /// Node id of the opposing claim.
    pub right_id: String,
    /// Human-readable label of the opposing claim.
    pub right_label: String,
    /// Detector confidence in `0.0 ..= 1.0`.
    pub confidence: f64,
    /// Number of evidence rows backing the left-hand claim.
    pub left_evidence_count: usize,
    /// Number of evidence rows backing the opposing claim.
    pub right_evidence_count: usize,
    /// Detection time.
    pub detected_at: DateTime<Utc>,
}

/// A canonical claim whose supporting evidence base has shifted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftView {
    /// Node id of the drifting claim.
    pub node_id: String,
    /// Human-readable label of the drifting claim.
    pub label: String,
    /// Why the evidence base shifted (`evidence_superseded`,
    /// `evidence_removed`, `evidence_weakened`).
    pub reason: DriftReason,
    /// Number of evidence rows present when the claim was promoted.
    pub evidence_at_promotion: usize,
    /// Number of those evidence rows still valid now.
    pub evidence_remaining: usize,
    /// Detection time.
    pub detected_at: DateTime<Utc>,
}

/// One step in an explained query plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainStepView {
    /// Retrieval mode attempted at this step (snake_case wire tag).
    pub mode: String,
    /// Cost rank — lower is cheaper. Mirrors
    /// [`RetrievalMode::cost_rank`].
    pub cost_rank: u8,
    /// Per-step wall-clock budget in milliseconds, if any.
    pub time_budget_ms: Option<u64>,
}

/// The query planner's rationale for a retrieval — the *"why this
/// answer"* explainer. The chain is ordered cheapest-first; the first
/// step that returns a hit answers the query and later steps are
/// fallbacks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryExplanationView {
    /// The query text the plan was produced for.
    pub query: String,
    /// Query class assigned by the classifier (snake_case wire tag).
    pub class: String,
    /// Ordered retrieval chain.
    pub steps: Vec<ExplainStepView>,
    /// Plain-language explanation of why this chain was chosen.
    pub rationale: String,
    /// Wall-clock time the plan was produced.
    pub planned_at: DateTime<Utc>,
}

/// Detect opposing canonical claims for a single scope.
///
/// # Errors
///
/// * [`crate::FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called.
/// * [`crate::FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: owned strings across the boundary.
pub fn reasoning_contradictions(
    handle: RuntimeHandle,
    scope_id: String,
) -> FfiResult<Vec<ContradictionView>> {
    metrics::instrument(metrics::inc_reasoning_contradictions, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            let Some(graph) = scope_graph(rt, scope) else {
                return Ok(Vec::new());
            };
            let oracle = NegationOracle;
            let edges = ContradictionDetector::new(&oracle).scan(&graph);
            let mut out: Vec<ContradictionView> = edges
                .iter()
                .map(|e| ContradictionView {
                    id: e.id.to_string(),
                    left_id: e.left.as_uuid().to_string(),
                    left_label: node_label(&graph, e.left),
                    right_id: e.right.as_uuid().to_string(),
                    right_label: node_label(&graph, e.right),
                    confidence: e.confidence,
                    left_evidence_count: e.left_evidence.len(),
                    right_evidence_count: e.right_evidence.len(),
                    detected_at: e.detected_at,
                })
                .collect();
            // Deterministic order so the UI list is stable across calls.
            out.sort_by(|a, b| {
                a.left_id
                    .cmp(&b.left_id)
                    .then_with(|| a.right_id.cmp(&b.right_id))
            });
            Ok(out)
        })
    })
}

/// Detect canonical claims whose evidence base has shifted for a single
/// scope.
///
/// The evidence snapshot for a canonical claim is reconstructed from the
/// memory plane's supersession pointers: when a canonical observation
/// supersedes older observations, the older observations' evidence rows
/// are the ones that shifted. An older observation that has been
/// archived (decayed out) or cryptographically deleted contributes
/// `removed` evidence; one merely marked superseded contributes
/// `superseded` evidence. The [`DriftDetector`] then classifies the
/// reason exactly as it would for any other snapshot source.
///
/// # Errors
///
/// * [`crate::FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called.
/// * [`crate::FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: owned strings across the boundary.
pub fn reasoning_drift(handle: RuntimeHandle, scope_id: String) -> FfiResult<Vec<DriftView>> {
    metrics::instrument(metrics::inc_reasoning_drift, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(Vec::new());
            }
            let Some(umo) = rt.user_memory(scope) else {
                return Ok(Vec::new());
            };
            let objects = capped_objects(umo.list(&MemoryFilter::any()));
            let graph = project_memory_graph(
                objects
                    .iter()
                    .copied()
                    .filter_map(memory_object_to_projection),
            );
            let snapshots = drift_snapshots(&objects);
            let markers = DriftDetector::new().scan(&graph, &snapshots);
            let mut out: Vec<DriftView> = markers
                .iter()
                .map(|m| DriftView {
                    node_id: m.node.as_uuid().to_string(),
                    label: node_label(&graph, m.node),
                    reason: m.reason,
                    evidence_at_promotion: m.evidence_at_promotion,
                    evidence_remaining: m.evidence_remaining,
                    detected_at: m.detected_at,
                })
                .collect();
            out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
            Ok(out)
        })
    })
}

/// Explain how the substrate would plan a retrieval for `query`.
///
/// This is a pure function of the query text — it reads no scope data —
/// so it never leaks across scopes. `scope_id` is still validated so the
/// gateway's per-scope authorisation envelope is uniform across all
/// reasoning routes.
///
/// # Errors
///
/// * [`crate::FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`crate::FfiError::InvalidQuery`] if `query` is empty or
///   whitespace-only.
#[allow(clippy::needless_pass_by_value)] // FFI: owned strings across the boundary.
pub fn reasoning_explain_query(scope_id: String, query: String) -> FfiResult<QueryExplanationView> {
    metrics::instrument(metrics::inc_reasoning_explain, || {
        // Validate the scope id for a uniform authorisation envelope even
        // though the plan itself reads no scope data.
        parse_scope_id(&scope_id)?;
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(crate::FfiError::InvalidQuery {
                message: "query text must not be empty".to_string(),
            });
        }
        let plan = QueryPlanner::new().plan(trimmed);
        let steps: Vec<ExplainStepView> = plan
            .steps
            .iter()
            .map(|s| ExplainStepView {
                mode: mode_tag(s.mode).to_string(),
                cost_rank: s.mode.cost_rank(),
                time_budget_ms: s.time_budget_ms,
            })
            .collect();
        let rationale = build_rationale(plan.class, &steps);
        Ok(QueryExplanationView {
            query: plan.query,
            class: class_tag(plan.class).to_string(),
            steps,
            rationale,
            planned_at: plan.planned_at,
        })
    })
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Project the requested scope's live user-memory into a single-scope
/// concept graph, capped at [`REASONING_MAX_NODES`]. Returns `None` for
/// a forgotten scope or one with no user memory, so callers short-
/// circuit to an empty result.
fn scope_graph(
    rt: &mut crate::runtime::FfiRuntime,
    scope: evidence_store::ScopeId,
) -> Option<ConceptGraph> {
    if rt.is_scope_forgotten(scope) {
        return None;
    }
    let umo = rt.user_memory(scope)?;
    let objects = capped_objects(umo.list(&MemoryFilter::any()));
    Some(project_memory_graph(
        objects.into_iter().filter_map(memory_object_to_projection),
    ))
}

/// Keep the highest-retention observations up to [`REASONING_MAX_NODES`]
/// so a pathologically large scope cannot blow the pairwise
/// contradiction scan's budget. Below the cap the input is returned
/// untouched (and its order does not matter — the projection is
/// id-keyed).
fn capped_objects(mut objects: Vec<&MemoryObject>) -> Vec<&MemoryObject> {
    if objects.len() <= REASONING_MAX_NODES {
        return objects;
    }
    objects.sort_by(|a, b| {
        b.retention_score
            .partial_cmp(&a.retention_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    objects.truncate(REASONING_MAX_NODES);
    objects
}

/// Reconstruct per-node evidence snapshots from the memory plane's
/// supersession pointers. A snapshot is built for each observation that
/// supersedes at least one other observation; the [`DriftDetector`]
/// only acts on those whose node is `Canonical` in the projected graph.
fn drift_snapshots(objects: &[&MemoryObject]) -> HashMap<NodeId, EvidenceSnapshot> {
    // Map each successor id to the predecessors it superseded.
    let mut predecessors: HashMap<uuid::Uuid, Vec<&MemoryObject>> = HashMap::new();
    for obj in objects {
        if let Some(succ) = obj.superseded_by {
            predecessors.entry(succ).or_default().push(obj);
        }
    }

    let mut out = HashMap::new();
    for succ in objects {
        let Some(preds) = predecessors.get(&succ.id) else {
            continue;
        };
        let still_valid: Vec<EvidenceId> = succ.source_refs.clone();

        let mut superseded: Vec<EvidenceId> = Vec::new();
        let mut removed: Vec<EvidenceId> = Vec::new();
        // Seed with the successor's still-valid evidence so a predecessor
        // reusing the same row is not also counted as superseded/removed.
        let mut seen: HashSet<EvidenceId> = still_valid.iter().copied().collect();
        for pred in preds {
            // Evidence under an archived/deleted predecessor is gone from
            // the working set ("removed"); evidence under a merely
            // superseded predecessor was replaced ("superseded").
            let bucket = match pred.state {
                MemoryState::Archived | MemoryState::Deleted => &mut removed,
                _ => &mut superseded,
            };
            for e in &pred.source_refs {
                if seen.insert(*e) {
                    bucket.push(*e);
                }
            }
        }

        let mut baseline = still_valid.clone();
        baseline.extend(superseded.iter().copied());
        baseline.extend(removed.iter().copied());

        out.insert(
            NodeId::from_uuid(succ.id),
            EvidenceSnapshot {
                baseline,
                still_valid,
                superseded,
                removed,
            },
        );
    }
    out
}

/// Look up a node's label, falling back to its id when the node is
/// absent (e.g. trimmed by the node cap).
fn node_label(graph: &ConceptGraph, id: NodeId) -> String {
    graph
        .get_node(id)
        .map_or_else(|| id.as_uuid().to_string(), |n| n.label.clone())
}

/// Stable snake_case wire tag for a [`QueryClass`], matching its serde
/// representation.
const fn class_tag(class: QueryClass) -> &'static str {
    match class {
        QueryClass::PointLookup => "point_lookup",
        QueryClass::Relational => "relational",
        QueryClass::Temporal => "temporal",
        QueryClass::Holistic => "holistic",
        QueryClass::Other => "other",
    }
}

/// Stable snake_case wire tag for a [`RetrievalMode`], matching its
/// serde representation.
const fn mode_tag(mode: RetrievalMode) -> &'static str {
    match mode {
        RetrievalMode::Summary => "summary",
        RetrievalMode::Fts => "fts",
        RetrievalMode::SemanticVector => "semantic_vector",
        RetrievalMode::GraphTraversal => "graph_traversal",
        RetrievalMode::RawEvidence => "raw_evidence",
    }
}

/// Compose the plain-language rationale shown beside the plan.
fn build_rationale(class: QueryClass, steps: &[ExplainStepView]) -> String {
    let chain = steps
        .iter()
        .map(|s| s.mode.as_str())
        .collect::<Vec<_>>()
        .join(" → ");
    let why_class = match class {
        QueryClass::PointLookup => {
            "a single-entity lookup, so a summary card or keyword hit usually answers it"
        }
        QueryClass::Relational => {
            "a relationship between entities, so typed-edge graph traversal is tried first"
        }
        QueryClass::Temporal => {
            "recency-sensitive, so keyword and semantic search over recent rows lead"
        }
        QueryClass::Holistic => {
            "a broad, holistic ask, so the graph and pre-computed summaries lead"
        }
        QueryClass::Other => "unclassified, so the default cheapest-first chain is used",
    };
    format!(
        "Classified as {class}: {why_class}. The substrate attempts the cheapest satisfying \
         retrieval mode first and only falls back to a more expensive one if it misses: {chain}. \
         The first mode that returns a hit answers the query; later modes are fallbacks.",
        class = class_tag(class),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use evidence_store::ScopeId;

    fn obj(scope: ScopeId, state: MemoryState, label: &str) -> MemoryObject {
        let mut o = MemoryObject::new_candidate(scope, memory_manager::SensitivityClass::Useful);
        o.state = state;
        o.metadata = serde_json::json!({ "content": label });
        o
    }

    #[test]
    fn projection_label_is_plain_content_text() {
        // The `content` metadata key is what `memory_summary` reads, so a
        // projected node's label must be the plain string — not a JSON
        // blob — and the NegationOracle must match on it end-to-end.
        let scope = ScopeId::new_v4();
        let a = obj(scope, MemoryState::Canonical, "we will ship on friday");
        let b = obj(scope, MemoryState::Canonical, "we will not ship on friday");
        let graph = project_memory_graph(
            [&a, &b]
                .into_iter()
                .filter_map(memory_object_to_projection),
        );
        assert_eq!(
            node_label(&graph, NodeId::from_uuid(a.id)),
            "we will ship on friday",
        );
        let oracle = NegationOracle;
        let edges = ContradictionDetector::new(&oracle).scan(&graph);
        assert_eq!(edges.len(), 1, "opposing claims should be flagged");
    }

    #[test]
    fn capped_objects_keeps_highest_retention() {
        let scope = ScopeId::new_v4();
        let mut objs: Vec<MemoryObject> = (0..(REASONING_MAX_NODES + 10))
            .map(|i| {
                let mut o = obj(scope, MemoryState::Canonical, "x");
                o.retention_score = f64::from(u32::try_from(i).unwrap_or(u32::MAX));
                o
            })
            .collect();
        let refs: Vec<&MemoryObject> = objs.iter().collect();
        let capped = capped_objects(refs);
        assert_eq!(capped.len(), REASONING_MAX_NODES);
        // The lowest-retention objects were dropped.
        let min_kept = capped
            .iter()
            .map(|o| o.retention_score)
            .fold(f64::INFINITY, f64::min);
        assert!(min_kept >= 10.0);
        objs.clear();
    }

    #[test]
    fn drift_snapshots_reconstruct_supersession() {
        let scope = ScopeId::new_v4();
        let mut successor = obj(scope, MemoryState::Canonical, "new");
        let mut predecessor = obj(scope, MemoryState::Superseded, "old");
        predecessor.superseded_by = Some(successor.id);
        let e_old = EvidenceId::new_v4();
        let e_new = EvidenceId::new_v4();
        predecessor.source_refs = vec![e_old];
        successor.source_refs = vec![e_new];

        let objs = vec![&successor, &predecessor];
        let snaps = drift_snapshots(&objs);
        let snap = snaps
            .get(&NodeId::from_uuid(successor.id))
            .expect("snapshot for successor");
        assert_eq!(snap.still_valid, vec![e_new]);
        assert_eq!(snap.superseded, vec![e_old]);
        assert!(snap.removed.is_empty());
        assert_eq!(snap.baseline.len(), 2);
    }

    #[test]
    fn drift_snapshots_mark_archived_predecessor_removed() {
        let scope = ScopeId::new_v4();
        let mut successor = obj(scope, MemoryState::Canonical, "new");
        let mut predecessor = obj(scope, MemoryState::Archived, "old");
        predecessor.superseded_by = Some(successor.id);
        let e_old = EvidenceId::new_v4();
        predecessor.source_refs = vec![e_old];
        successor.source_refs = vec![EvidenceId::new_v4()];

        let objs = vec![&successor, &predecessor];
        let snaps = drift_snapshots(&objs);
        let snap = snaps.get(&NodeId::from_uuid(successor.id)).unwrap();
        assert_eq!(snap.removed, vec![e_old]);
        assert!(snap.superseded.is_empty());
    }

    #[test]
    fn explain_query_classifies_and_explains() {
        let scope = ScopeId::new_v4().as_uuid().to_string();
        let view = reasoning_explain_query(scope, "what was approved by finance".to_string())
            .expect("plan");
        assert_eq!(view.class, "relational");
        assert_eq!(view.steps.first().unwrap().mode, "graph_traversal");
        assert!(view.rationale.contains("relational"));
        // Steps are ordered cheapest-satisfying-first by the planner; the
        // chain is non-empty.
        assert!(!view.steps.is_empty());
    }

    #[test]
    fn explain_query_rejects_empty_query() {
        let scope = ScopeId::new_v4().as_uuid().to_string();
        let err = reasoning_explain_query(scope, "   ".to_string()).unwrap_err();
        assert!(matches!(err, crate::FfiError::InvalidQuery { .. }));
    }

    #[test]
    fn explain_query_rejects_bad_scope() {
        let err =
            reasoning_explain_query("not-a-uuid".to_string(), "hello".to_string()).unwrap_err();
        assert!(matches!(err, crate::FfiError::InvalidId { .. }));
    }

    #[test]
    fn tags_match_serde_round_trip() {
        // class_tag / mode_tag must equal the serde wire tags so the DTO
        // strings line up with anything that deserializes the engine enums.
        let _ = Utc::now();
        assert_eq!(
            serde_json::to_value(QueryClass::PointLookup).unwrap(),
            serde_json::Value::String(class_tag(QueryClass::PointLookup).to_string())
        );
        assert_eq!(
            serde_json::to_value(RetrievalMode::SemanticVector).unwrap(),
            serde_json::Value::String(mode_tag(RetrievalMode::SemanticVector).to_string())
        );
    }
}
