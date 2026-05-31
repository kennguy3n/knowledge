# Knowledge

A privacy-first continual knowledge and context substrate for AI,
written in Rust with post-quantum cryptography from day one.

Knowledge is the shared cognitive substrate behind the KChat
platform. It serves a consumer (B2C) and an enterprise (B2B)
surface over a single memory model — one layered, decaying,
scope-aware memory plane that surfaces (chat, search, agents,
exports) consume instead of reaching back into raw evidence on
every call.

> **Status: pre-1.0.** The Rust shared core, the encrypted
> evidence plane, the observation and decay engines, the concept
> graph, the synthesis pipeline, the export plane, and the
> post-quantum cryptography are wired and exercised end-to-end by
> an in-tree demo. The mobile (iOS / Android) and desktop
> (macOS / Windows) host shells, the live connector OAuth2
> transport, and the server-side synthesis service are scaffolded
> but not wired to production endpoints. See
> [Status](#status) for the precise wiring state.

- **What it is.** A Rust workspace that implements a layered
  privacy-first memory substrate: encrypted evidence plane,
  observation engine, decay state machine, sparse concept graph,
  scope-window synthesis, portable concept exports, and an
  on-device inference router. The same substrate serves both
  surfaces over the same memory model.
- **What it isn't.** Not a chat-with-your-files product, not a
  vector database with a UI on top, and not a host surface. The
  API is pre-1.0 — the wire shape may shift before the first
  tagged release — but the internals are production-grade:
  real SQLCipher storage, real per-scope DEK forgetting, real
  HTTP transport for connectors, real on-device inference
  dispatch, real metrics + health envelope. This is a substrate
  for surfaces, not a surface itself.

---

## Quick start

The shared core is a Cargo workspace. Build, test, lint, and
run the end-to-end demo with:

```bash
cargo build --all-targets
cargo test --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo run -p demo --release
```

### Prerequisites

- A stable Rust toolchain on **Rust 1.85+**. Install with
  `rustup install stable` and add `clippy` and `rustfmt` via
  `rustup component add clippy rustfmt`.
- A C toolchain that can build the bundled SQLCipher + OpenSSL
  sources used by `rusqlite`'s
  `bundled-sqlcipher-vendored-openssl` feature. On Debian / Ubuntu:
  `sudo apt install build-essential`.

The first build compiles `openssl-src` and SQLCipher and is slow
(a few minutes); incremental rebuilds are fast.

### The `demo` binary

`cargo run -p demo --release` drives a synthetic multi-scope
dataset through every public substrate API — evidence
ingestion, observation extraction, memory management, the
concept graph, the synthesis pipeline, permissions, crypto,
export, the agent contract, reasoning, connectors, and audit —
and writes a reconciled markdown report to
`results/demo_results.md`. An in-tree integration test re-runs
the binary to pin the public contract.

CI runs the same fmt / clippy / build / test sequence on every
push and pull request.

---

## Status

The substrate is split into three cooperating surfaces — an
on-device surface, a server surface for connector-based ingestion,
and an inference layer that serves both. The wiring state of each
piece:

### Shared core (Rust workspace) — wired

- SQLCipher-backed encrypted evidence plane with content-aware
  storage routing (inline ≤ 512 B / dedup body table > 512 B /
  noise ring buffer), per-scope wraps, and FTS5 lexical index.
- XChaCha20-Poly1305 AEAD; hybrid X25519 + ML-KEM-768 (Kyber)
  KEM; ML-DSA-65 (Dilithium) provenance signatures; SPHINCS+
  reserved for archival co-signing.
- Observation engine (lexicon-first extraction), decay state
  machine, retention scoring, working memory.
- Sparse typed concept graph with supersession and contradiction
  edges; persistence layer.
- Hybrid (lexical + semantic + recency) retrieval.
- Zanzibar-style relation graph with reachability checks.
- Tenant lifecycle, append-only audit log.
- Synthesis-pipeline window manager, GBNF schema, and AEAD
  publish / consume path.
- Portable concept profile exports and a policy simulator.
- Cryptographic forgetting via per-scope DEK destruction, with a
  `DELETE` + `REBUILD` FTS5 purge in the same transaction.
- CRDT-based delta sync of synthesis objects.
- On-device inference router with an HTTP `llama-server`
  adapter, exercised by an integration test against a real
  `llama-server` process when `LLAMA_SERVER_BINARY` /
  `LLAMA_SERVER_MODEL` are set.

### Platform FFI surface — wired

The UniFFI (iOS / Android) and N-API (macOS / Windows) surfaces
expose the core lifecycle, evidence, crypto, memory, and
synthesis APIs. The currently **shared** entry points (mirrored
across both bindings) are:

`open_store`, `close_store`, `ingest_message`, `query`,
`get_evidence`, `forget`, `forget_scope`, `encrypt`, `decrypt`,
`generate_keypair`, `get_user_memory`, `pin`, `unpin`,
`list_memories`, `run_decay_sweep`, `get_channel_memory`,
`escape_fts_query`, `trigger_synthesis`, `health_check`,
`create_connector`, `authenticate_connector`, `sync_connector`,
`refresh_connector_token`, `list_connectors`, `remove_connector`,
`set_oauth_client_secret_resolver`,
`clear_oauth_client_secret_resolver`,
`set_key_storage_resolver`, `clear_key_storage_resolver`,
`open_store_with_resolver`,
`start_webhook_server`, `stop_webhook_server`,
`register_webhook_dispatch`, `unregister_webhook_dispatch`,
`list_webhook_servers`,
`start_sync_scheduler`, `stop_sync_scheduler`,
`configure_sync_schedule`, `clear_sync_schedule`,
`sync_scheduler_status`, `configure_sync_auto_synthesize`,
`configure_synthesis_engine`, `trigger_server_synthesis`,
`synthesis_status`, `list_recent_syntheses`,
`admit_approved_document`, `revoke_approved_document`,
`replace_approved_document`, `list_approved_documents`,
`try_init_tracing`.

The three master-key resolver entry points
(`set_key_storage_resolver`, `clear_key_storage_resolver`,
`open_store_with_resolver`) follow the same registration shape
as the OAuth-secret resolver: hosts hand in a callback object
backed by Keychain (iOS), Keystore (Android), DPAPI (Windows),
or any platform-specific secure-element wrapper, and the
substrate consumes it via the `KeyStorageResolver` contract
documented at `crates/ffi/src/key_storage.rs:26-110` (module
docs at 26-60, trait declaration + three method signatures —
`store_key` / `load_key` / `delete_key` — at 92-110). The
cold-boot integration point is `open_store_with_resolver(path,
key_id, resolver)` — hardware-backed hosts call this instead
of `open_store(path, master_key_hex)` so the 32-byte master
key never enters the host's address space as a long-lived
plaintext string. See `crates/ffi/src/runtime.rs`
`open_store_with_resolver` for the substrate consumer and
`crates/napi/src/bindings.rs` `js_open_store_with_resolver` for
the N-API adapter.

Three entry points are intentionally surface-specific rather than
mirrored across both bindings:

* `init` — **N-API only.** A JS-facing bootstrap helper
  (`crates/napi/src/lib.rs:86`) that parses a JSON config blob and
  primes the core for Electron / Node hosts. Mobile hosts (iOS /
  Android) drive the equivalent setup through their native shell
  init sequence (Swift `init` / Kotlin `Application.onCreate`), so
  there's no UniFFI export.
* `core_version` — **N-API only.** A JS-facing bootstrap helper
  (`crates/ffi/src/health.rs:396-405`) that returns the workspace
  semver baked into the build. Mobile hosts read the same value out
  of the embedded `Info.plist` / Gradle `BuildConfig`, so there's no
  UniFFI export.
* `metrics_snapshot` — **UniFFI only.** A read-only diagnostics
  surface for mobile observability tiles (iOS / Android). The
  N-API surface exposes the same data through `health_check`'s
  envelope, so an extra entry point would be redundant on Electron.

See
[Observability — metrics, tracing, health](#observability--metrics-tracing-health)
for the full surface contract.

The N-API surface in `crates/napi` exposes these as `camelCase`
JS names (e.g. `openStore`, `ingestMessage`, `coreVersion`,
`healthCheck`) via `napi-derive`'s standard rename; the Rust /
UniFFI surface keeps the `snake_case` names above. `init`,
`core_version`, and `health_check` are JS-facing bootstrap
helpers — `init` parses a JSON config blob and primes the core
(N-API only — see the surface-specific list above),
`core_version` returns the workspace semver baked into the build
(N-API only),
and `health_check` returns a `HealthStatus` envelope sourced
from the substrate's metrics + tracing layer (see
[Observability — metrics, tracing, health](#observability--metrics-tracing-health)
below). `healthCheck()` called without a handle returns a
bridge-only envelope; called with an open-store handle it
includes per-subsystem probes (`evidence_store`, `crypto`,
`memory_manager`, `inference_router`, `connector`,
`synthesis_engine`) that run real I/O against the open runtime.

`trigger_synthesis` dispatches a `SynthSummary` task through the
on-device `InferenceRouter`, persists the resulting recap into
the scope's `ChannelMemoryObject`, and flushes the channel memory
blob to disk — returning the synthesis object id (or
`FfiError::InferenceFailure` if the SLM produced unusable output,
`FfiError::Unavailable` if no adapter is bootstrapped for the
current build / device tier). The router currently runs the
llama.cpp adapter (behind the `http-client` feature against a
local `llama-server`); the MLX and fallback adapters are wired as
follow-on integration points (callback bridge and lexicon-based
classifier respectively). Host UI shells (Swift, Kotlin,
Electron) are out of scope for this repository.

`trigger_server_synthesis` is the server-side counterpart
(Phase 7). It dispatches a domain- or tenant-tier synthesis run
for the named scope through the configured
`HttpManagedEndpointSynthesizer`, gathers admissible inputs
(channel outputs feeding a domain; domain outputs and approved
documents feeding a tenant), enforces the hierarchy constraints
imposed by `synthesis_pipeline`, and persists the resulting
`SynthesisObject` into the encrypted evidence store. Dispatch
follows the same gather → dispatch → apply locking discipline as
`trigger_synthesis` and `sync_connector`: the runtime mutex is
held while gathering inputs and re-acquired to apply the outcome,
but the multi-second HTTP call runs unlocked. Returns the
synthesis window UUID, which the host polls via
`synthesis_status` or enumerates via `list_recent_syntheses`. The
engine slot is wired through `configure_synthesis_engine` (which
supports an optional scope-binding allow-list mirroring the
`TeeWorker` semantics) and lives behind the `http-client`
feature; minimal builds reject configuration with
`FfiError::Unavailable`. The scheduler's per-instance
`configure_sync_auto_synthesize` toggle dispatches a fire-and-
forget domain-tier synthesis after every successful sync,
subject to a per-scope cooldown.

### Observability — metrics, tracing, health

The substrate ships a process-wide observability layer in
`crates/ffi`. Every public FFI entry point increments lock-free
`AtomicU64` counters before delegating to the underlying core,
so the snapshot is updated unconditionally — whether the call
succeeds, fails, or panics.

**Counters and gauges** are exposed via
[`ffi::metrics::snapshot()`](crates/ffi/src/metrics.rs) and
embedded in the health envelope. Counters include
`ingest_total`, `query_total`, `synthesis_triggered_total`,
`decay_sweeps_total`, `forgets_total`, `forget_scopes_total`,
`encrypt_total`, `decrypt_total`, `create_connector_total`,
`authenticate_connector_total`, `sync_connector_total`,
`refresh_connector_token_total`, `list_connectors_total`,
`remove_connector_total`,
`set_oauth_client_secret_resolver_total`,
`clear_oauth_client_secret_resolver_total`,
`set_key_storage_resolver_total`,
`clear_key_storage_resolver_total`,
`start_webhook_server_total`, `stop_webhook_server_total`,
`register_webhook_dispatch_total`,
`unregister_webhook_dispatch_total`,
`list_webhook_servers_total`,
`webhook_dispatch_ok_total`,
`webhook_dispatch_bad_request_total`,
`webhook_dispatch_bad_gateway_total`,
`start_sync_scheduler_total`, `stop_sync_scheduler_total`,
`configure_sync_schedule_total`, `clear_sync_schedule_total`,
`sync_scheduler_status_total`, `sync_scheduler_ticks_total`,
`sync_scheduler_dispatches_attempted_total`,
`sync_scheduler_dispatches_succeeded_total`,
`sync_scheduler_dispatches_failed_total`,
`sync_scheduler_dispatches_skipped_in_progress_total`,
`configure_synthesis_engine_total`,
`trigger_server_synthesis_total`,
`trigger_server_synthesis_throttled_total` (Phase 10 Item 5 — rate-
shaping token-bucket rejections), `synthesis_status_total`,
`list_recent_syntheses_total`, `replay_synthesis_total` (Phase 10
Item 4 — versioned re-run of an existing window),
`configure_sync_auto_synthesize_total`,
`admit_approved_document_total`,
`revoke_approved_document_total`,
`replace_approved_document_total`,
`list_approved_documents_total`, `connector_status_total` (Phase 10
Item 3 — per-instance health probe symmetric with
`synthesis_status`), `stuck_pending_window_recovered_total` (Phase
10 Item 1 — age-based open_store sweep marking unrecoverable
Pending windows Failed-with-retry), plus per-`FfiError`-kind
counters under `errors_by_kind` (`unimplemented`, `invalid_id`,
`not_found`, `evidence`, `memory`, `synthesis`, `crypto`,
`unavailable`, `inference_failure`, `connector`, `throttled`).
Gauges include `open_handles` (live runtime registry size) and
`tombstone_count` (destroyed-DEK registry size on the most
recently observed handle).

**Health probe** is exposed as `ffi::health_check(handle:
Option<RuntimeHandle>)` and surfaced to JS as
`healthCheck(handle?)`. Returns a `HealthStatus` envelope with
`core_version`, `uptime_secs`, `tracing_initialized`, an
ordered `subsystems[]` array, and the full `metrics`
snapshot. With an open handle the probe runs real I/O —
`evidence_store` issues `SELECT COUNT(*)` against the open
SQLCipher connection, `crypto` verifies the master key is
non-zero, `memory_manager` reports rehydrated user / channel
memory counts, `inference_router` returns per-adapter
availability via `InferenceRouter::adapter_states()`, and
`connector` reports the per-runtime connector-instance count,
authenticated-token count, and the per-`SyncStatus` distribution
across all registered connectors (downgrading to `Degraded` if
any connector is in `Failed`), and `synthesis_engine` reports
whether the server-side synthesis slot is configured, the
current window / object / domain-memory / tenant-memory counts,
and whether a scope-binding allow-list is installed (downgrading
to `Degraded` if the slot is configured but no allow-list is
set). Without a handle (or with the `0n` sentinel) the probe
returns a bridge-only envelope suitable for an Electron
`app.whenReady` liveness check before `openStore`.

**Tracing** events are emitted via the `tracing` facade
throughout the workspace. To install a global subscriber from
the host enable the `tracing-subscriber` feature on `ffi` /
`napi_addon` and call `ffi::try_init_tracing(directive)` (Rust)
or `initTracing(directive)` (JS / N-API), or
`tryInitTracing(directive)` from the UniFFI-generated
Swift / Kotlin bindings (iOS / Android hosts that prefer the
substrate's `fmt::Layer + EnvFilter` stack over installing
`tracing-subscriber` directly from Swift / Kotlin). The
directive uses
[`tracing_subscriber::EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)'s
`RUST_LOG` syntax — `EnvFilter` uses `::` as the hierarchy
separator (not `_` or `-`), and each workspace crate is its own
target. To enable debug across the substrate, enumerate the
targets explicitly, e.g.
`RUST_LOG=ffi=debug,evidence_store=debug,inference_router=info`.
The call is idempotent: a second invocation is a no-op so the
host can install at startup without guarding against re-init.

### Connectors and server-side surface

The nine connectors (Google Drive, OneDrive, Notion, Jira,
Confluence, Figma, HubSpot, Slack, Email) all run over the
shared `HttpTransport` machinery in `connector_framework` —
`BlockingHttpTransport` (reqwest, behind the `http-client`
feature) with exponential backoff, `Retry-After` parsing, and
configurable per-request timeout. OAuth2 flows go through
the real `OAuth2Client` / `ConfiguredRefresher`
(`authorization_code` + `refresh_token` grants against any
RFC-6749 token endpoint). Unit tests inject the framework's
`MockHttpTransport` so the connector's request shape and
response parsing are exercised end-to-end without crossing the
network. Webhook subscription registration and incremental
delta sync use the same transport ladder.

The server-side synthesis service is a first-class Rust
subsystem in this repository: `synthesis_engine` implements the
`SynthesisEngine` trait via `HttpManagedEndpointSynthesizer` (a
`reqwest::blocking::Client` adapter wrapped in the same retry /
timeout / `Retry-After` ladder used by connectors), gated behind
the `http-client` feature. The FFI surface exposes it through
`configure_synthesis_engine`, `trigger_server_synthesis`
(rate-shaped by a token-bucket gate that surfaces
`FfiError::Throttled` with the `Retry-After` window the host
should honour), `synthesis_status`, `list_recent_syntheses`, and
`replay_synthesis` (Phase 10 Item 4 — re-runs an existing window
through the engine and writes a versioned
`synthesis_object_versions` row so callers can inspect every
historical output for a given window). Hierarchy enforcement and
scope-binding checks are applied at the FFI layer before the HTTP
dispatch. Connector instances expose a symmetric per-instance
health probe via `connector_status` (Phase 10 Item 3). The Go
gateway / SLM frontends live outside this repository.

---

## Project structure

```
knowledge/
├── Cargo.toml                 workspace manifest
├── rustfmt.toml               formatter config
├── deny.toml                  cargo-deny advisory + license config
├── crates/                    workspace crates (see table below)
├── docs/
│   ├── DESIGN.md              product thesis, planes, memory model
│   └── PLATFORMS.md           per-platform tuning and integration
├── blog/                      long-form write-ups
├── results/                   generated demo reports (gitignored)
├── ARCHITECTURE.md            implementation architecture
├── CONTRIBUTING.md            build, test, and contribution guide
├── SECURITY.md                vulnerability disclosure + threat model
└── README.md                  this file
```

| Crate | Responsibility |
|---|---|
| `crypto` | Post-quantum primitives: hybrid X25519 + ML-KEM-768 KEM, ML-DSA-65 and SPHINCS+ signatures, XChaCha20-Poly1305 AEAD, BLAKE3 hashing. |
| `evidence_store` | SQLCipher-backed encrypted evidence plane, content-hash dedup, FTS5 lexical index, hybrid retrieval. |
| `observation_engine` | Lexicon-first extraction of entities, facts, tasks, and decisions; importance-classified pipeline. |
| `memory_manager` | Decay state machine, retention scoring, working memory, channel / domain / tenant memory objects, privacy-strip invariant. |
| `concept_graph` | Sparse typed concept graph with supersession, contradiction, and typed-edge traversal; encrypted persistence. |
| `synthesis_pipeline` | Scope-window synthesis (channel / domain / tenant), GBNF schema types, elected-device election, encrypted publish / consume. |
| `synthesis_engine` | Server-side synthesis engine: `HttpManagedEndpointSynthesizer` (reqwest blocking adapter, gated on `http-client`), deterministic test-stub synthesizer, and confidential-compute `TeeWorker` wrapper with scope-binding enforcement. |
| `sync_engine` | CRDT add-wins set + append-only operation log with merge; SQLCipher-backed persistence. |
| `permission_service` | Zanzibar-style relation graph with reachability checks. |
| `tenant_service` | Tenant lifecycle (create / activate / suspend / delete), per-tenant configuration, member provisioning. |
| `audit_service` | Append-only audit log of canonical promotions, exports, agent proposals, and policy changes. |
| `agent_contract` | Proposal-only write contract for software agents: typed proposals, lifecycle, promotion to canonical. |
| `export_plane` | Portable concept profiles, export policies, controls, and a read-only policy simulator. |
| `connector_framework` | OAuth2 token vault, sync state, webhook subscription, channel-scoped attachment, and ACL sync. |
| `connectors` | Vendor connector implementations (Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, Email). |
| `inference_router` | On-device inference routing across MLX, `llama.cpp`, and a fallback adapter, with device-tier gating. |
| `reasoning_engine` | Contradiction and drift detection, multi-hop traversal, query planning, workflow memory, community summaries. |
| `ffi` | UniFFI bindings surface for iOS and Android. |
| `napi` | N-API addon surface for macOS and Windows. |
| `demo` | Public-API-only end-to-end driver that exercises every crate against a synthetic dataset. |
| `integration_tests` | Cross-crate integration tests that pin the workspace contract. |

Each crate's public API is documented in its `src/lib.rs`. Run
`cargo doc --no-deps --open` to browse the rendered docs.

---

## Two surfaces, one substrate

Knowledge runs as two cooperating surfaces over a single shared
substrate, so the same memory model applies whether a fact came
from a local message or a SharePoint document.

| Surface | Shell | Native bindings | Inference |
|---|---|---|---|
| iOS | Swift native UI | Rust core via UniFFI | Core ML / MLX |
| Android | Kotlin native UI | Rust core via JNI | ONNX Runtime + `llama.cpp` NDK |
| macOS | Electron + React | Rust core via N-API (Swift bridge) | MLX preferred |
| Windows | Electron + React | Rust core via N-API (C++ bridge) | DirectML EP + CPU EP |
| Server | Go API gateway + Rust synthesis | (out of repo) | Managed endpoint or TEE worker |

The on-device surface ingests the streams a user already
generates and builds an always-fresh knowledge object scoped to
the user, the channels they participate in, and any communities
they belong to.

The on-server surface authenticates against shared document
management and collaboration systems through the connector
inventory and builds an always-fresh knowledge object scoped to
the tenant's domains and channels. Connector ACLs are mirrored
into the substrate's relation graph; citations are preserved on
every derived observation.

The full design — including the layered six-plane substrate,
the memory model, the synthesis hierarchy, and the deployment
modes — lives in [docs/DESIGN.md](./docs/DESIGN.md). The
component map, data flow, server architecture, and crypto layer
live in [ARCHITECTURE.md](./ARCHITECTURE.md).

---

## Privacy and post-quantum cryptography

Privacy is the substrate, not a feature. Three properties hold
by construction:

1. **Raw evidence is encrypted at rest, locally, with keys the
   user or the tenant holds.** Cross-device sync moves synthesis
   objects, not raw bodies. Raw evidence stays on the originating
   device unless policy explicitly allows otherwise.
2. **Cryptographic forgetting via key destruction.** Deletion is
   enforced by destroying the per-scope or per-epoch keys, not
   by best-effort row deletes. A scope is gone the moment its
   key is gone. The FTS5 plaintext index for the forgotten scope
   is purged in the same transaction (`DELETE` + `REBUILD`) so
   no residual tokens survive on disk.
3. **Post-quantum from day one.** All new key exchanges use a
   hybrid X25519 + ML-KEM-768 (Kyber) construction. Provenance
   and manifest signing use ML-DSA-65 (Dilithium); SPHINCS+ is
   reserved for high-assurance archival co-signing. Group keying
   for shared channel / domain memory uses MLS with post-quantum
   extensions.

The cryptographic details are in
[docs/DESIGN.md §9](./docs/DESIGN.md#9-post-quantum-cryptography)
and [ARCHITECTURE.md §8](./ARCHITECTURE.md#8-post-quantum-crypto-layer).

---

## Device tiering

The substrate is optimised for high-end, mid-tier, and low-end
devices, mirroring the
[KChat on-device model strategy](https://github.com/kennguy3n/slm-chat-demo/blob/main/docs/kchat-on-device-model-strategy.md):

| Tier | RAM | Compute strategy | SLM |
|---|---|---|---|
| Low | 2 – 3 GB | Lexicon classifiers + XLM-R INT4 embeddings only. | Disabled |
| Medium | 4 – 6 GB | XLM-R INT8 + Bonsai-1.7B gated to active scope synthesis. | Gated |
| High | 8+ GB | XLM-R INT8 + always-on Bonsai-1.7B (MLX 2-bit on Apple Silicon, GGUF Q4_K_M elsewhere). | Always |

Graceful degradation is the rule: low-tier devices never enter
the SLM path; medium-tier devices gate the SLM behind heat,
battery, and RAM checks; the substrate always remains queryable
on lexicon + XLM-R retrieval even when the SLM is unavailable.
See [docs/PLATFORMS.md](./docs/PLATFORMS.md) for the per-platform
breakdown.

---

## Where to read more

- [docs/DESIGN.md](./docs/DESIGN.md) — product thesis,
  strategic principles, layered substrate, memory model and
  decay machine, on-device model strategy, knowledge hierarchy,
  permissions and agent writes, deployment modes, post-quantum
  cryptography, and connector integration.
- [ARCHITECTURE.md](./ARCHITECTURE.md) — concrete component
  map, Rust shared core, on-device inference, server
  architecture, data flow, permissions, decay state machine,
  and post-quantum crypto layer.
- [docs/PLATFORMS.md](./docs/PLATFORMS.md) — device-tuning and
  per-platform integration notes for iOS, Android, macOS, and
  Windows.
- [CONTRIBUTING.md](./CONTRIBUTING.md) — build, test, and
  contribution workflow.
- [SECURITY.md](./SECURITY.md) — vulnerability disclosure
  process and threat model.
- [blog/adaptive-memory-storage-for-on-device-ai.md](./blog/adaptive-memory-storage-for-on-device-ai.md)
  — long-form write-up of the adaptive on-device memory model.
- [blog/ai-privacy-spectrum-across-industries.md](./blog/ai-privacy-spectrum-across-industries.md)
  — long-form write-up of the five AI processing modes (no AI,
  local AI only, local AI + external data sources via the
  server-side connector pipeline, hybrid TEE, full
  server-side), with a deep dive on how connector ownership
  (`ConnectorAttachment.scope_id`) determines whether
  connector-sourced knowledge becomes channel knowledge
  (shared) or user knowledge (private). Grounded in B2C and
  B2B scenarios across industries and jurisdictions, with a
  threat model covering external attackers, the KChat
  operator, and the infrastructure operator.

## Reference repositories

| Repo | Role |
|---|---|
| [`kennguy3n/slm-chat-demo`](https://github.com/kennguy3n/slm-chat-demo) | Reference for model selection, device tiering, and the cross-repo on-device model strategy ([`docs/kchat-on-device-model-strategy.md`](https://github.com/kennguy3n/slm-chat-demo/blob/main/docs/kchat-on-device-model-strategy.md)). |
| [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism) | Modified `llama.cpp` runtime (PrismML `prism` branch) used as the on-device SLM serving layer for Bonsai-1.7B. Ships SIMD repack kernels for the `Q1_0_g128` ternary format across CUDA, Metal, Vulkan, AVX-512 VNNI, AVX-VNNI, AVX2, and ARM NEON. |

These are the two upstreams Knowledge depends on; everything else
in the substrate is implemented in this repository.

---

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE)
  or <http://www.apache.org/licenses/LICENSE-2.0>), or
- MIT license ([LICENSE-MIT](./LICENSE-MIT) or
  <https://opensource.org/license/MIT>),

at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this work
by you, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or
conditions.
