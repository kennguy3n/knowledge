//! Error type for the concept graph.

use thiserror::Error;
use uuid::Uuid;

use crate::edge::EdgeId;
use crate::node::NodeId;

/// Errors surfaced by the concept graph.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    /// The requested node id is not present in the graph.
    #[error("concept node not found: {0}")]
    NodeNotFound(Uuid),

    /// The requested edge id is not present in the graph.
    #[error("concept edge not found: {0}")]
    EdgeNotFound(Uuid),

    /// `add_node` was called with an id that already exists.
    #[error("duplicate concept node: {0}")]
    DuplicateNode(Uuid),

    /// `add_edge` referenced a `from` or `to` node that does not exist.
    #[error("dangling edge — endpoint node missing: {0}")]
    DanglingEdge(Uuid),

    /// `supersede_node` was called with the same id for both arguments.
    #[error("a node cannot supersede itself: {0}")]
    SelfSupersession(Uuid),

    /// `mark_contradiction` was called with the same id for both arguments.
    #[error("a node cannot contradict itself: {0}")]
    SelfContradiction(Uuid),
}

impl GraphError {
    /// Convenience: build a `NodeNotFound` from a [`NodeId`].
    pub fn node_not_found(id: NodeId) -> Self {
        Self::NodeNotFound(id.0)
    }

    /// Convenience: build an `EdgeNotFound` from an [`EdgeId`].
    pub fn edge_not_found(id: EdgeId) -> Self {
        Self::EdgeNotFound(id.0)
    }
}

/// Convenience result alias.
pub type Result<T, E = GraphError> = std::result::Result<T, E>;
