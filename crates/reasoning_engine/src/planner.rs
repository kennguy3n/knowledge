//! Query planner — routes queries to the cheapest satisfying
//! retrieval mode.
//!
//! Per `ARCHITECTURE.md` §2.1 ("cheapest retrieval mode first"),
//! the planner classifies an incoming
//! query, picks an ordered chain of retrieval modes, and records
//! which steps were tried and which produced the answer.
//!
//! The hierarchy from cheapest to most expensive is:
//!
//! 1. [`RetrievalMode::Summary`] — pre-computed
//!    summary card lookup.
//! 2. [`RetrievalMode::FTS`] — SQLite FTS5 keyword search.
//! 3. [`RetrievalMode::SemanticVector`] — embedding-based
//!    nearest-neighbour search.
//! 4. [`RetrievalMode::GraphTraversal`] — typed-edge multi-hop
//!    traversal.
//! 5. [`RetrievalMode::RawEvidence`] — fall back to raw
//!    evidence rows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Retrieval modes the substrate can satisfy a query with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    /// Pre-computed `summaries` card lookup.
    Summary,
    /// SQLite FTS5 keyword search.
    Fts,
    /// Embedding-based nearest-neighbour search.
    SemanticVector,
    /// Multi-hop typed-edge graph traversal.
    GraphTraversal,
    /// Fall back to raw evidence rows.
    RawEvidence,
}

impl RetrievalMode {
    /// Cost rank — lower is cheaper. Used to break ties when
    /// multiple heuristics suggest different orderings.
    pub const fn cost_rank(self) -> u8 {
        match self {
            Self::Summary => 0,
            Self::Fts => 1,
            Self::SemanticVector => 2,
            Self::GraphTraversal => 3,
            Self::RawEvidence => 4,
        }
    }
}

/// Categories the [`QueryClassifier`] may assign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    /// "Who is X" / "What is Y" — single entity lookup.
    PointLookup,
    /// "Who decided X" / "What was approved by Y" — typed
    /// relation between entities.
    Relational,
    /// Recency-sensitive ("yesterday", "this week").
    Temporal,
    /// "Catch me up", "summarise the project" — broad,
    /// holistic.
    Holistic,
    /// Unknown / fallback.
    Other,
}

/// Lightweight rule-based classifier. The full implementation
/// can swap this for an SLM call without changing the planner's
/// shape.
#[derive(Debug, Clone, Default)]
pub struct QueryClassifier;

impl QueryClassifier {
    /// Classify a free-form query string.
    pub fn classify(&self, q: &str) -> QueryClass {
        let lower = q.to_lowercase();
        let temporal_markers = [
            "yesterday",
            "today",
            "last week",
            "this week",
            "last month",
            "recent",
            "recently",
            "just now",
        ];
        let holistic_markers = [
            "catch me up",
            "summary",
            "summarise",
            "summarize",
            "overview",
            "tldr",
            "tl;dr",
            "what's going on",
            "what is going on",
            "the project",
        ];
        let relational_markers = [
            "decided by",
            "approved by",
            "assigned to",
            "owns",
            "owner of",
            "linked to",
            "part of",
            "depends on",
            "blocked by",
            "blocking",
        ];
        let point_markers = ["who is", "what is", "what's", "who's"];

        if temporal_markers.iter().any(|m| lower.contains(m)) {
            return QueryClass::Temporal;
        }
        if holistic_markers.iter().any(|m| lower.contains(m)) {
            return QueryClass::Holistic;
        }
        if relational_markers.iter().any(|m| lower.contains(m)) {
            return QueryClass::Relational;
        }
        if point_markers.iter().any(|m| lower.starts_with(m)) {
            return QueryClass::PointLookup;
        }
        QueryClass::Other
    }
}

/// Heuristics mapping a [`QueryClass`] to an ordered fallback
/// chain of [`RetrievalMode`]s.
#[derive(Debug, Clone)]
pub struct PlannerHeuristics {
    /// Per-class chains. Stored as a `Vec` of `(class, chain)`
    /// rather than a `HashMap` so the order is deterministic
    /// and the struct remains `Serialize`-friendly.
    pub chains: Vec<(QueryClass, Vec<RetrievalMode>)>,
    /// Default chain used for [`QueryClass::Other`] and any
    /// class without an explicit override.
    pub default_chain: Vec<RetrievalMode>,
}

impl Default for PlannerHeuristics {
    fn default() -> Self {
        use RetrievalMode::{Fts, GraphTraversal, RawEvidence, SemanticVector, Summary};
        Self {
            chains: vec![
                (
                    QueryClass::PointLookup,
                    vec![Summary, Fts, SemanticVector, RawEvidence],
                ),
                (
                    QueryClass::Relational,
                    vec![GraphTraversal, Fts, RawEvidence],
                ),
                (
                    QueryClass::Temporal,
                    vec![Fts, SemanticVector, Summary, RawEvidence],
                ),
                (
                    QueryClass::Holistic,
                    vec![GraphTraversal, Summary, RawEvidence],
                ),
            ],
            default_chain: vec![Summary, Fts, SemanticVector, GraphTraversal, RawEvidence],
        }
    }
}

impl PlannerHeuristics {
    /// Look up the fallback chain for a class; falls back to
    /// `default_chain` if the class has no override.
    pub fn chain_for(&self, class: QueryClass) -> &[RetrievalMode] {
        for (c, chain) in &self.chains {
            if *c == class {
                return chain;
            }
        }
        &self.default_chain
    }

    /// Override the chain for a class.
    pub fn with_chain(mut self, class: QueryClass, chain: Vec<RetrievalMode>) -> Self {
        // Drop any existing entry with the same class so the
        // override is always honoured.
        self.chains.retain(|(c, _)| *c != class);
        self.chains.push((class, chain));
        self
    }
}

/// One step in a [`QueryPlan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalStep {
    /// Mode to attempt.
    pub mode: RetrievalMode,
    /// Wall-clock budget for this individual step, in
    /// milliseconds. `None` means the step inherits the
    /// caller's overall budget.
    pub time_budget_ms: Option<u64>,
}

impl RetrievalStep {
    /// Construct a step with the supplied mode and no per-step
    /// time budget.
    pub fn new(mode: RetrievalMode) -> Self {
        Self {
            mode,
            time_budget_ms: None,
        }
    }

    /// Override the per-step budget.
    pub fn with_budget_ms(mut self, ms: u64) -> Self {
        self.time_budget_ms = Some(ms);
        self
    }
}

/// Plan emitted by the [`QueryPlanner`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryPlan {
    /// Original query text.
    pub query: String,
    /// Class assigned by the classifier.
    pub class: QueryClass,
    /// Ordered list of retrieval steps. Earlier steps are
    /// cheaper; later steps are fallbacks.
    pub steps: Vec<RetrievalStep>,
    /// Wall-clock time the plan was produced.
    pub planned_at: DateTime<Utc>,
}

/// Outcome of executing one step in a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// Step succeeded — stop the chain.
    Success,
    /// Step ran but produced no useful answer — try the next.
    NoMatch,
    /// Step exceeded its budget — try the next.
    BudgetExhausted,
    /// Step errored — try the next.
    Error,
}

/// Result of executing a [`QueryPlan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanExecutionResult {
    /// Plan that was executed.
    pub plan: QueryPlan,
    /// Per-step (mode, outcome) records, in the order they were
    /// attempted.
    pub attempts: Vec<(RetrievalMode, StepOutcome)>,
    /// Mode that produced the answer (the first to return
    /// `Success`), if any.
    pub answered_by: Option<RetrievalMode>,
}

impl PlanExecutionResult {
    /// True iff some step in the chain returned `Success`.
    pub fn succeeded(&self) -> bool {
        self.answered_by.is_some()
    }
}

/// Top-level planner type.
#[derive(Debug, Clone, Default)]
pub struct QueryPlanner {
    /// Classifier used by [`Self::plan`].
    pub classifier: QueryClassifier,
    /// Heuristics used by [`Self::plan`].
    pub heuristics: PlannerHeuristics,
}

impl QueryPlanner {
    /// Construct a planner with default classifier and
    /// heuristics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the heuristics.
    pub fn with_heuristics(mut self, heuristics: PlannerHeuristics) -> Self {
        self.heuristics = heuristics;
        self
    }

    /// Produce a plan for `query`.
    pub fn plan(&self, query: &str) -> QueryPlan {
        let class = self.classifier.classify(query);
        let chain = self.heuristics.chain_for(class);
        let steps = chain
            .iter()
            .copied()
            .map(RetrievalStep::new)
            .collect::<Vec<_>>();
        QueryPlan {
            query: query.to_string(),
            class,
            steps,
            planned_at: Utc::now(),
        }
    }

    /// Execute `plan` against `executor`. The executor is a
    /// closure called once per step until it returns `Success`
    /// or the chain is exhausted.
    pub fn execute<F>(&self, plan: QueryPlan, mut executor: F) -> PlanExecutionResult
    where
        F: FnMut(RetrievalMode) -> StepOutcome,
    {
        let mut attempts = Vec::new();
        let mut answered_by = None;
        for step in &plan.steps {
            let outcome = executor(step.mode);
            attempts.push((step.mode, outcome));
            if outcome == StepOutcome::Success {
                answered_by = Some(step.mode);
                break;
            }
        }
        PlanExecutionResult {
            plan,
            attempts,
            answered_by,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_routes_point_lookups() {
        let c = QueryClassifier;
        assert_eq!(c.classify("Who is Sara?"), QueryClass::PointLookup);
        assert_eq!(c.classify("What is Atlas?"), QueryClass::PointLookup);
    }

    #[test]
    fn classifier_routes_relational_queries() {
        let c = QueryClassifier;
        assert_eq!(c.classify("Who decided by Sara?"), QueryClass::Relational);
        assert_eq!(
            c.classify("What is assigned to Eng?"),
            QueryClass::Relational
        );
    }

    #[test]
    fn classifier_routes_temporal_queries() {
        let c = QueryClassifier;
        assert_eq!(c.classify("What changed yesterday"), QueryClass::Temporal);
        assert_eq!(c.classify("Recent decisions"), QueryClass::Temporal);
    }

    #[test]
    fn classifier_routes_holistic_queries() {
        let c = QueryClassifier;
        assert_eq!(c.classify("Catch me up"), QueryClass::Holistic);
        assert_eq!(
            c.classify("Give me an overview of the project"),
            QueryClass::Holistic
        );
    }

    #[test]
    fn point_lookup_plan_starts_with_summary_or_fts() {
        let p = QueryPlanner::new();
        let plan = p.plan("Who is Sara?");
        assert_eq!(plan.class, QueryClass::PointLookup);
        assert_eq!(plan.steps.first().unwrap().mode, RetrievalMode::Summary);
        assert_eq!(plan.steps.get(1).unwrap().mode, RetrievalMode::Fts);
    }

    #[test]
    fn relational_plan_starts_with_graph() {
        let p = QueryPlanner::new();
        let plan = p.plan("Who is the decider on the launch decided by Sara?");
        assert_eq!(plan.class, QueryClass::Relational);
        assert_eq!(
            plan.steps.first().unwrap().mode,
            RetrievalMode::GraphTraversal
        );
    }

    #[test]
    fn holistic_plan_starts_with_graph_or_summary() {
        let p = QueryPlanner::new();
        let plan = p.plan("Catch me up on the launch");
        assert_eq!(plan.class, QueryClass::Holistic);
        let first = plan.steps.first().unwrap().mode;
        assert!(matches!(
            first,
            RetrievalMode::GraphTraversal | RetrievalMode::Summary
        ));
    }

    #[test]
    fn execute_stops_at_first_success() {
        let p = QueryPlanner::new();
        let plan = p.plan("Who is Sara?");
        let mut calls = 0_usize;
        let res = p.execute(plan, |mode| {
            calls += 1;
            if mode == RetrievalMode::Fts {
                StepOutcome::Success
            } else {
                StepOutcome::NoMatch
            }
        });
        assert_eq!(calls, 2);
        assert_eq!(res.answered_by, Some(RetrievalMode::Fts));
        assert!(res.succeeded());
    }

    #[test]
    fn execute_falls_through_until_exhaustion() {
        let p = QueryPlanner::new();
        let plan = p.plan("Who is Sara?");
        let res = p.execute(plan, |_| StepOutcome::NoMatch);
        assert!(!res.succeeded());
        assert!(res.answered_by.is_none());
        assert_eq!(res.attempts.len(), 4);
    }

    #[test]
    fn override_chain_takes_precedence() {
        let h = PlannerHeuristics::default()
            .with_chain(QueryClass::PointLookup, vec![RetrievalMode::RawEvidence]);
        let p = QueryPlanner::new().with_heuristics(h);
        let plan = p.plan("Who is Sara?");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].mode, RetrievalMode::RawEvidence);
    }
}
