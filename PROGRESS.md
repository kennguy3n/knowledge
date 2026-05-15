# Knowledge — Progress Tracker

This tracker captures per-phase deliverable status for the
Knowledge substrate. The phase scope itself lives in
[PHASES.md](./PHASES.md); see [README.md](./README.md) for an
overview of the system, and
[`docs/MODULE_STATUS.md`](./docs/MODULE_STATUS.md) for a per-crate
classification.

Every phase has *some* deliverables that are real and others that
are still contract-level skeletons. The status taxonomy below is
applied uniformly so it is clear which is which.

For a curated chronological view of major milestones see
[`docs/DEVELOPMENT_LOG.md`](./docs/DEVELOPMENT_LOG.md).

---

## Status taxonomy

Each deliverable is tagged with one of the following:

- **Runtime complete** — works without mocks on real inputs. Real
  cryptography / real persistence / real algorithms — no stub
  backend, no `NoOpSynthesizer`, no hardcoded fixture. Has tests
  that exercise the actual production code path.
- **Contract complete** — the public surface (types, traits, error
  variants, audit shapes) is shipped and exercised by tests, but
  the implementation behind the surface is a stub, mock, fixture
  parser, or `Unimplemented` FFI return. Callers can wire against
  the API today; the substrate behind it lands later.
- **Production ready** — *not used yet*. Reserved for items that
  have shipped to a live integration, passed an external security
  review, and have operational monitoring. Nothing in this
  tracker is at this tier.
- **Not started** — no code, only a placeholder in PHASES.md.

The intent is to be honest about which pieces are real and which
are skeletons. "Runtime complete" does not imply production
readiness — it just means the code path executes against real
data instead of fixtures.

---

## Overall status summary

| Phase | Status | Notes |
|-------|--------|-------|
| Phase 0: Foundation | Mixed | Runtime: evidence store, crypto, CI. Contract: iOS / Android / macOS / Windows FFI bindings (every export returns `Unimplemented`). |
| Phase 1: Personal Memory (on-device) | Mixed | Runtime: memory manager state machine, retention scoring, lexicon classifier, working memory, hybrid retrieval. Contract: XLM-R embeddings, SLM-assisted observation pipeline (no real SLM wired), episodic memory (uses `NoOpSynthesizer`). |
| Phase 2: Channel Memory | Mixed | Runtime: window manager, AEAD publish/consume, GBNF schema, MLS leaf KEMs, ML-DSA-65 provenance. Contract: elected-device synthesizer (drives `NoOpSynthesizer`). |
| Phase 3: Domain & Tenant Memory | Mixed | Runtime: hierarchy enforcement, concept graph, permission service, tenant service, audit log. Contract: server-side synthesis engine (skeleton + stub `ManagedEndpointSynthesizer`), Go gateway (not in this repo). |
| Phase 4: Connector Integration | Contract | All connectors are fixture-driven parsers — no live OAuth2 transport, no live webhooks, no live ACL sync. Connector framework defines the OAuth2 token-vault types but no real provider implementation. |
| Phase 5: Portable Concept Profiles & Export | Runtime | Export plane, portable concept profile, agent contract, policy simulator, and audit trail are real implementations. |
| Phase 6: Graph UX & Advanced Reasoning | Mixed | Runtime: query planner, contradiction adjudication, incremental recompute. Contract: Graph-of-Thought executor and community summaries (router logic exists, no real SLM backend). |
| Phase 7: PQ Hardening & Confidential Compute | Mixed | Runtime: ML-KEM-768, ML-DSA-65, hybrid enforcement audit trail, scope / epoch DEK destroy. Contract: confidential compute worker (uses `MockTeeRuntime`). Stub: SPHINCS+ (BLAKE3-keyed placeholder, not a real lattice signer). Not started: red-team test suite, memory quality metrics. |

For a per-crate breakdown see
[`docs/MODULE_STATUS.md`](./docs/MODULE_STATUS.md).

---

## Status matrix (per module / capability)

| Area | Status | Notes |
|------|--------|-------|
| Evidence store (SQLCipher, FTS5, AEAD, dedup, ring buffer) | Runtime complete | Real SQLCipher with bundled OpenSSL; FTS5 plaintext index. See the forgetting caveat in `crates/evidence_store/tests/forgetting_fts.rs`. |
| Crypto: BLAKE3, AEAD, HKDF, hybrid X25519 + ML-KEM-768 | Runtime complete | Real RustCrypto `ml-kem` backend; hybrid combiner is real. `StubKemBackend` is feature-gated test-only. |
| Crypto: ML-DSA-65 provenance signer | Runtime complete | Real RustCrypto `ml-dsa`. `TestSigner` (HMAC-SHA256) is feature-gated test-only. |
| Crypto: SPHINCS+ | Stub | `crates/crypto/src/sphincs.rs` is a BLAKE3-keyed placeholder. Real backend not wired. |
| Memory manager (decay FSM, retention scoring, working memory) | Runtime complete | Pure-Rust implementation; tests cover the decay state machine end-to-end. |
| Observation engine (lexicon classifier + extractor pipeline) | Runtime complete | Lexicon path runs on real text. SLM-assisted path is contract-only. |
| Episodic memory (session / thread summaries) | Contract complete | Driven by `NoOpSynthesizer`; no SLM wired. |
| XLM-R embeddings via shared ONNX | Contract complete | Skeleton + dimension config. No ONNX Runtime invocation in this repo; embeddings fall back to 0.0 when no model is configured. |
| Synthesis pipeline (windows, AEAD publish/consume, GBNF schema, election) | Runtime complete | Window manager, AAD-bound publish/consume, election eligibility checks are all real. The only synthesizer that ships in this crate is `NoOpSynthesizer`. |
| Synthesis pipeline: actual SLM-backed synthesizer | Contract complete | `NoOpSynthesizer` is the only implementation. Bonsai-1.7B via `llama-server` is not wired. |
| Synthesis engine (server-side, hierarchy enforcement) | Mixed | Hierarchy enforcement and audit shapes are real. The synthesizers it drives (`ManagedEndpointSynthesizer` stub) are contract-only. |
| Synthesis engine: confidential-compute TEE worker | Contract complete | Uses `MockTeeRuntime`; production calls into Intel TDX / AMD SEV-SNP / Nitro Enclaves SDKs are not wired. |
| Concept graph (sparse typed graph) | Runtime complete | Persistent SQLCipher-backed graph with IsA / PartOf / Contradicts edges. |
| Permission service (Zanzibar-style ReBAC) | Runtime complete | Real relation tuples + reachability checks. |
| Tenant service | Runtime complete | Lifecycle, per-tenant keys, member provisioning. |
| Audit service | Runtime complete | Append-only audit log with hash-linked entries. |
| Agent contract (proposal-only API) | Runtime complete | Real shape + lifecycle for observation / concept / relation / summary proposals. |
| Export plane (portable concept profiles, policy simulator) | Runtime complete | Real least-privilege views; no raw-document export. |
| Connectors (Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, Gmail / Microsoft Graph) | Contract complete | All are fixture parsers. No live API transport, no real OAuth2 refresh against a real IdP, no real webhook ingest. |
| Connector framework (OAuth2 token vault, delta sync, webhooks) | Contract complete | Type surface only; no real HTTP / network transport. |
| ACL sync from source systems into the relation graph | Contract complete | Sync types defined; no live provider feed. |
| Inference router | Contract complete | Router logic exists; adapter implementations need real backends (llama.cpp / MLX / ONNX). |
| Reasoning engine (multi-hop, GoT, community summaries) | Mixed | Traversal / contradiction adjudication / incremental recompute are real. GoT executor is contract-only when it needs an SLM. |
| FFI (UniFFI for iOS, JNI for Android) | Contract complete | Every exported function returns `Unimplemented`. Build pipeline produces a real artifact; the artifact has no behaviour. |
| N-API (macOS / Windows addon) | Contract complete | Forwards to the FFI surface; same `Unimplemented` story. |
| Sync engine (CRDT delta sync of synthesis objects) | Contract complete | Deliberate stub until Phase 2's multi-device path lands. |
| Red-team privacy / prompt-injection suite | Not started | Listed in Phase 7 but no test code exists yet. |
| Memory quality metrics (retention precision, contradiction rate, decay tuning) | Not started | Listed in Phase 7 but no metrics pipeline exists yet. |

---

## Phase 0 — Foundation

- [x] **Runtime complete:** Rust shared-core scaffold (evidence
  store, crypto, sync engine module layout)
- [x] **Runtime complete:** SQLCipher local store with hybrid
  X25519 + ML-KEM-768 key derivation
- [x] **Runtime complete:** Encrypted append-only evidence
  ingestion (messages, files, chunks)
- [x] **Runtime complete:** BLAKE3 content-hash deduplication
- [x] **Contract complete:** On-device importance classifier —
  real lexicon path; SLM path (Bonsai-1.7B via shared
  `llama-server`) is not wired in this repo
- [x] **Contract complete:** iOS framework binding (UniFFI
  `.xcframework`) — every exported function returns
  `Unimplemented`
- [x] **Contract complete:** Android JNI binding (per-ABI shared
  libraries) — every exported function returns `Unimplemented`
- [x] **Contract complete:** macOS / Windows N-API addon binding —
  forwards to the FFI surface, same `Unimplemented` story
- [x] **Runtime complete:** Unit test suite for the evidence
  store, crypto, and lexicon importance classifier
- [x] **Runtime complete:** CI pipeline (lint, unit tests,
  multi-target build)

---

## Phase 1 — Personal Memory (on-device)

- [x] **Runtime complete:** Memory manager with the full decay
  state machine
- [x] **Runtime complete:** Observation engine (entity, fact,
  task, decision extraction — lexicon path)
- [x] **Contract complete:** Lexicon → XLM-R → SLM-assisted
  observation pipeline — lexicon stage runs on real text; XLM-R
  and SLM stages are skeletons
- [x] **Contract complete:** Episodic memory (session / thread
  summaries) — driven by `NoOpSynthesizer`, no Bonsai-1.7B wired
- [x] **Runtime complete:** Working memory (TTL-evicting context
  window)
- [x] **Contract complete:** XLM-R embeddings via shared ONNX
  artifact (INT8 / INT4) — dimension config and adapter trait
  ship; no ONNX Runtime invocation
- [x] **Runtime complete:** Hybrid retrieval (FTS5 lexical +
  semantic vector + recency) — vector slot is real when an
  `EmbeddingModel` is configured, falls back to 0.0 otherwise
- [x] **Runtime complete:** Retention scoring (pinning, retrieval
  frequency, age, non-use)
- [x] **Runtime complete:** User Memory Object CRUD (read / pin /
  unpin / forget)
- [x] **Runtime complete:** Privacy strip on every synthesis
  output
- [x] **Runtime complete:** Decay state machine, observation
  pipeline, and retrieval tests

---

## Phase 2 — Channel Memory (on-device + shared)

- [x] **Runtime complete:** Channel Memory Object (recaps,
  decisions, open questions, active tasks)
- [x] **Runtime complete:** Synthesis pipeline (window manager,
  typed synthesis objects, GBNF schema)
- [x] **Runtime complete:** MLS group keying with hybrid X25519 +
  ML-KEM-768 leaf KEMs
- [x] **Runtime complete:** Encrypted synthesis-object
  publication with scope / window / object AAD binding
- [x] **Runtime complete:** Channel-scoped importance tagging
- [x] **Contract complete:** Elected-member-device synthesizer
  (tier / battery / heartbeat eligibility) — eligibility logic is
  real; the synthesizer it drives is `NoOpSynthesizer`
- [x] **Contract complete:** Multi-device CRDT sync of synthesis
  objects (add-wins, supersession, contradictions) — types exist;
  `sync_engine` is a deliberate stub
- [x] **Runtime complete:** Provenance bundles signed with
  ML-DSA-65
- [x] **Runtime complete:** End-to-end tests for the elected-
  device window manager + publish/consume path

---

## Phase 3 — Domain & Tenant Memory (B2B server)

- [x] **Runtime complete:** Domain Memory Object (cross-channel
  workstreams, dependencies, risks, procedures)
- [x] **Runtime complete:** Tenant Memory Object (canonical
  policy, product taxonomy, stable org knowledge)
- [x] **Contract complete:** Server-side synthesis service —
  Rust synthesis engine skeleton ships with a stub
  `ManagedEndpointSynthesizer`; Go gateway lives outside this
  repo
- [x] **Runtime complete:** Type-enforced hierarchy: domain
  consumes channel only, tenant consumes domain + approved docs
  only
- [x] **Runtime complete:** Sparse concept graph with SQLCipher
  persistence
- [x] **Runtime complete:** Zanzibar-style permission service
  with reachability checks
- [x] **Contract complete:** Managed AI endpoint synthesizer —
  HTTP client surface defined; `MockHttpClient` only
- [x] **Runtime complete:** Tenant service (lifecycle, per-tenant
  keys, member provisioning)
- [x] **Runtime complete:** End-to-end tests for the channel →
  domain → tenant synthesis chain
- [x] **Runtime complete:** Append-only audit log

---

## Phase 4 — Connector Integration (on-server)

Every connector in this phase is **Contract complete** — fixture
parsers that exercise the substrate's evidence / observation /
permission shapes, without a real OAuth2 transport or webhook
ingest. The substrate plumbing they feed is real; the wire side
is not.

- [x] **Contract complete:** Connector framework (OAuth2 token
  vault types, refresh shape, incremental delta sync shape,
  webhook shape) — no live HTTP transport
- [x] **Contract complete:** Google Drive connector (fixture
  parser)
- [x] **Contract complete:** OneDrive connector (fixture parser)
- [x] **Contract complete:** Notion connector (fixture parser)
- [x] **Contract complete:** Jira connector (fixture parser)
- [x] **Contract complete:** Confluence connector (fixture
  parser)
- [x] **Contract complete:** Figma connector (fixture parser)
- [x] **Contract complete:** HubSpot connector (fixture parser)
- [x] **Contract complete:** Slack connector (fixture parser)
- [x] **Contract complete:** Email connector (Gmail + Microsoft
  Graph fixture parsers)
- [x] **Runtime complete:** Channel-scoped connector attachment
  (the substrate side that owns the attachment record)
- [x] **Contract complete:** ACL sync from source systems into
  the relation graph — sync types defined; no live provider feed
- [x] **Runtime complete:** Document observation pipeline
  (chunking, importance tagging, entity / topic extraction —
  lexicon path)
- [x] **Runtime complete:** Citation rendering with stable links
  back to source documents
- [x] **Contract complete:** Connector-specific integration tests
  against vendor fixtures (not against live APIs)

---

## Phase 5 — Portable Concept Profiles & Export

- [x] **Runtime complete:** Portable concept profile (approved
  concepts, constraints, reasoning for an external context)
- [x] **Runtime complete:** Export plane with least-privilege
  views and no raw-document export by default
- [x] **Runtime complete:** Agent write contract (proposal-only
  API for observations, concepts, relations, summaries)
- [x] **Runtime complete:** Agent proposal schema with scope,
  provenance, evidence refs, confidence, sensitivity, TTL,
  identity
- [x] **Runtime complete:** Per-concept / -summary / -workflow
  export controls with policy preview
- [x] **Runtime complete:** Audit trail for every export, agent
  proposal, and canonical promotion
- [x] **Runtime complete:** Policy simulator (read-only export
  preview)
- [x] **Runtime complete:** End-to-end tests for the export plane
  and the agent proposal lifecycle

---

## Phase 6 — Graph UX & Advanced Reasoning

- [x] **Runtime complete:** Concept graph visualization data
  shape with scope-filtered exploration
- [x] **Runtime complete:** Contradiction and drift detection
  with explicit edges and an adjudication workflow
- [x] **Runtime complete:** Multi-hop reasoning over the concept
  graph with typed-edge traversal and budgets
- [x] **Runtime complete:** Workflow memory for successful
  reasoning traces and tool-use patterns
- [x] **Contract complete:** Graph-of-Thought reasoning for
  complex queries — DAG executor exists; the SLM steps it dispatches
  to require a real backend not wired here
- [x] **Runtime complete:** Query planner (cheapest-first routing
  with explicit fallbacks)
- [x] **Contract complete:** Community summaries (GraphRAG-style,
  bottom-up over reachable scopes) — clustering is real, summary
  text generation needs an SLM
- [x] **Runtime complete:** Incremental graph updates (recompute
  only touched branches)
- [x] **Runtime complete:** Tests covering query planning,
  contradiction adjudication, and incremental recompute

---

## Phase 7 — Post-Quantum Hardening & Confidential Compute

- [x] **Runtime complete:** ML-KEM-768 (Kyber) for all key
  exchanges (RustCrypto `ml-kem`)
- [x] **Runtime complete:** ML-DSA-65 (Dilithium) for provenance
  signatures (RustCrypto `ml-dsa`)
- [x] **Runtime complete:** Hybrid classical + PQ enforcement
  with key-exchange audit trail
- [x] **Runtime complete:** Post-quantum MLS extensions (hybrid
  leaf KEMs, ML-DSA-65 commits)
- [ ] **Stub:** SPHINCS+ co-signatures — `crates/crypto/src/sphincs.rs`
  is a BLAKE3-keyed placeholder, not a real SPHINCS+ implementation
- [x] **Contract complete:** Confidential-compute worker
  (attested TEE) for shared synthesis — wraps a `TeeRuntime`
  trait; the only ships-with-it implementation is `MockTeeRuntime`
- [x] **Runtime complete:** Attestation reports bound to
  synthesizer keys with audit-trail linkage (under the mock TEE)
- [x] **Runtime complete:** Cryptographic forgetting — per-scope
  and per-epoch DEK destroy paths, zeroize-on-drop, tombstone
  registry. **Caveat:** the SQLite FTS5 plaintext index in
  `evidence_store` is *not* erased by DEK destruction. See
  `crates/evidence_store/tests/forgetting_fts.rs` for the
  test that documents the gap
- [ ] **Not started:** Red-team privacy and prompt-injection test
  suites
- [ ] **Not started:** Memory quality metrics (retention
  precision, contradiction detection rate, decay tuning)
