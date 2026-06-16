# The Decision Playbook

> **TL;DR:** The whole rebuild on one page — the build checklist in
> dependency order, the cost model that makes ~$0/user real, and a
> head-to-head decision matrix against the products you'd otherwise buy.
> Use this post to brief an engineer and a CFO from the same sheet.

## The rebuild checklist (dependency order)

Build bottom-up; each layer depends on the one above it in this list:

1. **`crypto`** — hybrid X25519 + ML-KEM-768 KEM, ML-DSA-65 / SPHINCS+
   signatures, XChaCha20-Poly1305, BLAKE3. *Gate: the key hierarchy
   (master → DEK → CEK) before any SQL.*
2. **`evidence_store`** — SQLCipher at rest, FTS5, content-aware routing,
   per-scope key wraps for cryptographic forgetting.
3. **`observation_engine`** — lexicon-first multilingual extraction with
   a CI-gated per-type F1 floor.
4. **`memory_manager` + `concept_graph`** — decay state machine, typed
   graph with supersession.
5. **`inference_router`** — `DeviceTier` gating + Core ML/ANE → ONNX →
   MLX → llama.cpp → managed-cloud → fallback.
6. **`synthesis_pipeline`** — deterministic (fixed seed + greedy),
   verify-and-retry, deterministic eval + public leaderboard with a
   byte-for-byte CI gate.
7. **`reasoning_engine`** — contradiction / drift / explain, scope-
   isolated and bounded, wired FFI → substrate → gateway → UI.
8. **`connector_framework` + `connectors`** — secret-redacting cassette
   harness, `ConnectorMaturity` labels, 140 connectors across 10 markets.
9. **`sync_engine` + `sync_relay`** — add-wins CRDT, sealed deltas,
   untrusted ciphertext relay.
10. **`server`** — Go gateway, Zanzibar permissions, append-only audit,
    multi-tenant fair-share.
11. **`ffi` / `napi` + reference UI + installer + `device_bench`** — four
    binary shapes from one core; prove it on real devices.

The invariant that ties it together: **the editing/memory path makes no
network call.** Connectors, sync, and cloud inference are additive and
degrade gracefully.

## The cost model: why ~$0/user is real

The recurring theme across the build is that each layer removes a
*recurring cloud bill*, not just a feature:

| Layer | Cloud-product cost | On-device equivalent |
|---|---|---|
| Storage | Per-user vector + blob storage | SQLCipher file on the user's disk (~612 B/msg) |
| Retrieval | Per-query ANN compute | Local FTS5 + rerank (~$0/query) |
| Inference | Per-token API spend | On-device SLM (managed-cloud is opt-in fallback) |
| Sync | Central sync tier | Untrusted relay (ciphertext buffer) |
| Extraction | Per-call NLP API | On-device lexicon + XLM-R |

The marginal infrastructure cost per added user trends to zero because
the work happens on hardware the user already owns. The
[cost-engineering post](../10-cost-engineering-zero-marginal.md) and
[`cost-model.md`](../../docs/operator/cost-model.md) do the full math.

## The decision matrix

When to choose Knowledge — and, honestly, when not to. (Full detail in
the [comparison](../../docs/product/comparison.md).)

| If your priority is… | Choose | Why |
|---|---|---|
| Turnkey assistant over M365/Google/Notion | Copilot / Glean / Notion AI | Finished product, great UX, vendor SLA |
| A managed ANN index, BYO everything else | Pinecone / Weaviate | Simplest if you only need vector search |
| Lowest-friction cloud agent memory | Mem0 / Zep / Letta | Hosted API, central recall analytics |
| Managed cloud ETL into a warehouse | Fivetran / Airbyte / Nango | Centralized pipelines, big SaaS catalogue |
| **Private, embeddable, offline, ~$0/user memory** | **Knowledge** | On-device store, crypto-forgetting, reasoning, regional connectors |

### When *not* to choose Knowledge

- You want a hosted product users log into, not a library you embed.
- You need centralized analytics across all users' content (Knowledge
  keeps content on-device by design).
- Your team has no appetite for shipping native code into mobile/desktop
  apps.

## The honest scorecard

What this rebuild wins on, with the receipts:

- **Privacy & forgetting** — on-device store; "delete" is key
  destruction, not a soft flag (post 2).
- **Cost** — ~$0 marginal infrastructure per user (cost table above).
- **Offline** — the network-free invariant; works on a plane.
- **Post-quantum** — hybrid KEM + ML-DSA-65 today, against
  harvest-now-decrypt-later.
- **Measured multilingual quality** — a reproducible per-language
  leaderboard, including the languages where the default model is *weak*.
- **Reasoning** — contradiction / drift / explain, which similarity-only
  tools don't surface.

And where it doesn't win: raw single-language synthesis quality (a
1.7B/4B on-device model is not a frontier cloud model), turnkey UX out of
the box (it's a substrate), and mainstream US-SaaS connector depth
(traded for regional reach). Saying that plainly is the same contract the
rest of this series holds — and it's what makes the wins credible.

## Where to go next

- The *why* in narrative form: the [Building Knowledge series](../00-series-index.md).
- The running system with screenshots: the [Executives field series](../executive-personas/README.md).
- Start building: [for developers](../../docs/getting-started/for-developers.md),
  the [API cookbook](../../docs/guides/api-cookbook.md), and the
  [architecture doc](../../docs/technical/architecture.md).

---
*Part 12 of "How to Build Knowledge." [Previous: Packaging & Shipping](11-packaging-and-shipping.md) | [Series index](README.md)*
