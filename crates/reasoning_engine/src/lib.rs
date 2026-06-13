//! `reasoning_engine` — reasoning surface for the Knowledge
//! substrate.
//!
//! Per `docs/technical/design.md` §11.1, the reasoning
//! engine layers four capabilities on top of the concept graph:
//!
//! * Contradiction and drift detection — `[contradiction]` /
//!   `[drift]`.
//! * Multi-hop typed-edge traversal with explicit budgets —
//!   [`traversal`].
//! * Query planning that routes to the cheapest satisfying
//!   retrieval mode (Summary → FTS → Vector → Graph → Raw) —
//!   [`planner`].
//! * Workflow memory — recording successful reasoning traces and
//!   abstracting them into reusable patterns — [`workflow`].
//!
//! These modules are independently usable but compose into the
//! reasoning plane described in `docs/technical/design.md` §3.4.

#![deny(missing_docs)]

// UNSTABLE — community detection; API may change.
pub mod community;
// STABLE
pub mod contradiction;
// STABLE
pub mod drift;
// STABLE
pub mod error;
// UNSTABLE — graph-of-thought reasoning; API may change.
pub mod graph_of_thought;
// STABLE
pub mod planner;
// STABLE
pub mod traversal;
// UNSTABLE — workflow memory; API still evolving.
pub mod workflow;

// UNSTABLE — community detection; API may change.
pub use community::{
    Community, CommunityDetector, CommunityHierarchy, CommunityId, CommunityQueryRouter,
    CommunitySummary, CommunitySummaryGenerator,
};
// STABLE
pub use contradiction::{
    AdjudicationOutcome, AdjudicationState, AdjudicationWorkflow, ContradictionDetector,
    ContradictionEdge, NegationOracle, OpposingClaimOracle, PrefixNegationOracle,
};
// STABLE
pub use drift::{DriftDetector, DriftMarker, DriftReason, EvidenceSnapshot};
// STABLE
pub use error::{ReasoningError, Result};
// UNSTABLE — graph-of-thought reasoning; API may change.
pub use graph_of_thought::{
    Expander, GoTError, GoTExecutor, GoTPlan, GoTQuery, GoTResult, GoTStrategy, GraphExpander,
    ScoredPath, StaticExpander, ThoughtEdge, ThoughtGraph, ThoughtId, ThoughtNode, ThoughtType,
};
// STABLE
pub use planner::{
    PlanExecutionResult, PlannerHeuristics, QueryClass, QueryClassifier, QueryPlan, QueryPlanner,
    RetrievalMode, RetrievalStep, StepOutcome,
};
// STABLE
pub use traversal::{
    GraphTraversal, PathScorer, TraversalBudget, TraversalDirection, TraversalQuery,
    TraversalResult, TraversedPath,
};
// UNSTABLE — workflow memory; API still evolving.
pub use workflow::{
    PatternMatcher, RecordedStep, TraceRecorder, WorkflowMemory, WorkflowPattern, WorkflowTrace,
};
