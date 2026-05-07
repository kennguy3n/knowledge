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
| Phase 2: Channel Memory (on-device + shared) | In progress | ~78% (7 of 9) |
| Phase 3: Domain & Tenant Memory (B2B server) | In progress | ~80% (8 of 10) |
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

- [x] Channel Memory Object — recaps, decisions, open questions, active tasks (`crates/memory_manager/src/channel_memory.rs`)
- [x] Synthesis pipeline — window manager, typed synthesis objects, GBNF schema types, no-op synthesizer (recursive SLM summarization stage stubbed pending Bonsai-1.7B integration) (`crates/synthesis_pipeline/`)
- [ ] MLS group keying for shared channel memory (hybrid X25519 + ML-KEM-768 leaf KEMs)
- [x] Encrypted synthesis object publication — XChaCha20-Poly1305 with `(scope_id, window_id, object_id)` AAD binding (`crates/synthesis_pipeline/src/publish.rs`)
- [x] Channel-scoped importance tagging (promote only high-value observations) (`crates/observation_engine/src/promotion.rs`)
- [x] Synthesizer role: elected member device path for small groups (tier / battery / heartbeat eligibility) (`crates/synthesis_pipeline/src/election.rs`)
- [x] Multi-device sync via CRDT for synthesis objects (add-wins + supersession + contradictions) (`crates/sync_engine/`)
- [ ] Provenance bundles signed with ML-DSA-65 on every synthesis output — Phase-2 surface lands as a `ProvenanceBundle` + HMAC-SHA256 `TestSigner`; ML-DSA-65 signer is reserved for Phase 7 (`crates/crypto/src/provenance.rs`)
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
- [ ] Managed AI endpoint as synthesizer for B2B channels / domains — `ManagedEndpointSynthesizer` skeleton ships in `crates/synthesis_engine/`; remote endpoint wiring is reserved for Phase 4 once the Go gateway lands
- [x] Tenant service — tenant lifecycle, per-tenant encryption keys, member provisioning (`crates/tenant_service/`)
- [x] End-to-end tests for the channel → domain → tenant synthesis chain (`crates/synthesis_engine/tests/hierarchy_e2e.rs`)
- [x] Append-only audit log (`crates/audit_service/`) covering canonical promotions, exports, agent proposals, policy changes, member provisioning, tenant lifecycle, key destruction

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
| 2026-05-07 | **Phase 3 — Domain & Tenant Memory, first delivery.** Landed eight of the ten Phase-3 deliverables across four new crates and three extensions, plus the append-only audit-log surface called out in `ARCHITECTURE.md` §4.1. New crate `crates/permission_service`: Zanzibar-style relation tuples (`ObjectRef`, `Relation`, `SubjectRef`) with the ten object types from `ARCHITECTURE.md` §6 (Tenant / Domain / Channel / User / Device / Concept / Summary / Workflow / ExportProfile / Agent), the seven-relation namespace (`owner`, `admin`, `editor`, `member`, `synthesizer`, `viewer`, `proposer`) with default inheritance (`owner ⇒ admin ⇒ editor ⇒ member ⇒ viewer`), an in-memory `TupleStore`, and a `check_permission` reachability query that walks both direct tuples and userset rewrites. New crate `crates/tenant_service`: `Tenant` / `TenantConfig` / `TenantMember` data model, the `Active / Suspended / Deleted` lifecycle state machine with key-destruction reference on delete, role-based member provisioning, and config validation (storage caps, sub-minute synthesis windows). New crate `crates/synthesis_engine`: server-side synthesis-engine skeleton with the `SynthesisEngine` trait (`synthesize_domain`, `synthesize_tenant`) and a `ManagedEndpointSynthesizer` deterministic stub that the future managed-AI endpoint will replace. New crate `crates/audit_service`: append-only `AuditLog` with `AuditEntryBuilder`, `AuditQuery` (scope / action / actor / time-range filters), and the eight `AuditActionType`s (`CanonicalPromotion / Export / AgentProposal / PolicyChange / MemberProvisioned / MemberRemoved / TenantLifecycle / KeyDestruction`). Extended `crates/memory_manager` with `DomainMemoryObject` (workstreams, dependencies, risks, procedures, registered channel scopes, archive-on-decay sweep) and `TenantMemoryObject` (canonical policies, product taxonomy, stable org knowledge, admitted approved-document refs; tenant items default to `SensitivityClass::Critical` with no passive decay — only explicit deprecation per `PROPOSAL.md` §4.3). Extended `crates/synthesis_pipeline` with `crates/synthesis_pipeline/src/hierarchy.rs`: `ChannelOutput` (only constructible from `ChannelRecap` synthesis objects), `DomainOutput` (only from `DomainSummary`), `DomainSynthesisInput` (only constructible from `ChannelOutput`s registered on a `DomainMemoryObject`; raw `ChannelMemoryObject` rejected at the type level), `TenantSynthesisInput` (only constructible from `DomainOutput`s registered on a `TenantMemoryObject` plus `ApprovedDocument`s admitted to that tenant; channel-tier objects rejected at the type level), `WindowScopeTier` (`Channel / Domain / Tenant`), and a `HierarchyEnforcedWindowManager` blanket impl on `SynthesisWindowManager` whose `validate_domain_input` / `validate_tenant_input` refuse cross-tier admission. Extended `crates/concept_graph` with `PersistentConceptGraph`: SQLCipher-backed wrapper over the Phase-2 in-memory adjacency list, `concept_nodes` + `concept_edges` schema with plaintext lifecycle / relation tags for scope-filtered queries, and per-scope AEAD encryption (`scope:{uuid}:concept:v1` HKDF context) of the JSON-encoded node/edge payloads with `(scope_id, id)` bound into the AAD. Test coverage added: 18 permission-service tests across tuple CRUD, reachability over inheritance chains, transitive userset rewrites, and negative cases; 10 tenant-service tests covering the lifecycle state machine + member provisioning + config validation; 6 concept-graph persistence tests covering encrypted round-trip, scope filtering, supersession persistence, and reload-after-restart; 8 synthesis-engine tests covering the trait contract + window completion + hierarchy validation; 9 audit-service tests covering append-only invariants, scope / action / actor / time filters, and chronological ordering; 10 domain-memory tests + 11 tenant-memory tests covering CRUD, lifecycle, archive sweeps, no-passive-decay enforcement, and explicit deprecation; and a 460-line end-to-end test (`crates/synthesis_engine/tests/hierarchy_e2e.rs`) that wires `memory_manager`, `synthesis_pipeline`, `synthesis_engine`, `concept_graph`, `permission_service`, `audit_service`, and `crypto` together: it builds the channel → domain → tenant synthesis chain, signs a provenance bundle at every tier, persists the tenant summary as a canonical concept through the encrypted graph, appends audit entries with sequence + scope filtering, and asserts the cross-tier permission gates (outsider blocked at tenant, channel member blocked at tenant admin, admin reaches every tier through the userset-rewrite chain). The two remaining Phase-3 items — the Go-side gateway in front of the `synthesis_engine` and the production managed-AI endpoint adapter — are intentionally still open and tracked under Phase 4. `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all` all pass against the new surface. |
| 2026-05-07 | **Phase 2 — Channel Memory, first delivery.** Landed seven of the nine Phase-2 deliverables across four new crates and three extensions. New crate `crates/concept_graph`: sparse typed concept graph per `PROPOSAL.md` §3.3 / `ARCHITECTURE.md` §2.1 with `ConceptNode` (id, label, definition, scope_id, state, created_at, updated_at, metadata), seven typed relations (`IsA`, `PartOf`, `DecidedBy`, `Supersedes`, `Contradicts`, `DerivedFrom`, `AssignedTo`), in-memory `HashMap`-backed adjacency, scope-aware add / remove / supersede / mark-contradiction / typed-edge traversal. New crate `crates/synthesis_pipeline`: per-scope `SynthesisWindowManager` with `Pending / InProgress / Complete / Failed` status machine, typed `SynthesisObject` (`EpisodicSummary / ChannelRecap / DomainSummary / TenantSummary`), GBNF grammar types (`ImportanceTag`, `EntityList`, `ObservationRow`, `SummaryBundle`) per `ARCHITECTURE.md` §3.5, `SynthesisPipeline` trait with a `NoOpSynthesizer` for end-to-end wiring tests, encrypted `publish_synthesis_object` / `consume_synthesis_object` with `(scope_id, window_id, object_id)` bound into the AEAD AAD, and a `SynthesizerElection` skeleton (tier / battery > 20 % / heartbeat-TTL eligibility, election by tier-then-recency, voluntary `step_down`, re-election on offline). Extended `crates/memory_manager` with `ChannelMemoryObject` (recap, decisions, open questions, active tasks; reuses `MemoryObject` for individual items; archive-on-decay sweep). Extended `crates/crypto` with the PROV data model (`ProvenanceBundle`, `ProvenanceSigner` trait, `TestSigner` via HMAC-SHA256; ML-DSA-65 signer slot reserved for Phase 7). Fleshed out `crates/sync_engine` from a stub into a real add-wins observed-remove CRDT (`AddWinsSet<T>`) plus an append-only `OpLog<T>` with `Add / Remove / Supersede` ops and `merge_logs` producing deterministic merged state. Extended `crates/observation_engine` with a `ChannelPromotionPolicy` (min importance class, min corroboration count, max noise ratio) and hardened the `LexiconExtractor` with URL / email / date-time-ref / numeric-ref / question detection (sentences ending in `?` or starting with an interrogative word are now `Question` observations). Updated `Cargo.toml` workspace members and `[lints]` blocks across the new crates. Test coverage added: 11 promotion tests, 22 extraction edge-case tests (empty / whitespace-only / >10 KB / URL-only / mention-only / Unicode + emoji / multi-line / mixed-case / dedup / interrogative + question-mark detection), 14 graph tests, 20+ synthesis-pipeline tests across window / object / schema / pipeline / publish / election, 12 CRDT tests covering add-wins semantics + merge commutativity + idempotency + supersession contradictions, 10 channel-memory tests covering CRUD + lifecycle + decay-sweep, and 8 provenance tests covering signing round-trip + tamper detection + wrong-key rejection. The two remaining Phase-2 items (MLS group keying for the shared-channel keying path and the ML-DSA-65 signer for the provenance plane) depend on Phase-7 PQ hardening and are intentionally still open. |
| 2026-05-07 | **Phase 1 — Personal Memory, first delivery.** Two evidence-plane bug fixes that were holding back cross-scope reads and ring-buffer telemetry: (1) `body_store` rows are now encrypted with a scope-independent key derived from the master key with the context label `b"body_store:v1"`, so a body inserted from scope A and dedup-shared into scope B can be decrypted from either evidence row; (2) `ring_buffer.created_at` now carries Unix epoch seconds (matching `RingBufferEntry.created_at`'s documented unit and the `evidence` table's `created_at`), instead of microseconds. New crate `crates/memory_manager`: the seven-state decay machine (`Candidate / Reinforced / Consolidated / Canonical / Superseded / Archived / Deleted`) per `ARCHITECTURE.md` §7, a `MemoryObject` carrying retention + sensitivity (`Critical / Important / Useful / Noise`) bookkeeping, weighted retention scoring (pinning, retrieval frequency, cross-source corroboration, contradiction signals, age, non-use) per `PROPOSAL.md` §4.2 with per-class half-lives and a hard pinning floor (`>=0.9`), a decay sweep (low-score Candidate → Archived; TTL-elapsed Superseded → Archived), a bounded TTL-evicting `WorkingMemory` window, a `UserMemoryObject` CRUD wrapper (read / pin / unpin / forget / list / decay sweep) that auto-promotes pinned Candidates, and a `PrivacyStrip` + `SynthesisOutput<T>` pair whose only public constructor demands a strip — encoding compute location, model name + version, egress bytes, and data scope per Phase-1 §1. New crate `crates/observation_engine`: an `ObservationExtractor` trait with a Phase-1 `LexiconExtractor` baseline (capitalised words + `@mentions` + `#tags` as entities, action verbs / `TODO` / `ACTION` / `TASK` as tasks, `decided` / `agreed` / `approved` as decisions, declarative sentences as facts) and an `ObservationPipeline` that chains extraction → reuse of the evidence-plane `ImportanceClassifier` → Candidate observation creation. New module `crates/evidence_store/src/retrieval.rs`: a `HybridRetriever` exposing `search_fts` (FTS5 lexical), `search_recency` (created-at decay), and `search_hybrid` (weighted fan-in over FTS5, recency, and a stubbed `0.0` semantic-vector score until XLM-R lands). 72 new tests cover every valid + invalid state-machine transition, pinning + decay scenarios, working-memory TTL eviction, the privacy-strip type-system invariant, lexicon extraction over entity / task / decision / fact inputs, end-to-end pipeline runs, retention scoring extremes, hybrid retrieval ordering with custom weights, and end-to-end memory + observation lifecycles. `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all` (125 tests) all pass; the workspace clippy config relaxes `cast_precision_loss` and `float_cmp` (consistent with the existing cast allowances) to support the f64-based scoring path. |
| 2026-05-07 | **Phase 0 — Foundation, first delivery.** Landed the Cargo workspace at the repo root with three library crates (`crates/crypto`, `crates/evidence_store`, `crates/sync_engine`). Implemented the post-quantum `crypto` crate: BLAKE3 content hashing, XChaCha20-Poly1305 AEAD, HKDF-SHA256 key derivation, and a hybrid X25519 + ML-KEM-768 KEM with a concatenate-then-KDF combiner — both halves real, ML-KEM-768 via the RustCrypto `ml-kem` crate behind a swappable `KemBackend` trait. Implemented the SQLCipher-backed `evidence_store` crate: schema with `evidence` (append-only via triggers), `body_store` (BLAKE3-keyed dedup with `ref_count`), `ring_buffer` (FIFO eviction at a configurable cap, default 5 MB), and an `evidence_fts` FTS5 virtual table with the substrate-canonical `unicode61 remove_diacritics 2` tokenizer. Built the size-threshold storage router (inline `≤ 512 B`, body-table `> 512 B`, ring-buffer for noise) and the per-scope-keyed AEAD ingestion pipeline. Added a lexicon-only `ImportanceClassifier` trait + `LexiconClassifier` Phase-0 fallback with a configurable lexicon (default English chat lexicon). Added a 53-test suite covering hashing, AEAD round-trip + tamper-detection, KDF determinism, hybrid KEM round-trip, schema bootstrap, ingestion round-trip on every storage path, content-hash dedup ref-count, ring-buffer FIFO eviction + size-cap enforcement, FTS5 indexing + search, classifier behaviour, and the append-only invariant on `evidence`. Added `.github/workflows/ci.yml` running `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo build --all-targets`, and `cargo test --all` with cargo caching. Remaining Phase 0 items (SLM-backed importance classifier via Bonsai-1.7B, iOS / Android / macOS / Windows platform bindings) are out of scope for this delivery and tracked above. |
| 2026-05-06 | **Initial project setup.** Replaced the empty `README.md` and added `PROPOSAL.md`, `ARCHITECTURE.md`, `PHASES.md`, and `PROGRESS.md`. Documents define the privacy-first continual knowledge / context substrate (B2C + B2B over one platform), the layered six-plane substrate (evidence → observation → semantic → reasoning → export → action), the six-stage memory model with decay state machine, the strict knowledge hierarchy (`user → community → channel` for B2C, `user → domain → channel` per tenant for B2B), the on-device model strategy referencing [`kennguy3n/slm-chat-demo`](https://github.com/kennguy3n/slm-chat-demo) (Bonsai-1.7B + XLM-R + device tiering), the modified llama.cpp inference runtime via [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism), the post-quantum crypto layer (hybrid X25519 + ML-KEM-768 KEM, ML-DSA-65 + SPHINCS+ signatures, post-quantum MLS), the connector inventory (Google Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, email), the three deployment modes (local-only / enterprise server-side / confidential-compute hybrid), the Zanzibar-style relation graph + cryptographic capability permission model with proposal-only agent writes, and the seven-phase delivery plan with a 30 / 60 / 90-day implementation timeline targeting Phases 0 → 2. No implementation work has shipped yet — development begins with Phase 0. |
