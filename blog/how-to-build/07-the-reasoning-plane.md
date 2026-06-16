# The Reasoning Plane

> **TL;DR:** Most retrieval and memory tools answer one question: *what
> is similar to X?* The `reasoning_engine` answers three the market
> mostly doesn't: *what contradicts X*, *how has belief about X drifted
> as evidence changed*, and *why was this answer retrieved* — privately,
> on-device, scope-isolated and bounded. This post builds it and wires it
> end-to-end through FFI → substrate → gateway → UI, and argues it's the
> sharpest differentiator in the stack.

## What you are building

`reasoning_engine`: contradiction detection, drift detection, multi-hop
explain (plus query planning, workflow memory, Graph-of-Thought, and
community summaries). It reads the `concept_graph` and `evidence_store`
built earlier and is surfaced to the product through three endpoints.

## Build it: three questions over the graph

The three headline capabilities all walk the typed concept graph from
post 4:

1. **Contradiction** — find observations whose claims conflict
   (supersession and contradiction edges make this a graph walk, not an
   LLM guess). "We decided to use Postgres" vs. "We're going with
   DynamoDB" is a flagged contradiction with both evidence refs.
2. **Drift** — track how belief about a concept changed over time as new
   evidence arrived. Useful for "when did we change our mind about the
   vendor, and why?"
3. **Explain** — multi-hop traversal that answers *why* a given answer
   was retrieved, returning the evidence path. This is the
   explainability the `HybridRetriever`'s component scores (post 4) feed
   into.

Two safety properties you build in from the start: scans are
**scope-isolated** (a reasoning query never crosses the scope boundary)
and **bounded** (a 256-node cap), so a pathological graph can't turn a
reasoning call into an unbounded traversal on a phone.

## Build it: wire it end-to-end

A capability that only exists in a Rust crate isn't a product feature.
The reasoning plane is reachable the whole way out:

```text
FFI  reasoning_contradictions / reasoning_drift / reasoning_explain_query
  └─► substrate  /reasoning/*
        └─► gateway  POST /api/v1/reasoning/{contradictions,drift,explain}
              └─► reference UI panel (apps/knowledge-ui/)
```

That full path — `crates/reasoning_engine/`,
`crates/ffi/src/reasoning.rs`, `server/`, and the UI panel — is the
difference between "we have a reasoning crate" and "a user can click
*explain this* and see the evidence chain." The same concept graph the
reasoning plane walks is the one projected on the Memory page:

![The Memory page with a live concept graph projected from real user-memory observations — the typed nodes the reasoning plane traverses for contradiction, drift, and explain.](../executive-personas/assets/06-concept-graph-populated.png)

## The business decision: similarity vs. understanding

**Scenario.** An exec asks the assistant, "Are we still planning to
launch in Q3?" A similarity-only system returns the most *similar*
messages — which might be the optimistic ones from two months ago. A
reasoning system surfaces the *contradiction*: a later message that
slipped the date, with both sources.

- **Similarity-only tools (vector DBs; most memory layers).** Answer
  "what's like this." Fast, simple, and often enough. They don't tell you
  when your memory disagrees with itself.
- **Knowledge's reasoning plane.** Answers "what *conflicts* with this,"
  "how did it *change*," and "*why* this answer." For decisions —
  finance, legal, ops — surfacing contradiction and drift is the feature
  that prevents acting on stale or conflicting memory.

In the [comparison](../../docs/product/comparison.md), the
"reasoning plane (contradiction / drift / explain)" row is **No** for
every cloud product and every memory layer except Knowledge (Zep has a
temporal knowledge graph but no contradiction/drift/explain surface).
That's not an accident of features — it's a consequence of having built a
*memory graph with typed edges* (post 4) instead of a flat vector index.

## How a competitor would build this

To add contradiction/drift, a similarity-first product would have to
build a typed temporal graph on top of its vector store and a traversal
engine over it — essentially retrofitting posts 3–4 of this guide. It's
doable, but it's why the capability is rare: it falls out naturally only
if you designed the memory layer as a graph from the start. The lesson
for a rebuild: **decide early whether you're building search or memory**;
reasoning is cheap if you chose memory and expensive if you chose search.

## What's next

The substrate now ingests, extracts, retrieves, synthesizes, and
reasons — all on-device. To be useful it has to pull in the knowledge
that already lives in the user's other tools. Next: the connector
framework and 140 connectors, with an honest liveness story.

---
*Part 7 of "How to Build Knowledge." [Previous: Synthesis & Honest Eval](06-synthesis-and-eval.md) | [Next: 140 Connectors, Honestly](08-connectors.md) | [Series index](README.md)*
