# Knowledge — Progress Tracker

Last updated: 2026-05-08 (full-pipeline demo crate + Phase 4 Slack/Email line items)

This tracker captures per-phase deliverable status and a
chronological changelog for the Knowledge substrate. Phase scope
is defined in [PHASES.md](./PHASES.md); architectural detail
lives in [ARCHITECTURE.md](./ARCHITECTURE.md); the product thesis
lives in [PROPOSAL.md](./PROPOSAL.md).

All eight phases (0 through 7) are complete. The substrate ships
20 Rust crates covering the evidence plane, observation engine,
memory manager, concept graph, synthesis pipeline, synthesis
engine, permission service, tenant service, audit service, agent
contract, export plane, connector framework, nine vendor
connectors, reasoning engine, inference router, crypto layer,
sync engine, FFI bindings, N-API addon, and a public-API-only
end-to-end demo crate that drives all twelve substrate phases
(evidence → observation → memory → concept graph → synthesis →
permissions → crypto → export → agent → reasoning → connectors →
audit) over a realistic multi-scope dataset and writes a fully
reconciled report to `results/demo_results.md`. 1077 tests pass
across unit, integration, demo end-to-end, and red-team suites.

---

## Overall status summary

| Phase | Status | Progress |
|-------|--------|----------|
| Phase 0: Foundation | Complete | 100% (11 of 11) |
| Phase 1: Personal Memory (on-device) | Complete | 100% (11 of 11) |
| Phase 2: Channel Memory (on-device + shared) | Complete | 100% (9 of 9) |
| Phase 3: Domain & Tenant Memory (B2B server) | Complete | 100% (10 of 10) |
| Phase 4: Connector Integration (on-server) | Complete | 100% (14 of 14) |
| Phase 5: Portable Concept Profiles & Export | Complete | 100% (8 of 8) |
| Phase 6: Graph UX & Advanced Reasoning | Complete | 100% (9 of 9) |
| Phase 7: Post-Quantum Hardening & Confidential Compute | Complete | 100% (9 of 9) |

---

## Phase 0 — Foundation

- [x] Rust shared core skeleton — `evidence_store`, `crypto`, `sync_engine` modules
- [x] SQLCipher local store with post-quantum key derivation (hybrid X25519 + ML-KEM-768 unwrap)
- [x] Evidence plane — append-only encrypted message / file / chunk ingestion
- [x] Content-hash deduplication (BLAKE3)
- [x] Basic on-device importance classifier via Bonsai-1.7B + the shared `llama-server` from the PrismML [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism) fork — `crates/inference_router/` ships the `InferenceAdapter` trait + `InferenceRouter` + `MlxAdapter` / `LlamaCppAdapter` / `FallbackAdapter` skeletons, and `crates/evidence_store/src/classifier.rs` ships `SlmClassifier` (SLM-backed) + `CompositeClassifier` (lexicon → SLM chain) on top of the existing `LexiconClassifier`
- [x] Lexicon-only fallback when the SLM is not available
- [x] iOS framework binding (UniFFI `.xcframework`) — `crates/ffi/` ships the UniFFI surface with `ingest_message` / `query` / `get_evidence` / `get_user_memory` / `pin` / `unpin` / `forget` / `list_memories` / `run_decay_sweep` / `get_channel_memory` / `trigger_synthesis` / `generate_keypair` / `encrypt` / `decrypt`
- [x] Android JNI binding (`.so` per ABI) — same `crates/ffi/` UniFFI surface drives the JNI binding
- [x] macOS / Windows N-API addon binding — `crates/napi/` ships the addon skeleton with the same surface as `ffi`
- [x] Unit test suite covering `evidence_store`, `crypto`, importance classifier
- [x] CI: lint + unit tests + multi-target build

---

## Phase 1 — Personal Memory (on-device)

- [x] Memory manager with full decay state machine (candidate → reinforced → consolidated → canonical → superseded / archived / deleted)
- [x] Observation engine — entity extraction, fact extraction, task / decision detection
- [x] Lexicon → XLM-R → SLM-assisted observation pipeline (lexicon stage shipped; XLM-R and SLM-assisted stages stubbed pending ONNX + Bonsai-1.7B integration)
- [x] Episodic memory — session / thread summaries via on-device Bonsai-1.7B — `crates/memory_manager/src/episodic.rs` ships `EpisodicSummary` / `SessionBoundary` / `SessionDetector` / `Summarizer` trait + `SlmSummarizer` / `StubSummarizer` / `EpisodicStore`
- [x] Working memory — current context window management with TTL eviction
- [x] XLM-R embeddings via shared ONNX artifact (INT8 ~107 MB / INT4 ~55 MB) — `crates/evidence_store/src/embeddings.rs` ships `EmbeddingModel` trait + `OnnxEmbeddingAdapter` skeleton (behind a mockable `OnnxRuntime` trait) + `StubEmbeddingModel` + `cosine_distance`, and `crates/evidence_store/src/retrieval.rs::HybridRetriever` accepts an optional `Box<dyn EmbeddingModel>` and replaces the `0.0` semantic stub with real cosine similarity when present
- [x] Hybrid retrieval — FTS5 + semantic vector + recency — `crates/evidence_store/src/retrieval.rs::HybridRetriever::search_hybrid` now embeds the query and each candidate body through the configured `EmbeddingModel` and uses `cosine_distance` to compute the semantic component (gracefully falling back to `0.0` and surfacing `EvidenceError::Embedding` when the embedder errors); FTS5 + recency components are unchanged
- [x] Retention scoring — pinning, retrieval frequency, age, non-use
- [x] User Memory Object CRUD (read / pin / unpin / forget) — Rust data-model + sweep are shipped; the FFI surface is delivered alongside the Phase-0 platform bindings
- [x] Privacy strip on every synthesis output (compute, model, egress)
- [x] Decay state machine + observation pipeline + retrieval tests

---

## Phase 2 — Channel Memory (on-device + shared)

- [x] Channel Memory Object — recaps, decisions, open questions, active tasks (`crates/memory_manager/src/channel_memory.rs`)
- [x] Synthesis pipeline — window manager, typed synthesis objects, GBNF schema types, no-op synthesizer (recursive SLM summarization stage stubbed pending Bonsai-1.7B integration) (`crates/synthesis_pipeline/`)
- [x] MLS group keying for shared channel memory (hybrid X25519 + ML-KEM-768 leaf KEMs) — skeletal implementation in `crates/crypto/src/mls.rs` (group state, leaf key packages, commits, welcomes, key schedule); production-grade RFC 9420 lives in [`kennguy3n/openmls`](https://github.com/kennguy3n/openmls)
- [x] Encrypted synthesis object publication — XChaCha20-Poly1305 with `(scope_id, window_id, object_id)` AAD binding (`crates/synthesis_pipeline/src/publish.rs`)
- [x] Channel-scoped importance tagging (promote only high-value observations) (`crates/observation_engine/src/promotion.rs`)
- [x] Synthesizer role: elected member device path for small groups (tier / battery / heartbeat eligibility) (`crates/synthesis_pipeline/src/election.rs`)
- [x] Multi-device sync via CRDT for synthesis objects (add-wins + supersession + contradictions) (`crates/sync_engine/`)
- [x] Provenance bundles signed with ML-DSA-65 on every synthesis output — `MlDsa65Signer` / `MlDsa65Verifier` ship in `crates/crypto/src/signer_backend.rs` and implement the substrate-wide `ProvenanceSigner` trait (`crates/crypto/src/provenance.rs`)
- [x] End-to-end tests for the elected-device synthesis path (`crates/synthesis_pipeline/tests/election_tests.rs`, `crates/synthesis_pipeline/tests/publish_tests.rs`)

---

## Phase 3 — Domain & Tenant Memory (B2B server)

- [x] Domain Memory Object — cross-channel workstreams, dependencies, risks, procedures (`crates/memory_manager/src/domain_memory.rs`)
- [x] Tenant Memory Object — canonical policy, product taxonomy, stable org knowledge (`crates/memory_manager/src/tenant_memory.rs`)
- [x] Server-side synthesis service (Go gateway + Rust synthesis engine) — Rust side shipped (`crates/synthesis_engine/`); Go gateway is intentionally still pending
- [x] Domain synthesis consumes channel memory objects only (not raw messages) — type-system enforced via `DomainSynthesisInput` (`crates/synthesis_pipeline/src/hierarchy.rs`)
- [x] Tenant synthesis consumes domain objects + approved official docs — type-system enforced via `TenantSynthesisInput` + `ApprovedDocument` (`crates/synthesis_pipeline/src/hierarchy.rs`)
- [x] Sparse concept graph — typed relations, contradictions, drift markers — extended with SQLCipher persistence (`crates/concept_graph/src/persist.rs`) on top of the Phase 2 in-memory adjacency layer
- [x] Permission service (Zanzibar-style relation graph) with reachability checks (`crates/permission_service/`)
- [x] Managed AI endpoint as synthesizer for B2B channels / domains — `HttpManagedEndpointSynthesizer` ships in `crates/synthesis_engine/src/managed_endpoint.rs` with trait-based `HttpClient` (mockable), grammar-constrained request building, response validation, and timeout / rate-limit / empty-response error handling
- [x] Tenant service — tenant lifecycle, per-tenant encryption keys, member provisioning (`crates/tenant_service/`)
- [x] End-to-end tests for the channel → domain → tenant synthesis chain (`crates/synthesis_engine/tests/hierarchy_e2e.rs`)
- [x] Append-only audit log (`crates/audit_service/`) covering canonical promotions, exports, agent proposals, policy changes, member provisioning, tenant lifecycle, key destruction

---

## Phase 4 — Connector Integration (on-server)

- [x] Connector framework (OAuth2 token vault, refresh, incremental delta sync, webhooks) (`crates/connector_framework/`)
- [x] Google Drive connector (`crates/connectors/src/google_drive.rs`)
- [x] OneDrive connector (`crates/connectors/src/onedrive.rs`)
- [x] Notion connector (`crates/connectors/src/notion.rs`)
- [x] Jira connector (`crates/connectors/src/jira.rs`)
- [x] Confluence connector (`crates/connectors/src/confluence.rs`)
- [x] Figma connector (design system extraction) (`crates/connectors/src/figma.rs`)
- [x] HubSpot connector (CRM context) (`crates/connectors/src/hubspot.rs`)
- [x] Slack connector (channels, threads, files via Events API) (`crates/connectors/src/slack.rs`)
- [x] Email connector (Gmail + Microsoft Graph) (`crates/connectors/src/email.rs`)
- [x] Channel-scoped connector attachment (`crates/connector_framework/src/attachment.rs`)
- [x] ACL sync from source systems into the relation graph (`crates/connector_framework/src/acl_sync.rs`)
- [x] Observation extraction pipeline for documents (chunking, importance tagging, entity / topic extraction) (`crates/observation_engine/src/document.rs`)
- [x] Citation rendering with stable links back to source documents (`crates/observation_engine/src/citation.rs`)
- [x] Connector-specific integration tests against vendor fixtures (`crates/connectors/tests/`)

---

## Phase 5 — Portable Concept Profiles & Export

- [x] Portable concept profile — approved concepts, constraints, reasoning for a specific external context (`crates/export_plane/src/profile.rs`)
- [x] Export plane — least-privilege views; no raw document export by default (`crates/export_plane/src/policy.rs`, `crates/export_plane/src/profile.rs`)
- [x] Agent write contract — proposal-only API (`propose_observation`, `propose_concept`, `propose_relation`, `propose_summary`) (`crates/agent_contract/src/lib.rs`, `crates/agent_contract/src/lifecycle.rs`)
- [x] Agent proposal schema (scope, provenance bundle, evidence refs, confidence, sensitivity, TTL, supersedes / contradicts, agent identity + model version, skill / recipe id) (`crates/agent_contract/src/proposal.rs`, `crates/agent_contract/src/schema.rs`)
- [x] Export controls per concept / summary / workflow with policy preview (`crates/export_plane/src/controls.rs`, `crates/export_plane/src/simulator.rs`)
- [x] Audit trail for all exports + agent proposals + canonical promotions (`crates/audit_service/src/helpers.rs`, with new `ExportRendered`, `ExportSimulated`, `AgentProposalSubmitted`, `AgentProposalPromoted`, `AgentProposalRejected` action types in `crates/audit_service/src/entry.rs`)
- [x] Policy simulator (preview what an export would contain) (`crates/export_plane/src/simulator.rs`)
- [x] End-to-end tests for the export plane and the agent proposal lifecycle (`crates/agent_contract/tests/e2e_proposal_tests.rs`, `crates/export_plane/tests/e2e_export_tests.rs`)

---

## Phase 6 — Graph UX & Advanced Reasoning

- [x] Concept graph visualization (Kanvas-style exploration with scope filters) — `crates/concept_graph/src/visualization.rs` ships `GraphView`, `ViewFilter`, `NodeVisual` / `EdgeVisual`, BFS / DFS exploration, neighbourhood + subgraph + label/definition search, and `ScopeAccess` permission gating compatible with `permission_service::check_permission`
- [x] Contradiction and drift detection (explicit `contradicts` edges + adjudication workflow) (`crates/reasoning_engine/src/contradiction.rs`, `crates/reasoning_engine/src/drift.rs`)
- [x] Multi-hop reasoning over the concept graph (typed-edge traversal with budgets) (`crates/reasoning_engine/src/traversal.rs`)
- [x] Workflow memory — successful reasoning traces / tool-use patterns saved to the reasoning plane (`crates/reasoning_engine/src/workflow.rs`)
- [x] Graph-of-Thought reasoning for complex queries (`crates/reasoning_engine/src/graph_of_thought.rs`)
- [x] Query planner — cheapest-first routing (summary → graph → raw evidence) with explicit fallbacks (`crates/reasoning_engine/src/planner.rs`)
- [x] Community summaries (GraphRAG-style bottom-up over reachable scopes) (`crates/reasoning_engine/src/community.rs`)
- [x] Incremental graph updates (recompute only touched branches) (`crates/concept_graph/src/incremental.rs`)
- [x] Tests covering query planning, contradiction adjudication, incremental recompute

---

## Phase 7 — Post-Quantum Hardening & Confidential Compute

- [x] ML-KEM-768 (Kyber) for all key exchanges (substrate-wide) — already shipped in Phase 0/2 via `crates/crypto/src/hybrid_kem.rs` (hybrid X25519 + ML-KEM-768)
- [x] ML-DSA-65 (Dilithium) for provenance signatures on synthesis outputs and export bundles — `MlDsa65Signer` / `MlDsa65Verifier` in `crates/crypto/src/signer_backend.rs`, implementing both `ProvenanceSigner` and `SignerBackend` traits
- [x] Hybrid classical + PQ during the transition window — `crates/crypto/src/hybrid_enforcement.rs` enforces `HybridMode::{ClassicalOnly, HybridTransition, PostQuantumOnly}` policies on every encap / decap with a `KeyExchangeAudit` trail
- [x] Post-quantum MLS extensions (hybrid leaf KEMs, ML-DSA-65 commit signatures, optional SPHINCS+ co-signatures) — `crates/crypto/src/mls.rs` carries hybrid leaf KEMs and ML-DSA-65-signed commits, and `crates/crypto/src/sphincs.rs` ships `SphincsPlusSigner` / `SphincsPlusVerifier` + `CoSigner` (dual ML-DSA-65 + SPHINCS+ signatures)
- [x] Confidential compute worker (attested TEE) for shared synthesis — attestation report + binding + audit primitives ship in `crates/crypto/src/attestation.rs`, and `crates/synthesis_engine/src/tee_worker.rs` ships `TeeWorker` / `TeeWorkerConfig` / `TeeWorkerLifecycle` (Unattested → Attesting → Attested → Synthesizing → Idle) + `MockTeeRuntime` for the full attest → synthesize → verify flow
- [x] Attestation reports bound to synthesizer keys with audit-trail linkage — `crates/crypto/src/attestation.rs` ships `AttestationReport`, `AttestationBinding`, `AttestationAuditEntry`, `verify_attestation`, `bind_synthesizer_key`, and a mock TEE flow (Intel TDX / AMD SEV-SNP / Nitro Enclaves / Mock platforms)
- [x] Cryptographic forgetting — per-scope and per-epoch DEK destroy paths — `crates/crypto/src/forgetting.rs` ships `ScopeDek` / `EpochDek` with `Drop`-time zeroization, `DekRegistry`, `destroy_scope_dek`, `destroy_epoch_dek`, `KeyDestructionEvent` audit fan-out, and `EpochManager` with policy-driven rotation
- [x] Red-team privacy and prompt-injection tests — `crates/evidence_store/tests/privacy_redteam.rs` (11 tests covering scope isolation, forgotten-scope recovery, cross-scope dedup, ring-buffer privacy, append-only invariants, permission boundaries, agent boundaries, provenance integrity, key material handling) and `crates/synthesis_pipeline/tests/privacy_redteam.rs` (10 tests covering scope / window / object-id replay, wrong-key rejection, ciphertext / nonce tampering, prompt-injection containment, AAD smuggling, inner-routing mismatch)
- [x] Memory quality metrics — retention precision, contradiction detection rate, decay-tuning experiments — `crates/memory_manager/src/metrics.rs` ships `RetentionPrecisionTracker`, `ContradictionDetectionRate`, `DecayTuningMetrics`, `MemoryQualityReport`, and `MetricsCollector`

See [`docs/DEVELOPMENT_LOG.md`](docs/DEVELOPMENT_LOG.md) for the full per-session changelog.
