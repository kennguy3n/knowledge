# Integration Guide

How to consume the Knowledge substrate from a host product (KChat,
third-party apps, custom agents).

---

## 1. Adding the workspace as a dependency

### Git submodule (recommended for monorepos)

```bash
git submodule add https://github.com/kennguy3n/knowledge.git deps/knowledge
```

Then reference the crates you need as path dependencies in your
product's `Cargo.toml`:

```toml
[dependencies]
knowledge_ffi      = { path = "deps/knowledge/crates/ffi" }
knowledge_crypto   = { path = "deps/knowledge/crates/crypto" }
evidence_store     = { path = "deps/knowledge/crates/evidence_store" }
```

### Git dependency (for standalone products)

```toml
[dependencies]
knowledge_ffi = { git = "https://github.com/kennguy3n/knowledge.git", package = "ffi" }
```

Pin a specific revision or tag when one is published:

```toml
knowledge_ffi = { git = "https://github.com/kennguy3n/knowledge.git", rev = "abc1234", package = "ffi" }
```

---

## 2. Which crates to depend on

Pick the smallest set of crates your use case requires:

| Use case | Crates | Notes |
|---|---|---|
| **Evidence ingest only** | `evidence_store`, `crypto` | Encrypted storage + query. No memory, no synthesis. |
| **Full on-device pipeline** | `ffi` (or `napi` for Electron) | Single entry point wrapping evidence, memory, observation, synthesis, connectors, crypto. |
| **Evidence + memory + observation** | `evidence_store`, `memory_manager`, `observation_engine`, `crypto` | Custom pipeline without synthesis or connectors. |
| **With connectors** | Add `connector_framework` + `connectors` to any of the above | OAuth2-based sync from Google Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, Email. |
| **With on-device inference** | Add `inference_router` | Dispatches SLM tasks through MLX / llama.cpp / Fallback. |
| **Server-side synthesis** | `synthesis_engine`, `synthesis_pipeline` | Domain and tenant summary generation via managed endpoints. |
| **Permissions only** | `permission_service` | Zanzibar-style relation graph, usable standalone. |
| **Agent integration** | `agent_contract` | Proposal-only write contract for LLM agents. |
| **Concept export** | `export_plane`, `concept_graph` | Portable concept profiles for external tools. |

> **Tip:** If you are building a mobile or desktop host shell, depend
> on `ffi` (UniFFI) or `napi` (N-API) rather than the internal crates
> directly. These surface crates are the stable consumer API.

---

## 3. Minimal code example

### Initialize store, ingest evidence, query, read synthesis

```rust
use crypto::{derive_key, ContentHash};
use evidence_store::EvidenceStore;

// 1. Open an encrypted store.
let master_key = crypto::MasterKey::generate();
let store = EvidenceStore::open("./data/my_scope.db", &master_key)?;

// 2. Ingest a message.
let id = store.ingest(
    "channel-1",
    "Alice",
    "We decided to use Rust for the rewrite.",
    evidence_store::ImportanceClass::Important,
)?;

// 3. Query.
let results = store.query("Rust rewrite", 10)?;
for r in &results {
    println!("{}: {}", r.id, r.snippet);
}

// 4. Read synthesis (requires an InferenceRouter to be wired).
// See the `ffi` crate's `trigger_synthesis` for the full flow.
```

### Via the FFI surface (Swift / Kotlin / Electron)

The `ffi` and `napi` crates expose the same logical contract.
A typical Electron integration:

```js
const knowledge = require('./knowledge.node');

// Initialize
knowledge.init(JSON.stringify({ dataDir: './data' }));
const handle = knowledge.openStore('/path/to/store.db', masterKeyHex);

// Ingest
knowledge.ingestMessage(handle, JSON.stringify({
  scope: 'channel-1',
  sender: 'Alice',
  body: 'We decided to use Rust for the rewrite.',
}));

// Query
const results = knowledge.query(handle, JSON.stringify({
  text: 'Rust rewrite',
  limit: 10,
}));

// Cleanup
knowledge.closeStore(handle);
```

---

## 4. Platform-specific build instructions

### Prerequisites (all platforms)

- Rust **1.85+** (`rustup install stable`)
- `clippy` + `rustfmt` (`rustup component add clippy rustfmt`)
- C toolchain for bundled SQLCipher + OpenSSL
  (Debian/Ubuntu: `sudo apt install build-essential`)

### iOS via UniFFI

The `crates/ffi` crate produces a static library consumed by Swift
through UniFFI-generated bindings.

```bash
# Install iOS targets
rustup target add aarch64-apple-ios x86_64-apple-ios aarch64-apple-ios-sim

# Build the static library
cargo build -p ffi --release --target aarch64-apple-ios

# Generate Swift bindings (uses crates/uniffi-bindgen)
cargo run -p uniffi-bindgen -- generate \
    crates/ffi/src/knowledge.udl \
    --language swift \
    --out-dir generated/swift/

# Create xcframework
xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/release/libknowledge_ffi.a \
    -headers generated/swift/ \
    -output Knowledge.xcframework
```

See `crates/ffi/scripts/build_ios.sh` if present for the canonical
build script.

### Android via JNI

```bash
# Install Android targets
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android

# Build with cargo-ndk (install: cargo install cargo-ndk)
cargo ndk -p ffi --release \
    -t arm64-v8a -t armeabi-v7a -t x86_64 \
    -o ./jniLibs

# Generate Kotlin bindings
cargo run -p uniffi-bindgen -- generate \
    crates/ffi/src/knowledge.udl \
    --language kotlin \
    --out-dir generated/kotlin/
```

### macOS / Windows desktop via N-API

The `crates/napi` crate produces a `.node` addon for Electron / Node.

```bash
cd crates/napi
npm install
npx napi build --platform --release
```

The resulting `.node` file is loaded by Node via
`require('./<platform>.node')`.

### Feature flags to enable

For a production build with full networking:

```toml
[dependencies]
ffi = { path = "deps/knowledge/crates/ffi", features = ["http-client"] }
```

For a minimal offline build (no reqwest, no network):

```toml
[dependencies]
ffi = { path = "deps/knowledge/crates/ffi" }
```

---

## 5. Feature flag reference

Features are declared per-crate. The table below covers
consumer-relevant flags; `test-support` flags are omitted (they
are for the substrate's own test suite).

| Crate | Feature | What it enables |
|---|---|---|
| `ffi` | `http-client` | Real reqwest-backed HTTP for inference (llama.cpp loopback), connectors (OAuth2 + sync), and server synthesis. Without it, network-dependent subsystems return `FfiError::Unavailable`. |
| `ffi` | `tracing-subscriber` | Installs a `tracing` subscriber via `try_init_tracing`. Without it, tracing events go nowhere (library-side default). |
| `napi` | `tracing-subscriber` | Same as above, forwarded into `ffi`. |
| `connector_framework` | `http-client` | Reqwest-backed `BlockingHttpTransport` + `OAuth2Client`. |
| `connector_framework` | `async-runtime` | Tokio + async-trait bridge for async connector driving. |
| `connector_framework` | `async-http-client` | Async reqwest transport (independent of `http-client`). |
| `connector_framework` | `webhook-server` | Embedded HTTP webhook receiver. |
| `connectors` | `http-client` | Forwards to `connector_framework/http-client`. |
| `connectors` | `live-integration` | Enables live provider integration tests (requires env vars). |
| `inference_router` | `http-client` | Reqwest-backed `HttpLlamaServerClient` for llama.cpp. |
| `inference_router` | `async-http-client` | Async llama.cpp client under tokio. |
| `evidence_store` | `onnx-runtime` | ONNX Runtime + HuggingFace tokenizer for real embeddings. |
| `synthesis_engine` | `http-client` | Reqwest-backed `BlockingHttpClientAdapter` for managed AI endpoints. |
| `synthesis_engine` | `nitro-tee` | AWS Nitro Enclave attestation runtime. |
| `synthesis_pipeline` | `http-client` | Forwards to `inference_router/http-client`. |

### Common feature combinations

```toml
# Full-featured mobile/desktop build
ffi = { path = "…", features = ["http-client", "tracing-subscriber"] }

# Minimal offline build (classification only, no network)
ffi = { path = "…" }

# Server-side synthesis with TEE
synthesis_engine = { path = "…", features = ["http-client", "nitro-tee"] }

# Connectors with async runtime
connector_framework = { path = "…", features = ["async-runtime", "async-http-client", "webhook-server"] }
```

---

## 6. Go server integration path

For server-side deployments (connector-driven ingestion, multi-tenant
synthesis, SCIM provisioning), the Go gateway (`server/cmd/gateway`)
provides a REST API over the Rust substrate:

```
┌─────────────┐     HTTP      ┌──────────────────┐     HTTP       ┌────────────────┐
│ Your App    │ ───────────── │ Go Gateway :8080 │ ──loopback──── │ Substrate :9090│
│ (any lang)  │   REST/JSON   │  (auth, rate-    │   (internal)   │  (Rust core)   │
└─────────────┘               │   limit, CORS)   │                └────────────────┘
                              └──────────────────┘
```

This path is ideal when:

- You are building a web application / backend service (not a
  native mobile/desktop app).
- You need multi-tenant isolation with JWT auth.
- You want connector-driven ingestion from SaaS tools (Notion,
  Drive, Slack, etc.) with OAuth2 managed server-side.
- You want per-IP and per-tenant rate limiting out of the box.

### Quick integration

```bash
# 1. Start substrate + gateway (see docs/QUICKSTART.md Mode 2)
cargo run -p substrate_server --release &
cd server && KNOWLEDGE_API_KEY=my-key go run ./cmd/gateway &

# 2. From your app, call the REST API
curl -X POST http://localhost:8080/api/v1/ingest \
  -H "Authorization: Bearer my-key" \
  -H "Content-Type: application/json" \
  -d '{"scope_id":"...","body":"...","importance":"Important"}'
```

See [API_REFERENCE.md](API_REFERENCE.md) for the full endpoint
documentation, authentication details, and SSE streaming format.

### Connector integration (real content fetching)

All 10 connectors now perform **real document-content fetching** —
not just metadata sync. When a connector syncs, it:

1. Discovers new/changed documents via the provider's delta API
2. Fetches the full document body (respecting provider rate limits)
3. Ingests the content into the substrate evidence store
4. Emits `DocumentCreated` / `DocumentUpdated` events

Supported providers: Google Drive, OneDrive, Notion, Jira,
Confluence, Figma, HubSpot, Slack, Email, Salesforce.

---

## 7. Further reading

- [README.md](../README.md) — project overview and quick start.
- [ARCHITECTURE.md](../ARCHITECTURE.md) — component map, data flow,
  permission model, crypto layer.
- [API_REFERENCE.md](./API_REFERENCE.md) — Go gateway REST endpoints.
- [QUICKSTART.md](./QUICKSTART.md) — three deployment modes.
- [DESIGN.md](./DESIGN.md) — product thesis, strategic principles,
  memory model.
- [PLATFORMS.md](./PLATFORMS.md) — per-platform tuning notes.
- [COST_MODEL.md](./COST_MODEL.md) — per-user cost breakdown.
- [BENCHMARKS.md](./BENCHMARKS.md) — performance measurements.
- [DEPENDENCY_POLICY.md](./DEPENDENCY_POLICY.md) — MSRV, pinning
  rationale, Dependabot config.
- [CONTRIBUTING.md](../CONTRIBUTING.md) — build, test, lint, PR flow.
- [SECURITY.md](../SECURITY.md) — responsible disclosure.
