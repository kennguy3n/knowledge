# Knowledge

A privacy-first, post-quantum secure knowledge substrate for AI applications.
On-device by default. **$0 / user / month** at any scale.

Knowledge is the shared cognitive substrate behind the KChat platform.
It provides layered, decaying, scope-aware memory that surfaces (chat,
search, agents, exports) consume — so they never have to reach back into
raw evidence on every call.

---

## Why Knowledge

| Capability | Knowledge | Server-side RAG | Vector DB + LLM |
|---|:---:|:---:|:---:|
| Per-user marginal cost (100M users) | **$0** | $0.10–$1.00 | $0.05–$0.50 |
| Data leaves user device | **No** | Yes | Yes |
| Post-quantum encryption (ML-KEM-768) | **Yes** | No | No |
| Cryptographic forgetting (key destruction) | **Yes** | Soft-delete | Soft-delete |
| Multilingual extraction (22 languages) | **Yes** | Language-dependent | Language-dependent |
| On-device SLM synthesis | **Yes** | N/A | N/A |
| Provenance on every synthesis | **Yes** | Rarely | No |
| Works fully offline | **Yes** | No | No |

See [docs/COST_MODEL.md](docs/COST_MODEL.md) for the full per-user
cost breakdown across B2C, Hybrid, and Enterprise deployment profiles.

---

## Getting started in 5 minutes

### Prerequisites

- Rust **1.85+** (`rustup install stable && rustup component add clippy rustfmt`)
- C toolchain for bundled SQLCipher + OpenSSL (Debian/Ubuntu: `sudo apt install build-essential`)
- Go 1.23+ (for the server surface)

### On-device demo (no server required)

```bash
git clone https://github.com/kennguy3n/knowledge.git
cd knowledge
cargo run -p demo --release
```

The demo drives a synthetic multi-scope dataset through every substrate
API — evidence ingestion, observation extraction, memory management,
concept graph, synthesis pipeline, permissions, crypto, export, agent
contract, reasoning, connectors, and audit — and writes a reconciled
report to `results/demo_results.md`.

### Server surface (Go gateway + Rust substrate)

```bash
# Terminal 1: start the Rust substrate server (HTTP loopback on :9090)
cargo run -p substrate_server --release

# Terminal 2: start the Go API gateway (serves :8080)
cd server && go run ./cmd/gateway

# Terminal 3: ingest → query → synthesize
curl -s -X POST http://localhost:8080/api/v1/ingest \
  -H "Content-Type: application/json" \
  -d '{"scope_id":"11111111-1111-1111-1111-111111111111","body":"We decided to use Rust for the rewrite.","source":"Manual","importance":"Important"}'

curl -s -X POST http://localhost:8080/api/v1/query \
  -H "Content-Type: application/json" \
  -d '{"scope_id":"11111111-1111-1111-1111-111111111111","query_text":"Rust rewrite","limit":10}'

curl -s -X POST http://localhost:8080/api/v1/synthesis/trigger \
  -H "Content-Type: application/json" \
  -d '{"scope_id":"11111111-1111-1111-1111-111111111111"}'
```

See [docs/QUICKSTART.md](docs/QUICKSTART.md) for full setup instructions
across all three deployment modes.

---

## Language support

The observation engine validates extraction across **22 languages**
via the built-in `LexiconRegistry` (decision / task / imperative /
stop-word tables per BCP-47 primary subtag):

| Language | Tag | Language | Tag | Language | Tag |
|---|---|---|---|---|---|
| Arabic | `ar` | German | `de` | Indonesian | `id` |
| Burmese | `my` | Hebrew | `he` | Italian | `it` |
| Chinese | `zh` | Hindi | `hi` | Japanese | `ja` |
| English | `en` | Khmer | `km` | Korean | `ko` |
| French | `fr` | Lao | `lo` | Malay | `ms` |
| Portuguese | `pt` | Russian | `ru` | Spanish | `es` |
| Tagalog | `tl` | Thai | `th` | Tibetan | `bo` |
| Vietnamese | `vi` | | | | |

Language detection runs per-sentence; the extractor resolves the
matching lexicon automatically and falls back to English when no
configured lexicon exists for the detected language.

---

## Architecture overview

Knowledge is split into three cooperating surfaces:

1. **On-device surface** — iOS (Swift/UniFFI), Android (Kotlin/JNI),
   macOS + Windows (Electron/N-API). Runs the full substrate locally.
2. **Server surface** — Go API gateway + Rust substrate loopback.
   Runs connector pipelines, cross-tenant synthesis, permissions, and
   audit.
3. **Inference layer** — On-device: MLX (Apple Silicon), llama.cpp
   (CPU/Metal), ONNX Runtime (XLM-R). Server: confidential compute
   (TEE) or managed AI endpoint.

```
On-device:  raw input → evidence plane → observation engine → memory manager
            → concept graph → synthesis pipeline → export plane

Server:     connector sync (OAuth2 + webhook) → same pipeline as above
            → domain/tenant synthesis (managed AI / TEE) → export
```

For the full module map, service topology, and data-flow diagrams, see
[ARCHITECTURE.md](ARCHITECTURE.md).

---

## Crate map

| Crate | Role |
|---|---|
| `evidence_store` | Encrypted SQLCipher storage, FTS5, hybrid retrieval |
| `observation_engine` | Multilingual entity/fact extraction, 22-language lexicon registry |
| `memory_manager` | Decay state machine, retention scoring, working memory |
| `concept_graph` | Sparse typed graph with supersession/contradiction edges |
| `synthesis_pipeline` | Scope-window synthesis, GBNF grammar, elected-device election |
| `synthesis_engine` | Server-side synthesis (TEE worker, managed endpoint) |
| `crypto` | Hybrid PQC (X25519+ML-KEM-768), ML-DSA-65, SPHINCS+, XChaCha20 |
| `inference_router` | On-device SLM dispatch (MLX → llama.cpp → fallback) |
| `connector_framework` | OAuth2 transport, webhook, incremental delta sync |
| `connectors` | 9 stable + 1 unstable: Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, Email, GitHub _(unstable)_ |
| `permission_service` | Zanzibar-style relation graph with reachability checks |
| `tenant_service` | Tenant lifecycle, per-tenant keys, member provisioning |
| `audit_service` | Append-only audit log |
| `agent_contract` | Proposal-only write contract for LLM agents |
| `export_plane` | Portable concept profiles, policy-gated evidence packs |
| `sync_engine` | CRDT-based delta sync of synthesis objects |
| `reasoning_engine` | Graph-of-Thought reasoning traces |
| `ffi` | UniFFI bindings (iOS/Android) |
| `napi` | N-API addon (Electron/Node.js) |
| `benchmarks` | Criterion.rs production benchmark suite |

---

## Performance

Collected on the reference hardware (AMD EPYC 7763, 8 vCPU, 31 GiB).
See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for the full suite.

| Metric | Result |
|---|---|
| Ingest throughput (100K messages) | **~1,043 msgs/sec** |
| FTS phrase query (100K rows, 50 scopes) | p50 **13.56 ms** |
| Hybrid retrieval (10K rows) | **9.70 ms** |
| AEAD encrypt 64 KB | 80.4 µs (778 MiB/s) |
| Hybrid KEM encap (X25519 + ML-KEM-768) | 159.9 µs |
| ML-DSA-65 sign / verify | 320 µs / 77 µs |
| Decay sweep (100K objects) | **5.26 ms** (19M rows/sec) |
| Storage per message (at 500K) | **612 bytes** |
| Connector sync (10K docs) | **~6,750 docs/sec** |

---

## Version compatibility

| Component | Current |
|---|---|
| Rust MSRV | 1.85.0 |
| Schema version | v8 |
| Wire format | MessagePack (sync), JSON (REST API) |
| FFI ABI | UniFFI 0.28 (iOS/Android), N-API 6 (Electron) |
| Go server | Go 1.23+ |
| Post-quantum | ML-KEM-768 (FIPS 203), ML-DSA-65 (FIPS 204) |

---

## Build & test

```bash
# Full build
cargo build --all-targets --all-features

# Test suite
cargo test --all --all-features

# Lint
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

# MSRV gate
cargo +1.85.0 build --all-features

# Go server
cd server && go test -race -count=1 ./...
cd server && golangci-lint run ./...

# Benchmarks (production suite, ~30 min)
cargo bench -p benchmarks
```

---

## Documentation

| Document | Contents |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Component map, module table, service topology, data flow |
| [docs/DESIGN.md](docs/DESIGN.md) | Product thesis, strategic principles, memory model |
| [docs/API_REFERENCE.md](docs/API_REFERENCE.md) | REST API endpoints, auth, rate limiting, SSE streaming |
| [docs/QUICKSTART.md](docs/QUICKSTART.md) | Three deployment modes: on-device, hybrid SME, enterprise |
| [docs/INTEGRATION_GUIDE.md](docs/INTEGRATION_GUIDE.md) | Embedding the substrate in host products |
| [docs/BENCHMARKS.md](docs/BENCHMARKS.md) | Criterion benchmark results and methodology |
| [docs/COST_MODEL.md](docs/COST_MODEL.md) | Per-user cost analysis across deployment profiles |
| [docs/COMPLIANCE.md](docs/COMPLIANCE.md) | GDPR / SOC 2 / HIPAA control mapping |
| [docs/SUPPLY_CHAIN.md](docs/SUPPLY_CHAIN.md) | Dependency policy, SBOM, cargo-deny/audit gates |
| [docs/PLATFORMS.md](docs/PLATFORMS.md) | Per-platform tuning (iOS, Android, macOS, Windows) |
| [docs/ELECTRON_SECURITY.md](docs/ELECTRON_SECURITY.md) | Electron N-API threat model and hardening checklist |
| [docs/HOST_KEY_HANDLING.md](docs/HOST_KEY_HANDLING.md) | Master-key management per platform |
| [SECURITY.md](SECURITY.md) | Vulnerability disclosure, threat model, audit scope |
| [CHANGELOG.md](CHANGELOG.md) | Release history |

---

## Security

- Post-quantum hybrid encryption (X25519 + ML-KEM-768)
- Cryptographic forgetting via DEK destruction
- Zanzibar-style permissions with reachability checks
- Proposal-only agent contract (agents cannot write canonical state)
- PROV-signed provenance on every synthesis
- `cargo-audit` + `cargo-deny` + CycloneDX SBOM in CI

Report vulnerabilities to **ken@uney.com** (see [SECURITY.md](SECURITY.md)).

---

## License

See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT) for details.
