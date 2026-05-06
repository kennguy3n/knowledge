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

- Rust shared core skeleton — `evidence_store`, `crypto`,
  `sync_engine` modules
- SQLCipher local store with post-quantum key derivation
  (hybrid X25519 + ML-KEM-768 unwrap of the per-user master
  key)
- Evidence plane — append-only encrypted message / file / chunk
  ingestion, content-hash deduplication
- Basic on-device importance classifier — Bonsai-1.7B
  via the shared `llama-server` from the PrismML
  [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism)
  fork, with a lexicon-only fallback when the SLM is not
  available
- Platform bindings:
  - iOS framework (UniFFI `.xcframework`)
  - Android JNI (`.so` per ABI)
  - macOS / Windows N-API addon
- Unit test suite covering `evidence_store`, `crypto`, and
  the importance classifier
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

- Memory manager with the full decay state machine
  (candidate → reinforced → consolidated → canonical →
  superseded / archived / deleted)
- Observation engine — entity extraction, fact extraction,
  task / decision detection, lexicon + XLM-R + SLM-assisted
  pipeline
- Episodic memory — session / thread summaries via on-device
  Bonsai-1.7B
- Working memory — current context window management with
  TTL eviction
- XLM-R embeddings via shared ONNX artifact (INT8 ~107 MB / INT4
  ~55 MB; same artifact as `slm-guardrail` and
  `chat-storage-search`)
- Hybrid retrieval — lexical (FTS5) + semantic (vector) +
  recency, fan-in scoring
- Retention scoring — pinning, retrieval frequency, age,
  non-use
- User Memory Object CRUD (read / pin / unpin / forget) over
  the FFI surface
- Privacy strip rendered on every synthesis output (compute
  location, model, egress)
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
- Synthesis pipeline — recursive summarization per channel
  window with grammar-constrained decoding
- MLS group keying for shared channel memory — leaf key
  packages carry hybrid X25519 + ML-KEM-768 KEM
- Encrypted synthesis object publication — synthesizer
  publishes once per scope window; other members consume
- Channel-scoped importance tagging — promote only high-value
  observations into channel memory
- Synthesizer role — elected member device path for small
  groups
- Multi-device sync via CRDT for synthesis objects, with
  add-wins + supersession + contradictions
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

- Domain Memory Object — cross-channel workstreams,
  dependencies, risks, procedures
- Tenant Memory Object — canonical policy, product taxonomy,
  stable org knowledge
- Server-side synthesis service (Go gateway + Rust synthesis
  engine)
- Domain synthesis consumes channel memory objects only (not
  raw messages) — strict hierarchy enforced cryptographically
- Tenant synthesis consumes domain objects + approved official
  docs
- Sparse concept graph — typed relations (`is_a`, `part_of`,
  `decided_by`, `supersedes`, `contradicts`,
  `derived_from`, `assigned_to`, …), contradictions, drift
  markers
- Permission service (Zanzibar-style relation graph) with
  reachability checks
- Managed AI endpoint as synthesizer for B2B channels /
  domains
- Tenant service: tenant lifecycle, per-tenant encryption
  keys, member provisioning
- End-to-end tests for the channel → domain → tenant
  synthesis chain

**Exit criteria:** B2B tenants have domain-level consolidated
knowledge that compounds decisions, concepts, and workflows
over time, and the server-side synthesizer respects the strict
hierarchy.

---

## Phase 4 — Connector Integration (on-server)

**Goal:** Ingest from external systems and build knowledge from
shared documents using the same memory hierarchy as on-device.

**Deliverables:**

- Connector framework — OAuth2 token vault, refresh flow,
  incremental delta sync, webhook subscription
- Google Drive connector
- OneDrive connector
- Notion connector
- Jira connector
- Confluence connector
- Figma connector — design system extraction (components,
  tokens, comments)
- HubSpot connector — CRM context (contacts, companies,
  deals, notes)
- Channel-scoped connector attachment (same pattern as
  slm-chat-demo Phase 5)
- ACL sync from source systems into the substrate's relation
  graph
- Observation extraction pipeline for documents — chunking,
  importance tagging, entity / topic extraction
- Citation rendering with stable links back to source
  documents
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
  reasoning for a specific external tool / context
- Export plane — least-privilege views; no raw document export
  by default
- Agent write contract — proposal-only API
  (`propose_observation`, `propose_concept`,
  `propose_relation`, `propose_summary`)
- Agent proposal schema — scope, provenance bundle,
  evidence refs, confidence, sensitivity, TTL,
  `supersedes` / `contradicts` links, agent identity + model
  version, skill / recipe id
- Export controls per concept / summary / workflow with
  policy preview
- Audit trail for all exports + agent proposals + canonical
  promotions
- Policy simulator — preview what an export would contain
  without producing the export
- End-to-end tests for the export plane and the agent
  proposal lifecycle

**Exit criteria:** External tools receive approved concept
profiles only, agents can propose but not directly write
canonical memory, and all exports are auditable with a
fresh trail entry.

---

## Phase 6 — Graph UX & Advanced Reasoning

**Goal:** Graph-powered synthesis, contradiction detection, and
workflow memory — pay for the graph where it earns its cost.

**Deliverables:**

- Concept graph visualization (Kanvas-style exploration with
  scope filters)
- Contradiction and drift detection — explicit `contradicts`
  edges with adjudication workflows
- Multi-hop reasoning over the concept graph — typed-edge
  traversal with budgets
- Workflow memory — successful reasoning traces and tool-use
  patterns saved to the reasoning plane
- Graph-of-Thought reasoning for complex queries
- Query planner — route to the cheapest mode (summary →
  graph → raw evidence) with explicit fallbacks
- Community summaries (GraphRAG-style bottom-up across
  scopes the user has reachable)
- Incremental graph updates — recompute only touched
  branches when an observation is promoted or superseded
- Tests covering query planning, contradiction adjudication,
  and incremental recompute

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

- ML-KEM-768 (Kyber) for all key exchanges (substrate-wide)
- ML-DSA-65 (Dilithium) for provenance signatures on every
  synthesis output and every export bundle
- Hybrid classical + PQ during the transition window
- Post-quantum MLS extensions (hybrid leaf KEMs, ML-DSA-65
  commit signatures, optional SPHINCS+ co-signatures for
  archival group ops)
- Confidential compute worker — attested TEE (Intel TDX / AMD
  SEV-SNP / Nitro Enclaves) for shared synthesis
- Attestation reports bound to synthesizer keys, with
  audit-trail linkage
- Cryptographic forgetting — per-scope and per-epoch DEK
  destroy paths, with policy-driven epoch rotation
- Red-team privacy and prompt-injection tests
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

- Rust core skeleton (`evidence_store`, `crypto`,
  `sync_engine` modules)
- SQLCipher local store with hybrid X25519 + ML-KEM-768 key
  derivation
- Evidence plane — append-only encrypted ingestion +
  content-hash dedup
- Basic importance tagging via the shared `llama-server`
  (PrismML `kennguy3n/llama.cpp@prism` fork) running
  Bonsai-1.7B
- Platform bindings — iOS UniFFI framework, Android JNI,
  macOS / Windows N-API addon
- CI: lint + unit tests + multi-target build

### Days 31 – 60

- Memory manager — decay state machine
  (candidate → reinforced → consolidated → canonical →
  superseded / archived / deleted)
- Observation engine — entities, facts, tasks, decisions
- Episodic summaries via on-device Bonsai-1.7B
- XLM-R embeddings (INT8 / INT4) wired through the shared
  ONNX artifact
- Hybrid retrieval — FTS5 + vector + recency
- User Memory Object CRUD over the FFI surface
- Privacy strip on every synthesis output

### Days 61 – 90

- Channel Memory Object — recaps, decisions, open questions,
  tasks
- MLS group keying for shared channel memory (hybrid leaf KEMs)
- Synthesis pipeline — channel-scope window synthesis with
  grammar-constrained decoding
- Multi-device CRDT sync for encrypted synthesis objects
- Elected-device synthesizer role for small groups
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
