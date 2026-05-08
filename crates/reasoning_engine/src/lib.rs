//! `reasoning_engine` — Phase 6 reasoning surface for the Knowledge
//! substrate.
//!
//! Per `PROPOSAL.md` §11.1 and `PHASES.md` Phase 6, the reasoning
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
//! reasoning plane described in `PROPOSAL.md` §3.4.

#![deny(missing_docs)]

pub mod contradiction;
pub mod drift;
pub mod error;
pub mod planner;
pub mod traversal;
pub mod workflow;

pub use contradiction::{
    AdjudicationOutcome, AdjudicationState, AdjudicationWorkflow, ContradictionDetector,
    ContradictionEdge,
};
pub use drift::{DriftDetector, DriftMarker, DriftReason};
pub use error::{ReasoningError, Result};
pub use planner::{
    PlanExecutionResult, PlannerHeuristics, QueryClass, QueryClassifier, QueryPlan, QueryPlanner,
    RetrievalMode, RetrievalStep, StepOutcome,
};
pub use traversal::{
    GraphTraversal, PathScorer, TraversalBudget, TraversalDirection, TraversalQuery,
    TraversalResult, TraversedPath,
};
pub use workflow::{
    PatternMatcher, RecordedStep, TraceRecorder, WorkflowMemory, WorkflowPattern, WorkflowTrace,
};
