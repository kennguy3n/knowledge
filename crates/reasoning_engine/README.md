# reasoning_engine

Reasoning surface for the Knowledge substrate.

## Purpose

Layers four capabilities on top of the concept graph: contradiction
and drift detection, multi-hop typed-edge traversal, query planning
(Summary -> FTS -> Vector -> Graph -> Raw), and workflow memory
(recording and abstracting reasoning traces into reusable patterns).

## Public API summary

| Type / Function | Description |
|---|---|
| `ContradictionDetector` / `ContradictionEdge` | Contradiction detection. |
| `DriftDetector` / `DriftMarker` | Concept drift detection. |
| `GraphTraversal` / `TraversalBudget` | Multi-hop traversal. |
| `QueryPlanner` / `QueryPlan` / `RetrievalMode` | Cost-aware query planning. |
| `WorkflowMemory` / `TraceRecorder` / `PatternMatcher` | Workflow memory. |
| `GoTExecutor` / `ThoughtGraph` | Graph-of-Thought reasoning. |
| `CommunityDetector` / `CommunityQueryRouter` | Community detection and routing. |

## Links

- [docs/DESIGN.md](../../docs/DESIGN.md) §11.1 — Reasoning engine.
- [docs/DESIGN.md](../../docs/DESIGN.md) §3.4 — Reasoning plane.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
