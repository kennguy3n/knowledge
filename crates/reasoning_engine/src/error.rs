//! Error types for the reasoning engine.

use thiserror::Error;

/// Errors raised by the reasoning engine.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReasoningError {
    /// A traversal exceeded its configured budget.
    #[error("traversal budget exceeded ({0})")]
    BudgetExceeded(&'static str),

    /// A node referenced by a query / detector / propagator was not
    /// present in the graph.
    #[error("node not found")]
    NodeNotFound,

    /// An adjudication transition was attempted from an invalid
    /// state.
    #[error("invalid adjudication transition")]
    InvalidAdjudicationTransition,

    /// A workflow trace lookup missed.
    #[error("workflow trace not found")]
    TraceNotFound,
}

/// Convenience alias.
pub type Result<T, E = ReasoningError> = std::result::Result<T, E>;
