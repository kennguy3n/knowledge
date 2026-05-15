//! `concept_graph` — sparse typed concept graph for the Knowledge
//! substrate.
//!
//! Per `ARCHITECTURE.md` §2.1 and `docs/DESIGN.md` §3.3, the semantic
//! plane is a *sparse* concept graph with typed relations
//! (`is_a`, `part_of`, `decided_by`, `supersedes`, `contradicts`,
//! `derived_from`, `assigned_to`, …). Scope-aware: each node / edge
//! is bound to a scope (user, channel, domain, tenant) and inherits
//! its access policy.
//!
//! Phase 2 ships the in-memory adjacency-list implementation:
//! typed nodes, typed edges, scope inheritance, supersession,
//! contradiction tracking, and typed-edge traversal. Persistence to
//! the encrypted store and CRDT delta sync land in later phases —
//! the public surface here is what the `synthesis_pipeline`,
//! `memory_manager`, and `sync_engine` crates already type
//! integrations against.
//!
//! Cross-references:
//!
//! * Module map: `ARCHITECTURE.md` §2.1.
//! * Typed relations: `docs/DESIGN.md` §3.3.
//! * Phase 2 deliverables: `docs/internal/PHASES.md` Phase 2.

#![deny(missing_docs)]

pub mod edge;
pub mod error;
pub mod graph;
pub mod incremental;
pub mod node;
pub mod persist;
pub mod visualization;

pub use edge::{ConceptEdge, EdgeId, RelationType};
pub use error::{GraphError, Result};
pub use graph::ConceptGraph;
pub use incremental::{
    AffectedSubgraph, ChangeEvent, IncrementalUpdateEngine, RecomputeScope, UpdatePropagation,
};
pub use node::{ConceptNode, NodeId, NodeState};
pub use persist::PersistentConceptGraph;
pub use visualization::{
    explore_from, neighborhood, search_nodes, subgraph_for_scope, AllowAllScopes, AllowedScopeSet,
    EdgeVisual, GraphView, NodeVisual, PositionHint, ScopeAccess, TruncationReason, ViewFilter,
    DEFAULT_MAX_NODES,
};
