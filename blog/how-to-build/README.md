# How to Build Knowledge — A Rebuild Guide

> **TL;DR:** A build-order companion to the [Building Knowledge
> series](../00-series-index.md). Where that series explains *why* the
> substrate is designed the way it is, this one is a **rebuild guide**:
> if you wanted to build a privacy-first, on-device knowledge substrate
> like this one from an empty repo, what would you build, in what order,
> and *why would you choose each design over what the market leaders
> ship?* Every post pairs the **engineering** ("what to build and how")
> with the **business decision** ("the scenario, the trade-off, and the
> competitor we are choosing differently from").

This series is written for two readers sitting at the same table: the
**engineer** who has to assemble the system, and the **product/business**
owner who has to defend why it was built this way against an off-the-
shelf alternative. Each post stands alone but follows the dependency
order you would actually build in — lower layers first, product surface
last.

It is grounded in the real code: the crate names, public APIs,
benchmark numbers, and eval tables here all come from this repository
(`crates/`, `server/`, `docs/technical/`). Where a capability has an
honest limitation, this guide names it — the same contract the rest of
the [documentation](../../docs/) holds itself to.

## The build order

The substrate is a Rust workspace of focused crates compiled into mobile
(`ffi`/UniFFI), desktop (`napi`), and server (Go gateway + Rust workers)
shapes. You build it bottom-up, because each layer's correctness depends
on the one below it:

```text
crypto ─► evidence_store ─► observation_engine ─► memory_manager ─┐
                                                  concept_graph ──┤
                                                                  ▼
                          inference_router ─► synthesis_pipeline ─► reasoning_engine
                                                                  │
        connector_framework ─► connectors                        │
                          sync_engine ─► sync_relay              │
                                                                  ▼
        server (gateway, tenant/permission/audit) ─► ffi / napi ─► reference UI / installer
```

## The posts

1. **[Architecture & Build Order](01-architecture-and-build-order.md)** —
   the product thesis, the on-device-vs-cloud fork in the road, the
   crate dependency graph, and the one invariant that shapes everything:
   the editing path never touches the network. *Business: build-vs-buy,
   and why ship a substrate instead of a product.*
2. **[The Encrypted Store](02-the-encrypted-store.md)** — build `crypto`
   and `evidence_store`: SQLCipher at rest, the DEK/CEK key hierarchy,
   content-aware storage routing, and **cryptographic forgetting**.
   *Business: the regulated-erasure scenario, vs. soft-delete vendors.*
3. **[Observation & Extraction](03-observation-and-extraction.md)** —
   build `observation_engine`: lexicon-first multilingual extraction
   with a published per-type F1 floor across 22 languages. *Business:
   on-device NLP vs. cloud extraction APIs.*
4. **[Retrieval & the Memory Graph](04-retrieval-and-memory.md)** — build
   the `HybridRetriever`, `memory_manager` decay, and `concept_graph`.
   *Business: the power-user latency budget, vs. server-side RAG.*
5. **[Inference Routing on Device](05-inference-routing.md)** — build
   `inference_router`: device-tier gating and the Core ML/ANE → ONNX →
   MLX → llama.cpp → managed-cloud → fallback chain. *Business: the
   1.7B-vs-4B model decision, measured not asserted.*
6. **[Synthesis & Honest Eval](06-synthesis-and-eval.md)** — build
   `synthesis_pipeline` plus the deterministic eval harness and the
   public multilingual leaderboard. *Business: shipping a quality bar
   you publish, vs. vendors who don't.*
7. **[The Reasoning Plane](07-the-reasoning-plane.md)** — build
   `reasoning_engine` (contradiction / drift / explain) end-to-end
   through FFI → substrate → gateway → UI. *Business: the differentiator
   vs. similarity-only retrieval and memory layers.*
8. **[140 Connectors, Honestly](08-connectors.md)** — build
   `connector_framework` + `connectors`, the cassette liveness harness,
   and the maturity-label contract. *Business: contract-stable vs.
   live-verified, and regional coverage vs. US-centric ETL.*
9. **[Sync & Multi-Device](09-sync-and-multi-device.md)** — build
   `sync_engine` + `sync_relay`: an add-wins CRDT over an untrusted
   relay that only ever holds ciphertext. *Business: no-trusted-server
   sync, vs. cloud-native sync.*
10. **[The Server & Multi-Tenancy](10-server-and-multitenancy.md)** —
    build the Go gateway, `tenant_service`, `permission_service`, and
    `audit_service`. *Business: self-host/in-region vs. SaaS, at 5,000
    tenants.*
11. **[Packaging & Shipping](11-packaging-and-shipping.md)** — wrap the
    core in `ffi`/UniFFI and `napi`, ship the reference UI, the
    one-command installer, and the device benchmark matrix. *Business:
    zero-to-running, and the no-ops SMB deployment.*
12. **[The Decision Playbook](12-decision-playbook.md)** — the whole
    rebuild on one page: the build checklist, the cost model, and a
    head-to-head decision matrix against the products you'd otherwise
    buy.

## Where to go next

- Want the *why* behind each subsystem in narrative form? Read the
  [Building Knowledge series](../00-series-index.md).
- Want to see the running system, screenshots and all? Read the
  [Executives on the Substrate field series](../executive-personas/README.md).
- Ready to actually start? The [developer getting-started
  guide](../../docs/getting-started/for-developers.md) and the
  [architecture doc](../../docs/technical/architecture.md) are the entry
  points this series builds on.
