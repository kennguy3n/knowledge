# Architecture & Build Order

> **TL;DR:** Before you write a line of code, you make one decision that
> determines everything downstream: does the knowledge live on a server
> or on the device? Knowledge picks the device, and that single choice
> dictates the crate graph, the cost model, the privacy posture, and the
> competitor set. This post lays out the build order — bottom-up, crypto
> first — and the one invariant that keeps the design honest: **the
> editing/memory path never makes a network call.**

## What you are building

A *substrate*, not a product. Knowledge is a Rust workspace that an app
embeds to get private, on-device memory: ingest evidence, extract
observations, retrieve and synthesize, reason over it, sync it across a
user's devices, and connect it to the tools they already use — all with
the encrypted store living on the user's hardware (or, for B2B, your
in-region infrastructure).

The system is three cooperating surfaces over **one shared Rust core**
(see [`architecture.md`](../../docs/technical/architecture.md) §1):

- **On-device surface** — iOS (Swift), Android (Kotlin), macOS/Windows
  (Electron + React), each consuming the core via FFI.
- **Server surface** — a Go API gateway plus Rust workers running the
  connector pipeline and cross-tenant synthesis.
- **Inference layer** — on-device SLMs (Core ML/ANE, ONNX, MLX,
  llama.cpp) with a managed-cloud fallback, serving both surfaces.

## The fork in the road: device vs. cloud

Every adjacent product — Microsoft 365 Copilot, Glean, Notion AI,
Pinecone-backed RAG, the agent memory layers (Mem0, Zep, Letta) — keeps
the knowledge in a vendor cloud. That is a reasonable choice; it makes
cross-user analytics, central indexing, and turnkey UX easy. It also
puts the most sensitive thing a user owns — the full text of their
private content — on someone else's computer, bills you per user per
month, and breaks the moment the device is offline.

Knowledge takes the other fork. Once you decide the store lives on the
device, three things follow automatically and shape the entire build:

| Consequence | What it forces | Competitor contrast |
|---|---|---|
| No central corpus | Crypto and isolation must be airtight per-scope | Cloud products centralize; one breach is total |
| Compute is bounded by the device | Retrieval/synthesis must run on a phone CPU | Cloud products throw a datacenter at it |
| No global view | Designs can't depend on cross-user signals | Cloud products mine the aggregate |

The payoff is the trade you *want* for a privacy product: **~$0 marginal
infrastructure cost per user**, works offline, and "delete" can mean
cryptographic erasure rather than a soft-delete flag.

## The one invariant: a network-free editing path

The discipline that keeps "on-device" from quietly eroding is a hard
architectural rule: **the core memory path — ingest, store, retrieve,
decay, synthesize on-device — must not depend on any network call.**
Connectors, sync, and managed-cloud inference are *additive* layers that
sit outside that path and degrade gracefully when absent. If you can
unplug the network and the assistant still remembers and answers, the
invariant holds. (Sibling products in this codebase enforce the same
idea with a CI "local-first sentinel" that fails the build if a core
crate gains a network dependency — a pattern worth copying.)

## The build order

You build bottom-up because each layer's correctness rests on the one
below it. Crypto is first because everything is encrypted; the store is
next because everything is persisted; the product surface is last
because it composes the rest.

```text
1. crypto                 — primitives: hybrid KEM, signatures, AEAD, hashing
2. evidence_store         — encrypted SQLCipher store + hybrid retriever
3. observation_engine     — multilingual fact/entity/task/decision extraction
4. memory_manager         — decay state machine, retention, user memory
   concept_graph          — typed concept graph with supersession
5. inference_router       — device-tier gating + accelerator chain
6. synthesis_pipeline     — windowed recaps + deterministic eval
7. reasoning_engine       — contradiction / drift / explain
8. connector_framework    — OAuth vault, sync state, ACLs
   connectors             — 140 vendor implementations across 10 markets
9. sync_engine            — add-wins CRDT + delta transport
   sync_relay             — untrusted ciphertext relay
10. server                — Go gateway, tenant/permission/audit services
11. ffi / napi            — UniFFI (mobile) + N-API (desktop) bindings
    reference UI + installer
```

Each numbered group is a post in this series.

## The business decision: substrate vs. product

The first business choice is not technical at all: **do you build a
product users log into, or a substrate you embed into your product?**

- **Buy a product (Copilot/Glean/Notion AI).** Fastest to value if your
  problem is "answer questions over our Microsoft/Google/Notion data."
  You get great UX and a vendor SLA. You give up data residency, offline
  use, per-user economics, and the ability to ship it inside *your* app.
- **Buy a vector DB (Pinecone/Weaviate).** Right if all you need is a
  managed ANN index and you'll build the rest. You still run retrieval
  in the cloud and bring your own crypto, connectors, and reasoning.
- **Build on a substrate (this).** Right when memory must be private,
  embeddable, work offline, and cost ~$0/user — and when "we keep your
  data on your device" has to be literally true, not a marketing line.

The honest version of the pitch: if you want a turnkey assistant over a
vendor cloud, buy one of the products — they are good. Build on a
substrate when the privacy, cost, and embeddability constraints are the
*point* of your product, not a nice-to-have.

## What's next

With the thesis and build order fixed, the next post starts at the
bottom of the stack: the `crypto` primitives and the `evidence_store`
that turns "on-device" from a slogan into an encrypted file on disk you
can cryptographically erase.

---
*Part 1 of "How to Build Knowledge." [Next: The Encrypted Store](02-the-encrypted-store.md) | [Series index](README.md)*
