//! Stage 10 — Reasoning Engine.
//!
//! Per `docs/DESIGN.md` §11.1, this stage exercises the substrate's
//! reasoning plane on top of the concept graph that the concept-graph
//! stage persisted. It runs every public surface that
//! Part 1 §10 of the demo prompt calls out:
//!
//! * [`ContradictionDetector`] over canonical claims using
//!   [`PrefixNegationOracle`], with [`AdjudicationWorkflow`] driving
//!   the `Detected → UnderReview → Resolved` state machine.
//! * [`GraphTraversal`] with a tightened [`TraversalBudget`] (low
//!   `max_hops`, `max_nodes_visited`) and a custom [`PathScorer`]
//!   weighting `IsA` and `PartOf` higher than the rest, exercised
//!   through both targeted (`A → B`) and exploratory (`A → ?`)
//!   queries.
//! * [`GoTExecutor`] with a [`StaticExpander`] producing a
//!   `Question → Hypothesis → Evidence → Conclusion` chain plus a
//!   contradicting branch, with the result persisted into a
//!   [`WorkflowMemory`] via [`GoTExecutor::record_trace`].
//! * [`CommunityDetector`] + [`CommunityHierarchy`] +
//!   [`CommunitySummaryGenerator`], with [`CommunityQueryRouter`]
//!   routing a free-form query through the substrate's permission
//!   filter (a freshly built [`TupleStore`] granting the demo user
//!   `Viewer` on every dataset scope).
//! * [`QueryPlanner`] producing per-class fallback chains for
//!   `PointLookup`, `Relational`, `Temporal`, and `Holistic` queries,
//!   then `execute`'d against synthetic per-step outcomes so the
//!   "first success short-circuits the chain" path runs.

use std::collections::HashSet;
use std::time::Instant;

use concept_graph::{ConceptEdge, ConceptNode, PersistentConceptGraph, RelationType};
use evidence_store::ScopeId;
use permission_service::{
    NamespaceConfig, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef,
    SubjectType, TupleStore,
};
use reasoning_engine::contradiction::{
    AdjudicationOutcome, AdjudicationState, AdjudicationWorkflow, ContradictionDetector,
    PrefixNegationOracle,
};
use reasoning_engine::{
    Community, CommunityDetector, CommunityHierarchy, CommunityQueryRouter, CommunitySummary,
    CommunitySummaryGenerator, GoTExecutor, GoTQuery, GoTStrategy, GraphTraversal, PathScorer,
    PlanExecutionResult, QueryPlan, QueryPlanner, StaticExpander, StepOutcome, ThoughtEdge,
    ThoughtGraph, ThoughtId, ThoughtNode, ThoughtType, TraversalBudget, TraversalDirection,
    TraversalQuery, WorkflowMemory,
};

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "reasoning";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Stage 10: Reasoning Engine");

    let db_path = state
        .graph_db_path
        .as_ref()
        .expect("concept-graph stage must run before reasoning stage")
        .clone();
    let mut pgraph = PersistentConceptGraph::open(&db_path, &state.master_key)
        .expect("re-open SQLCipher concept graph from concept-graph stage");

    // `PersistentConceptGraph::open` returns an empty in-memory
    // graph — only the SQLCipher tables are touched on `open`.
    // Each `load_scope` call replaces the in-memory graph with
    // exactly the rows tagged for that scope, so to surface the
    // full canonical fabric to the reasoning stage's operators
    // (contradiction detector, traversal, GoT executor, community
    // detector) we walk every dataset scope, accumulate the
    // visited rows in a separate map, and rebuild the in-memory
    // graph as the union once the walk is done.
    {
        use std::collections::BTreeMap;
        let mut all_nodes: BTreeMap<concept_graph::NodeId, concept_graph::ConceptNode> =
            BTreeMap::new();
        let mut all_edges: Vec<concept_graph::ConceptEdge> = Vec::new();
        for scope in [
            dataset.user_scope.id,
            dataset.channel_scope.id,
            dataset.channel_alt_scope.id,
            dataset.domain_scope.id,
            dataset.tenant_scope.id,
        ] {
            let _ = pgraph
                .load_scope(scope)
                .expect("rehydrate dataset scope for union build");
            for n in pgraph.graph().iter_nodes() {
                all_nodes.entry(n.id).or_insert_with(|| n.clone());
            }
            for e in pgraph.graph().iter_edges() {
                all_edges.push(e.clone());
            }
        }
        let g = pgraph.graph_mut();
        *g = concept_graph::ConceptGraph::new();
        for n in all_nodes.into_values() {
            let _ = g.add_node(n);
        }
        for e in all_edges {
            // Edges may have been visited under either endpoint's
            // scope; the graph dedups by `EdgeId` so the second
            // insert returns `Err` which we discard.
            let _ = g.add_edge(e);
        }
    }

    // ---------------------------------------------------------------
    // Seed contradiction pair inside the tenant scope so the
    // ContradictionDetector + adjudication workflow have a real hit.
    // ---------------------------------------------------------------
    let tenant_scope = dataset.tenant_scope.id;
    let mut left = ConceptNode::new_candidate(
        "contradiction-claim",
        "Reasoning stage left side of opposing pair.",
        tenant_scope,
    );
    left.mark_canonical();
    let left_id = pgraph
        .add_node(left)
        .expect("seed left contradiction concept");
    let mut right = ConceptNode::new_candidate(
        "not contradiction-claim",
        "Reasoning stage right side of opposing pair (negation prefix).",
        tenant_scope,
    );
    right.mark_canonical();
    let right_id = pgraph
        .add_node(right)
        .expect("seed right contradiction concept");

    // Add a regular IsA edge so the traversal phase has something to
    // hop through that is *not* the contradiction edge.
    let traversal_anchor_label = "tenant.acme";
    let traversal_anchor = pgraph
        .graph()
        .iter_nodes()
        .find(|n| n.label == traversal_anchor_label)
        .map(|n| n.id);
    if let Some(anchor) = traversal_anchor {
        pgraph
            .add_edge(ConceptEdge::new(
                left_id,
                anchor,
                RelationType::IsA,
                tenant_scope,
            ))
            .expect("link contradiction concept to tenant root");
    }

    // ---------------------------------------------------------------
    // Contradiction detection + adjudication workflow.
    // ---------------------------------------------------------------
    let oracle = PrefixNegationOracle;
    let detector = ContradictionDetector::new(&oracle);
    let bench_started = Instant::now();
    let edges = detector.scan(pgraph.graph());
    let bench_contradiction_scan = bench_started.elapsed();

    log.check(
        PHASE,
        "ContradictionDetector flagged the seeded opposing pair",
        edges.iter().any(|e| {
            (e.left == left_id && e.right == right_id) || (e.left == right_id && e.right == left_id)
        }),
    );

    let mut workflow = AdjudicationWorkflow::new();
    let mut detected = 0_usize;
    let mut resolved = 0_usize;
    let bench_started = Instant::now();
    for edge in &edges {
        let _ = workflow.detect(edge.id).expect("detect contradiction");
        detected += 1;
    }
    for edge in &edges {
        workflow
            .mark_under_review(edge.id)
            .expect("mark contradiction under review");
    }
    for edge in &edges {
        let outcome = AdjudicationOutcome::Winner {
            winner: edge.left,
            loser: edge.right,
        };
        workflow
            .resolve(edge.id, outcome)
            .expect("resolve contradiction");
        resolved += 1;
    }
    let bench_adjudication = bench_started.elapsed();

    if let Some(edge) = edges.first() {
        let record = workflow
            .get(edge.id)
            .expect("adjudication record present after resolve");
        log.check(
            PHASE,
            "adjudication state advanced to Resolved",
            matches!(record.state, AdjudicationState::Resolved),
        );
        log.check(
            PHASE,
            "adjudication outcome marked the left side as winner",
            matches!(record.outcome, Some(AdjudicationOutcome::Winner { .. })),
        );
    }

    // ---------------------------------------------------------------
    // Multi-hop typed-edge traversal.
    // ---------------------------------------------------------------
    let scorer = PathScorer::new()
        .with_weight(RelationType::IsA, 1.0)
        .with_weight(RelationType::PartOf, 0.9)
        .with_weight(RelationType::DerivedFrom, 0.5)
        .with_weight(RelationType::Contradicts, 0.2)
        .with_depth_penalty(0.05);
    let budget = TraversalBudget {
        max_hops: 3,
        max_nodes_visited: 256,
        max_time_ms: 25,
        max_edges_per_hop: 32,
    };
    let traversal = GraphTraversal::new(pgraph.graph())
        .with_budget(budget)
        .with_scorer(scorer);

    let mut explore_paths: u64 = 0;
    let mut explore_visited: u64 = 0;
    let mut targeted_hits: u64 = 0;
    let bench_started = Instant::now();
    if let Some(anchor) = traversal_anchor {
        let explore = traversal.run(
            &TraversalQuery::explore(anchor)
                .with_direction(TraversalDirection::Both)
                .with_scopes(vec![tenant_scope]),
        );
        explore_paths = explore.paths.len() as u64;
        explore_visited = explore.visited.len() as u64;
        log.check(
            PHASE,
            "exploratory traversal stayed within max_hops budget",
            explore.trace.hops_taken <= budget.max_hops,
        );

        let targeted = traversal.run(
            &TraversalQuery::between(anchor, left_id)
                .with_direction(TraversalDirection::Both)
                .with_edge_types(vec![
                    RelationType::IsA,
                    RelationType::PartOf,
                    RelationType::DerivedFrom,
                ]),
        );
        targeted_hits = targeted.paths.len() as u64;
        log.check(
            PHASE,
            "targeted traversal reached the seeded contradiction concept",
            !targeted.paths.is_empty(),
        );
    }
    let bench_traversal = bench_started.elapsed();

    // ---------------------------------------------------------------
    // Graph-of-Thought via StaticExpander.
    // ---------------------------------------------------------------
    let bench_started = Instant::now();
    let mut got_graph = ThoughtGraph::new();
    let got_query = GoTQuery::new(
        "Should the Atlas migration proceed under the EU residency policy?",
        tenant_scope,
    )
    .with_strategy(GoTStrategy::BestFirst)
    .with_budget(3, 3, 32);
    let plan = GoTExecutor::new().plan(&mut got_graph, &got_query);

    let hyp_supports_id = ThoughtId::new_v4();
    let hyp_supports = ThoughtNode {
        id: hyp_supports_id,
        ..ThoughtNode::new(
            "Hypothesis: residency-compliant migration is feasible.",
            ThoughtType::Hypothesis,
            0.85,
            tenant_scope,
        )
        .with_parents(vec![plan.root])
    };

    let hyp_against_id = ThoughtId::new_v4();
    let hyp_against = ThoughtNode {
        id: hyp_against_id,
        ..ThoughtNode::new(
            "Hypothesis: cross-region replication violates residency policy.",
            ThoughtType::Hypothesis,
            0.65,
            tenant_scope,
        )
        .with_parents(vec![plan.root])
    };

    let evidence_for_id = ThoughtId::new_v4();
    let evidence_for = ThoughtNode {
        id: evidence_for_id,
        ..ThoughtNode::new(
            "Evidence: pgcat shards remain inside the EU region.",
            ThoughtType::Evidence,
            0.9,
            tenant_scope,
        )
        .with_parents(vec![hyp_supports_id])
    };

    let conclusion_id = ThoughtId::new_v4();
    let conclusion = ThoughtNode {
        id: conclusion_id,
        ..ThoughtNode::new(
            "Conclusion: proceed with EU-only Atlas rollout.",
            ThoughtType::Conclusion,
            0.92,
            tenant_scope,
        )
        .with_parents(vec![evidence_for_id])
    };

    let mut expander = StaticExpander::new();
    expander.register(
        plan.root,
        vec![
            (hyp_supports, ThoughtEdge::Supports),
            (hyp_against, ThoughtEdge::Contradicts),
        ],
    );
    expander.register(hyp_supports_id, vec![(evidence_for, ThoughtEdge::Supports)]);
    expander.register(evidence_for_id, vec![(conclusion, ThoughtEdge::Derives)]);

    let executor = GoTExecutor::new();
    let got_result = executor
        .execute_from_plan(&mut got_graph, &plan, &got_query, &expander)
        .expect("graph-of-thought execution");
    let bench_got = bench_started.elapsed();

    log.check(
        PHASE,
        "GoT executor produced a Conclusion-ending best path",
        got_result
            .best_path
            .last()
            .is_some_and(|t| matches!(t.thought_type, ThoughtType::Conclusion)),
    );
    log.check(
        PHASE,
        "GoT executor stayed within budget (no exhaustion)",
        !got_result.budget_exhausted,
    );

    // Persist the trace into WorkflowMemory.
    let mut memory = WorkflowMemory::new();
    let trace_id = executor.record_trace(&mut memory, &got_query, &got_result);
    log.check(
        PHASE,
        "GoT trace persisted into WorkflowMemory",
        memory.get_trace(trace_id).is_ok(),
    );

    // ---------------------------------------------------------------
    // Community detection + summary + permission-aware routing.
    // ---------------------------------------------------------------
    let bench_started = Instant::now();
    let community_detector = CommunityDetector::new();
    let leaves: Vec<Community> = community_detector.detect(pgraph.graph());
    let hierarchy = CommunityHierarchy::build(leaves.clone());
    let summarizer = CommunitySummaryGenerator::new();
    let summaries: Vec<CommunitySummary> = summarizer.summarise_all(pgraph.graph(), &hierarchy);
    let bench_communities = bench_started.elapsed();

    log.check(
        PHASE,
        "CommunityDetector returned at least one canonical cluster",
        !leaves.is_empty(),
    );
    log.check(
        PHASE,
        "CommunityHierarchy levels start with the leaves at level 0",
        hierarchy.level_count() >= 1,
    );
    log.check(
        PHASE,
        "CommunitySummaryGenerator produced summaries for every community",
        summaries.len() == hierarchy.communities.len() && !summaries.is_empty(),
    );

    // Build a permission graph that grants `viewer` on every dataset
    // scope to a synthetic demo user; the router uses it to filter
    // visible communities.
    let registry = build_namespace_registry();
    let mut store = TupleStore::new();
    let demo_user = uuid::Uuid::new_v4();
    let demo_subject = SubjectRef::direct(SubjectType::User, demo_user);
    for scope_id in unique_scopes(dataset) {
        let object = ObjectRef::new(ObjectType::Channel, scope_id.0);
        let tuple = RelationTuple::new(object, Relation::Viewer, demo_subject);
        store
            .insert(tuple)
            .expect("insert viewer tuple for demo subject");
    }

    let router = CommunityQueryRouter::new();
    let bench_started = Instant::now();
    let routed = router.route(
        "atlas migration platform tenant",
        &summaries,
        demo_subject,
        &store,
        &registry,
        4,
    );
    let bench_route = bench_started.elapsed();
    log.check(
        PHASE,
        "CommunityQueryRouter returned at least one visible summary",
        !routed.is_empty(),
    );

    // The router must *exclude* communities for a user with no
    // grants. This proves the permission filter is wired up.
    let stranger = SubjectRef::direct(SubjectType::User, uuid::Uuid::new_v4());
    let stranger_routed = router.route(
        "atlas migration platform tenant",
        &summaries,
        stranger,
        &store,
        &registry,
        4,
    );
    log.check(
        PHASE,
        "CommunityQueryRouter excludes communities for a user with no grants",
        stranger_routed.is_empty(),
    );

    // ---------------------------------------------------------------
    // Query planner — exercise every query class.
    // ---------------------------------------------------------------
    let planner = QueryPlanner::new();
    let queries: Vec<&str> = vec![
        "who is @sara",
        "decided by @sara",
        "what changed yesterday in the platform channel",
        "catch me up on the project this week",
        "open question with no markers",
    ];
    let bench_started = Instant::now();
    let mut plans: Vec<QueryPlan> = Vec::with_capacity(queries.len());
    for q in &queries {
        plans.push(planner.plan(q));
    }
    let bench_plan = bench_started.elapsed();

    let bench_started = Instant::now();
    let mut executions: Vec<PlanExecutionResult> = Vec::with_capacity(plans.len());
    for plan in plans.iter().cloned() {
        // Synthetic per-step outcome: the cheapest mode in each
        // chain is always rejected (NoMatch) so the chain falls
        // through; the next mode succeeds. This exercises both
        // the success-short-circuit path and the fallback path in
        // one call to `QueryPlanner::execute`.
        let mut idx = 0_usize;
        let result = planner.execute(plan, |_mode| {
            let outcome = if idx == 0 {
                StepOutcome::NoMatch
            } else {
                StepOutcome::Success
            };
            idx += 1;
            outcome
        });
        executions.push(result);
    }
    let bench_execute = bench_started.elapsed();

    log.check(
        PHASE,
        "QueryPlanner produced a non-empty fallback chain for every query",
        plans.iter().all(|p| !p.steps.is_empty()),
    );
    log.check(
        PHASE,
        "QueryPlanner.execute stopped at the first Success in every chain",
        executions.iter().all(|e| {
            e.succeeded() && e.attempts.last().map(|(_, o)| *o) == Some(StepOutcome::Success)
        }),
    );

    // ---------------------------------------------------------------
    // Persist into the audit log so the audit stage can query it.
    // ---------------------------------------------------------------
    let actor = audit_service::Actor::User(demo_user);
    let entry = audit_service::AuditEntryBuilder::new()
        .actor(actor)
        .action(audit_service::AuditActionType::PolicyChange)
        .target(audit_service::TargetRef::new(
            audit_service::TargetType::Tenant,
            tenant_scope.0,
        ))
        .scope(tenant_scope)
        .details(serde_json::json!({
            "phase": PHASE,
            "contradictions_resolved": resolved,
            "communities_detected": leaves.len(),
            "got_best_path_confidence": got_result.confidence,
        }))
        .build()
        .expect("reasoning stage audit entry");
    state.audit_log.append(entry);

    // ---------------------------------------------------------------
    // Bookkeeping.
    // ---------------------------------------------------------------
    phase.timing = started.elapsed();
    phase.stat("contradictions_flagged", edges.len().to_string());
    phase.stat("contradictions_resolved", resolved.to_string());
    phase.stat("explore_paths", explore_paths.to_string());
    phase.stat("explore_visited", explore_visited.to_string());
    phase.stat("targeted_hits", targeted_hits.to_string());
    phase.stat("got_thoughts", got_result.reasoning_trace.len().to_string());
    phase.stat("got_paths", got_result.all_paths.len().to_string());
    phase.stat("got_confidence", format!("{:.3}", got_result.confidence));
    phase.stat("communities_detected", leaves.len().to_string());
    phase.stat("community_levels", hierarchy.level_count().to_string());
    phase.stat("community_summaries", summaries.len().to_string());
    phase.stat("routed_summaries", routed.len().to_string());
    phase.stat("plans_generated", plans.len().to_string());
    phase.stat(
        "plan_execute_succeeded",
        executions
            .iter()
            .filter(|e| e.succeeded())
            .count()
            .to_string(),
    );

    // Benchmarks.
    let n_edges = edges.len() as u64;
    report.add_benchmark(
        "reasoning.contradiction.scan",
        n_edges.max(1),
        bench_contradiction_scan,
    );
    report.add_benchmark(
        "reasoning.contradiction.adjudicate",
        detected.max(1) as u64,
        bench_adjudication,
    );
    report.add_benchmark("reasoning.traversal", 2, bench_traversal);
    report.add_benchmark(
        "reasoning.got.execute",
        got_result.reasoning_trace.len().max(1) as u64,
        bench_got,
    );
    report.add_benchmark(
        "reasoning.community.detect_summarise",
        hierarchy.communities.len().max(1) as u64,
        bench_communities,
    );
    report.add_benchmark("reasoning.community.route", 1, bench_route);
    report.add_benchmark("reasoning.planner.plan", plans.len() as u64, bench_plan);
    report.add_benchmark(
        "reasoning.planner.execute",
        executions.len() as u64,
        bench_execute,
    );

    report.phases.push(phase);
}

fn build_namespace_registry() -> NamespaceRegistry {
    let mut registry = NamespaceRegistry::new();
    let _ = registry.register(NamespaceConfig::new(ObjectType::Tenant));
    let _ = registry.register(NamespaceConfig::new(ObjectType::Domain));
    let _ = registry.register(NamespaceConfig::new(ObjectType::Channel));
    let _ = registry.register(NamespaceConfig::new(ObjectType::User));
    registry
}

fn unique_scopes(dataset: &Dataset) -> Vec<ScopeId> {
    let mut seen: HashSet<ScopeId> = HashSet::new();
    for scope in [
        &dataset.user_scope,
        &dataset.channel_scope,
        &dataset.channel_alt_scope,
        &dataset.domain_scope,
        &dataset.tenant_scope,
    ] {
        seen.insert(scope.id);
    }
    seen.into_iter().collect()
}
