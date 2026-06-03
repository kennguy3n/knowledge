//! `concept_graph` — sparse typed concept graph for the Knowledge
//! substrate.
//!
//! Per `docs/technical/architecture.md` §2.1 and `docs/technical/design.md` §3.3, the semantic
//! plane is a *sparse* concept graph with typed relations
//! (`is_a`, `part_of`, `decided_by`, `supersedes`, `contradicts`,
//! `derived_from`, `assigned_to`, …). Scope-aware: each node / edge
//! is bound to a scope (user, channel, domain, tenant) and inherits
//! its access policy.
//!
//! Ships the in-memory adjacency-list implementation: typed nodes,
//! typed edges, scope inheritance, supersession, contradiction
//! tracking, and typed-edge traversal. Persistence to the encrypted
//! store and CRDT delta sync are not yet wired — the public surface
//! here is what the `synthesis_pipeline`, `memory_manager`, and
//! `sync_engine` crates already type integrations against.
//!
//! Cross-references:
//!
//! * Module map: `docs/technical/architecture.md` §2.1.
//! * Typed relations: `docs/technical/design.md` §3.3.

#![deny(missing_docs)]

// STABLE
pub mod edge;
// STABLE
pub mod error;
// STABLE
pub mod graph;
// UNSTABLE — incremental update engine; API may change.
pub mod incremental;
// STABLE
pub mod node;
// STABLE
pub mod persist;
// UNSTABLE — visualization helpers; API may change.
pub mod visualization;

// STABLE
pub use edge::{ConceptEdge, EdgeId, RelationType};
// STABLE
pub use error::{GraphError, Result};
// STABLE
pub use graph::ConceptGraph;
// UNSTABLE — incremental update engine; API may change.
pub use incremental::{
    AffectedSubgraph, ChangeEvent, IncrementalUpdateEngine, RecomputeScope, UpdatePropagation,
};
// STABLE
pub use node::{ConceptNode, NodeId, NodeState};
// STABLE
pub use persist::PersistentConceptGraph;
// UNSTABLE — visualization helpers; API may change.
pub use visualization::{
    explore_from, neighborhood, search_nodes, subgraph_for_scope, AllowAllScopes, AllowedScopeSet,
    EdgeVisual, GraphView, NodeVisual, PositionHint, ScopeAccess, TruncationReason, ViewFilter,
    DEFAULT_MAX_NODES,
};
