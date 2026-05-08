# Knowledge — Privacy-First Continual Knowledge System

Knowledge is a continual updating, always-fresh knowledge / context
system for AI that puts privacy at the center and incorporates
post-quantum cryptographic thinking from day one. It is the shared
cognitive substrate behind the KChat platform — a single layered
memory system that serves both the consumer surface (B2C) and the
enterprise surface (B2B) without forking the substrate.

The system observes the streams a user (or a tenant) already
generates — chat messages, files, media, document-management
content, design files, tickets, CRM records — and continuously
synthesizes them into structured, decaying, scoped memory:
observations, concepts, summaries, decisions, workflows. Raw
evidence stays where it should (on the device or inside the
tenant's perimeter); only consented, derived knowledge flows
outward, and it flows out as portable concept profiles, not as
copies of the underlying documents.

For the deeper product thesis, system design, phasing, and progress
see [PROPOSAL.md](./PROPOSAL.md), [ARCHITECTURE.md](./ARCHITECTURE.md),
[PHASES.md](./PHASES.md), and [PROGRESS.md](./PROGRESS.md).

---

## Quick start

The shared core is a Cargo workspace targeting **Rust 1.75+ (stable)**.
Phase 0 ships the encrypted evidence plane and the post-quantum
crypto primitives; Phase 1 adds the on-device personal-memory plane
(decay state machine, retention scoring, working memory, lexicon
observation extraction, and FTS5 + recency hybrid retrieval). Phase
2 adds the channel-memory plane: a `ChannelMemoryObject` with
recap / decisions / open-questions / active-tasks, the
`synthesis_pipeline` window manager + typed synthesis objects +
GBNF schema types, encrypted publish / consume with `(scope_id,
window_id, object_id)` AAD binding, channel-scoped promotion
policy, a synthesizer-election skeleton (tier / battery /
heartbeat eligibility), the typed `concept_graph`, an add-wins
observed-remove CRDT in `sync_engine`, and the `ProvenanceBundle`
PROV data model. Phase 3 adds the domain- and tenant-tier memory
planes plus the server-side surface around them: a
`DomainMemoryObject` and a `TenantMemoryObject` (the latter with
*no* passive decay — only explicit deprecation), a Zanzibar-style
`permission_service` with reachability over inheritance chains, a
`tenant_service` with the tenant lifecycle / member provisioning /
per-tenant key references, a `synthesis_engine` skeleton with the
`SynthesisEngine` trait + `ManagedEndpointSynthesizer` stub, an
append-only `audit_service`, type-system-enforced hierarchy
(`DomainSynthesisInput` / `TenantSynthesisInput` / `ApprovedDocument`
in `synthesis_pipeline::hierarchy`), and SQLCipher persistence for
the `concept_graph`. Phase 5 adds the agent write contract and the
export plane: the `agent_contract` crate (proposal-only API—
`AgentProposal<T>` + four typed payloads + `AgentIdentity` + the
`Proposed → UnderReview → Promoted/Rejected` lifecycle with an
`AutoPromotionPolicy`), and the `export_plane` crate (portable
concept profiles, `ExportPolicy` + `PolicyEngine`, deny-by-default
`ExportControlRegistry`, a read-only `PolicySimulator`, and a
`ConceptApprovalWorkflow` that bridges canonical `concept_graph`
nodes into approved exports). `audit_service` is extended with the
five Phase-5 action types (`ExportRendered`, `ExportSimulated`,
`AgentProposalSubmitted`, `AgentProposalPromoted`,
`AgentProposalRejected`) and helper logging functions. Phase 4
adds the on-server connector substrate: a new
`connector_framework` crate (the `Connector` trait,
`OAuth2TokenVault` + `TokenRefresher`, `SyncState` /
`WebhookSubscription` / `ConnectorEvent`, channel-scoped
`AttachmentRegistry` integrated with `permission_service`, and an
`AclSyncEngine` that maps source-system permissions onto
substrate relation tuples), plus document-pipeline + citation
support in `observation_engine` (`DocumentChunker` /
`SlidingWindowChunker`, `DocumentObservationPipeline`, and a
`Citation` / `CitationRegistry` / `CitationRenderer` surface).
Phase 6 adds the reasoning plane: a new `reasoning_engine` crate
with `ContradictionDetector` + `DriftDetector` +
`AdjudicationWorkflow`, typed-edge `GraphTraversal` over the
concept graph with budgets and path scoring, a `QueryPlanner`
that routes between `Summary / Fts / SemanticVector /
GraphTraversal / RawEvidence` retrieval modes, and a
`WorkflowMemory` that stores reasoning traces and matches
patterns; `concept_graph` is extended with an
`IncrementalUpdateEngine` that recomputes only touched branches
on promotion / supersession / contradiction. The platform
bindings, the SLM-backed importance classifier, MLS group keying,
the ML-DSA-65 signer, the seven Phase-4 vendor connectors
(Google Drive / OneDrive / Notion / Jira / Confluence / Figma /
HubSpot), the concept-graph visualization, and Graph-of-Thought
reasoning are tracked in [PROGRESS.md](./PROGRESS.md) but not yet
shipped.

### Prerequisites

- A stable Rust toolchain (`rustup install stable` and
  `rustup component add clippy rustfmt`).
- A C toolchain that can build the bundled SQLCipher + OpenSSL
  sources used by `rusqlite`'s `bundled-sqlcipher-vendored-openssl`
  feature (on Debian / Ubuntu: `sudo apt install build-essential`).

### Build

```bash
cargo build --all-targets
```

The first build compiles `openssl-src` and SQLCipher and is therefore
slow (a few minutes); incremental rebuilds are fast.

### Test

```bash
cargo test --all
```

This runs the inline unit tests inside each crate and the integration
test files under `crates/*/tests/`. The Phase-4 + Phase-6 crates
(`connector_framework`, `reasoning_engine`) and extensions
(`observation_engine` document + citation modules,
`concept_graph` incremental updates), the Phase-5 crates
(`agent_contract`, `export_plane`), the Phase-3 crates
(`permission_service`, `tenant_service`, `synthesis_engine`,
`audit_service`), the Phase-2 crates (`concept_graph`,
`synthesis_pipeline`, `sync_engine`), and the extended Phase-1
crates (`memory_manager`, `observation_engine`, `crypto`) are all
covered by `cargo test --all` (609 tests passing as of the
Phase-4 + Phase-6 first delivery). The end-to-end channel →
domain → tenant synthesis chain is exercised by
`crates/synthesis_engine/tests/hierarchy_e2e.rs`; the agent
proposal lifecycle and the export plane pipeline are exercised by
`crates/agent_contract/tests/e2e_proposal_tests.rs` and
`crates/export_plane/tests/e2e_export_tests.rs`.

### Lint

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

The CI pipeline (see `.github/workflows/ci.yml`) runs the same four
commands on every push and pull request.

## Project structure

```
knowledge/
├── Cargo.toml                 # workspace manifest (Rust 1.75+, edition 2021)
├── rustfmt.toml               # repo-wide formatting config
├── crates/
│   ├── crypto/                # post-quantum primitives (BLAKE3, XChaCha20-Poly1305,
│   │                          #   HKDF-SHA256, hybrid X25519 + ML-KEM-768 KEM,
│   │                          #   Phase 2 ProvenanceBundle + HMAC TestSigner)
│   ├── evidence_store/        # SQLCipher-backed encrypted evidence plane
│   │                          #   (ingest, dedup, ring buffer, FTS5, classifier,
│   │                          #   hybrid FTS + recency retrieval)
│   ├── memory_manager/        # personal-memory plane: decay state machine,
│   │                          #   retention scoring, working memory, user-memory
│   │                          #   CRUD, privacy-strip invariant + Phase 2
│   │                          #   ChannelMemoryObject (recap / decisions /
│   │                          #   open questions / active tasks) + Phase 3
│   │                          #   DomainMemoryObject (workstreams /
│   │                          #   dependencies / risks / procedures) and
│   │                          #   TenantMemoryObject (canonical policies /
│   │                          #   product taxonomy / no-passive-decay)
│   ├── observation_engine/    # observation extractor + lexicon-first pipeline
│   │                          #   (entities / tasks / decisions / facts /
│   │                          #   questions; Phase 2 URL / email / date /
│   │                          #   numeric extraction; ChannelPromotionPolicy)
│   ├── concept_graph/         # Phase 2 sparse typed concept graph (nodes,
│   │                          #   7 typed edges, scope-binding, supersession,
│   │                          #   contradiction tracking, typed traversal)
│   │                          #   + Phase 3 PersistentConceptGraph: SQLCipher
│   │                          #   schema, scope-filtered queries, AEAD-encrypted
│   │                          #   node / edge round-trip
│   ├── synthesis_pipeline/    # Phase 2 synthesis: window manager, typed
│   │                          #   SynthesisObject, GBNF schema types, no-op
│   │                          #   synthesizer, encrypted publish / consume,
│   │                          #   synthesizer-role election + Phase 3
│   │                          #   hierarchy module (DomainSynthesisInput /
│   │                          #   TenantSynthesisInput / ApprovedDocument /
│   │                          #   WindowScopeTier; type-system enforcement of
│   │                          #   the channel → domain → tenant flow rules)
│   ├── synthesis_engine/      # Phase 3 server-side synthesis-engine skeleton:
│   │                          #   SynthesisEngine trait, ManagedEndpointSynthesizer
│   │                          #   stub, end-to-end channel → domain → tenant test
│   ├── sync_engine/           # Phase 2 CRDT delta sync: AddWinsSet
│   │                          #   observed-remove set + append-only OpLog
│   │                          #   with merge_logs / supersede
│   ├── permission_service/    # Phase 3 Zanzibar-style relation tuples (10 object
│   │                          #   types, 7 relations with default inheritance),
│   │                          #   in-memory TupleStore, check_permission
│   │                          #   reachability query over userset rewrites
│   ├── tenant_service/        # Phase 3 tenant lifecycle (Active / Suspended /
│   │                          #   Deleted), per-tenant encryption key references,
│   │                          #   member provisioning, config validation
│   ├── audit_service/         # Phase 3 append-only audit log: AuditEntryBuilder,
│   │                          #   AuditQuery (scope / action / actor / time),
│   │                          #   AuditActionTypes (canonical promotion,
│   │                          #   export, agent proposal, policy change,
│   │                          #   member provisioned / removed, tenant
│   │                          #   lifecycle, key destruction; Phase 5 adds
│   │                          #   ExportRendered / ExportSimulated /
│   │                          #   AgentProposalSubmitted / AgentProposalPromoted /
│   │                          #   AgentProposalRejected + helpers)
│   ├── agent_contract/        # Phase 5 agent write contract: AgentProposal<T>,
│   │                          #   ObservationProposal / ConceptProposal /
│   │                          #   RelationProposal / SummaryProposal,
│   │                          #   AgentIdentity, schema validation,
│   │                          #   Proposed → UnderReview → Promoted/Rejected
│   │                          #   lifecycle, AutoPromotionPolicy, ProposalStore,
│   │                          #   promote_to_canonical → CanonicalArtifact
│   ├── export_plane/          # Phase 5 export plane: PortableConceptProfile,
│   │                          #   ApprovedConcept, ExportView (ConceptsOnly /
│   │                          #   WithSummaries / WithEvidencePack), EvidencePack,
│   │                          #   ExportPolicy + PolicyEngine (least-privilege),
│   │                          #   ExportControlRegistry (deny-by-default per
│   │                          #   concept / summary / workflow), PolicySimulator
│   │                          #   (read-only preview), ConceptApprovalWorkflow
│   ├── connector_framework/   # Phase 4 connector substrate: Connector trait
│   │                          #   (authenticate / initial_sync / incremental_sync /
│   │                          #   subscribe_webhook / handle_webhook_event),
│   │                          #   OAuth2TokenVault + TokenRefresher with HKDF
│   │                          #   SecretToken wrappers, SyncState (Full/Incremental),
│   │                          #   WebhookSubscription + HMAC-SHA256 verifier,
│   │                          #   ConnectorConfig / ConnectorInstance / ConnectorEvent
│   │                          #   (DocumentCreated / Updated / Deleted /
│   │                          #   PermissionChanged), channel-scoped
│   │                          #   AttachmentRegistry (one-connector-per-source),
│   │                          #   AclSyncEngine + PermissionMapping into
│   │                          #   permission_service relation tuples
│   └── reasoning_engine/      # Phase 6 reasoning plane: ContradictionDetector
│                              #   + AdjudicationWorkflow (Detected → UnderReview
│                              #   → Resolved), DriftDetector + DriftMarker,
│                              #   GraphTraversal (typed-edge BFS with
│                              #   TraversalBudget + TraversalQuery + PathScorer,
│                              #   targeted + exploratory modes), QueryPlanner
│                              #   (RetrievalMode: Summary / Fts / SemanticVector /
│                              #   GraphTraversal / RawEvidence; QueryClassifier;
│                              #   PlannerHeuristics; PlanExecutionResult),
│                              #   WorkflowMemory (WorkflowTrace, WorkflowPattern,
│                              #   PatternMatcher, TraceRecorder)
├── .github/workflows/ci.yml   # fmt + clippy + build + test on push / PR
├── PROPOSAL.md                # product thesis
├── ARCHITECTURE.md            # system architecture
├── PHASES.md                  # phase 0 → 7 delivery plan
└── PROGRESS.md                # per-phase checklist + changelog
```

Each crate's public API is documented in its `src/lib.rs`. Run
`cargo doc --no-deps --open` to browse the rendered docs locally.

---

## Two surfaces, one substrate

Knowledge runs as two cooperating surfaces over a single shared
substrate, so the same memory model and the same synthesis rules
apply whether a fact came from a local DM or a SharePoint document.

### 1. On-device surface

Native or near-native clients on every form factor a user actually
holds:

| Platform | Shell | Native bindings |
|---|---|---|
| iOS | Swift native UI | Rust shared core via UniFFI; Core ML / MLX for inference |
| Android | Kotlin native UI | Rust shared core via JNI; ONNX Runtime + llama.cpp NDK |
| macOS | Electron + React | Rust core via Swift N-API addon; MLX preferred runtime |
| Windows | Electron + React | Rust core via C++ N-API addon; DirectML EP + CPU EP |

The on-device surface accesses and updates information the user
already has on the device — chat messages, free-form text, files,
media — and continuously builds an always-fresh on-device knowledge
/ context object scoped to the user, the channels they participate
in, and (optionally) the communities they belong to.

### 2. On-server surface

A server-side surface that authenticates against shared document
management and collaboration systems and continuously builds an
always-fresh knowledge / context object scoped to a tenant's
domains and channels:

- Google Drive
- OneDrive
- Notion
- Jira
- Confluence
- Figma
- HubSpot
- Slack, email (later phases)

Each connector follows the same `connector → evidence plane →
observation plane → semantic plane` pipeline as the on-device
surface, with ACLs synced from the source system and citations
preserved on every derived observation.

---

## Knowledge hierarchy (max 3 levels)

Knowledge objects are organized into a strict, scope-aware
hierarchy that synthesizes upward — never the other way around.

**B2C (max 3 levels):**

```
user → community → channel
```

- **User Memory Object** — the person's personal memory: facts,
  pinned items, episodic summaries, working context.
- **Community Memory Object** *(optional)* — shared synthesized
  knowledge for a community the user belongs to.
- **Channel Memory Object** — recaps, decisions, open questions,
  tasks for a specific channel within a community.

**B2B (max 3 levels per tenant):**

```
user → domain → channel
```

- **User Memory Object** — the employee's personal scope.
- **Domain Memory Object** — cross-channel workstreams,
  dependencies, risks, procedures for a logical work area.
- **Channel Memory Object** — channel-scoped recaps, decisions,
  open questions, tasks.

Synthesis is strict: only channel synthesis touches raw messages;
domain synthesis consumes channel outputs only; tenant-level
synthesis (where present) consumes domain outputs only. This
hierarchy is enforced cryptographically — see
[PROPOSAL.md §6 Knowledge hierarchy and synthesis](./PROPOSAL.md#6-knowledge-hierarchy-and-synthesis).

---

## Tech stack

| Layer | Stack |
|---|---|
| iOS shell | Swift native UI; Rust core via UniFFI |
| Android shell | Kotlin native UI; Rust core via JNI |
| macOS shell | Electron 31 + React renderer; Rust core via Swift N-API addon |
| Windows shell | Electron 31 + React renderer; Rust core via C++ N-API addon |
| Shared core | Rust library compiled to iOS framework, Android `.so`, macOS / Windows N-API addon |
| Local store | SQLCipher (AES-256-GCM) + SQLite FTS5 + content-hash dedup |
| On-device inference | `llama-server` (PrismML `kennguy3n/llama.cpp@prism` fork) for Bonsai-1.7B; MLX runtime on Apple Silicon; ONNX Runtime for XLM-R embeddings |
| Server services | Go (API gateway, connector service, permission service, tenant service, export service, audit) + Rust (synthesis engine, crypto, vector store) |
| Server storage | PostgreSQL (relational + provenance), pgvector (embeddings), MinIO / S3 (objects), NATS JetStream (async) |
| Sync | CRDT-based delta sync; MLS group keying for shared encrypted memory objects |
| Crypto | Hybrid X25519 + ML-KEM-768 (Kyber) KEM, ML-DSA-65 (Dilithium) signatures, BLAKE3 hashing, XChaCha20-Poly1305 segments, SPHINCS+ as stateless backup |

The Rust shared core is the single source of truth for the
evidence store, the observation engine, the memory state machine,
the concept graph, the synthesis pipeline, the crypto layer, and
the sync engine. Every platform reuses it via FFI; UI is the only
thing each platform owns.

### Device tiering

The system is optimized for high-end, mid-tier, and low-end
devices — and on Windows for both CPU-only and CPU+GPU
configurations. Tiering is based on RAM, sustained compute, and
thermal envelope, mirroring the `slm-chat-demo`
[on-device model strategy](https://github.com/kennguy3n/slm-chat-demo/blob/main/docs/kchat-on-device-model-strategy.md):

| Tier | RAM | Compute strategy | SLM |
|---|---|---|---|
| Low | 2–3 GB | Lexicon classifiers + XLM-R INT4 embeddings only; no SLM | Disabled |
| Medium | 4–6 GB | XLM-R INT8 + Bonsai-1.7B SLM gated to active scope synthesis | Gated |
| High | 8+ GB | Always-on Bonsai-1.7B (MLX 2-bit on Apple Silicon, GGUF Q4_K_M elsewhere) | Always |

Windows specifically supports two profiles — **CPU-only** (AVX2
minimum, AVX-512 / AVX-VNNI when available) and **CPU+GPU**
(DirectML EP for embeddings, llama.cpp Vulkan / CUDA backend for
SLM). The shared `llama-server` sidecar handles both transparently.

---

## Model strategy

The on-device model strategy is shared across the KChat platform —
this repo references the canonical model selection document in
`slm-chat-demo`:

- **`kennguy3n/slm-chat-demo`** —
  [`docs/kchat-on-device-model-strategy.md`](https://github.com/kennguy3n/slm-chat-demo/blob/main/docs/kchat-on-device-model-strategy.md)
  is the cross-repo reference for model inventory, platform
  packaging, device tiering, and delivery phases.

The headline picks for the Knowledge substrate:

| Role | Model | Format | On-disk | Notes |
|---|---|---|---|---|
| Synthesizer SLM (channel / episodic / domain) | Bonsai-1.7B (Qwen3-derived) | GGUF Q4_K_M | ~237 MB | Served by the shared `llama-server` from the PrismML `kennguy3n/llama.cpp@prism` fork — see [§3 in ARCHITECTURE.md](./ARCHITECTURE.md#3-on-device-inference-architecture). |
| Synthesizer SLM (Apple Silicon) | Bonsai-1.7B | MLX 2-bit | ~248 MB | Preferred runtime on iOS / macOS via Apple MLX. |
| Embeddings + classification | XLM-R | INT8 ONNX (~107 MB) / INT4 ONNX (~55 MB) | <110 MB | Multilingual encoder used for retrieval, importance tagging, entity extraction, and contradiction detection. Same artifact is shared with `slm-guardrail` and `chat-storage-search` to eliminate redundant weights. |

The runtime is deliberately a *fork* — `kennguy3n/llama.cpp@prism`
on the `prism` branch — because it ships SIMD repack kernels for
the `Q1_0_g128` ternary format used in Bonsai derivatives across
CUDA, Metal, Vulkan, AVX-512 VNNI, AVX-VNNI, AVX2, and ARM NEON.
That fork is the only on-device SLM runtime the substrate
ships with. See
[`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism)
for the kernel implementations and dispatcher.

A shared `llama-server` sidecar pattern (`--parallel 2`, mmap
weights, 60 s idle-unload, warm-up at boot) is used so that
multiple subsystems (Knowledge synthesis, KChat skills, CV-Guard
SLM consultation, slm-guardrail when SLM-promoted) share a single
loaded model rather than each holding their own copy.

Thinking is disabled on the synthesizer model (a closed
`<think>\n</think>\n` pair is prepended to the prompt) — Knowledge
uses Graph-of-Thought traces in the reasoning plane instead of
in-model chain-of-thought, so synthesis stays auditable.

---

## Privacy and post-quantum cryptography

Privacy is the substrate, not a feature. Three properties hold by
construction:

1. **Raw evidence is encrypted at rest, locally, with keys the
   user (or the tenant) holds.** Cross-device sync moves
   *synthesis objects*, not raw bodies; raw evidence stays on the
   originating device unless policy explicitly allows otherwise.
2. **Cryptographic forgetting via key destruction.** True deletion
   is enforced by destroying the per-scope or per-epoch keys —
   not by best-effort row deletes. A scope is gone the moment its
   key is gone.
3. **Post-quantum thinking from day one.** All new key exchanges
   use a hybrid X25519 + ML-KEM-768 (Kyber) construction;
   provenance and manifest signing use ML-DSA-65 (Dilithium); a
   stateless SPHINCS+ backup is kept for high-assurance signing.
   Group keying for shared channel / domain memory uses MLS with
   post-quantum extensions.

See [PROPOSAL.md §9](./PROPOSAL.md#9-post-quantum-cryptography)
and [ARCHITECTURE.md §8](./ARCHITECTURE.md#8-post-quantum-crypto-layer)
for the cryptographic details.

---

## Device considerations

The system is built to behave well on the device the user actually
owns, not the device the engineer used to develop it. Three axes
are continuously monitored and actively shape behaviour:

- **Storage.** Content-aware storage routing — short text messages
  (≤ 512 B) stored inline in evidence rows; large bodies (files,
  chunks, transcripts) stored in a deduplicated body table with
  BLAKE3 content-hash dedup; noise-class messages held in a
  fixed-size ring buffer that auto-expires. Cold encrypted segments
  (XChaCha20-Poly1305 + per-epoch keys) for the long tail. Caps
  are configurable per tier (250 MB safety on mobile, 1 GB+ on
  desktop with SLM loaded).
- **Memory.** Models are mmap'd; the SLM unloads after 60 s idle;
  at most one heavy model resident on mobile at a time. Hard
  caps: 250 MB on mobile (without SLM resident), 1 GB on desktop
  with SLM resident.
- **Battery.** Below 20% the synthesis pipeline skips heavy work
  (channel / domain synthesis is deferred), only sensory
  observations + lexicon-based importance tagging continue.
  Sync becomes batch-only and waits for AC + Wi-Fi.

Graceful degradation is the rule: low tier devices never enter the
SLM path; medium-tier devices gate the SLM behind heat / battery /
RAM checks; the substrate always remains queryable on lexicon +
XLM-R retrieval even when the SLM is unavailable.

---

## Project documents

| Document | Purpose |
|---|---|
| [PROPOSAL.md](./PROPOSAL.md) | Product thesis, strategic principles, layered substrate, memory + decay, model strategy, hierarchy, permissions, deployment modes, post-quantum cryptography, integration surface |
| [ARCHITECTURE.md](./ARCHITECTURE.md) | System overview, Rust shared core, on-device inference, server architecture, data flow, permissions, decay state machine, post-quantum crypto layer, device optimization, platform-specific notes |
| [PHASES.md](./PHASES.md) | Phase 0 → Phase 7 delivery plan with goals, deliverables, exit criteria, and a 30 / 60 / 90-day implementation timeline |
| [PROGRESS.md](./PROGRESS.md) | Per-phase deliverable checklist, overall status table, and changelog |

---

## Reference repositories

| Repo | Role |
|---|---|
| [`kennguy3n/slm-chat-demo`](https://github.com/kennguy3n/slm-chat-demo) | Reference for model selection, device thinking, and the cross-repo on-device model strategy ([`docs/kchat-on-device-model-strategy.md`](https://github.com/kennguy3n/slm-chat-demo/blob/main/docs/kchat-on-device-model-strategy.md)). |
| [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism) | Modified llama.cpp inference runtime (PrismML `prism` branch) used as the on-device SLM serving layer for Bonsai-1.7B. Ships SIMD repack kernels for the `Q1_0_g128` ternary format across CUDA, Metal, Vulkan, AVX-512 VNNI, AVX-VNNI, AVX2, and ARM NEON. |

These are the two upstreams Knowledge depends on; everything else
in the substrate is implemented in this repo's planned phases.
