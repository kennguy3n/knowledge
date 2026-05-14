# Knowledge — Phased delivery plan

This document captures the phased delivery plan for the Knowledge
substrate. It takes the platform from a Rust core skeleton to a
post-quantum hardened, confidential-compute-capable, multi-tenant
knowledge / context substrate that serves both B2C and B2B over
the same memory model.

Each phase has a goal, a list of deliverables, and explicit exit
criteria. The implementation priority timeline at the end of the
document maps the first 90 days of work onto Phases 0 → 2.

For the substrate design see [PROPOSAL.md](./PROPOSAL.md) and
[ARCHITECTURE.md](./ARCHITECTURE.md). Per-phase deliverable status
lives in [PROGRESS.md](./PROGRESS.md).

---

## Phase 0 — Foundation

**Goal:** Stand up the core Rust library, the local evidence
store, and a basic on-device observation engine, with platform
bindings on all four target platforms.

**Deliverables:**

- Rust shared core skeleton with the evidence store, crypto, and
  sync engine modules in place
- SQLCipher local store with post-quantum key derivation (hybrid
  X25519 + ML-KEM-768 unwrap of the per-user master key)
- Evidence plane — append-only encrypted ingestion with
  content-hash deduplication
- On-device importance classifier backed by Bonsai-1.7B via the
  shared [PrismML `llama-server`](https://github.com/kennguy3n/llama.cpp/tree/prism),
  with a lexicon-only fallback when the SLM is not available
- Platform bindings on all four targets (iOS UniFFI framework,
  Android JNI per-ABI shared libraries, macOS / Windows N-API
  addon)
- Unit test suite covering the evidence store, crypto primitives,
  and the importance classifier
- CI: lint, unit tests, multi-target build

**Exit criteria:** A user on any of the four supported platforms
can ingest messages, the substrate stores them as encrypted
evidence, and the on-device SLM produces basic importance tags
on every ingest.

---

## Phase 1 — Personal Memory (on-device)

**Goal:** A real on-device user memory object with decay,
episodic summaries, and basic retrieval.

**Deliverables:**

- Memory manager with the full decay state machine (candidate →
  reinforced → consolidated → canonical → superseded / archived /
  deleted)
- Observation engine — entity, fact, task, and decision extraction
  through a lexicon + XLM-R + SLM-assisted pipeline
- Episodic memory — session / thread summaries via on-device
  Bonsai-1.7B
- Working memory — current context window management with TTL
  eviction
- XLM-R embeddings via a shared ONNX artifact (INT8 ~107 MB / INT4
  ~55 MB), reused across `slm-guardrail` and `chat-storage-search`
- Hybrid retrieval combining FTS5 lexical, semantic-vector, and
  recency components with fan-in scoring
- Retention scoring driven by pinning, retrieval frequency, age,
  and non-use
- User Memory Object CRUD (read / pin / unpin / forget) on the FFI
  surface
- Privacy strip on every synthesis output (compute location, model,
  egress)
- Unit + integration tests covering the decay state machine,
  observation pipeline, and retrieval

**Exit criteria:** A user on any device has a personal User
Memory Object that grows from their messages, decays
appropriately, and answers hybrid lexical + semantic + recency
queries — with a visible privacy strip on every synthesis
output.

---

## Phase 2 — Channel Memory (on-device + shared)

**Goal:** Channel-scoped synthesis with shared encrypted memory
objects, multi-device sync, and provenance bundles.

**Deliverables:**

- Channel Memory Object — recaps, decisions, open questions,
  active tasks
- Synthesis pipeline — recursive per-channel-window summarization
  with grammar-constrained decoding
- MLS group keying for shared channel memory, with hybrid
  X25519 + ML-KEM-768 leaf key packages
- Encrypted synthesis object publication — one synthesizer per
  scope window, all other members consume
- Channel-scoped importance tagging that promotes only high-value
  observations into channel memory
- Elected-member-device synthesizer for small groups
- Multi-device CRDT sync of synthesis objects with add-wins,
  supersession, and contradiction semantics
- Provenance bundles signed with ML-DSA-65 on every synthesis
  output
- End-to-end tests for the elected-device synthesis path

**Exit criteria:** Channel members share a synthesized memory
object without any of them having raw-message access to the
others' devices; synthesis runs once per scope window per
channel, not once per device.

---

## Phase 3 — Domain & Tenant Memory (B2B server)

**Goal:** Cross-channel domain synthesis and tenant-level
canonical knowledge, with the server-side synthesis service
running.

**Deliverables:**

- Domain Memory Object — cross-channel workstreams, dependencies,
  risks, procedures
- Tenant Memory Object — canonical policy, product taxonomy,
  stable org knowledge
- Server-side synthesis service (Go gateway + Rust synthesis
  engine)
- Strict hierarchy: domain synthesis consumes channel outputs
  only and tenant synthesis consumes domain outputs plus approved
  official docs only — enforced cryptographically and at the type
  level
- Sparse concept graph with typed relations (is-a, part-of,
  decided-by, supersedes, contradicts, derived-from, assigned-to),
  contradictions, and drift markers
- Permission service (Zanzibar-style relation graph) with
  reachability checks
- Managed AI endpoint as the synthesizer for B2B channels /
  domains
- Tenant service — lifecycle, per-tenant encryption keys, and
  member provisioning
- End-to-end tests for the channel → domain → tenant synthesis
  chain

**Exit criteria:** B2B tenants have domain-level consolidated
knowledge that compounds decisions, concepts, and workflows
over time, and the server-side synthesizer respects the strict
hierarchy.

---

## Phase 4 — Connector Integration (on-server)

**Goal:** Ingest from external systems and build knowledge from
shared documents using the same memory hierarchy as on-device.

**Deliverables:**

- Connector framework with OAuth2 token vault, refresh flow,
  incremental delta sync, and webhook subscription
- Google Drive connector
- OneDrive connector
- Notion connector
- Jira connector
- Confluence connector
- Figma connector — design-system extraction (components, tokens,
  comments)
- HubSpot connector — CRM context (contacts, companies, deals,
  notes)
- Slack connector — channels, threads, and files via the Events
  API
- Email connector — IMAP / Gmail / Microsoft Graph
- Channel-scoped connector attachment
- ACL sync from source systems into the relation graph
- Document observation pipeline — chunking, importance tagging,
  entity / topic extraction
- Citation rendering with stable links back to source documents
- Connector-specific integration tests against vendor fixtures

**Exit criteria:** The server surface can ingest from all listed
systems, extract observations, and feed them into the knowledge
hierarchy with proper ACL enforcement and citations.

---

## Phase 5 — Portable Concept Profiles & Export

**Goal:** Controlled knowledge export for external tools and
agents, with a strict proposal-only agent write contract.

**Deliverables:**

- Portable concept profile — approved concepts, constraints,
  reasoning for a specific external tool or context
- Export plane with least-privilege views and no raw-document
  export by default
- Agent write contract — proposal-only API for observations,
  concepts, relations, and summaries
- Agent proposal schema covering scope, provenance bundle,
  evidence refs, confidence, sensitivity, TTL, supersession /
  contradiction links, agent identity, and skill / recipe id
- Per-concept / -summary / -workflow export controls with policy
  preview
- Audit trail for every export, agent proposal, and canonical
  promotion
- Policy simulator — preview an export without producing it
- End-to-end tests for the export plane and the agent proposal
  lifecycle

**Exit criteria:** External tools receive approved concept
profiles only, agents can propose but not directly write
canonical memory, and all exports are auditable with a
fresh trail entry.

---

## Phase 6 — Graph UX & Advanced Reasoning

**Goal:** Graph-powered synthesis, contradiction detection, and
workflow memory — pay for the graph where it earns its cost.

**Deliverables:**

- Concept graph visualization with scope-filtered exploration
- Contradiction and drift detection with explicit contradiction
  edges and an adjudication workflow
- Multi-hop reasoning over the concept graph with typed-edge
  traversal and explicit budgets
- Workflow memory — successful reasoning traces and tool-use
  patterns persisted to the reasoning plane
- Graph-of-Thought reasoning for complex queries
- Query planner that routes to the cheapest viable mode (summary →
  graph → raw evidence) with explicit fallbacks
- Community summaries (GraphRAG-style bottom-up across reachable
  scopes)
- Incremental graph updates that recompute only touched branches
  when an observation is promoted or superseded
- Tests covering query planning, contradiction adjudication, and
  incremental recompute

**Exit criteria:** Users can explore knowledge visually, the
substrate detects contradictions, and complex queries use graph
traversal when (and only when) the planner determines it
beneficial.

---

## Phase 7 — Post-Quantum Hardening & Confidential Compute

**Goal:** Full post-quantum cryptography across every flow, and
attested confidential compute for shared synthesis over E2EE
group data.

**Deliverables:**

- ML-KEM-768 (Kyber) for all key exchanges across the substrate
- ML-DSA-65 (Dilithium) for provenance signatures on every
  synthesis output and every export bundle
- Hybrid classical + PQ during the transition window
- Post-quantum MLS extensions (hybrid leaf KEMs, ML-DSA-65 commit
  signatures, optional SPHINCS+ co-signatures for archival group
  ops)
- Confidential compute worker on an attested TEE (Intel TDX, AMD
  SEV-SNP, Nitro Enclaves) for shared synthesis
- Attestation reports bound to synthesizer keys with audit-trail
  linkage
- Cryptographic forgetting via per-scope and per-epoch DEK destroy
  paths, with policy-driven epoch rotation
- Red-team privacy and prompt-injection test suites
- Memory quality metrics — retention precision, contradiction
  detection rate, decay-tuning experiments

**Exit criteria:** Every cryptographic operation in the
substrate uses post-quantum algorithms (in hybrid mode through
the transition); shared synthesis runs in attested confidential
compute when the deployment requires it; the privacy
red-team test suite passes.

---

## Implementation priority timeline

The first 90 days of work map onto Phases 0 → 2; later phases
follow once the substrate's foundations are in place.

### First 30 days

- Rust core skeleton (evidence store, crypto, sync engine)
- SQLCipher local store with hybrid X25519 + ML-KEM-768 key
  derivation
- Evidence plane — append-only encrypted ingestion +
  content-hash dedup
- Importance tagging via the shared
  [PrismML `llama-server`](https://github.com/kennguy3n/llama.cpp/tree/prism)
  running Bonsai-1.7B
- Platform bindings on all four targets (iOS UniFFI, Android JNI,
  macOS / Windows N-API addon)
- CI: lint + unit tests + multi-target build

### Days 31 – 60

- Memory manager — full decay state machine
- Observation engine — entities, facts, tasks, decisions
- Episodic summaries via on-device Bonsai-1.7B
- XLM-R embeddings (INT8 / INT4) wired through the shared ONNX
  artifact
- Hybrid retrieval — FTS5 + vector + recency
- User Memory Object CRUD over the FFI surface
- Privacy strip on every synthesis output

### Days 61 – 90

- Channel Memory Object — recaps, decisions, open questions, tasks
- MLS group keying for shared channel memory (hybrid leaf KEMs)
- Synthesis pipeline — channel-scope window synthesis with
  grammar-constrained decoding
- Multi-device CRDT sync for encrypted synthesis objects
- Elected-device synthesizer for small groups
- Provenance bundles signed with ML-DSA-65
- End-to-end tests across the channel synthesis flow

---

## Cross-references

- [README.md](./README.md) — overview and surfaces
- [PROPOSAL.md](./PROPOSAL.md) — product thesis and substrate
- [ARCHITECTURE.md](./ARCHITECTURE.md) — system design
- [PROGRESS.md](./PROGRESS.md) — per-phase status and changelog
- [`kennguy3n/slm-chat-demo`](https://github.com/kennguy3n/slm-chat-demo) — on-device model strategy reference
- [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism) — modified llama.cpp inference runtime
