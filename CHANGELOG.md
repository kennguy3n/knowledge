# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Multi-device sync transport + untrusted relay.** Beyond the add-wins
  CRDT merge math and delta serialization/compaction/snapshot bootstrap,
  the transport layer now ships: a `SyncTransport` trait and a
  `SyncClient` push/pull API that seals every delta with **per-scope
  XChaCha20-Poly1305 AEAD**, plus a new `sync_relay` crate (axum,
  bearer-token auth, per-tenant isolation) that only ever stores **opaque
  ciphertext**. A ≥3-replica integration test exchanges deltas through a
  real relay across offline/partition scenarios and asserts deterministic
  convergence and that the relay sees only ciphertext.
  (`crates/sync_engine/src/transport.rs`, `crates/sync_relay/`.) Current
  limitations: the relay's `BlobStore` is an in-memory reference
  implementation (production backs it with durable/replicated storage; TLS
  terminates at ingress); `SyncClient` is a library-level capability not
  plumbed into the host-app lifecycle/FFI today; and post-quantum key
  establishment for cross-device key transport is not the live path today
  (scope keys are distributed out of band).
- **Reasoning plane surfaced end-to-end.** The `reasoning_engine`
  (contradiction detection, drift detection, multi-hop explain) is now
  reachable through the product: FFI `reasoning_contradictions` /
  `reasoning_drift` / `reasoning_explain_query` → substrate `/reasoning/*`
  → gateway `POST /api/v1/reasoning/{contradictions,drift,explain}` → a
  reference UI panel in `apps/knowledge-ui/`. Scans are scope-isolated and
  bounded (256-node cap). (`crates/reasoning_engine/`,
  `crates/ffi/src/reasoning.rs`, `server/`.)
- **On-device NPU / ANE inference adapters.** The inference routing chain
  is now **Core ML/ANE → ONNX Runtime → MLX → llama.cpp → managed-cloud →
  fallback**. Two feature-gated accelerator adapters share an
  `AcceleratorAdapter<C>` core with **zero native build dependencies** (the
  runtime is injected) and capability detection + graceful fallback:
  `CoreMlAdapter` (Apple Neural Engine, `coreml` feature) and
  `OnnxRuntimeAdapter` (ONNX Runtime Mobile + NPU EP — NNAPI / QNN-Hexagon
  on Android, Core ML EP on iOS; `onnx-runtime` feature).
  (`crates/inference_router/src/adapters/`.) See
  [docs/technical/inference-routing.md](docs/technical/inference-routing.md).
- **Synthesis quality eval harness + public multilingual leaderboard.**
  `demos/synthesis-eval/` and `crates/synthesis_pipeline/src/eval.rs` grade
  recaps with three deterministic, GPU-free scorers — term coverage,
  faithfulness/grounding (flags ungrounded entities), and in-language (a
  Unicode-script detector) — and gate regressions in CI.
  `demos/synthesis-eval/leaderboard.py` rolls the scorers up per language
  with a 1.7B-vs-4B model-tier comparison and a `--check` byte-for-byte CI
  gate; languages with no recorded run are listed as `pending`. The honest
  current state: the default Bonsai-1.7B Q2_0 has weak term coverage on
  several languages and fails in-language on some CJK/Arabic recaps, so the
  opt-in 4B model is the recommended default for non-Latin deployments.
  Docs: [docs/technical/synthesis-eval.md](docs/technical/synthesis-eval.md),
  [docs/technical/multilingual-leaderboard.md](docs/technical/multilingual-leaderboard.md).
- **Connector maturity labels + liveness harness.** Connector maturity is
  now an explicit `ConnectorMaturity { Unstable, ContractStable,
  LiveVerified }` enum
  ([`crates/connector_framework/src/config.rs`](crates/connector_framework/src/config.rs)).
  Most of the 140 built-in connectors are `contract-stable`; **5 exemplars
  (GitHub, Slack, Notion, MoMo, Stripe) are `live-verified`** via a
  cassette/VCR replay harness (`ReplayTransport` / `RecordingTransport`,
  which auto-redacts secrets) plus a weekly live workflow. See
  [docs/guides/add-a-connector.md](docs/guides/add-a-connector.md).
- **Portable device benchmark matrix.** A one-command benchmark target
  measures the real `evidence_store` / `HybridRetriever` path (ingest, FTS
  p50/p95, hybrid retrieval, peak RSS).
  [docs/technical/benchmarks-device.md](docs/technical/benchmarks-device.md)
  has the Linux row filled and other device rows marked
  `[pending real-device measurement]` — reproducible and honest rather than
  estimated.
- **Post-quantum threat-model whitepaper.**
  [docs/security/pqc-threat-model.md](docs/security/pqc-threat-model.md) is
  a code-grounded whitepaper: primitive inventory (ML-KEM-768, ML-DSA-65,
  XChaCha20-Poly1305, HKDF), HNDL + hybrid KEM, the key hierarchy and DEK
  cryptographic forgetting, residual risks / side-channels, a
  HIPAA/SOX/FERPA mapping, and an external-review checklist. Primitives
  live in `crates/crypto` (the hybrid KEM is exercised via `StubKemBackend`
  in unit tests; real platform attestation is mock/stub today).

- **Memory UI write path + server concept graph.** The Memory page now
  exposes an "Add a memory" form (observation type / content /
  sensitivity) that writes a user-memory observation through
  `POST /api/v1/memories` and refreshes the list and concept graph on
  success, plus per-row Pin/Unpin controls
  (`POST /api/v1/memories/{id}/pin|unpin`). The concept-graph section
  renders the substrate-projected graph from
  `GET /api/v1/memories/concept-graph`, falling back to the
  client-derived graph only when that route is unavailable. Honest
  empty-states are preserved when a scope genuinely has no memory.

- **Concept graph projected from live user-memory, with an end-to-end
  read path.** New `ffi::get_concept_graph(handle, scope_id) -> GraphView`
  derives the per-scope concept graph from the scope's live user-memory
  observations at read time (`concept_graph::project_memory_graph`)
  instead of a separately-persisted store, so the graph can never
  disagree with memory and needs no extra sync. Decayed/archived memories
  surface as `Superseded` nodes so the graph tracks the decay state
  machine. Exposed over HTTP as `GET /concept_graph/{scope_id}` on the
  substrate server, via the Go substrate client `ConceptGraph` method, and
  on the gateway as `GET /api/v1/memories/concept-graph?scope_id=…`. An
  N-API binding `getConceptGraph` mirrors it for desktop hosts. Empty or
  cryptographically-forgotten scopes yield an empty graph (`200` with
  empty `nodes`/`edges`), never `404`.

- **Opt-in Bonsai-4B Q2_0 synthesis upgrade path (prep-only; 1.7B stays
  the default).** `scripts/download-models.sh` gains a `--include-4b` flag
  (and `INCLUDE_4B=1` env) that additionally fetches the optional
  `bonsai-4b.gguf` GGUF and `bonsai-4b-mlx/` MLX directory; a plain run is
  unchanged and never touches the 4B artifacts. `deploy/Dockerfile.llama-server`
  documents building a 4B image via the existing `MODEL_URL` / `MODEL_SHA256`
  build-args, and `deploy/model-artifacts/{README.md,SHA256SUMS}` document
  the artifacts with **unpinned** checksums (the 4B artifact may not be
  published for a release yet — pin on release). Server-side / High-tier
  deployments may select 4B via image build-arg, runtime bind-mount, or
  `KNOWLEDGE_SLM_MODEL_PATH`; on-device Low/Medium tiers stay on 1.7B. The
  inference-router adapter contract is unchanged (output shape is
  GBNF-grammar-guaranteed regardless of model size). See
  `docs/technical/inference-routing.md` ("Model size: 1.7B default,
  optional 4B upgrade").

- **Deterministic, tunable on-device synthesis sampling
  (`SamplingConfig`).** Every llama.cpp `/completion` and managed-cloud
  `/chat/completions` request now carries an explicit `seed` plus the
  full sampling parameter set (`temperature`, `top_k`, `top_p`,
  `min_p`, `repeat_penalty`, `n_predict`). Previously the request body
  omitted `seed`, so with `llama-server`'s default (`-1`) an identical
  `(model, prompt)` pair drew a fresh sample every call — the root
  cause of synthesis producing a clean briefing one run and rambling
  meta-commentary the next. The new `SamplingConfig::synthesis_default()`
  preset is greedy + fixed-seed (byte-reproducible); every field is
  overridable via a `KNOWLEDGE_SLM_*` environment variable. The
  `LlamaCppAdapter` threads its `RouterConfig::sampling` onto every call
  (and `ManagedCloudAdapter::with_sampling` does the same for the
  managed path), so a programmatic `RouterConfig::with_sampling`
  override actually reaches the request body rather than being silently
  dropped. See
  [docs/technical/inference-routing.md](docs/technical/inference-routing.md#deterministic-sampling).

- **Synthesis quality hardening: prompt, verify-and-retry validator,
  adaptive budget, and metrics.** The `SynthSummary` prompt now leads
  with a hard "output only the JSON object" instruction and a single
  one-shot exemplar to steer the 2-bit model away from meta-commentary
  prefaces (`"The session highlights…"`). After parsing, the
  `LlamaCppSynthesizer` runs a deterministic
  [`score_bundle`](crates/synthesis_pipeline/src/quality.rs) quality
  check — flagging a recap that opens with a known meta-commentary
  phrase, is shorter than a minimum length, or ignores the evidence's
  salient terms — and, when flagged, retries **once** with a larger
  token budget and a fact-only suffix, keeping whichever attempt scores
  better by the same function. The first-attempt `n_predict` budget now
  scales with observation-row count (`adaptive_budget`, floored at the
  historical 512 and bounded under the synthesis deadline; the retry
  budget adds a fixed bonus and is then hard-**capped** at
  `RETRY_N_PREDICT` for every input so a retry can never breach the
  deadline). New `synthesis_pipeline::SynthesisMetrics` exposes
  `synthesis_retry_total`, `synthesis_retry_failed_total`,
  `synthesis_lowquality_total`, `synthesis_truncated_total`, and a
  recap-length signal via `LlamaCppSynthesizer::metrics_snapshot()`.
  `synthesis_retry_failed_total` makes the graceful-degradation path —
  a retry that *errors* and so keeps the first bundle — observable
  rather than silent. The GBNF shape contract and
  the `SummaryBundle::from_slm_str` salvage parser are unchanged. The
  quality logic is exposed as a pure, evidence-agnostic orchestration
  (`quality::salient_terms_from_texts` / `score_bundle_with_terms` /
  `verify_and_retry`) so a single scoring + retry contract is shared by
  every synthesis path. See
  [docs/guides/custom-synthesis.md](docs/guides/custom-synthesis.md).

- **On-device synthesis now runs the deterministic sampling +
  verify-and-retry path.** `ffi::trigger_synthesis` →
  `synthesize_scope` previously dispatched the `SynthSummary` task with a
  plain `InferenceRouter::dispatch` and a single parse, so neither the
  fixed seed/sampling knobs nor the quality validator reached the
  primary on-device path. It now dispatches via `dispatch_with_sampling`
  (carrying the deterministic `SamplingConfig`) and runs the shared
  `synthesis_pipeline::verify_and_retry` orchestration — the same
  adaptive budget, salient-term coverage scoring, and single bounded
  retry the server-tier `LlamaCppSynthesizer` uses. The FFI
  `MetricsSnapshot` gains additive (`#[serde(default)]`)
  `synthesis_lowquality_total`, `synthesis_retry_total`,
  `synthesis_retry_failed_total`, `synthesis_truncated_total`,
  `synthesis_recap_chars_total`, and `synthesis_recap_samples_total`
  counters (the on-device path also emits a `tracing::warn!` when a
  retry dispatch fails), and the managed-cloud adapter is wired with
  `with_sampling(config.sampling)` so a programmatic sampling override
  reaches that path too.

- **Per-call sampling override seam
  (`InferenceAdapter::generate_with_sampling` /
  `InferenceRouter::dispatch_with_sampling`).** A new default-delegating
  trait method lets a caller override an adapter's configured sampling
  for a single dispatch (used by the synthesis pipeline's adaptive
  budget / retry). Adapters that cannot vary sampling per call (the
  classifier `FallbackAdapter`) inherit the default and are unaffected.

- **End-to-end user-memory write path.** A new `add_user_memory` FFI
  export appends a `Candidate` observation to a scope's
  `UserMemoryObject`, persists it via the same `flush_user_memory` path
  pin/unpin/decay use, and returns the created `MemoryRecord`. It is
  surfaced as `POST /user_memory` on the substrate loopback, wired
  through the Go substrate client (`Client.CreateMemory`, routed as a
  write to the primary), and exposed at the gateway as
  `POST /api/v1/memories` (the write counterpart to the existing
  `GET /api/v1/memories` list). Validation is fail-closed: blank
  `observation_type`/`content` and unknown `sensitivity` are rejected
  with `400`, a malformed `scope_id` with `400`, and a write to a
  cryptographically-forgotten scope with `404`. Only the user memory
  tier is writable; channel/domain/tenant tiers remain synthesis-owned.
  A new `add_user_memory_total` metrics counter tracks write volume.

- **Consistent encrypted backup snapshots for the evidence store and
  concept graph.** `EvidenceStore::snapshot_to(&self, dest_path)` and
  `PersistentConceptGraph::snapshot_to(&self, dest_path)` write a
  transactionally-consistent, still-encrypted copy of each store's
  SQLCipher database via `VACUUM INTO`. The snapshot keeps the same page
  key (a backup, not a rekey — contrast `rotate_master_key`), so it
  re-opens under the identical master key, and runs in a single implicit
  transaction against the store's own connection so the copy has no torn
  pages even while the live store stays open. The destination is a
  standalone file with no `-journal` / `-wal` sidecar. This lets a
  single-file-DB consumer (e.g. an embedding app holding the stores open)
  fold the otherwise-live sibling databases into a backup/restore cycle
  without risking a torn file copy.
- **Host-facing backup entry point.** `EvidenceStore::snapshot_to` is now
  wired through the cross-platform FFI surface as `snapshot_store_to`
  (UniFFI, for iOS / Android) and `snapshotStoreTo` (N-API, for the
  Electron desktop addon), so mobile and desktop hosts can drive a
  consistent backup of an open store without closing it. The call is
  serialised behind the per-handle runtime mutex (no torn copy under
  concurrent ingest / query) and instrumented with a
  `snapshot_store_to_total` metrics counter.

### Changed

- **`inference_router::RouterConfig` no longer derives `Eq`.** It now
  embeds a `SamplingConfig` whose `f32` fields are `PartialEq` but not
  `Eq`, so `RouterConfig` is `PartialEq` only. Downstream code that
  relied on `RouterConfig: Eq` (e.g. as a `HashMap`/`HashSet` key or an
  `Eq` trait bound) must drop that requirement. `RouterConfig` is a
  `// STABLE` re-export, hence this changelog note.

- **New `EvidenceError::Snapshot` variant.** Downstream exhaustive
  `match` arms over `EvidenceError` must add the new case.

### Fixed

- **Connector transport now honors the HTTP-date form of `Retry-After`.**
  Per RFC 7231 §7.1.3 the header may be either `delay-seconds` or an
  HTTP-date; `HttpResponse::retry_after_seconds` (a `// STABLE` API)
  previously parsed only the integer form and returned `None` for dates,
  so the shared transport discarded the server's rate-limit window and
  fell back to its shorter local exponential backoff — retrying too
  eagerly against providers (GitHub, some Microsoft Graph endpoints) that
  emit the date form, and weakening the GitHub rate-limit classifier
  (`classify_github_failure` keys off `retry_after.is_some()`). All three
  HTTP-date formats (IMF-fixdate, RFC 850, asctime) are now parsed and
  resolved relative to "now", clamped at `0` for an already-elapsed
  deadline. Behavior is unchanged for the integer form and for
  unparseable values (still `None`). Adds an additive, non-breaking
  `HttpResponse::retry_after_seconds_at(now)` for deterministic tests.
  (`crates/connector_framework/src/http.rs`.)

- **User-facing search no longer 400s on malformed query text.** The
  FFI `query` entry point (gateway `POST /api/v1/query`) now rescues
  any FTS5 expression the parser rejects — an unbalanced or stray `"`,
  a dangling `revenue AND`, an incomplete `NEAR(`, an unmatched `(` —
  by retrying it once as a sanitised literal-token search, so a search
  box returns results instead of a parser error (matching what users
  expect when they type punctuation). Well-formed FTS5 expressions
  still run verbatim; only whitespace-only input (no searchable
  tokens) still surfaces `InvalidQuery` (`400`). The low-level
  `search_fts` primitive remains strict (the source of truth for query
  validity). `query` is a `// STABLE` export, hence this note.
  (`crates/ffi/src/lib.rs`.)
- **Gateway synthesis SSE status classification no longer matches
  substrings.** `isTerminalStatus` / `isSuccessStatus` compared the
  status doc with `strings.Contains`, so a value such as `incomplete`
  was wrongly treated as `complete` (terminating the stream early).
  They now match the lifecycle `status` / `state` fields by exact
  token equality against the canonical `WindowStatus` vocabulary
  (`pending` / `in_progress` / `complete` / `failed`) and its tolerated
  aliases, preserving classification for every real status while fixing
  the latent substring collision. (`server/internal/gateway/synthesis.go`.)
- **Gateway synthesis SSE status is now decoded once per poll and guarded
  against Rust↔Go vocabulary drift.** The terminal/success classifiers
  were merged into a single `classifySynthesisStatus` that unmarshals the
  status document once per SSE tick instead of twice, and a new contract
  test (`window_status_contract_test.go`) parses the substrate's
  `WindowStatus` enum (`crates/synthesis_pipeline/src/window.rs`) and
  fails CI if a variant is ever added without a matching entry in the
  gateway's success/failure/pending token sets — so a future status (e.g.
  `cancelled`) can no longer be silently misclassified as non-terminal and
  poll to the stream cap. No behavior change for existing statuses.
  (`server/internal/gateway/synthesis.go`.)

### Security

- **TEE attestation now fails closed for real platforms instead of
  fail-open.** `crypto::attestation::verify_attestation` previously
  returned `Ok(report.measurement == expected_measurement)` for the
  `intel_tdx` / `amd_sev_snp` / `nitro_enclaves` platforms — i.e. it
  accepted a real-platform report on a bare measurement comparison while
  the quote-signature / vendor-CA-chain check is still unimplemented.
  Because `measurement` is copied verbatim out of the (unverified) quote
  document (e.g. PCR0 from the Nitro `COSE_Sign1` envelope produced by
  `synthesis_engine::tee_runtime_nitro`, the only non-test `TeeRuntime`),
  an untrusted host operator could forge a report carrying the expected
  measurement and no valid platform signature and have it accepted —
  defeating the exact threat model TEE attestation defends against. The
  function now returns the new `CryptoError::AttestationUnsupported`
  error for those platforms until real quote verification lands; the
  `Mock` platform path is unchanged. `synthesis_engine::tee_worker`
  already treats the error as an attestation failure (stays
  `Unattested`, records a failure audit entry), now covered by an
  end-to-end regression test. The `attestation` module remains
  `// UNSTABLE`. (`crates/crypto/src/attestation.rs`,
  `crates/crypto/src/errors.rs`,
  `crates/synthesis_engine/src/tee_worker.rs`.)
- **Gateway control plane now authorizes, not just authenticates.**
  Previously every `/api/v1` control-plane route (tenant lifecycle,
  audit, export, permission graph, SCIM) was gated only by coarse
  authentication: handlers took the target tenant / scope / tuple from
  the request, so any authenticated tenant-user JWT could read another
  tenant's audit log (`GET /audit?tenant_id=…`), grant itself authority
  (`POST /permission/grant`), or drive tenant lifecycle and SCIM
  provisioning. The fully-built ReBAC middleware (`RequireRelation`) was
  mounted on no live route and the permission graph enforced nothing. A
  layered authorization model is now wired in the gateway:
  - The **service principal** (trusted backend via the static API key,
    or dev mode when no credentials are configured) bypasses every gate,
    so service-API-key-only deployments are unaffected.
  - **Platform-global** operations are **service-only** (tenant-user
    JWTs get `403`): tenant create / list-all / delete, SCIM directory
    (`/scim/v2`), and authorization-graph mutation/inspection
    (`/permission/grant|revoke|check`) — closing the open self-grant
    hole. New `middleware.RequireService`.
  - **Per-tenant** operations are **ReBAC-authorized against the tenant
    resolved from the request**, not an arbitrary caller-supplied value:
    audit reads (`viewer`, tenant from `?tenant_id`), tenant reads
    (`viewer`, tenant from URL `{id}`), tenant config/key/member
    mutations and profile export (`admin`). Omitting `tenant_id` on an
    audit query no longer reads across all tenants — the protected
    object is unresolvable, so the gate denies (service principals
    retain cross-tenant visibility). This activates the SCIM-populated
    permission graph and gives the Rust `check_permission` path its
    first live consumers.

  **Upgrade / migration (deny-by-default).** This is a breaking change
  for deployments that issue **tenant-user JWTs**; service-API-key-only
  deployments are unaffected (the service principal bypasses every
  gate). SCIM provisions only `group:<gid># member @ user:<uid>` tuples,
  not tenant-role tuples, so the newly ReBAC-gated routes deny all
  non-service callers until tenant roles exist. Operators must, as a
  rollout/onboarding step, provision the tenant `viewer` / `admin` roles
  via `/permission/grant` (itself service-only) for each tenant-user who
  needs tenant reads, audit, or export; otherwise those users will
  receive `403` after upgrade. This is the intended posture — it closes
  the cross-tenant read and self-grant holes — but it requires the
  grant step before tenant-user traffic is cut over.

  The embedded **data plane** (ingest / query / evidence / synthesis)
  remains `scope_id`-capability-secured and is intentionally unchanged
  here. (`server/internal/middleware/authz.go`,
  `server/internal/gateway/authz.go`,
  `server/internal/gateway/gateway.go`,
  `server/internal/tenant/tenant.go`.)

## [1.2.0] - 2026-06-10

Resource-efficiency release for the mobile / low-memory fleet, paired
with a supply-chain hardening pass that clears every open RustSec
advisory in the locked tree. Adds lazy on-device SLM weight download,
platform-aware sync scheduling, a three-way `MemoryProfile`, mobile
compaction tuning, a boundary-safe incremental-sync cursor, and 70 new
regional connectors (140 built-in total). Upgrades `async-nats` 0.46 → 0.49
to drop the vulnerable `rustls-webpki 0.102` / `time 0.3.45` subtree,
which raises the workspace MSRV to 1.88.

> **Breaking changes** — review before upgrading:
> - **MSRV raised 1.85 → 1.88.** Building the workspace now requires
>   Rust ≥ 1.88 (forced by `time 0.3.47`, pulled via the security-patched
>   `async-nats 0.49`). Consumers of the N-API surface need an
>   Electron / Node build toolchain on the same floor. See _Security_.
> - **`evidence_store::EvidenceStoreConfig`: the boolean `low_memory`
>   field is replaced by the `MemoryProfile` enum** (`Default` / `Medium`
>   / `Low`). Callers that set `low_memory: true` map to
>   `MemoryProfile::Low`; `false` maps to `MemoryProfile::Default`.
> - **Removed the inert `connectors::payfit::API_KEY_TOKEN_TYPE` and
>   `connectors::pennylane::API_KEY_TOKEN_TYPE` constants** (see
>   _Changed_).
> - **New enum variants on existing public enums** (`FfiError`,
>   `EvidenceError`, `ConnectorKind` / `ConnectorKindTag`); downstream
>   exhaustive `match` arms over these enums must add the new cases.

### Added

- **Lazy on-device SLM weight download (STABLE).** SLM weights
  (~248 MB MLX / ~237 MB GGUF) are no longer bundled in the installer;
  they are fetched on demand on first synthesis. `inference_router`
  gains `model_download_url` / `model_sha256` config, an atomic
  SHA-256-verified download (bytes stream into a `*.partial` sidecar
  and are renamed into place only after verification), an observable
  `ModelDownloadState` (Idle → InProgress → Complete/Failed) and a
  host progress callback. **Public API:** adds the
  `FfiError::ModelDownloading { progress_pct }` variant and the
  `set_model_download_progress_callback` / `model_download_state` FFI
  + N-API entry points. The substrate REST tier maps
  `ModelDownloading` to `503 Service Unavailable`.
- **`ffi::PlatformHint` (`Desktop` / `Mobile`) and
  `start_sync_scheduler_for_platform` (STABLE).** Lets a host pick the
  scheduler's battery-vs-freshness trade-off. `Mobile` doubles the
  default sync interval (30 min) and tick cadence (60 s) and coalesces
  every connector due in a tick onto a single batch timestamp so they
  are serviced in one wake window, minimising radio/CPU wake-ups. The
  legacy three-argument `start_sync_scheduler` is unchanged and keeps
  the `Desktop` behaviour. Exposes `MOBILE_SYNC_INTERVAL_SECS` /
  `MOBILE_SYNC_TICK_SECS`. The N-API `startSyncScheduler` gains an
  optional trailing `platformHint` argument (defaults to desktop).
- **`evidence_store::MemoryProfile` (`Default` / `Medium` / `Low`)
  (STABLE).** Replaces the boolean `low_memory` flag with a three-way
  profile and adds an intermediate 1 MiB page-cache tier
  (`MEDIUM_MEMORY_PAGE_CACHE_KIB`) for Medium device tiers, which keeps
  mmap enabled (unlike Low). `DeviceTier::Medium` now maps to it.
- **`sync_engine::CompactionPolicy::mobile_default()` /
  `MOBILE_MAX_DELTA_BYTES` (STABLE).** A 2 MiB adaptive compaction
  threshold (half the desktop default) used on mobile / Low-tier hosts
  to keep delta payloads and merge-time memory smaller.
- **`connector_framework::WatermarkCursor` (STABLE).** A backward-compatible
  incremental-sync cursor that stores the high-water `updated_at` instant
  together with the set of source ids observed at that instant, so records
  sharing the exact boundary second are no longer dropped (see _Fixed_).
  Legacy bare-timestamp cursors parse transparently as that watermark with an
  empty id set.
- **70 new regional connectors (140 built-in total across 10 markets) in 7 new regional batches.**
  UK (Monzo Business, Revolut Business, FreeAgent, GoCardless, Royal
  Mail, Deliveroo, Just Eat, Companies House, HMRC MTD, Starling),
  Germany (N26 Business, DATEV, lexoffice, DHL Business, Otto, Zalando,
  Deutsche Post, Personio, sevDesk, Billomat), France (Qonto,
  Pennylane, PayFit, Colissimo, Cdiscount, MangoPay, Brevo/Sendinblue,
  OVHcloud, Alan, Swile), Switzerland (PostFinance, TWINT, Swiss Post,
  Bexio, Abacus, Ricardo, Digitec Galaxus, SIX Payment, Klara, Beem),
  Australia (MYOB, Afterpay, Australia Post, Employment Hero, Deputy,
  Tyro, Prospa, SEEK, Campaign Monitor, Pinch), Latin America
  (MercadoLibre, Rappi, Nubank Business, PagSeguro, iFood, VTEX, Clip,
  Ualá, Falabella, Correos de México), and SEA-expanded (Shopee/Lazada
  regional, SeaMoney, GrabPay, Bukalapak, Blibli, Traveloka, AirAsia
  Super App, MyEG, GCash). Each implements the `Connector` trait
  (OAuth2/native auth, full→incremental sync, content fetch, optional
  webhooks, ACL projection) with `MockHttpTransport` unit tests, and is
  wired end-to-end through `ConnectorKind`, the FFI `ConnectorKindTag`,
  and webhook provider-id resolution. **Public API:** adds 70
  `ConnectorKind` / `ConnectorKindTag` variants.
- **Multilingual proof + eval coverage extended to all 22 lexicon
  languages.** The SME business-proof demo now spans 121 records / 11
  scopes / 21 source types across 8+ languages and 7+ regions with 51
  passing assertions; the cross-lingual recall benchmark and the
  inference-router Bonsai matrix both cover all 22 built-in lexicon
  languages; the observation-eval golden dataset gains European-language,
  file/media-metadata, and regional-connector-payload blocks.
- **CI quality gates.** A `connector_audit` job ties all-feature
  connector tests, the extraction-quality eval, the cross-lingual recall
  benchmark, and the dockerized SME demo into one regression gate; a
  weekly `competitor_benchmark` job fails on any >10% regression of
  ingest throughput / FTS latency / hybrid-retrieval latency versus the
  documented baselines (`docs/operator/perf-baselines.json`).

### Changed

- **`synthesis_engine::tee_worker` default attestation TTL shortened
  from 1 hour to 5 minutes (STABLE).** A `TeeWorker` built via
  `TeeWorkerConfig::new` now treats a cached attestation as fresh for
  only 300 s, so a stolen or replayed attestation report buys a much
  smaller window before the worker is forced to re-attest. Callers that
  relied on the old hour-long window must now re-attest more often or
  set `TeeWorkerConfig::attestation_ttl` explicitly. Part of the TEE
  synthesis-worker side-channel hardening (zeroize-on-drop of plaintext
  synthesis intermediates and best-effort enclave page pre-faulting are
  internal and add no public API). See
  `docs/security/tee-side-channels.md`.
- **Deduplicated the per-connector token-provenance auth dispatch.**
  The seven native-header connectors (Gojek, Odoo SEA, VNPay, Sapo,
  Tiki, Viettel Post, TrueMoney) each carried a copy of the same
  `apply_auth` body that branches on `OAuth2Token::token_type` to send
  a static credential via the provider-native header and an
  OAuth-issued token via `Authorization: <scheme>`. That logic now
  lives in a single shared helper,
  `connector_framework::apply_auth_by_provenance(req, token, native_header, marker)`;
  each connector's `apply_auth` is a one-line delegation passing its own
  native header name and `token_type` marker. No behavioural change on
  the wire — purely a refactor to remove the duplication.
- **TrueMoney connector now fails fast on a missing signing secret.**
  `TrueMoneyConnector::authenticate` validates `signing_secret` on the
  API-key path (via `Self::signing_secret(config)?`), so a misconfigured
  connector surfaces a `ConnectorError::Auth` at authenticate time rather
  than lazily on the first `signed_get` during `initial_sync`. This
  aligns TrueMoney with the existing `TikiConnector::authenticate`
  behaviour. The signing secret itself is still read per request when
  computing the HMAC signature.
- **PayFit and Pennylane now set the bearer `Authorization` header
  directly instead of routing through `apply_auth_by_provenance`.** Both
  connectors authenticate every credential shape (static API key *and*
  OAuth-issued token) as `Authorization: <scheme> <token>`, so they
  passed `"Authorization"` as the helper's `native_header` together with
  an `API_KEY_TOKEN_TYPE` marker that `authenticate` never assigned. The
  marker was therefore inert, but it was a latent footgun: had a token
  ever carried that `token_type`, the helper would have written the raw
  access token to `Authorization` with no `Bearer` scheme prefix. The
  `apply_auth` helper now builds the scheme-prefixed header directly (no
  behavioural change on the wire). **Public API:** removes the inert
  `connectors::payfit::API_KEY_TOKEN_TYPE` and
  `connectors::pennylane::API_KEY_TOKEN_TYPE` constants.
- **`MangoPayConnector::instance` is now `pub`** (and the connector gains
  a manual `Debug` impl that redacts the transport/oauth trait objects),
  restoring parity with the sibling connectors (`qonto`, `pennylane`,
  and the 80+ others that already expose `pub instance`). **Public API:**
  additive, non-breaking — widens the field visibility only.

### Fixed

- **Incremental sync no longer drops records at the watermark boundary
  (repo-wide).** Connectors that tracked incremental progress by a single
  high-water timestamp could permanently drop records sharing the exact
  boundary instant that were not part of the previous page (e.g. written in
  the same second/millisecond just after the prior snapshot, or split across a
  page boundary). Three distinct variants of this bug were found and fixed:
  - **Inclusive filter + client-side drop.** The connector requested
    `updated >= cursor` (or fetched everything) and then skipped each record
    with `updated <= cursor`, so a brand-new boundary record was discarded.
  - **Exclusive server filter.** The connector requested `updated > cursor`
    (e.g. `gt`/`date_updated_gt`), so the provider never returned the boundary
    record at all. These now query inclusively (`>=`, or one unit before the
    watermark where the API has no `gte` variant, as with ClickUp's
    millisecond `date_updated_gt`) and dedup client-side.
  - **Descending-sort truncation.** Desc-sorted connectors (Notion,
    Confluence) short-circuited pagination at the first row `<= watermark`,
    truncating same-instant rows that sorted after it. Pagination now stops
    only at the first row strictly `< watermark`, so boundary rows stay on the
    page and are deduped.

  All 122 affected connectors now persist and consume a
  `connector_framework::WatermarkCursor` (timestamp + boundary id-set), so a
  brand-new boundary-instant record is surfaced while already-emitted ids are
  not duplicated. The cursor wire format is backward compatible with existing
  persisted RFC-3339 bare-timestamp cursors, which continue seamlessly. The
  bare epoch-second/-millisecond cursors used by Stripe, HubSpot, Zalo,
  Intercom and ClickUp are not RFC-3339, so they parse as an empty watermark
  and trigger a one-time full re-walk on the first sync after upgrade
  (re-emitting records as idempotent updates) — never silent data loss. This
  expands the original 10 UK-connector fix (Monzo
  Business, Revolut Business, FreeAgent, GoCardless, Royal Mail, Deliveroo,
  Just Eat, Companies House, HMRC MTD, Starling) to every other region and
  cursor variant, including connectors missed by the first grep-based passes
  (Asana, Zoho, Tabby, the Vietnam/SEA set) and connectors previously
  mis-classified as bespoke (Stripe, HubSpot, Linear, Notion, Confluence,
  Intercom, ClickUp).
  Connectors that never used a timestamp watermark are unchanged: Figma
  (per-file version numbers), Slack (bespoke `SlackCursor`), the delta/
  page-token connectors (Box, Discord, Dropbox, the Google Drive/Docs/Sheets/
  Calendar family, the Microsoft Graph OneDrive/SharePoint/Teams family,
  Email), and Zendesk (server-windowed incremental export). Zoom and Google
  Meet already used a boundary-id cursor.
- **Malformed full-text queries now return `400`, not `500`.** A
  syntactically invalid FTS5 `MATCH` expression (unbalanced phrase
  quote, dangling boolean operator, bare `NEAR(`, …) is client input the
  server cannot parse, but it previously surfaced as
  `EvidenceError::Sqlite` → `FfiError::Evidence` →
  `500 Internal Server Error`, mislabelling bad input as an internal
  crash. The unicode61 lane (the documented source of truth for query
  validity) now classifies an FTS5 query-syntax error by its primary
  SQLite result code — `SQLITE_ERROR` on the otherwise-static `MATCH`
  `SELECT` can only originate from the bound query operand — into a new
  `EvidenceError::InvalidQuery`, which threads through a new
  `FfiError::InvalidQuery` to `400 Bad Request` (kind `InvalidQuery`).
  Valid queries still return `200`; genuine storage faults (`CORRUPT`,
  `IOERR`, …) stay `EvidenceError::Sqlite` → `500`. **Public API:** adds
  the `EvidenceError::InvalidQuery` and `FfiError::InvalidQuery`
  variants and a new `ErrorCounters.invalid_query` metrics field
  (wire-additive via `#[serde(default)]`).
- **`connectors::bexio::DEFAULT_API_BASE_URL` no longer double-versions
  the request path.** The default was `https://api.bexio.com/2.0` while
  every endpoint path also carries a `/v1` segment (`/v1/invoices`),
  so requests resolved to `https://api.bexio.com/2.0/v1/invoices` — two
  API-version segments. The default is now `https://api.bexio.com`, so
  requests resolve to `https://api.bexio.com/v1/invoices`, matching the
  bare-host + `/v1` convention used by the other nine Switzerland
  connectors. Overridable as before via `auth_config_json.api_base_url`.
- **Restored the single `/v1` request-path segment for six connectors
  whose base URL carries no version segment: `bexio`, `datev`,
  `lexoffice`, `otto`, `personio`, `sev_desk`.** A prior cleanup that
  removed doubled `…/vN/v1/…` segments from connectors whose base URL
  embedded the version over-stripped these six, whose
  `DEFAULT_API_BASE_URL` is versionless — host-only for five
  (e.g. `https://api.datev.de`) and host + non-version root for
  `sev_desk` (`https://my.sevdesk.de/api`): the `/v1` was dropped from
  *both* the base URL and the request path, so they emitted version-less
  production URLs (`https://api.datev.de/bookings`) that 404. The request
  paths now carry `/v1` exactly once (`https://api.datev.de/v1/bookings`,
  `https://my.sevdesk.de/api/v1/invoices`), matching the
  versionless-base + `/v1`-in-path convention shared with `billomat` and
  the other Switzerland/Germany connectors; each connector's
  `default_base_url_has_no_duplicate_version` test exercises the real
  `DEFAULT_API_BASE_URL` to guard this. These connectors are re-exported
  `// STABLE`; the change is to runtime HTTP-path behaviour only — their
  public types, functions, and constants (including the
  `DEFAULT_API_BASE_URL` values, which were already host-only) are
  unchanged. **Operator note:** the `auth_config_json.api_base_url`
  override must be **host-only** (e.g. `https://api.bexio.com`); a
  deployment that had set it to `…/v1` to work around the version-less
  regression should drop that segment now, otherwise the connector owning
  the `/v1` path will produce a doubled `…/v1/v1/…` URL.

### Security

- **Cleared all five open RustSec advisories in the locked dependency
  tree by upgrading `async-nats` 0.46 → 0.49.** The `0.46` line
  transitively pinned the unmaintained `rustls-webpki 0.102`, which
  carries four advisories — RUSTSEC-2026-0049 / -0098 / -0099 (X.509
  name-constraint bypasses) and RUSTSEC-2026-0104 (a reachable
  CRL-parsing panic) — and pinned `time 0.3.45`, affected by the
  RUSTSEC-2026-0009 parsing DoS. `0.49` moves the NATS TLS path onto the
  patched `rustls-webpki 0.103` / `time 0.3.47`. `cargo audit` now
  reports **0 vulnerabilities**. The NATS path is behind the non-default
  `replication-nats` feature, but `cargo audit` / `cargo deny` scan the
  whole lockfile, so the fix is unconditional. `async-nats` reuses the
  `rustls 0.23` / `tokio-rustls 0.26` already pulled in by `reqwest`'s
  `rustls-tls`, adding no second TLS stack.
- **Workspace MSRV raised 1.85 → 1.88** as the cost of the above:
  `time 0.3.47` requires Rust 1.88. The MSRV CI gate
  (`MSRV (1.88.0)`), the production `deploy/Dockerfile.substrate` build
  image (`rust:1.88`), `.cargo/config.toml`, dependabot pin rationale,
  and the security / operator docs were all updated to match. See
  [docs/security/dependency-policy.md](docs/security/dependency-policy.md).
- **Documented three reviewed `cargo deny` advisory exceptions** for the
  archived-upstream PQClean SPHINCS+ family
  (`pqcrypto-internals` / `pqcrypto-sphincsplus` / `pqcrypto-traits`,
  RUSTSEC-2026-0160 / -0162 / -0163). These are *unmaintained* notices,
  not vulnerabilities: SLH-DSA is a finalised NIST standard (FIPS 205)
  whose reference implementation is frozen by design, and no maintained
  pure-Rust equivalent with an audited PQClean-derived backend exists.
  The version-pinned ignores live in `deny.toml` with full rationale and
  are re-evaluated each update cycle; a future *vulnerability* advisory
  against the family is **not** masked and still hard-fails the gate.

## [1.1.0] - 2026-06-05

This release adds substrate high availability, an end-user reference web
UI, a bundled on-device SLM, 60 new connectors (70 stable total),
security-audit preparation, one-command setup, and performance hardening.

### Added

- **Substrate high availability (active-passive failover).** The
  substrate can now ship its SQLCipher WAL frames to one or more standby
  nodes over NATS JetStream, with leader election via a NATS key-value
  lease. A standby replays frames read-only and promotes itself when the
  primary's lease expires. New knobs: `KNOWLEDGE_SUBSTRATE_ROLE`
  (`primary` / `standby` / `auto` / `disabled`, also `--role`) and
  `KNOWLEDGE_REPLICATION_NATS_URL`; the NATS transport is gated behind
  the non-default `replication-nats` cargo feature so standalone and
  cross-compile builds stay lean. `/health` gains a `replication`
  object (`role`, `lag_frames`, `last_applied_at`, …) and
  `/internal/metrics` exposes `knowledge_replication_lag_frames` and
  related gauges. The gateway accepts `KNOWLEDGE_SUBSTRATE_URL_STANDBY`
  and routes writes to the primary (failing over on a `503`
  standby/unreachable response) while offloading reads to a standby. The
  Helm chart renders a StatefulSet (one PVC per pod) instead of the
  single Deployment when `substrate.ha.enabled=true`, docker-compose
  ships a commented-out standby service, and monitoring adds a
  replication-lag dashboard panel plus a `KnowledgeReplicationLagHigh`
  alert. The journal mode is role-asymmetric: a primary runs in
  `journal_mode=WAL` (auto-checkpoint disabled) so SQLite produces the
  `-wal` the shipper drains, while a standby stays in a rollback-journal
  mode so its raw page splicing stays coherent; `auto`-mode nodes switch
  modes on promotion/demotion, and the standby re-opens its read
  connection after each applied segment so replicated pages become
  visible.

- **60 new connectors — the catalog now spans 70 stable providers**
  (up from 10 in 1.0.0). All ship as **stable** with full `Connector`
  trait implementations and `MockHttpTransport` unit coverage:
  - *Productivity & CRM (10)* — Salesforce, ServiceNow, Zendesk, Linear,
    Asana, Monday, ClickUp, Freshdesk, Intercom, Pipedrive.
  - *Cloud storage & communication (10)* — Dropbox, Box, SharePoint,
    Teams, Discord, Zoom, Google Calendar, Google Docs, Google Sheets,
    Google Meet.
  - *Business & developer tools (10)* — QuickBooks, Xero, Stripe,
    Shopify, Airtable, GitLab, Bitbucket, Trello, Miro, DocuSign.
  - *Vietnam (10)* — Zalo, VNPay, MoMo, Tiki, Shopee VN, Lazada VN,
    Viettel Post, KiotViet, Sapo, Base.vn.
  - *Singapore / Thailand / SEA (10)* — LINE, Grab, Gojek, Talenox,
    Odoo (SEA), Fastwork, TrueMoney, SCB Easy, PromptPay, Tokopedia.
  - *GCC / Middle East (10)* — Careem, Talabat, Noon, Amazon.ae
    (SP-API), Tabby, Foodics, Zoho, Bayt, Fetchr, PayFort (Amazon
    Payment Services).

  Adds a crate-internal `signing` module providing the HMAC-SHA256,
  SHA-256, and AWS Signature v4 primitives several of these providers'
  auth schemes require.
- **Browser-based admin dashboard** (`admin/`) — a React + Vite SPA served
  on `:3001` for managing connectors, tenants, synthesis runs, the memory
  browser, and the audit log without the CLI or PromQL.
- **End-user reference web UI** (`apps/knowledge-ui/`) — a Next.js 14
  (App Router) chat / search / memory app served on `:3002`, wired into
  `deploy/docker-compose.yml`. It is the consumer-facing counterpart to
  `admin/`: a thin, fully client-side client over the gateway REST
  surface that lets end users chat with a scope, run hybrid search,
  browse synthesized memory and its decay state, stream synthesis
  progress over SSE, and cryptographically forget a conversation.
  Shipped as a static export behind nginx with a same-origin reverse
  proxy to the gateway.
- **Bundled SLM model.** The published `llama-server` image now bakes the
  Bonsai-1.7B GGUF in at `/models/bonsai-1.7b.gguf` (see
  `deploy/Dockerfile.llama-server`), so `docker compose up` has
  server-side synthesis working with **zero manual model download**.
  Operators can still override it by bind-mounting a different GGUF over
  that path. `scripts/download-models.sh` remains for native local /
  on-device dev (GGUF, MLX, ONNX), with SHA-256 verification against
  `deploy/model-artifacts/SHA256SUMS`.
- **Performance hardening.** A low-memory mode for constrained ("low"
  device tier) hosts (`EvidenceStoreConfig::low_memory`, which shrinks
  the SQLCipher page cache to `LOW_MEMORY_PAGE_CACHE_KIB`); per-device
  profile benchmark suites (`crates/benchmarks/benches/device_profile/`
  for low/medium/high tiers); and two new substrate latency histograms,
  `knowledge_open_store_duration_seconds` and the per-`(task, adapter)`
  `knowledge_slm_dispatch_duration_seconds`.
- **Security-audit preparation.** Audit-readiness docs
  (`docs/security/audit-scope.md`, `audit-guide.md`,
  `finding-template.md`, `key-rotation.md`); hardened default
  credentials (`.env.example` ships **no** default passwords —
  `docker compose` refuses to start until Postgres / MinIO / Grafana
  passwords are set); and a `cargo-fuzz` harness for the crypto
  primitives (`crates/crypto/fuzz/`: AEAD, HKDF, hybrid-KEM, ML-DSA,
  SPHINCS+, and cryptographic-forgetting round-trips).
- **Pre-built container images & Helm chart.** Multi-arch images publish
  to GHCR/Docker Hub on tagged releases; `deploy/docker-compose.images.yml`
  runs the stack with no local build, and `deploy/helm/knowledge` plus
  starter Terraform modules (`deploy/terraform/{aws,gcp}`) deploy it to
  Kubernetes.
- **Release & auto-update automation** — a tag-triggered release workflow
  (binaries, images, Helm chart) and an optional substrate update-check
  endpoint that compares the running version against the latest release.
- **Managed-cloud synthesis adapter.** `inference_router` gains a
  `ManagedCloudAdapter` that drives synthesis through an external
  OpenAI-compatible `/v1/chat/completions` endpoint (OpenAI, Groq,
  Together, a local Ollama, …) instead of a self-hosted `llama-server`.
  It sits between llama.cpp and the fallback in the priority chain
  (`MLX → llama.cpp → ManagedCloud → Fallback`), serves synthesis on any
  device tier (the compute is remote), and applies the same structured
  output constraint via the API's `response_format`. Wired into
  `build_inference_router` and auto-discovered from
  `KNOWLEDGE_MANAGED_INFERENCE_URL` / `_KEY` / `_MODEL` (default model
  `gpt-4o-mini`). New **stable** public API: the `AdapterKind::ManagedCloud`
  variant and the `http-client`-gated `HttpManagedInferenceClient`
  re-export. Adding the enum variant is a semver-breaking change for
  downstream code that matches `AdapterKind` exhaustively. The STABLE
  `InferenceAdapter` trait also gains a `benefits_from_warm_up` method
  (default `true`, so existing implementors are source-compatible) that
  lets an adapter opt out of `InferenceRouter::warm_up`;
  `ManagedCloudAdapter` returns `false` so warm-up never sends a billable
  no-op to a remote, pay-per-request endpoint with no local weights to
  page in.
- **One-command installers.** `scripts/install.sh` (bash) and
  `scripts/install.ps1` (PowerShell) take an SME from zero to a running
  stack: they check Docker + the Compose plugin, generate per-deployment
  secrets into `.env` (mode 600, never overwriting an existing file),
  prompt for on-device synthesis, start the published-image stack, wait
  for the gateway to report healthy, and print the URLs to open.
- **Admin first-run wizard.** The `admin/` dashboard shows a guided
  wizard (welcome → pick a source → OAuth → first sync) on a fresh
  deployment with no connectors, plus a Getting Started card on the
  Dashboard while fewer than three connectors are configured.
- **Offline master-key rotation.** New STABLE API for re-keying a
  deployed substrate without re-encrypting evidence bodies:
  `evidence_store::EvidenceStore::rotate_master_key` plus the
  `evidence_store::MasterKeyRotationReport` report type,
  `permission_service::PersistentTupleStore::rotate_master_key`, and the
  `substrate_server::key_rotation` module (with the `knowledge-rotate-key`
  binary). `scripts/rotate-master-key.sh` wraps it for Docker/Compose and
  `docs/security/key-rotation.md` documents the procedure, risks, and
  rollback.

### Changed

- **`trigger_synthesis` wired end-to-end.** Server / desktop / hybrid
  builds now compile the reqwest-backed llama.cpp adapter in by default,
  and the substrate auto-discovers a `llama-server` sidecar via
  `KNOWLEDGE_LLAMA_SERVER_URL`, so a `docker compose up` deployment has
  synthesis working out of the box. `Unavailable` is now returned only
  when no `SynthSummary`-capable adapter is linked **and** reachable.
- **`connectors::GitHubConnector` promoted from unstable to stable.** The
  GitHub connector now fully implements the `Connector` trait (real
  `fetch_content`, RFC 8288 `Link`-header pagination, and GitHub-aware
  rate-limit classification on both GET and POST paths) and is wired into
  the FFI `build_connector` factory, so hosts can instantiate
  `ConnectorKind::GitHub`. With the 60 connectors added this release, the
  catalog is now **70 stable** (was 9 stable + 1 unstable in 1.0.0).
- **`evidence_store::EvidenceError`** gains a `KeyRotation(String)` variant
  describing master-key rotation failures (destination already exists,
  integrity-verification mismatch). Downstream code that matches this enum
  exhaustively without a wildcard arm must add a `KeyRotation` arm.

### Fixed

- **Vietnam + Thailand connectors auth header by token provenance** —
  `connectors::VNPayConnector`, `connectors::SapoConnector`,
  `connectors::TikiConnector`, `connectors::ViettelPostConnector`
  (Vietnam) and `connectors::TrueMoneyConnector` (Thailand) now pick the
  request auth header from the token's provenance (recorded in
  `OAuth2Token::token_type`, mirroring the Discord connector and the
  earlier Gojek/Odoo fix): a static credential (API key / access token /
  session token) is sent in the provider-native header (`X-Api-Key` /
  `X-Sapo-Access-Token` / `tiki-api-key` / `Token` / `X-API-Key`), while
  a token minted by the OAuth2 code-exchange fallback is sent as
  `Authorization: Bearer`. Previously an OAuth-issued token was sent in
  the provider-native header, which would be rejected by an endpoint
  expecting a bearer token. For Tiki and TrueMoney the separate HMAC
  signature (`sign`/`timestamp` query pair and `X-Timestamp`/`X-Signature`
  headers respectively), keyed by the merchant secret, is unchanged
  and still applied to every request.
- **`connectors::GojekConnector` / `connectors::OdooSeaConnector` auth
  header by token provenance** — both connectors now pick the request
  auth header from the token's provenance (recorded in
  `OAuth2Token::token_type`, mirroring the Discord connector): a static
  credential (API key / session token) is sent in the provider-native
  header (`X-Gojek-Api-Key` / `X-Openerp-Session-Id`), while a token
  minted by the OAuth2 code-exchange fallback is sent as
  `Authorization: Bearer`. Previously an OAuth-issued token was sent in
  the provider-native header, which would be rejected by an endpoint
  expecting a bearer token.
- **`connectors::GitHubConnector` pagination** — `paginate_issues` /
  `paginate_comments` no longer fall back to manual `page=N` walking after
  following an opaque `Link` cursor, which could re-fetch and duplicate a
  page when a server emitted `Link` headers on some pages but not others.
- **`connectors::GitHubConnector::subscribe_webhook`** — a rate-limited
  `403`/`429` on webhook creation is now surfaced as a retriable `Sync`
  error instead of being mis-classified as `Auth`.

## [1.0.0] - 2026-06-03

First public release of Knowledge — a privacy-first, post-quantum
secure knowledge substrate for AI applications. On-device by default,
$0/user/month at any scale.

### Highlights

- **24-crate Rust workspace** delivering the full device-side
  substrate: ingestion, multilingual extraction, decaying memory,
  concept graph, synthesis, permissions, crypto, connectors, and
  export.
- **Post-quantum cryptography** — hybrid X25519 + ML-KEM-768
  (FIPS 203) key encapsulation and ML-DSA-65 (FIPS 204) signatures.
- **Multilingual extraction** across 22 languages with per-sentence
  language detection and a built-in lexicon registry.
- **Cryptographic forgetting** — scope teardown destroys the
  per-scope key so forgotten data is unrecoverable, not soft-deleted.
- **10 connectors** (9 stable + 1 unstable): Google Drive, OneDrive,
  Notion, Jira, Confluence, Figma, HubSpot, Slack, Email, and
  GitHub (unstable).
- **Go API gateway** with a full REST surface over the Rust substrate.
- **Three deployment modes**: on-device, hybrid, and enterprise.
- **Criterion.rs benchmark suite** with documented, reproducible
  results on reference hardware.

### Features

- **Storage & retrieval** — SQLCipher-encrypted evidence store with
  FTS5 full-text search (unicode61 baseline plus CJK/Thai trigram and
  bigram recall lanes), content-hash deduplicated body storage,
  hybrid lexical + semantic retrieval, and an append-only,
  trigger-enforced evidence table.
- **Memory model** — decay state machine with retention scoring and
  working-memory management; sparse typed concept graph with
  supersession and contradiction edges and paginated load for
  power-user scopes (100k+ concepts).
- **Synthesis** — scope-window synthesis with GBNF-constrained
  generation and elected-device coordination on-device, plus a
  server-side synthesis engine for the hybrid profile behind a
  TEE-gated attestation check.
- **On-device inference** — inference router dispatching across
  Apple MLX, llama.cpp (CPU/Metal), and a deterministic lexicon
  fallback, with device-tier gating.
- **Connectors** — OAuth2 transport, incremental delta sync, webhook
  subscriptions, per-provider token-bucket rate limiting, and real
  content fetching across all connectors.
- **Sync** — CRDT-based delta sync of synthesis objects with snapshot
  bootstrap and opt-in auto-compaction.
- **Host bindings** — UniFFI bindings for iOS (Swift) and Android
  (Kotlin) and an N-API addon for Electron/Node.js, including a
  resolver-driven cold-boot path so the 32-byte master key never
  lives in the host address space as a long-lived plaintext string.
- **Go API gateway** — evidence ingest/query/get, memory listing,
  scope forgetting, synthesis trigger/status/recent, connector
  CRUD + OAuth2 + sync + webhooks, tenant lifecycle + key rotation +
  member provisioning, Zanzibar permission grant/revoke/check, SCIM
  v2 provisioning, export rendering, and audit query — with dual-layer
  (per-IP and per-tenant) rate limiting, bearer/JWT auth, SSE
  streaming, Prometheus metrics, and a subsystem health endpoint.

### Security

- Post-quantum hybrid encryption (X25519 + ML-KEM-768) with
  HKDF-SHA-256 derivation and XChaCha20-Poly1305 AEAD.
- ML-DSA-65 signatures on every synthesis output, with
  SPHINCS+-SHAKE-128f co-signatures held in cold archival storage for
  stateless long-term verifiability.
- Cryptographic forgetting via per-scope DEK destruction, FTS5
  plaintext purge, and page rewrite so a vacuumed database carries no
  recoverable bytes.
- `KeyStorage` trait with hardware-backed implementations supplied by
  host shells (iOS Keychain, Android Keystore, Windows DPAPI,
  libsecret, Nitro / SEV-SNP TEE).
- Zanzibar-style permission service with reachability checks and
  per-scope isolation.
- Proposal-only agent contract: agents cannot write canonical state.
- Supply-chain controls: `cargo-audit`, `cargo-deny`, `cargo-fuzz`,
  and a CycloneDX SBOM in CI.

### Schema baseline

- The on-device evidence store ships a single initial schema, stamped
  `PRAGMA user_version = 1`. 1.0 carries **no migration path** from the
  pre-release internal iterations: databases created by pre-1.0 builds
  are not supported and must be recreated from source data. Opening a
  pre-release database fails fast with a schema error rather than
  attempting an in-place upgrade.

### Performance

Measured on reference hardware (AMD EPYC 7763, 8 vCPU, 31 GiB). See
[docs/technical/benchmarks.md](docs/technical/benchmarks.md) for the
full suite and methodology.

- Ingest throughput (100K messages): ~1,043 msgs/sec.
- FTS phrase query (100K rows, 50 scopes): p50 13.56 ms.
- Hybrid retrieval (10K rows): 9.70 ms.
- Decay sweep (100K objects): 5.26 ms (~19M rows/sec).
- AEAD encrypt 64 KB: 80.4 µs (778 MiB/s).
- Hybrid KEM encapsulation (X25519 + ML-KEM-768): 159.9 µs.
- ML-DSA-65 sign / verify: 320 µs / 77 µs.
- Storage per message (at 500K): 612 bytes.
- Connector sync (10K docs): ~6,750 docs/sec.

[Unreleased]: https://github.com/kennguy3n/knowledge/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/kennguy3n/knowledge/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/kennguy3n/knowledge/releases/tag/v1.1.0
[1.0.0]: https://github.com/kennguy3n/knowledge/releases/tag/v1.0.0
