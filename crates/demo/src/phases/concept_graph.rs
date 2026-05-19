//! Stage 4 — Concept Graph.
//!
//! Exercises every public surface of the concept graph:
//!
//! * Typed nodes ([`ConceptNode`]) and the seven typed relations from
//!   `docs/DESIGN.md` §3.3 (`IsA`, `PartOf`, `DecidedBy`, `Supersedes`,
//!   `Contradicts`, `DerivedFrom`, `AssignedTo`).
//! * Encrypted persistence via [`PersistentConceptGraph`] (SQLCipher
//!   round-trip via `add_node` / `add_edge` / `supersede_node` and
//!   `load_scope` rehydration).
//! * Touched-branch incremental updates via
//!   [`IncrementalUpdateEngine`] with the four [`ChangeEvent`]
//!   variants (`NodePromoted`, `NodeSuperseded`, `NodeContradicted`,
//!   `EdgeRemoved`).
//! * The visualization façade: [`explore_from`],
//!   [`subgraph_for_scope`], [`neighborhood`] and [`search_nodes`].
//!
//! ## Scope-cohesion contract
//!
//! [`PersistentConceptGraph::load_scope`] only rehydrates nodes and
//! edges whose `scope_id == scope`. It then re-validates the loaded
//! subgraph against [`concept_graph::ConceptGraph::add_edge`], which
//! returns `DanglingEdge` whenever an edge references a node that is
//! not in the loaded subset. The same contract is exercised
//! end-to-end by `crates/concept_graph/tests/persist_tests.rs`.
//!
//! Concretely: every persisted edge in this stage has both endpoints
//! in the same scope as the edge itself. The "logical hierarchy"
//! (channel → domain → tenant) is realised in two layers:
//!
//! 1. **Substrate-level**: all the canonical organisational concepts
//!    (tenant root, residency policy, engineering domain, platform /
//!    marketing channels, atlas plan, alex's user space) live in the
//!    tenant scope, so the cross-tier `PartOf` / `IsA` /
//!    `DecidedBy` / `AssignedTo` / `DerivedFrom` / `Supersedes` /
//!    `Contradicts` edges between them are tenant-scope-cohesive and
//!    survive `load_scope(tenant_scope)` round-tripping. The original
//!    tier is preserved in the `concept_kind` metadata field for
//!    visualization / search.
//! 2. **Per-scope clusters**: every non-tenant dataset scope (user,
//!    channel, channel_alt, domain) gets a small self-contained
//!    `IsA` cluster of its own (two nodes + one edge, all in that
//!    scope) so per-scope `load_scope` returns a non-empty,
//!    self-consistent subgraph for every scope. This is what
//!    `subgraph_for_scope` and the rehydration assertion actually
//!    exercise.

use std::collections::HashMap;
use std::time::Instant;

use concept_graph::{
    explore_from, neighborhood, search_nodes, subgraph_for_scope, AllowAllScopes, ChangeEvent,
    ConceptEdge, ConceptNode, EdgeId, IncrementalUpdateEngine, NodeId, NodeState,
    PersistentConceptGraph, RelationType, ViewFilter,
};
use evidence_store::ScopeId;
use tempfile::TempDir;

use crate::assertions::AssertionLog;
use crate::dataset::{Dataset, NamedScope, ScopeTier};
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "concept_graph";

/// Canonical concept anchors that this stage seeds into the persistent
/// graph. Picked so they cover every scope tier and resolve to terms
/// that actually appear in the synthetic dataset (so the visualization
/// search query in this stage has a real hit to find).
struct ConceptSeed {
    label: &'static str,
    definition: &'static str,
    /// Logical tier the concept *describes*. The on-disk `scope_id`
    /// is always the tenant scope (so cross-tier edges are
    /// scope-cohesive); the original tier is preserved in the
    /// `concept_kind` metadata field for downstream visualization.
    tier: ScopeTier,
}

const CONCEPT_SEEDS: &[ConceptSeed] = &[
    ConceptSeed {
        label: "tenant.acme",
        definition: "ACME tenant root concept (canonical org boundary).",
        tier: ScopeTier::Tenant,
    },
    ConceptSeed {
        label: "data-residency-policy",
        definition: "Tenant-wide policy: customer data must be EU-resident.",
        tier: ScopeTier::Tenant,
    },
    ConceptSeed {
        label: "domain.engineering",
        definition: "Engineering domain owning the platform migration.",
        tier: ScopeTier::Domain,
    },
    ConceptSeed {
        label: "channel.platform",
        definition: "Platform engineering Slack channel.",
        tier: ScopeTier::Channel,
    },
    ConceptSeed {
        label: "channel.marketing",
        definition: "Marketing Slack channel coordinating GA launch.",
        tier: ScopeTier::Channel,
    },
    ConceptSeed {
        label: "atlas-launch-plan",
        definition: "Project Atlas Q3 cutover plan (Aurora -> sharded Postgres).",
        tier: ScopeTier::Domain,
    },
    ConceptSeed {
        label: "user.alex",
        definition: "Personal scope for user Alex (private notes).",
        tier: ScopeTier::User,
    },
];

fn dataset_scopes(dataset: &Dataset) -> Vec<&NamedScope> {
    vec![
        &dataset.user_scope,
        &dataset.channel_scope,
        &dataset.channel_alt_scope,
        &dataset.domain_scope,
        &dataset.tenant_scope,
    ]
}

/// Insert a small intra-scope canonical cluster (two nodes joined by
/// an `IsA` edge, all in `scope`) so per-scope `load_scope` rehydration
/// returns a non-empty, self-consistent subgraph for that scope.
fn add_scope_local_cluster(
    g: &mut PersistentConceptGraph,
    scope: ScopeId,
    label: &str,
    typed_edges: &mut HashMap<RelationType, u64>,
) -> (NodeId, NodeId, EdgeId) {
    let mut parent = ConceptNode::new_candidate(
        format!("{label}.root"),
        format!("scope-local root concept for {label}"),
        scope,
    );
    parent.metadata = serde_json::json!({
        "concept_kind": "scope_local_root",
        "scope_label": label,
    });
    parent.mark_canonical();
    let parent_id = g.add_node(parent).expect("add scope-local root");

    let mut child = ConceptNode::new_candidate(
        format!("{label}.topic"),
        format!("scope-local topic concept for {label}"),
        scope,
    );
    child.metadata = serde_json::json!({
        "concept_kind": "scope_local_topic",
        "scope_label": label,
    });
    child.mark_canonical();
    let child_id = g.add_node(child).expect("add scope-local topic");

    let edge = ConceptEdge::new(child_id, parent_id, RelationType::IsA, scope);
    let edge_id = g.add_edge(edge).expect("add scope-local IsA edge");
    *typed_edges.entry(RelationType::IsA).or_default() += 1;

    (parent_id, child_id, edge_id)
}

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Stage 4: Concept Graph");

    // -- Open an encrypted, persistent concept graph in a fresh temp dir.
    let temp = TempDir::new().expect("tempdir for concept graph");
    let db_path = temp.path().join("concepts.db");
    let mut pgraph = PersistentConceptGraph::open(&db_path, &state.master_key)
        .expect("open SQLCipher concept graph");

    // -- Seed canonical anchor nodes in the tenant scope (see
    //    "Scope-cohesion contract" in the module docs).
    let tenant_scope = dataset.tenant_scope.id;
    let mut canonical_ids: Vec<NodeId> = Vec::new();
    let mut concept_by_tier: HashMap<ScopeTier, Vec<NodeId>> = HashMap::new();

    for (idx, seed) in CONCEPT_SEEDS.iter().enumerate() {
        let mut node = ConceptNode::new_candidate(seed.label, seed.definition, tenant_scope);
        node.metadata = serde_json::json!({
            "position": {"x": (idx as f64) * 120.0, "y": 0.0},
            "concept_kind": seed.tier.as_str(),
            "logical_tier": seed.tier.as_str(),
        });
        node.mark_canonical();
        let id = pgraph.add_node(node).expect("add canonical concept node");
        canonical_ids.push(id);
        concept_by_tier.entry(seed.tier).or_default().push(id);
    }
    let canonical_seed_count = canonical_ids.len() as u64;

    // -- Wire up scope hierarchy with PartOf / IsA edges.
    let mut typed_edge_count: HashMap<RelationType, u64> = HashMap::new();
    let add_typed = |g: &mut PersistentConceptGraph,
                     from: NodeId,
                     to: NodeId,
                     rel: RelationType,
                     scope: ScopeId,
                     counter: &mut HashMap<RelationType, u64>| {
        let edge = ConceptEdge::new(from, to, rel, scope);
        g.add_edge(edge).expect("add typed edge");
        *counter.entry(rel).or_default() += 1;
    };

    let tenant_root = concept_by_tier
        .get(&ScopeTier::Tenant)
        .and_then(|v| v.first())
        .copied();
    let domain_root = concept_by_tier
        .get(&ScopeTier::Domain)
        .and_then(|v| v.first())
        .copied();

    if let (Some(tenant), Some(domain)) = (tenant_root, domain_root) {
        add_typed(
            &mut pgraph,
            domain,
            tenant,
            RelationType::PartOf,
            tenant_scope,
            &mut typed_edge_count,
        );
    }
    if let Some(domain) = domain_root {
        for ch in concept_by_tier
            .get(&ScopeTier::Channel)
            .into_iter()
            .flatten()
        {
            add_typed(
                &mut pgraph,
                *ch,
                domain,
                RelationType::PartOf,
                tenant_scope,
                &mut typed_edge_count,
            );
        }
    }

    if let Some(tenant) = tenant_root {
        for id in canonical_ids.iter().copied() {
            if id == tenant {
                continue;
            }
            add_typed(
                &mut pgraph,
                id,
                tenant,
                RelationType::IsA,
                tenant_scope,
                &mut typed_edge_count,
            );
        }
    }

    // DecidedBy / AssignedTo: synthesise one decision-maker and one
    // assignee node so we exercise both relations (tenant-scope so
    // the resulting edges are scope-cohesive).
    let mut decider = ConceptNode::new_candidate("@sara", "decision-maker @sara", tenant_scope);
    decider.mark_canonical();
    let decider_id = pgraph.add_node(decider).expect("add decider");
    let mut assignee =
        ConceptNode::new_candidate("@eng-team", "assignee group @eng-team", tenant_scope);
    assignee.mark_canonical();
    let assignee_id = pgraph.add_node(assignee).expect("add assignee");
    canonical_ids.push(decider_id);
    canonical_ids.push(assignee_id);

    if let Some(tenant) = tenant_root {
        add_typed(
            &mut pgraph,
            tenant,
            decider_id,
            RelationType::DecidedBy,
            tenant_scope,
            &mut typed_edge_count,
        );
    }
    if let Some(domain) = domain_root {
        add_typed(
            &mut pgraph,
            domain,
            assignee_id,
            RelationType::AssignedTo,
            tenant_scope,
            &mut typed_edge_count,
        );
    }

    // DerivedFrom: every evidence-stage row gets a tombstone node
    // and a `concept --derived_from--> evidence` edge. The tombstone
    // node lives in the tenant scope (so the DerivedFrom edge stays
    // scope-cohesive); the original evidence row's scope is captured
    // in metadata for downstream provenance audits.
    let mut evidence_node_ids: Vec<NodeId> = Vec::new();
    for row in &state.ingested_rows {
        let mut ev_node = ConceptNode::new_candidate(
            format!("evidence:{}", row.evidence_id.as_uuid()),
            format!("provenance shim for {}", row.source_ref),
            tenant_scope,
        );
        ev_node.metadata = serde_json::json!({
            "evidence_id": row.evidence_id.as_uuid().to_string(),
            "source_ref": row.source_ref,
            "concept_kind": "evidence_row",
            "origin_scope": row.scope_label,
            "origin_scope_id": row.scope_id.as_uuid().to_string(),
        });
        ev_node.mark_canonical();
        let id = pgraph.add_node(ev_node).expect("add evidence node");
        evidence_node_ids.push(id);
    }
    if !evidence_node_ids.is_empty() {
        for (i, concept_id) in canonical_ids
            .iter()
            .copied()
            .take(canonical_seed_count as usize)
            .enumerate()
        {
            let ev = evidence_node_ids[i % evidence_node_ids.len()];
            add_typed(
                &mut pgraph,
                concept_id,
                ev,
                RelationType::DerivedFrom,
                tenant_scope,
                &mut typed_edge_count,
            );
        }
    }

    // -- Per-scope canonical clusters so that `load_scope(scope)` for
    //    every non-tenant dataset scope returns a non-empty,
    //    self-consistent subgraph. Each cluster is fully scope-local
    //    (two nodes + one `IsA` edge, all in the same scope), so the
    //    `DanglingEdge` invariant is respected.
    let mut scope_local_clusters: u64 = 0;
    let mut scope_local_node_count: u64 = 0;
    let mut scope_local_edge_count: u64 = 0;
    for scope in dataset_scopes(dataset) {
        if scope.id == tenant_scope {
            // Tenant scope already has the full canonical set; no
            // need for an extra mini-cluster.
            continue;
        }
        add_scope_local_cluster(&mut pgraph, scope.id, scope.label, &mut typed_edge_count);
        scope_local_clusters += 1;
        scope_local_node_count += 2;
        scope_local_edge_count += 1;
    }

    // -- Exercise supersession + the IncrementalUpdateEngine.
    let bench_started = Instant::now();
    let mut bench_ops: u64 = 0;
    let engine = IncrementalUpdateEngine::default();

    let candidate =
        ConceptNode::new_candidate("draft-process-v1", "draft process candidate", tenant_scope);
    let candidate_id = pgraph.add_node(candidate).expect("add candidate");
    let promotion = engine
        .propagate(
            pgraph.graph_mut(),
            ChangeEvent::NodePromoted { node: candidate_id },
        )
        .expect("promote candidate");
    bench_ops += 1;

    let mut successor = ConceptNode::new_candidate(
        "draft-process-v2",
        "newer process replacing v1",
        tenant_scope,
    );
    successor.mark_canonical();
    let successor_id = pgraph.add_node(successor).expect("add successor");
    canonical_ids.push(successor_id);
    pgraph
        .supersede_node(candidate_id, successor_id)
        .expect("persist supersession");
    *typed_edge_count
        .entry(RelationType::Supersedes)
        .or_default() += 1;
    bench_ops += 1;

    // ---- Snapshot persisted-only state -------------------------------
    //
    // Everything up to this point — `add_node`, `add_edge`, and
    // `supersede_node` — was mirrored to SQLCipher via the
    // [`PersistentConceptGraph`] wrapper. The next operations
    // (engine-only supersession pair, `NodeContradicted`,
    // `EdgeRemoved`) all mutate the in-memory graph *only*: the
    // substrate explicitly does not mirror `mark_contradiction` /
    // `remove_node` through the persistence layer (see the
    // doc-comment on [`PersistentConceptGraph`]) and the engine-only
    // pair is added through `graph_mut()` rather than the
    // persistence wrapper. Capture the persisted baseline (counts
    // and the substrate-level canonical id-set) here so the
    // rehydration assertion can compare against the exact subset
    // of state that survived to disk and the export stage only sees ids
    // the rehydrated graph actually contains.
    //
    // The substrate-level canonical id-set is exactly
    // `canonical_ids` (seeds + decider + assignee + successor) —
    // the evidence-stage shims are also persisted as Canonical
    // nodes but they are provenance fixtures, not exportable
    // concepts, so the export stage must not see them in
    // `state.canonical_concept_ids`.
    let persisted_node_count = pgraph.graph().node_count() as u64;
    let persisted_edge_count = pgraph.graph().edge_count() as u64;
    let persisted_tenant_canonical_ids: Vec<uuid::Uuid> =
        canonical_ids.iter().map(|id| id.0).collect();

    // Exercise the [`IncrementalUpdateEngine`]'s `NodeSuperseded`
    // branch on a *separate* candidate/successor pair so the
    // engine's in-memory `graph.supersede_node` call doesn't double
    // the persisted Supersedes edge added by `pgraph.supersede_node`
    // above. The pair below is engine-only — it's never persisted
    // through the [`PersistentConceptGraph`] wrapper, so it lives in
    // the in-memory graph after propagation but is dropped on the
    // rehydration loop, which is exactly the contract we want to
    // demonstrate.
    let mut engine_pred = ConceptNode::new_candidate(
        "engine-pred",
        "in-memory-only predecessor for engine propagation",
        tenant_scope,
    );
    engine_pred.mark_canonical();
    let engine_pred_id = engine_pred.id;
    pgraph
        .graph_mut()
        .add_node(engine_pred)
        .expect("add engine-pred (in-memory only)");
    let mut engine_succ = ConceptNode::new_candidate(
        "engine-succ",
        "in-memory-only successor for engine propagation",
        tenant_scope,
    );
    engine_succ.mark_canonical();
    let engine_succ_id = engine_succ.id;
    pgraph
        .graph_mut()
        .add_node(engine_succ)
        .expect("add engine-succ (in-memory only)");
    let supersession = engine
        .propagate(
            pgraph.graph_mut(),
            ChangeEvent::NodeSuperseded {
                predecessor: engine_pred_id,
                successor: engine_succ_id,
            },
        )
        .expect("propagate supersession");
    bench_ops += 1;

    // Contradiction: pick the first two tenant-tier seed concepts
    // (`tenant.acme` + `data-residency-policy`) so both endpoints
    // are guaranteed to be in tenant scope. The two `Contradicts`
    // edges produced by `mark_contradiction` are intentionally NOT
    // persisted (see snapshot comment above).
    let contradiction_pair: Option<(NodeId, NodeId)> = match concept_by_tier.get(&ScopeTier::Tenant)
    {
        Some(tenant_concepts) if tenant_concepts.len() >= 2 => {
            Some((tenant_concepts[0], tenant_concepts[1]))
        }
        _ => None,
    };
    let mut contradiction_propagated = false;
    if let Some((a, b)) = contradiction_pair {
        let contradiction = engine
            .propagate(pgraph.graph_mut(), ChangeEvent::NodeContradicted { a, b })
            .expect("propagate contradiction");
        *typed_edge_count
            .entry(RelationType::Contradicts)
            .or_default() += 2;
        contradiction_propagated = !contradiction.state_transitions.is_empty();
        bench_ops += 1;
    }

    // Snapshot the in-memory contradicted count *before* the
    // rehydration loop below, which resets the graph for each
    // `load_scope` and therefore drops the un-persisted contradiction
    // state.
    let total_contradicted = pgraph
        .graph()
        .iter_nodes()
        .filter(|n| n.state == NodeState::Contradicted)
        .count() as u64;

    // EdgeRemoved: drop one DerivedFrom edge so we observe a
    // non-empty `removed_edges` list in the propagation result. This
    // is also in-memory only — the row stays in SQLCipher, and the
    // rehydration assertion below uses the persisted baseline.
    let stray_edge_id = pgraph
        .graph()
        .iter_edges()
        .find(|e| e.relation == RelationType::DerivedFrom)
        .map(|e| e.id)
        .expect("at least one DerivedFrom edge");
    let removal = engine
        .propagate(
            pgraph.graph_mut(),
            ChangeEvent::EdgeRemoved {
                edge: stray_edge_id,
            },
        )
        .expect("propagate edge removal");
    bench_ops += 1;
    let edge_removed_observed = !removal.removed_edges.is_empty();

    let bench_total = bench_started.elapsed();

    // -- Visualization façade.
    let access = AllowAllScopes;
    let tenant_view = if let Some(tenant) = tenant_root {
        explore_from(
            pgraph.graph(),
            tenant,
            &ViewFilter {
                max_depth: Some(3),
                max_nodes: Some(64),
                ..ViewFilter::default()
            },
            &access,
        )
    } else {
        explore_from(
            pgraph.graph(),
            canonical_ids[0],
            &ViewFilter::default(),
            &access,
        )
    };

    let scopes = dataset_scopes(dataset);
    let mut subgraph_views = 0_u64;
    let mut subgraph_total_nodes = 0_u64;
    for scope in &scopes {
        let view = subgraph_for_scope(pgraph.graph(), scope.id, &ViewFilter::default(), &access);
        subgraph_total_nodes += view.nodes.len() as u64;
        subgraph_views += 1;
    }

    let neighborhood_view = neighborhood(
        pgraph.graph(),
        canonical_ids[0],
        1,
        &ViewFilter::default(),
        &access,
    );
    let search_results = search_nodes(pgraph.graph(), "atlas", &ViewFilter::default(), &access);

    // -- Persistence round-trip.
    let in_memory_node_count = pgraph.graph().node_count() as u64;
    let in_memory_edge_count = pgraph.graph().edge_count() as u64;
    state.concept_node_count = in_memory_node_count;
    state.concept_edge_count = in_memory_edge_count;
    state.canonical_concept_count = canonical_seed_count;
    // The export stage reopens the SQLCipher graph and rehydrates a single
    // scope (the tenant scope, where every cross-tier substrate
    // concept lives — see the "Scope-cohesion contract" docs above).
    // Surface only the *persisted* tenant-scope canonical ids
    // (snapshotted before the engine-only supersession pair was
    // added) so the export stage's approval workflow always sees concepts
    // the rehydrated graph actually contains; the per-scope
    // mini-clusters are still visible to this stage's local
    // visualization assertions.
    state.canonical_concept_ids = persisted_tenant_canonical_ids;

    let mut rehydrated_total_nodes = 0_usize;
    let mut rehydrated_total_edges = 0_usize;
    let mut rehydrated_per_scope: Vec<(String, usize, usize)> = Vec::new();
    for scope in &scopes {
        let (n, e) = pgraph
            .load_scope(scope.id)
            .expect("rehydrate scope from disk");
        rehydrated_total_nodes += n;
        rehydrated_total_edges += e;
        rehydrated_per_scope.push((scope.label.to_string(), n, e));
    }

    // After the rehydration loop the in-memory graph reflects the
    // last `load_scope` call (i.e. the tenant scope, since that's
    // last in `dataset_scopes`). Re-load the tenant scope explicitly
    // here so the canonical / superseded counts below are reported
    // against the persisted state. (Contradicted nodes are *not*
    // persisted; we already snapshotted that count above.)
    pgraph
        .load_scope(tenant_scope)
        .expect("rehydrate tenant scope for state counts");
    let total_canonical_after_supersede = pgraph
        .graph()
        .iter_nodes()
        .filter(|n| n.state == NodeState::Canonical)
        .count() as u64;
    let total_superseded = pgraph
        .graph()
        .iter_nodes()
        .filter(|n| n.state == NodeState::Superseded)
        .count() as u64;

    log.check(
        PHASE,
        "concept graph carries seven typed relation tags",
        typed_edge_count
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == 7,
    );
    log.check(
        PHASE,
        "supersession recorded the predecessor as Superseded",
        total_superseded >= 1
            && supersession.state_transitions.get(&engine_pred_id).copied()
                == Some(NodeState::Superseded),
    );
    log.check(
        PHASE,
        "promotion flipped Candidate -> Canonical (no-op safe)",
        promotion.affected.contains_node(candidate_id),
    );
    log.check(
        PHASE,
        "contradiction marked at least two nodes",
        contradiction_propagated && total_contradicted >= 2,
    );
    log.check(
        PHASE,
        "edge removal recorded a removed_edges entry",
        edge_removed_observed,
    );
    log.check(
        PHASE,
        "explore_from produced a non-empty view from the tenant root",
        !tenant_view.nodes.is_empty(),
    );
    log.check(
        PHASE,
        "subgraph_for_scope returned per-scope nodes for every scope",
        subgraph_total_nodes > 0 && subgraph_views == scopes.len() as u64,
    );
    log.check(
        PHASE,
        "every dataset scope rehydrated at least one node",
        rehydrated_per_scope.iter().all(|(_, n, _)| *n > 0),
    );
    log.check(
        PHASE,
        "neighborhood walk surfaced at least one neighbour",
        neighborhood_view.nodes.len() >= 2,
    );
    log.check(
        PHASE,
        "search_nodes located the seeded 'atlas' concept",
        !search_results.is_empty(),
    );
    log.check(
        PHASE,
        "PersistentConceptGraph rehydration matches persisted counts",
        rehydrated_total_nodes as u64 == persisted_node_count
            && rehydrated_total_edges as u64 == persisted_edge_count,
    );
    log.check(
        PHASE,
        "removed_edges from EdgeRemoved propagation == 1",
        removal.removed_edges.len() == 1,
    );

    phase.timing = started.elapsed();
    phase.stat("canonical_seeds", canonical_seed_count.to_string());
    phase.stat("scope_local_clusters", scope_local_clusters.to_string());
    phase.stat(
        "scope_local_extra_nodes",
        scope_local_node_count.to_string(),
    );
    phase.stat(
        "scope_local_extra_edges",
        scope_local_edge_count.to_string(),
    );
    phase.stat("persisted_nodes_baseline", persisted_node_count.to_string());
    phase.stat("persisted_edges_baseline", persisted_edge_count.to_string());
    phase.stat("total_nodes_in_memory", in_memory_node_count.to_string());
    phase.stat("total_edges_in_memory", in_memory_edge_count.to_string());
    phase.stat("rehydrated_node_total", rehydrated_total_nodes.to_string());
    phase.stat("rehydrated_edge_total", rehydrated_total_edges.to_string());
    for (label, n, e) in &rehydrated_per_scope {
        phase.stat(format!("rehydrated:{label}:nodes"), n.to_string());
        phase.stat(format!("rehydrated:{label}:edges"), e.to_string());
    }
    phase.stat(
        "canonical_after_supersede",
        total_canonical_after_supersede.to_string(),
    );
    phase.stat("superseded_total", total_superseded.to_string());
    phase.stat("contradicted_total", total_contradicted.to_string());
    for rel in [
        RelationType::IsA,
        RelationType::PartOf,
        RelationType::DecidedBy,
        RelationType::Supersedes,
        RelationType::Contradicts,
        RelationType::DerivedFrom,
        RelationType::AssignedTo,
    ] {
        phase.stat(
            format!("edges:{}", rel.as_str()),
            typed_edge_count.get(&rel).copied().unwrap_or(0).to_string(),
        );
    }
    phase.stat("subgraph_views", subgraph_views.to_string());
    phase.stat("subgraph_total_nodes", subgraph_total_nodes.to_string());
    phase.stat(
        "neighborhood_node_count",
        neighborhood_view.nodes.len().to_string(),
    );
    phase.stat("search_results", search_results.len().to_string());
    phase.note(
        "PersistentConceptGraph (SQLCipher) + IncrementalUpdateEngine + \
         all 7 RelationTypes + visualization façade.",
    );
    phase.note(
        "Substrate-level canonical concepts persisted in tenant scope; \
         per-scope intra-scope IsA clusters added so every dataset \
         scope's load_scope round-trip is non-empty and \
         scope-cohesive.",
    );

    report.count("concept_canonical_seeds", canonical_seed_count);
    report.count("concept_nodes_total", in_memory_node_count);
    report.count("concept_edges_total", in_memory_edge_count);
    report.count("concept_superseded_total", total_superseded);
    report.count("concept_contradicted_total", total_contradicted);
    report.add_phase(phase);
    report.add_benchmark("concept_graph_propagations", bench_ops, bench_total);

    state.graph_temp = Some(temp);
    state.graph_db_path = Some(db_path);
    drop(pgraph);
}
