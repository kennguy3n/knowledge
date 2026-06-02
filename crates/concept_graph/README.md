# concept_graph

Sparse typed concept graph for the Knowledge substrate.

## Purpose

Implements the semantic plane described in `ARCHITECTURE.md` §2.1
and `docs/DESIGN.md` §3.3. The concept graph stores typed nodes
(concepts) and typed edges (relations like `is_a`, `part_of`,
`supersedes`, `contradicts`) with scope awareness. Used by the
synthesis pipeline, memory manager, and reasoning engine.

## Public API summary

| Type / Function | Description |
|---|---|
| `ConceptGraph` | In-memory adjacency-list graph. |
| `PersistentConceptGraph` | SQLCipher-backed persistent graph. |
| `ConceptNode` / `NodeId` | Graph nodes with scope binding. |
| `ConceptEdge` / `EdgeId` | Typed, directed edges. |
| `RelationType` | Edge type enum (`IsA`, `PartOf`, `Supersedes`, …). |
| `IncrementalUpdateEngine` | Incremental subgraph update engine. |
| `explore_from`, `neighborhood`, `search_nodes` | Visualization / traversal helpers. |

## Usage example

```rust
use concept_graph::{ConceptGraph, ConceptNode, ConceptEdge, RelationType};

let mut graph = ConceptGraph::new();
let n1 = graph.add_node(ConceptNode::new(scope, "Rust"));
let n2 = graph.add_node(ConceptNode::new(scope, "Language"));
graph.add_edge(ConceptEdge::new(n1, n2, RelationType::IsA));
```

## Links

- [ARCHITECTURE.md](../../ARCHITECTURE.md) §2.1 — Module map.
- [docs/DESIGN.md](../../docs/DESIGN.md) §3.3 — Typed relations.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
