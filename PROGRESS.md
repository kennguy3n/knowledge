# Knowledge — Progress Tracker

Last updated: 2026-05-07

This tracker captures per-phase deliverable status and a
chronological changelog for the Knowledge substrate. Phase scope
is defined in [PHASES.md](./PHASES.md); architectural detail
lives in [ARCHITECTURE.md](./ARCHITECTURE.md); the product thesis
lives in [PROPOSAL.md](./PROPOSAL.md).

Phase 0 is largely in place (Rust workspace skeleton, the
post-quantum `crypto` crate, the SQLCipher-backed `evidence_store`,
the lexicon-only importance classifier, the unit + integration test
suite, and the CI pipeline). What remains in Phase 0 is the
SLM-backed importance classifier (Bonsai-1.7B) and the
iOS / Android / macOS / Windows platform bindings.

Phase 1 is in active development. The on-device personal-memory
plane has landed: the `memory_manager` crate (decay state machine,
retention scoring, working memory, user-memory CRUD, privacy-strip
invariant), the `observation_engine` crate (lexicon-first
extractor + importance pipeline), and a hybrid retrieval module
in `evidence_store` (FTS5 + recency + stub vector). The two
remaining Phase 1 items — episodic summarisation via Bonsai-1.7B
and XLM-R embeddings via a shared ONNX artifact — depend on the
SLM / ONNX runtime work tracked under Phase 0 and a separate model
deliverable.

---

## Overall status summary

| Phase | Status | Progress |
|-------|--------|----------|
| Phase 0: Foundation | In progress | ~64% (7 of 11) |
| Phase 1: Personal Memory (on-device) | In progress | ~82% (9 of 11) |
| Phase 2: Channel Memory (on-device + shared) | Not started | 0% |
| Phase 3: Domain & Tenant Memory (B2B server) | Not started | 0% |
| Phase 4: Connector Integration (on-server) | Not started | 0% |
| Phase 5: Portable Concept Profiles & Export | Not started | 0% |
| Phase 6: Graph UX & Advanced Reasoning | Not started | 0% |
| Phase 7: Post-Quantum Hardening & Confidential Compute | Not started | 0% |

---

## Phase 0 — Foundation

- [x] Rust shared core skeleton — `evidence_store`, `crypto`, `sync_engine` modules
- [x] SQLCipher local store with post-quantum key derivation (hybrid X25519 + ML-KEM-768 unwrap)
- [x] Evidence plane — append-only encrypted message / file / chunk ingestion
- [x] Content-hash deduplication (BLAKE3)
- [ ] Basic on-device importance classifier via Bonsai-1.7B + the shared `llama-server` from the PrismML [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism) fork
- [x] Lexicon-only fallback when the SLM is not available
- [ ] iOS framework binding (UniFFI `.xcframework`)
- [ ] Android JNI binding (`.so` per ABI)
- [ ] macOS / Windows N-API addon binding
- [x] Unit test suite covering `evidence_store`, `crypto`, importance classifier
- [x] CI: lint + unit tests + multi-target build

---

## Phase 1 — Personal Memory (on-device)

- [x] Memory manager with full decay state machine (candidate → reinforced → consolidated → canonical → superseded / archived / deleted)
- [x] Observation engine — entity extraction, fact extraction, task / decision detection
- [x] Lexicon → XLM-R → SLM-assisted observation pipeline (lexicon stage shipped; XLM-R and SLM-assisted stages stubbed pending ONNX + Bonsai-1.7B integration)
- [ ] Episodic memory — session / thread summaries via on-device Bonsai-1.7B
- [x] Working memory — current context window management with TTL eviction
- [ ] XLM-R embeddings via shared ONNX artifact (INT8 ~107 MB / INT4 ~55 MB)
- [x] Hybrid retrieval — FTS5 + semantic vector + recency (FTS5 + recency shipped; semantic-vector component stubbed at `0.0` until XLM-R lands)
- [x] Retention scoring — pinning, retrieval frequency, age, non-use
- [x] User Memory Object CRUD (read / pin / unpin / forget) — Rust data-model + sweep are shipped; the FFI surface is delivered alongside the Phase-0 platform bindings
- [x] Privacy strip on every synthesis output (compute, model, egress)
- [x] Decay state machine + observation pipeline + retrieval tests

---

## Phase 2 — Channel Memory (on-device + shared)

- [ ] Channel Memory Object — recaps, decisions, open questions, active tasks
- [ ] Synthesis pipeline — recursive summarization per channel window with grammar-constrained decoding
- [ ] MLS group keying for shared channel memory (hybrid X25519 + ML-KEM-768 leaf KEMs)
- [ ] Encrypted synthesis object publication
- [ ] Channel-scoped importance tagging (promote only high-value observations)
- [ ] Synthesizer role: elected member device path for small groups
- [ ] Multi-device sync via CRDT for synthesis objects (add-wins + supersession + contradictions)
- [ ] Provenance bundles signed with ML-DSA-65 on every synthesis output
- [ ] End-to-end tests for the elected-device synthesis path

---

## Phase 3 — Domain & Tenant Memory (B2B server)

- [ ] Domain Memory Object — cross-channel workstreams, dependencies, risks, procedures
- [ ] Tenant Memory Object — canonical policy, product taxonomy, stable org knowledge
- [ ] Server-side synthesis service (Go gateway + Rust synthesis engine)
- [ ] Domain synthesis consumes channel memory objects only (not raw messages)
- [ ] Tenant synthesis consumes domain objects + approved official docs
- [ ] Sparse concept graph — typed relations, contradictions, drift markers
- [ ] Permission service (Zanzibar-style relation graph) with reachability checks
- [ ] Managed AI endpoint as synthesizer for B2B channels / domains
- [ ] Tenant service — tenant lifecycle, per-tenant encryption keys, member provisioning
- [ ] End-to-end tests for the channel → domain → tenant synthesis chain

---

## Phase 4 — Connector Integration (on-server)

- [ ] Connector framework (OAuth2 token vault, refresh, incremental delta sync, webhooks)
- [ ] Google Drive connector
- [ ] OneDrive connector
- [ ] Notion connector
- [ ] Jira connector
- [ ] Confluence connector
- [ ] Figma connector (design system extraction)
- [ ] HubSpot connector (CRM context)
- [ ] Channel-scoped connector attachment
- [ ] ACL sync from source systems into the relation graph
- [ ] Observation extraction pipeline for documents (chunking, importance tagging, entity / topic extraction)
- [ ] Citation rendering with stable links back to source documents
- [ ] Connector-specific integration tests against vendor fixtures

---

## Phase 5 — Portable Concept Profiles & Export

- [ ] Portable concept profile — approved concepts, constraints, reasoning for a specific external context
- [ ] Export plane — least-privilege views; no raw document export by default
- [ ] Agent write contract — proposal-only API (`propose_observation`, `propose_concept`, `propose_relation`, `propose_summary`)
- [ ] Agent proposal schema (scope, provenance bundle, evidence refs, confidence, sensitivity, TTL, supersedes / contradicts, agent identity + model version, skill / recipe id)
- [ ] Export controls per concept / summary / workflow with policy preview
- [ ] Audit trail for all exports + agent proposals + canonical promotions
- [ ] Policy simulator (preview what an export would contain)
- [ ] End-to-end tests for the export plane and the agent proposal lifecycle

---

## Phase 6 — Graph UX & Advanced Reasoning

- [ ] Concept graph visualization (Kanvas-style exploration with scope filters)
- [ ] Contradiction and drift detection (explicit `contradicts` edges + adjudication workflow)
- [ ] Multi-hop reasoning over the concept graph (typed-edge traversal with budgets)
- [ ] Workflow memory — successful reasoning traces / tool-use patterns saved to the reasoning plane
- [ ] Graph-of-Thought reasoning for complex queries
- [ ] Query planner — cheapest-first routing (summary → graph → raw evidence) with explicit fallbacks
- [ ] Community summaries (GraphRAG-style bottom-up over reachable scopes)
- [ ] Incremental graph updates (recompute only touched branches)
- [ ] Tests covering query planning, contradiction adjudication, incremental recompute

---

## Phase 7 — Post-Quantum Hardening & Confidential Compute

- [ ] ML-KEM-768 (Kyber) for all key exchanges (substrate-wide)
- [ ] ML-DSA-65 (Dilithium) for provenance signatures on synthesis outputs and export bundles
- [ ] Hybrid classical + PQ during the transition window
- [ ] Post-quantum MLS extensions (hybrid leaf KEMs, ML-DSA-65 commit signatures, optional SPHINCS+ co-signatures)
- [ ] Confidential compute worker (attested TEE) for shared synthesis
- [ ] Attestation reports bound to synthesizer keys with audit-trail linkage
- [ ] Cryptographic forgetting — per-scope and per-epoch DEK destroy paths
- [ ] Red-team privacy and prompt-injection tests
- [ ] Memory quality metrics — retention precision, contradiction detection rate, decay-tuning experiments

---

## Changelog

| Date | Change |
|------|--------|
| 2026-05-07 | **Phase 1 — Personal Memory, first delivery.** Two evidence-plane bug fixes that were holding back cross-scope reads and ring-buffer telemetry: (1) `body_store` rows are now encrypted with a scope-independent key derived from the master key with the context label `b"body_store:v1"`, so a body inserted from scope A and dedup-shared into scope B can be decrypted from either evidence row; (2) `ring_buffer.created_at` now carries Unix epoch seconds (matching `RingBufferEntry.created_at`'s documented unit and the `evidence` table's `created_at`), instead of microseconds. New crate `crates/memory_manager`: the seven-state decay machine (`Candidate / Reinforced / Consolidated / Canonical / Superseded / Archived / Deleted`) per `ARCHITECTURE.md` §7, a `MemoryObject` carrying retention + sensitivity (`Critical / Important / Useful / Noise`) bookkeeping, weighted retention scoring (pinning, retrieval frequency, cross-source corroboration, contradiction signals, age, non-use) per `PROPOSAL.md` §4.2 with per-class half-lives and a hard pinning floor (`>=0.9`), a decay sweep (low-score Candidate → Archived; TTL-elapsed Superseded → Archived), a bounded TTL-evicting `WorkingMemory` window, a `UserMemoryObject` CRUD wrapper (read / pin / unpin / forget / list / decay sweep) that auto-promotes pinned Candidates, and a `PrivacyStrip` + `SynthesisOutput<T>` pair whose only public constructor demands a strip — encoding compute location, model name + version, egress bytes, and data scope per Phase-1 §1. New crate `crates/observation_engine`: an `ObservationExtractor` trait with a Phase-1 `LexiconExtractor` baseline (capitalised words + `@mentions` + `#tags` as entities, action verbs / `TODO` / `ACTION` / `TASK` as tasks, `decided` / `agreed` / `approved` as decisions, declarative sentences as facts) and an `ObservationPipeline` that chains extraction → reuse of the evidence-plane `ImportanceClassifier` → Candidate observation creation. New module `crates/evidence_store/src/retrieval.rs`: a `HybridRetriever` exposing `search_fts` (FTS5 lexical), `search_recency` (created-at decay), and `search_hybrid` (weighted fan-in over FTS5, recency, and a stubbed `0.0` semantic-vector score until XLM-R lands). 72 new tests cover every valid + invalid state-machine transition, pinning + decay scenarios, working-memory TTL eviction, the privacy-strip type-system invariant, lexicon extraction over entity / task / decision / fact inputs, end-to-end pipeline runs, retention scoring extremes, hybrid retrieval ordering with custom weights, and end-to-end memory + observation lifecycles. `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all` (125 tests) all pass; the workspace clippy config relaxes `cast_precision_loss` and `float_cmp` (consistent with the existing cast allowances) to support the f64-based scoring path. |
| 2026-05-07 | **Phase 0 — Foundation, first delivery.** Landed the Cargo workspace at the repo root with three library crates (`crates/crypto`, `crates/evidence_store`, `crates/sync_engine`). Implemented the post-quantum `crypto` crate: BLAKE3 content hashing, XChaCha20-Poly1305 AEAD, HKDF-SHA256 key derivation, and a hybrid X25519 + ML-KEM-768 KEM with a concatenate-then-KDF combiner — both halves real, ML-KEM-768 via the RustCrypto `ml-kem` crate behind a swappable `KemBackend` trait. Implemented the SQLCipher-backed `evidence_store` crate: schema with `evidence` (append-only via triggers), `body_store` (BLAKE3-keyed dedup with `ref_count`), `ring_buffer` (FIFO eviction at a configurable cap, default 5 MB), and an `evidence_fts` FTS5 virtual table with the substrate-canonical `unicode61 remove_diacritics 2` tokenizer. Built the size-threshold storage router (inline `≤ 512 B`, body-table `> 512 B`, ring-buffer for noise) and the per-scope-keyed AEAD ingestion pipeline. Added a lexicon-only `ImportanceClassifier` trait + `LexiconClassifier` Phase-0 fallback with a configurable lexicon (default English chat lexicon). Added a 53-test suite covering hashing, AEAD round-trip + tamper-detection, KDF determinism, hybrid KEM round-trip, schema bootstrap, ingestion round-trip on every storage path, content-hash dedup ref-count, ring-buffer FIFO eviction + size-cap enforcement, FTS5 indexing + search, classifier behaviour, and the append-only invariant on `evidence`. Added `.github/workflows/ci.yml` running `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo build --all-targets`, and `cargo test --all` with cargo caching. Remaining Phase 0 items (SLM-backed importance classifier via Bonsai-1.7B, iOS / Android / macOS / Windows platform bindings) are out of scope for this delivery and tracked above. |
| 2026-05-06 | **Initial project setup.** Replaced the empty `README.md` and added `PROPOSAL.md`, `ARCHITECTURE.md`, `PHASES.md`, and `PROGRESS.md`. Documents define the privacy-first continual knowledge / context substrate (B2C + B2B over one platform), the layered six-plane substrate (evidence → observation → semantic → reasoning → export → action), the six-stage memory model with decay state machine, the strict knowledge hierarchy (`user → community → channel` for B2C, `user → domain → channel` per tenant for B2B), the on-device model strategy referencing [`kennguy3n/slm-chat-demo`](https://github.com/kennguy3n/slm-chat-demo) (Bonsai-1.7B + XLM-R + device tiering), the modified llama.cpp inference runtime via [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism), the post-quantum crypto layer (hybrid X25519 + ML-KEM-768 KEM, ML-DSA-65 + SPHINCS+ signatures, post-quantum MLS), the connector inventory (Google Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, email), the three deployment modes (local-only / enterprise server-side / confidential-compute hybrid), the Zanzibar-style relation graph + cryptographic capability permission model with proposal-only agent writes, and the seven-phase delivery plan with a 30 / 60 / 90-day implementation timeline targeting Phases 0 → 2. No implementation work has shipped yet — development begins with Phase 0. |
