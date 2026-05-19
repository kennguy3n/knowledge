# Knowledge — Privacy-First Continual Knowledge System

Knowledge is a continual, always-fresh knowledge / context substrate
for AI that puts privacy at the centre and adopts post-quantum
cryptography from day one. It is the shared cognitive substrate
behind the KChat platform — one layered memory system serving both
the consumer surface (B2C) and the enterprise surface (B2B) without
forking the substrate.

If you are evaluating this project for the first time, jump to
[Evaluating this project?](#evaluating-this-project) below. For the
product thesis and system design, read [docs/DESIGN.md](./docs/DESIGN.md).
For the component map, data flow, and crypto layer, read
[ARCHITECTURE.md](./ARCHITECTURE.md). For per-platform tuning, read
[docs/PLATFORMS.md](./docs/PLATFORMS.md).

---

## Quick start

The shared core is a Cargo workspace targeting **Rust 1.85+ (stable)**.

### Prerequisites

- A stable Rust toolchain (`rustup install stable` and
  `rustup component add clippy rustfmt`).
- A C toolchain that can build the bundled SQLCipher + OpenSSL
  sources used by `rusqlite`'s `bundled-sqlcipher-vendored-openssl`
  feature (on Debian / Ubuntu: `sudo apt install build-essential`).

### Build, test, lint, demo

```bash
cargo build --all-targets
cargo test --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo run -p demo --release
```

The first build compiles `openssl-src` and SQLCipher and is
therefore slow (a few minutes); incremental rebuilds are fast. CI
runs the same fmt / clippy / build / test sequence on every push
and pull request.

The `demo` crate is a public-API-only end-to-end driver that seeds
a multi-scope dataset and walks every advertised stage of the
substrate using only the public APIs of each crate. It writes a
reconciled report to `results/demo_results.md` and an integration
test re-spawns it to validate the contract.

---

## Evaluating this project?

If you are reading this to decide whether the substrate is the
right fit for an integration, a paper, or a review, this is the
short version:

- **What it is.** A Rust workspace that implements a layered
  privacy-first memory substrate — encrypted evidence plane,
  observation engine, decay state machine, sparse concept graph,
  scope-window synthesis, portable concept exports, and an
  on-device inference router. The same substrate serves a
  consumer (B2C) and an enterprise (B2B) surface over the same
  memory model.
- **What is real today.** Real SQLCipher persistence, real
  XChaCha20-Poly1305 AEAD, real hybrid X25519 + ML-KEM-768
  (Kyber) KEM, real ML-DSA-65 (Dilithium) provenance signatures,
  the full decay state machine, hybrid (lexical + semantic +
  recency) retrieval, the concept graph, Zanzibar-style
  permissions, the export plane and policy simulator, the audit
  log, and the synthesis-pipeline window manager + GBNF schema +
  AEAD publish / consume path. An on-device inference router
  with an HTTP `llama-server` adapter ships and is exercised by
  an integration test.
- **What is partially wired.** The platform FFI surface wires
  the core evidence store, cryptography, and memory
  management: `open_store`, `close_store`, `ingest_message`,
  `query`, `get_evidence`, `forget`, `forget_scope`, `encrypt`,
  `decrypt`, `generate_keypair`, `get_user_memory`, `pin`,
  `unpin`, `list_memories`, `run_decay_sweep`,
  `get_channel_memory`, `escape_fts_query` are live.
  `trigger_synthesis` returns `Unavailable`.
- **What is contract-only.** Connectors (Drive, OneDrive,
  Notion, Jira, Confluence, Figma, HubSpot, Slack, Email) are
  fixture parsers with no live OAuth2 transport. The server-side
  synthesis service is a Rust skeleton; the Go gateway lives
  outside this repo.
- **What it is not.** Not a chat-with-your-files product, not a
  vector database with a UI bolted on top, not a production
  release. It is a substrate for surfaces, not a surface itself.

### Known limitations

The most important honesty caveats for anyone evaluating privacy
claims:

- **FTS5 plaintext index purge is best-effort.** `forget()` and
  `forget_scope()` now call `purge_fts_for_scope()` to delete
  FTS5 tokens for the forgotten scope, and persisted tombstones
  ensure the purge survives a crash / restart. However, SQLite
  FTS5 `DELETE` may leave residual data in shadow tables until
  `OPTIMIZE` or `REBUILD` is run.
- **Platform shells are partially wired.** The Rust FFI core
  covers evidence, crypto, and memory management; synthesis
  remains `Unavailable`. Wiring the host UIs
  is not yet implemented.

---

## Project structure

```
knowledge/
├── Cargo.toml                 # workspace manifest
├── rustfmt.toml               # repo-wide formatting config
├── results/                   # generated demo reports
├── crates/
│   ├── crypto/                # post-quantum and classical primitives
│   ├── evidence_store/        # encrypted evidence plane + hybrid retrieval
│   ├── memory_manager/        # decay state machine and memory objects
│   ├── observation_engine/    # observation extraction and pipelines
│   ├── concept_graph/         # sparse typed concept graph
│   ├── synthesis_pipeline/    # scope-window synthesis and publication
│   ├── synthesis_engine/      # server-side synthesis (engine + TEE worker)
│   ├── sync_engine/           # CRDT delta sync of synthesis objects
│   ├── permission_service/    # Zanzibar-style relations and reachability
│   ├── tenant_service/        # tenant lifecycle and member provisioning
│   ├── audit_service/         # append-only audit log
│   ├── agent_contract/        # proposal-only agent write contract
│   ├── export_plane/          # portable concept profiles and policies
│   ├── connector_framework/   # OAuth2, sync, webhooks, ACL sync
│   ├── connectors/            # vendor connector implementations
│   ├── inference_router/      # on-device inference routing and adapters
│   ├── reasoning_engine/      # contradictions, traversal, GoT, summaries
│   ├── ffi/                   # UniFFI surface for iOS and Android
│   ├── napi/                  # N-API addon for macOS and Windows
│   └── demo/                  # public-API-only end-to-end driver
├── .github/workflows/ci.yml   # fmt + clippy + build + test
├── ARCHITECTURE.md            # system architecture
├── docs/
│   ├── DESIGN.md              # product thesis and substrate design
│   ├── PLATFORMS.md           # per-platform integration notes
└── blog/                      # long-form write-ups
```

Each crate's public API is documented in its `src/lib.rs`. Run
`cargo doc --no-deps --open` to browse the rendered docs locally.

---

## Two surfaces, one substrate

Knowledge runs as two cooperating surfaces over a single shared
substrate, so the same memory model and the same synthesis rules
apply whether a fact came from a local DM or a SharePoint document.

### On-device surface

Native or near-native clients on every form factor a user actually
holds:

| Platform | Shell | Native bindings |
|---|---|---|
| iOS | Swift native UI | Rust shared core via UniFFI; Core ML / MLX for inference |
| Android | Kotlin native UI | Rust shared core via JNI; ONNX Runtime + llama.cpp NDK |
| macOS | Electron + React | Rust core via Swift N-API addon; MLX preferred runtime |
| Windows | Electron + React | Rust core via C++ N-API addon; DirectML EP + CPU EP |

The on-device surface ingests the streams a user already
generates and continuously builds an always-fresh knowledge
object scoped to the user, the channels they participate in,
and (optionally) the communities they belong to.

### On-server surface

A server-side surface that authenticates against shared document
management and collaboration systems and continuously builds an
always-fresh knowledge object scoped to a tenant's domains and
channels. Connectors cover Google Drive, OneDrive, Notion, Jira,
Confluence, Figma, HubSpot, Slack, and Email (Gmail + Microsoft
Graph).

Each connector follows the same `connector → evidence plane →
observation plane → semantic plane` pipeline as the on-device
surface, with ACLs synced from the source system and citations
preserved on every derived observation.

---

## Knowledge hierarchy (max 3 levels)

Knowledge objects are organised into a strict, scope-aware
hierarchy that synthesises upward — never the other way around.

**B2C (max 3 levels):**

```
user → community → channel
```

**B2B (max 3 levels per tenant):**

```
user → domain → channel
```

Synthesis is strict: only channel synthesis touches raw messages;
domain synthesis consumes channel outputs only; tenant-level
synthesis (where present) consumes domain outputs only. The
hierarchy is enforced cryptographically and at the type level —
see
[docs/DESIGN.md §6 Knowledge hierarchy and synthesis](./docs/DESIGN.md#6-knowledge-hierarchy-and-synthesis).

---

## Tech stack

| Layer | Stack |
|---|---|
| iOS shell | Swift native UI; Rust core via UniFFI |
| Android shell | Kotlin native UI; Rust core via JNI |
| macOS shell | Electron 31 + React; Rust core via Swift N-API addon |
| Windows shell | Electron 31 + React; Rust core via C++ N-API addon |
| Shared core | Rust workspace compiled to an iOS framework, Android `.so`s, and a macOS / Windows N-API addon |
| Local store | SQLCipher (AES-256-CBC + HMAC-SHA512 per page) + SQLite FTS5 + content-hash dedup |
| On-device inference | `llama-server` (PrismML [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism)) for Bonsai-1.7B; MLX runtime on Apple Silicon; ONNX Runtime for XLM-R embeddings |
| Server services | Go (API gateway, connectors, permissions, tenants, exports, audit) + Rust (synthesis engine, crypto, vector store) |
| Server storage | PostgreSQL (relational + provenance), pgvector (embeddings), MinIO / S3 (objects), NATS JetStream (async) |
| Sync | CRDT delta sync; MLS group keying with hybrid leaf KEMs for shared encrypted memory objects |
| Crypto | Hybrid X25519 + ML-KEM-768 KEM, ML-DSA-65 signatures, BLAKE3 hashing, XChaCha20-Poly1305 segments, SPHINCS+ as stateless backup |

The Rust shared core is the single source of truth for the
evidence store, the observation engine, the memory state machine,
the concept graph, the synthesis pipeline, the crypto layer, and
the sync engine. Every platform reuses it via FFI; UI is the only
thing each platform owns.

### Device tiering

The substrate is optimised for high-end, mid-tier, and low-end
devices — and on Windows for both CPU-only and CPU+GPU
configurations. Tiering is based on RAM, sustained compute, and
thermal envelope, mirroring the `slm-chat-demo`
[on-device model strategy](https://github.com/kennguy3n/slm-chat-demo/blob/main/docs/kchat-on-device-model-strategy.md):

| Tier | RAM | Compute strategy | SLM |
|---|---|---|---|
| Low | 2–3 GB | Lexicon classifiers + XLM-R INT4 embeddings only; no SLM | Disabled |
| Medium | 4–6 GB | XLM-R INT8 + Bonsai-1.7B SLM gated to active scope synthesis | Gated |
| High | 8+ GB | Always-on Bonsai-1.7B (MLX 2-bit on Apple Silicon, GGUF Q4_K_M elsewhere) | Always |

Graceful degradation is the rule: low-tier devices never enter
the SLM path; medium-tier devices gate the SLM behind heat /
battery / RAM checks; the substrate always remains queryable on
lexicon + XLM-R retrieval even when the SLM is unavailable. See
[docs/PLATFORMS.md](./docs/PLATFORMS.md) for the per-platform
breakdown.

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
   key is gone. See [Known limitations](#known-limitations) above
   for the FTS5 caveat.
3. **Post-quantum thinking from day one.** All new key exchanges
   use a hybrid X25519 + ML-KEM-768 (Kyber) construction;
   provenance and manifest signing use ML-DSA-65 (Dilithium); a
   stateless SPHINCS+ backup is kept for high-assurance signing.
   Group keying for shared channel / domain memory uses MLS with
   post-quantum extensions.

See [docs/DESIGN.md §9](./docs/DESIGN.md#9-post-quantum-cryptography)
and [ARCHITECTURE.md §8](./ARCHITECTURE.md#8-post-quantum-crypto-layer)
for the cryptographic details.

---

## Where to read more

- [docs/DESIGN.md](./docs/DESIGN.md) — product thesis,
  strategic principles, layered substrate, memory model,
  model strategy, hierarchy, permissions, deployment modes,
  post-quantum cryptography, and the integration surface.
- [ARCHITECTURE.md](./ARCHITECTURE.md) — concrete component
  map, Rust shared core, on-device inference, server
  architecture, data flow, permissions, decay state machine,
  and post-quantum crypto layer.
- [docs/PLATFORMS.md](./docs/PLATFORMS.md) — device-tuning and
  per-platform integration notes for iOS, Android, macOS, and
  Windows.
- [blog/adaptive-memory-storage-for-on-device-ai.md](./blog/adaptive-memory-storage-for-on-device-ai.md)
  — long-form write-up of the adaptive on-device memory model.

---

## Reference repositories

| Repo | Role |
|---|---|
| [`kennguy3n/slm-chat-demo`](https://github.com/kennguy3n/slm-chat-demo) | Reference for model selection, device tiering, and the cross-repo on-device model strategy ([`docs/kchat-on-device-model-strategy.md`](https://github.com/kennguy3n/slm-chat-demo/blob/main/docs/kchat-on-device-model-strategy.md)). |
| [`kennguy3n/llama.cpp@prism`](https://github.com/kennguy3n/llama.cpp/tree/prism) | Modified llama.cpp runtime (PrismML `prism` branch) used as the on-device SLM serving layer for Bonsai-1.7B. Ships SIMD repack kernels for the `Q1_0_g128` ternary format across CUDA, Metal, Vulkan, AVX-512 VNNI, AVX-VNNI, AVX2, and ARM NEON. |

These are the two upstreams Knowledge depends on; everything else
in the substrate is implemented in this repo.
