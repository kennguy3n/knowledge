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
  vector database with a UI on top, not a production release.
  It is a substrate for surfaces, not a surface itself.

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

### Platform FFI surface — partially wired

The UniFFI (iOS / Android) and N-API (macOS / Windows) surfaces
expose the core lifecycle, evidence, crypto, and memory APIs.
The currently live entry points are:

`open_store`, `close_store`, `ingest_message`, `query`,
`get_evidence`, `forget`, `forget_scope`, `encrypt`, `decrypt`,
`generate_keypair`, `get_user_memory`, `pin`, `unpin`,
`list_memories`, `run_decay_sweep`, `get_channel_memory`, and
`escape_fts_query`.

`trigger_synthesis` returns `Unavailable` until the on-device
SLM inference path lands. Host UI shells (Swift, Kotlin,
Electron) are out of scope for this repository.

### Connectors and server-side surface — contract-only

The nine connectors (Google Drive, OneDrive, Notion, Jira,
Confluence, Figma, HubSpot, Slack, Email) are fixture parsers
implementing the substrate's `Connector` trait. Live OAuth2
transport, webhook subscription, and incremental delta sync are
not yet implemented. The server-side synthesis service is a
Rust skeleton in this repository; the Go gateway lives outside.

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
| `synthesis_engine` | Server-side synthesis skeleton: managed-endpoint synthesizer wrapper and confidential-compute TEE worker scaffold. |
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
