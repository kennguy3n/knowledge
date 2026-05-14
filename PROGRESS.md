# Knowledge — Progress Tracker

This tracker captures per-phase deliverable status for the
Knowledge substrate. The phase scope itself lives in
[PHASES.md](./PHASES.md); see [README.md](./README.md) for an
overview of the system.

All eight phases (0 through 7) are complete and all tests are
passing on `main`. For a curated chronological view of major
milestones see [`docs/DEVELOPMENT_LOG.md`](./docs/DEVELOPMENT_LOG.md).

---

## Overall status summary

| Phase | Status | Progress |
|-------|--------|----------|
| Phase 0: Foundation | Complete | 100% |
| Phase 1: Personal Memory (on-device) | Complete | 100% |
| Phase 2: Channel Memory (on-device + shared) | Complete | 100% |
| Phase 3: Domain & Tenant Memory (B2B server) | Complete | 100% |
| Phase 4: Connector Integration (on-server) | Complete | 100% |
| Phase 5: Portable Concept Profiles & Export | Complete | 100% |
| Phase 6: Graph UX & Advanced Reasoning | Complete | 100% |
| Phase 7: Post-Quantum Hardening & Confidential Compute | Complete | 100% |

---

## Phase 0 — Foundation

- [x] Rust shared core skeleton (evidence store, crypto, sync engine)
- [x] SQLCipher local store with hybrid X25519 + ML-KEM-768 key derivation
- [x] Encrypted append-only evidence ingestion (messages, files, chunks)
- [x] BLAKE3 content-hash deduplication
- [x] On-device importance classifier (Bonsai-1.7B via shared `llama-server`, with lexicon fallback)
- [x] iOS framework binding (UniFFI `.xcframework`)
- [x] Android JNI binding (per-ABI shared libraries)
- [x] macOS / Windows N-API addon binding
- [x] Unit test suite for the evidence store, crypto, and importance classifier
- [x] CI pipeline (lint, unit tests, multi-target build)

---

## Phase 1 — Personal Memory (on-device)

- [x] Memory manager with the full decay state machine
- [x] Observation engine (entity, fact, task, and decision extraction)
- [x] Lexicon → XLM-R → SLM-assisted observation pipeline
- [x] Episodic memory (session / thread summaries via Bonsai-1.7B)
- [x] Working memory (TTL-evicting context window)
- [x] XLM-R embeddings via a shared ONNX artifact (INT8 / INT4)
- [x] Hybrid retrieval (FTS5 lexical + semantic vector + recency)
- [x] Retention scoring (pinning, retrieval frequency, age, non-use)
- [x] User Memory Object CRUD (read / pin / unpin / forget)
- [x] Privacy strip on every synthesis output
- [x] Decay state machine, observation pipeline, and retrieval tests

---

## Phase 2 — Channel Memory (on-device + shared)

- [x] Channel Memory Object (recaps, decisions, open questions, active tasks)
- [x] Synthesis pipeline (window manager, typed synthesis objects, GBNF schema)
- [x] MLS group keying with hybrid X25519 + ML-KEM-768 leaf KEMs
- [x] Encrypted synthesis-object publication with scope / window / object AAD binding
- [x] Channel-scoped importance tagging
- [x] Elected-member-device synthesizer (tier / battery / heartbeat eligibility)
- [x] Multi-device CRDT sync of synthesis objects (add-wins, supersession, contradictions)
- [x] Provenance bundles signed with ML-DSA-65
- [x] End-to-end tests for the elected-device synthesis path

---

## Phase 3 — Domain & Tenant Memory (B2B server)

- [x] Domain Memory Object (cross-channel workstreams, dependencies, risks, procedures)
- [x] Tenant Memory Object (canonical policy, product taxonomy, stable org knowledge)
- [x] Server-side synthesis service (Rust synthesis engine; Go gateway pending)
- [x] Type-enforced hierarchy: domain consumes channel only, tenant consumes domain + approved docs only
- [x] Sparse concept graph with SQLCipher persistence
- [x] Zanzibar-style permission service with reachability checks
- [x] Managed AI endpoint synthesizer for B2B channels and domains
- [x] Tenant service (lifecycle, per-tenant keys, member provisioning)
- [x] End-to-end tests for the channel → domain → tenant synthesis chain
- [x] Append-only audit log

---

## Phase 4 — Connector Integration (on-server)

- [x] Connector framework (OAuth2 token vault, refresh, incremental delta sync, webhooks)
- [x] Google Drive connector
- [x] OneDrive connector
- [x] Notion connector
- [x] Jira connector
- [x] Confluence connector
- [x] Figma connector (design-system extraction)
- [x] HubSpot connector (CRM context)
- [x] Slack connector (Events API)
- [x] Email connector (Gmail + Microsoft Graph)
- [x] Channel-scoped connector attachment
- [x] ACL sync from source systems into the relation graph
- [x] Document observation pipeline (chunking, importance tagging, entity / topic extraction)
- [x] Citation rendering with stable links back to source documents
- [x] Connector-specific integration tests against vendor fixtures

---

## Phase 5 — Portable Concept Profiles & Export

- [x] Portable concept profile (approved concepts, constraints, reasoning for an external context)
- [x] Export plane with least-privilege views and no raw-document export by default
- [x] Agent write contract (proposal-only API for observations, concepts, relations, summaries)
- [x] Agent proposal schema with scope, provenance, evidence refs, confidence, sensitivity, TTL, and identity
- [x] Per-concept / -summary / -workflow export controls with policy preview
- [x] Audit trail for every export, agent proposal, and canonical promotion
- [x] Policy simulator (read-only export preview)
- [x] End-to-end tests for the export plane and the agent proposal lifecycle

---

## Phase 6 — Graph UX & Advanced Reasoning

- [x] Concept graph visualization with scope-filtered exploration
- [x] Contradiction and drift detection with explicit edges and an adjudication workflow
- [x] Multi-hop reasoning over the concept graph with typed-edge traversal and budgets
- [x] Workflow memory for successful reasoning traces and tool-use patterns
- [x] Graph-of-Thought reasoning for complex queries
- [x] Query planner (cheapest-first routing with explicit fallbacks)
- [x] Community summaries (GraphRAG-style, bottom-up over reachable scopes)
- [x] Incremental graph updates (recompute only touched branches)
- [x] Tests covering query planning, contradiction adjudication, and incremental recompute

---

## Phase 7 — Post-Quantum Hardening & Confidential Compute

- [x] ML-KEM-768 (Kyber) for all key exchanges
- [x] ML-DSA-65 (Dilithium) for provenance signatures
- [x] Hybrid classical + PQ enforcement with key-exchange audit trail
- [x] Post-quantum MLS extensions (hybrid leaf KEMs, ML-DSA-65 commits, optional SPHINCS+ co-signatures)
- [x] Confidential compute worker (attested TEE) for shared synthesis
- [x] Attestation reports bound to synthesizer keys with audit-trail linkage
- [x] Cryptographic forgetting (per-scope and per-epoch DEK destroy paths)
- [x] Red-team privacy and prompt-injection test suites
- [x] Memory quality metrics (retention precision, contradiction detection rate, decay tuning)
