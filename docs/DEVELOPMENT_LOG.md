# Development Log

A curated, reverse-chronological view of major milestones in the
Knowledge substrate. For the live phase-status summary and the
per-phase deliverable checklist see [`PROGRESS.md`](../PROGRESS.md).

## 2026-05-08 — End-to-end demo and substrate hardening

- Shipped a single end-to-end demo crate that drives the substrate
  from raw ingest through user, channel, domain, and tenant
  synthesis against a synthetic multi-channel dataset, emitting a
  per-phase report and per-operation benchmarks to
  `results/demo_results.md`.
- Hardened the substrate with six production bug fixes (retrieval
  embedding propagation, episodic session-boundary semantics,
  TEE-worker lifecycle, hybrid-retrieval semantic component,
  decay-sweep correctness, and concept-graph supersession) and a
  matching set of targeted regression tests.
- Added FFI and inference-router integration test suites covering
  the externally observable surface that platform shells consume.

## 2026-05 — Phase 7 completion

- Wrapped the post-quantum hardening track: ML-KEM-768 for all key
  exchanges, ML-DSA-65 provenance signatures on every synthesis
  output, hybrid leaf KEMs in MLS, and optional SPHINCS+ co-signing
  for archival group operations.
- Brought the confidential-compute worker online with attested
  TEE-bound synthesizer keys and audit-trail linkage.
- Wired up cryptographic forgetting via per-scope and per-epoch
  DEK destroy paths.

## 2026-04 — Phase 6 graph UX and reasoning

- Concept-graph visualization with scope-filtered exploration.
- Contradiction and drift detection with explicit edges and an
  adjudication workflow.
- Multi-hop reasoning, Graph-of-Thought traces, and a query
  planner that routes to the cheapest viable mode (summary →
  graph → raw evidence).
- Incremental graph updates that recompute only touched branches
  on promotion or supersession.

## 2026-03 — Phase 5 export plane

- Portable concept profiles with least-privilege views and no
  raw-document export by default.
- Agent write contract: proposal-only API for observations,
  concepts, relations, and summaries with full provenance and
  identity metadata.
- Audit trail covering every export, agent proposal, and
  canonical promotion, plus a read-only policy simulator that
  previews exports without producing them.

## 2026-02 — Phase 4 connector integration

- Connector framework with OAuth2 token vault, refresh flow,
  incremental delta sync, and webhook subscription.
- Vendor connectors for Google Drive, OneDrive, Notion, Jira,
  Confluence, Figma, HubSpot, Slack, and Email.
- Channel-scoped attachment and ACL sync from source systems into
  the relation graph.
- Document observation pipeline (chunking, importance tagging,
  entity / topic extraction) and citation rendering with stable
  links back to source documents.

## 2026-01 — Phase 3 server-side synthesis

- Domain and Tenant Memory Objects with a type-enforced hierarchy
  (domain consumes channel outputs only; tenant consumes domain
  outputs plus approved official docs).
- Server-side synthesis service and managed AI endpoint for B2B
  channels and domains.
- Sparse concept graph with SQLCipher persistence and a
  Zanzibar-style permission service with reachability checks.

## 2025-12 — Phase 2 channel memory

- Channel Memory Objects with recursive per-channel-window
  summarization and grammar-constrained decoding.
- MLS group keying with hybrid X25519 + ML-KEM-768 leaf packages.
- Multi-device CRDT sync of synthesis objects with add-wins,
  supersession, and contradiction semantics.
- ML-DSA-65 provenance signatures on every synthesis output.

## 2025-11 — Phase 1 personal memory

- Full decay state machine and observation engine (entities,
  facts, tasks, decisions) through a lexicon → XLM-R →
  SLM-assisted pipeline.
- Episodic and working memory, with hybrid lexical / semantic /
  recency retrieval.
- User Memory Object CRUD on the FFI surface, including the
  forget path.
- Privacy strip rendered on every synthesis output.

## 2025-10 — Phase 0 foundation

- Rust shared-core workspace with the evidence store, crypto, and
  sync engine modules in place.
- SQLCipher-backed local store with hybrid X25519 + ML-KEM-768 key
  derivation, append-only encrypted ingestion, and BLAKE3
  content-hash deduplication.
- Platform bindings on all four targets (iOS UniFFI framework,
  Android JNI per-ABI shared libraries, macOS / Windows N-API
  addon).
- Importance classifier backed by an on-device SLM with a
  lexicon-only fallback.
- CI covering lint, unit tests, and multi-target build.
