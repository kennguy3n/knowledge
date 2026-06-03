# Changelog

All notable changes to this project will be documented in this
file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Workspace surface

- 22-crate Rust workspace covering the device-side substrate end
  to end:
  - **Data plane**: `evidence_store`, `memory_manager`,
    `observation_engine`, `synthesis_pipeline`,
    `synthesis_engine`, `inference_router`.
  - **Identity / cryptography**: `crypto`, `permission_service`,
    `audit_service`, `agent_contract`, `tenant_service`,
    `reasoning_engine`.
  - **Connectivity**: `connector_framework`, `connectors`
    (Notion, Google Drive, OneDrive, Slack, Jira, Confluence,
    Figma, HubSpot, Email), `sync_engine`, `export_plane`.
  - **Host bindings**: `ffi` (UniFFI, used by Swift / Kotlin),
    `napi` (Node N-API, used by the Electron host), `demo`.
  - **Test / build tooling**: `integration_tests`, `uniffi-bindgen`.

### Added — Cryptography

- Post-quantum hybrid key encapsulation: X25519 + ML-KEM-768
  combiner with HKDF-SHA-256 key derivation and
  XChaCha20-Poly1305 AEAD wrap.
- ML-DSA-65 (FIPS 204) signatures over every synthesis output,
  with SPHINCS+-SHAKE-128f-simple co-signatures kept in cold
  archival storage for stateless long-term verifiability.
- Cryptographic forgetting via DEK destruction: the 9-step
  scope teardown wipes the per-scope DEK, purges FTS5
  plaintext, and rewrites the SQLCipher pages so a vacuumed
  database carries no recoverable bytes.
- New `KeyStorage` trait plus `InMemoryKeyStorage` reference
  implementation; host shells supply hardware-backed
  implementations (iOS Keychain, Android Keystore, Windows
  DPAPI, libsecret, Nitro / SEV-SNP TEE) via the
  `ffi::KeyStorageResolver` callback.

### Added — Storage and sync

- SQLCipher-encrypted `evidence_store` with FTS5, content-
  aware routing into ring buffers, append-only audit log,
  and `v1 → v8+` schema migrations.
- CRDT-based `sync_engine` with delta envelopes, snapshot
  bootstrap, and an opt-in auto-compaction threshold (default
  10 000 ops) that rolls tombstones into snapshots before the
  log unbounded-grows.
- Paginated `PersistentConceptGraph::load_scope_paginated` for
  power-user scopes (100k+ concepts) so device boots never
  page the full graph into memory.

### Added — Performance

- BFS traversal in `concept_graph::traverse_typed` now uses a
  `VecDeque` so traversal stays O(n) instead of O(n²) on the
  per-step `Vec::insert(0, ...)`.
- `permission_service::TupleStore` carries a secondary index
  on `(object, relation)` so the Zanzibar-style reachability
  walker no longer linearly scans every tuple on every check.
- `audit_service::AuditLog` carries secondary indexes on
  `scope_id`, `action_type`, `actor_id`, and `entry_id`; the
  `query` planner picks the most selective index and falls
  back to a linear scan only for time-range-only queries.
- 100 000-node `concept_graph` benchmarks (`bench_add_node_100k`,
  `bench_traversal_100k`) and a multi-scope `load_bench` for
  `integration_tests` that measures ingest throughput plus
  cross-scope FTS p99 latency.

### Added — Inference

- On-device inference router with three backends:
  - `llama.cpp` (CPU + Metal),
  - Apple MLX,
  - a deterministic lexicon fallback used when neither
    accelerated runtime is reachable.
- Server-side `synthesis_engine` `HttpManagedEndpointSynthesizer`
  for the hybrid deployment profile, behind a
  `TeeWorker`-gated attestation check.
- Real `NitroTeeRuntime` (`feature = "nitro-tee"`) that calls
  `/dev/nsm`, parses the COSE_Sign1 attestation document, and
  exposes PCR0 as the report's enclave measurement.

### Added — Cost controls

- Per-endpoint rate limiter on
  `HttpManagedEndpointSynthesizer` with a `with_rate_limit`
  builder and an `EngineError::engine("rate limited; retry
  after …")` return path.
- `SynthesisBatcher` for server-side dispatch — sequential
  flush of pending requests through one shared rate limiter
  so a burst across thousands of scopes does not overload the
  upstream API.
- Per-provider token-bucket rate limiter on the connector
  framework's `BlockingHttpTransport` keyed by request host
  (e.g. `api.notion.com`, `graph.microsoft.com`).

### Added — Documentation and process

- `SECURITY.md`: vulnerability disclosure process, threat
  model, known limitations, planned third-party audit scope,
  and per-platform key storage backing.
- `docs/ELECTRON_SECURITY.md`: required `BrowserWindow`
  settings, CSP, IPC allowlist, preload isolation, main-
  process posture, auto-update note, and an 11-point review
  checklist for Electron hosts embedding the N-API addon.
- `docs/COST_MODEL.md`: per-user marginal cost breakdown for
  the B2C (on-device only), Hybrid (tenant synthesis), and
  Enterprise (connector polling) deployment tiers, plus the
  comparison table vs. server-side competitors.
- `README.md` "Observability — metrics, tracing, health":
  reorganised the counter catalogue from a prose-flow list (49
  counters, 16 silently omitted) into a grouped exhaustive
  inventory of all 65 counters defined in
  [`crates/ffi/src/metrics.rs`](crates/ffi/src/metrics.rs). The
  10 sub-headings (Runtime lifecycle, Memory, Synthesis, Decay,
  Crypto, Approved-document, Connectors, Resolvers, Webhook,
  Sync scheduler) mirror the responsibility partition of the
  FFI surface, so a future contributor adding a new entry point
  has an unambiguous home for the matching counter. Verified
  exhaustively with `diff <(grep '^\s\+pub\s\+[a-z_]\+_total'
  crates/ffi/src/metrics.rs | extract-names) <(grep -oE
  '\`[a-z_]+_total\`' README.md | sort -u)` returning empty.

### Added — CI / release engineering

- Pinned MSRV at Rust 1.85.
- CI: `rustfmt`, `clippy --all-targets --all-features`,
  `cargo-audit`, `cargo-deny`, `cargo-fuzz` (3 targets),
  cross-compile matrix for iOS and Android.
- `.github/dependabot.yml`: weekly bumps for `cargo` and
  `github-actions` ecosystems.
- `.github/CODEOWNERS`: explicit review for `crates/crypto/`,
  `crates/ffi/`, `crates/napi/`, `crates/evidence_store/`,
  and `SECURITY.md`.
- `.github/workflows/release.yml`: tag-triggered (`v*`)
  release workflow that runs `cargo test --all-features` and
  publishes a GitHub release with auto-generated notes.

### Changed — FFI surface

- Added `open_store_with_resolver(path: String, key_id: String,
  resolver: Arc<dyn KeyStorageResolver>) -> FfiResult<RuntimeHandle>`
  on the UniFFI surface. This is the substrate-side consumer of
  the `KeyStorageResolver` contract — hardware-backed hosts now
  call `open_store_with_resolver` instead of `open_store(path,
  master_key_hex)` so the 32-byte master key never enters the
  host's address space as a long-lived plaintext hex string. The
  resolver is consulted exactly once during open (the substrate
  reuses the existing `open_store_inner` path with the resolved
  hex) and is then stashed on the freshly-allocated runtime so
  subsequent operations reach the same backing store without a
  second `set_key_storage_resolver` call. A new
  `open_store_with_resolver_total` metric counter mirrors
  `open_store_total` for ratio-tracking. Unknown `key_id`s are
  re-tagged from the resolver's `NotFound { kind: "key" }`
  envelope to `NotFound { kind: "master_key" }` so the host can
  distinguish a master-key provisioning miss from a generic
  key-id miss; invalid hex from the resolver surfaces as
  `FfiError::InvalidId` via the shared `parse_master_key_hex`
  validation; every other `FfiError` from the resolver
  propagates verbatim. `crates/ffi/src/key_storage.rs` no
  longer documents the registration as a "forward-compatibility
  plumbing hook"; `SECURITY.md` §"Key storage" and `README.md`
  surface-specific list have been updated to reflect the new
  resolver-driven cold-boot path.
- Wired N-API counterparts for the three master-key resolver
  entry points so the Electron host can now drive the
  resolver-backed cold-boot path symmetrically with iOS / Android:
  `setKeyStorageResolver(handle, resolver, timeoutMs?)`,
  `clearKeyStorageResolver(handle)`,
  `openStoreWithResolver(path, keyId, resolver, timeoutMs?)`.
  The JS-callback adapter (`JsKeyStorageResolver` in
  `crates/napi/src/bindings.rs`) mirrors the OAuth-secret resolver
  precedent — three `ThreadsafeFunction` slots for `loadKey` /
  `storeKey` / `deleteKey`, a `std::sync::mpsc::sync_channel(1)`
  one-shot to ferry each result back to the substrate, and a
  configurable `recv_timeout` (default 30 s, vs OAuth's 5 s,
  because master-key unlocks can involve a Keychain biometric /
  password prompt that takes the user 10–20 s to satisfy).
  Pass `timeoutMs: 0` is rejected as an `InvalidArgument` (a zero
  timeout would always race the JS event loop). Sync-waiting the
  substrate on a JS callback is safe here because the substrate's
  three-phase locking pattern guarantees the runtime mutex is
  NOT held while a resolver call is in flight. A JS exception
  inside a callback surfaces as `FfiError::Unavailable {
  subsystem: "host-key-store: <method> threw: ..." }`; a
  callback that does not return within `recv_timeout` surfaces
  as `FfiError::Unavailable { subsystem: "host-key-store:
  <method> timed out after Xms" }`.
- Wired `try_init_tracing` through the UniFFI export so iOS /
  Android hosts can install the substrate's
  `Registry::default().with(fmt::Layer).with(EnvFilter)` stack
  from the UniFFI-generated Swift / Kotlin bindings instead of
  shipping their own `tracing-subscriber` configuration. The
  Rust signature is now
  `pub fn try_init_tracing(directive: String) -> FfiResult<()>`
  — UniFFI marshals owned `String`s but cannot bridge `&str`,
  so the borrowed-parameter shape this function used to carry
  was the one blocker on a `#[uniffi::export]`. The function is
  re-exported from `napi_addon` as `initTracing(directive)`
  (signature unchanged from the JS caller's perspective) and
  remains feature-gated behind the `tracing-subscriber` Cargo
  feature on `ffi`. Idempotency, metrics wiring
  (`init_tracing_total` counter + `tracing_initialized` flag),
  and the "first-directive wins" concurrency contract are
  unchanged. Closes the "deferred to a follow-up" note that
  previously appeared in `README.md`'s surface-specific list.

### Changed — Dependencies

- Documented and Dependabot-filtered the MSRV-gated dependency
  pins so the review queue no longer cycles unmergeable PRs:
  `rusqlite` (`0.36.x` line; `>=0.37` requires Rust 1.94+ via
  `libsqlite3-sys 0.36+`'s adoption of the stable `cfg_select!`
  macro), `aws-nitro-enclaves-nsm-api` (`0.4.x` line; `>=0.5`
  sets `rust-version = 1.92`), and `criterion` (`0.7.x` line;
  `>=0.8` requires Rust 1.86). Each pin has (a) an inline
  rationale next to its `version = "…"` declaration in the
  relevant `Cargo.toml`, and (b) a `versions:` block in
  [`.github/dependabot.yml`](.github/dependabot.yml) that
  filters out only the gated range so patch / intermediate
  bumps still surface. The cross-cutting summary lives in
  [`docs/DEPENDENCIES.md`](docs/DEPENDENCIES.md). The
  ignore blocks will be deleted in the same PR that bumps the
  workspace MSRV forward.
- Bumped `rusqlite` from `0.32.1` to `0.36.0` (workspace pin).
  `0.36.x` is the new MSRV ceiling — `>=0.37` (via
  `libsqlite3-sys >=0.36`) requires Rust 1.94+ for the stable
  `cfg_select!` macro. **Bundled SQLite transition** as a
  side-effect of `libsqlite3-sys 0.30.1 → 0.34.0`: the
  SQLCipher-vendored SQLite moved from `3.45.3` (April 2024,
  SQLCipher 4.6.0 fork) to `3.46.1` (August 2024, SQLCipher
  4.6.1 fork). The upstream SQLite FTS5 changelog between
  these versions has no documented changes to `unicode61` or
  `trigram` default-config behaviour. A new canary test suite
  (`crates/evidence_store/tests/bundled_sqlite_canary.rs`) now
  pins the bundled SQLite version, the `sqlite_source_id()`
  timestamp, and the recall behaviour of both tokenisers
  against a multi-script corpus; any future rusqlite /
  libsqlite3-sys bump that moves the bundle will fail those
  literal assertions and force a deliberate maintainer ack via
  the audit procedure documented in the canary's module-level
  docs. The bump requires no `.rs` file changes — the
  substrate's rusqlite API surface (`SqliteFailure` tuple
  variant, `ffi::Error::new`, `pragma_update` with `i64` /
  `&str` args, `Connection::open` / `Connection::open_in_memory`)
  is source-compatible across the `0.33 → 0.36` range; the
  intermediate breaking changes documented in the rusqlite
  changelog (VTab API rework, reentrant
  `Connection::call_loadable_extension` signature) don't
  intersect with anything the substrate calls.
- Bumped `axum` from `0.7` to `0.8` (workspace pin). Axum 0.8
  changed the router path-pattern syntax: a leading `:` on a
  segment now panics at registration time
  (`Path segments must not start with ':'. For capture groups,
  use '{capture}'.`). Updated the webhook receiver's provider-id
  capture in
  `crates/connector_framework/src/webhook_server.rs::build_router`
  from `/webhooks/:provider_id` to `/webhooks/{provider_id}`. The
  `Path<String>` extractor in `webhook_handler` keeps the same
  signature — only the route pattern changed.
- Bumped `criterion` from `0.5` to `0.7` (workspace pin).
  `criterion::black_box` was deprecated in `0.6` in favour of the
  standard-library `std::hint::black_box`, and `-D warnings`
  turns the deprecation lint into a hard error. The four
  workspace bench files now import `black_box` from
  `std::hint::` directly: `crates/crypto/benches/crypto_bench.rs`,
  `crates/concept_graph/benches/graph_bench.rs`,
  `crates/evidence_store/benches/store_bench.rs`,
  `crates/integration_tests/benches/load_bench.rs`. Stopped at
  `0.7` (not `0.8`) because criterion `0.8.0` raised its MSRV to
  `1.86` and the workspace's `MSRV (1.85.0)` CI gate pins us to
  `1.85`; bumping to `0.8` is gated on a workspace MSRV bump.
- Bumped `hmac` 0.12 → 0.13, `hkdf` 0.12 → 0.13, `sha2` 0.10 →
  0.11 in lockstep. These three crates share the same underlying
  `digest` trait, and `digest 0.10` → `digest 0.11` is the only
  ABI change driving the major-version bumps. The single source
  change required is that `hmac 0.13` moved
  `Hmac::new_from_slice` from an inherent method to a `KeyInit`
  trait method; `crates/crypto/src/provenance.rs` now imports
  `hmac::KeyInit`. All other call sites (`Hmac<Sha256>::new`,
  `Mac::update`, `Mac::finalize`, `Hkdf::<Sha256>::new`,
  `Hkdf::expand`) are unchanged. Workspace `Cargo.toml` carries a
  lockstep-bump comment to keep future Dependabot bumps from
  inadvertently splitting the trio.
- Bumped `tokenizers` 0.21 → 0.23 (gated behind the
  `evidence_store/onnx-runtime` feature). API surface used
  (`Tokenizer::from_file`, `tokenizer.encode(text, true)`) is
  unchanged between the two versions; no source changes
  required. The bump also pulls in `fancy-regex` 0.14 → 0.17
  transitively.
- Bumped `rand` 0.8 → 0.9. `OsRng` is now fallible-only
  (`TryRngCore` / `TryCryptoRng`); call sites that want the
  old infallible surface use `OsRng.unwrap_err()` to restore
  it via panic-on-failure. `rand::thread_rng()` was renamed to
  `rand::rng()`. The workspace deliberately keeps `rand_core`
  at `0.6` because `x25519-dalek 2`'s
  `X25519Secret::random_from_rng` and `ml-kem 0.2`'s
  `KemCore::generate` both consume the `rand_core 0.6`
  `CryptoRngCore` trait; `rand 0.9` coexists fine with
  `rand_core 0.6` (parallel `RngCore` hierarchies).

### Changed — Security posture

- Standardised every production AEAD-nonce generation site on
  `rand::rngs::OsRng.unwrap_err()` (direct OS CSPRNG) so that
  every cryptographic byte the substrate emits — long-lived
  keys *and* per-encryption nonces — comes from the same
  audited source. Previously a subset of nonce sites
  (`audit_service`, `evidence_store`, `ffi::encrypt`,
  `permission_service`, `synthesis_pipeline`, `tenant_service`)
  used `rand::rng()` (`ThreadRng = ReseedingRng<ChaCha12Core,
  OsRng>`), which is cryptographically suitable for nonces but
  added a userspace CSPRNG layer to the audit story without
  benefit on the hot path. The remaining `rand::rng()` call
  sites are all `#[cfg(test)]` helpers or in `tests/` files
  and are intentional (test-only key/nonce fabrication).
  Documented the policy and the rationale in `SECURITY.md`
  §"Random number generation".
- Dropped the unused `rand_core` workspace dependency from
  `crates/concept_graph/Cargo.toml`. The crate's `persist.rs`
  imports `RngCore` and `TryRngCore` from `rand` (0.9), not
  `rand_core` directly; no source / test / bench file in
  `crates/concept_graph/` references the crate. The
  `rand_core = "0.6"` workspace pin is still required and
  still active for `crates/crypto/` (`kem.rs`, `hybrid_kem.rs`)
  which DOES import `rand_core::OsRng` directly to pass to
  `ml-kem 0.2` / `x25519-dalek 2`.

### Added — Go API gateway (Session A)

- Full Go HTTP gateway in `server/` with Chi router, wiring the
  Rust substrate via HTTP loopback over FFI (`crates/substrate_server`).
- Endpoints: evidence ingest/query/get, memory listing, scope
  forgetting, synthesis trigger/status/recent, connectors CRUD +
  OAuth2 + sync + webhooks, tenant CRUD + config + key rotation +
  member lifecycle, Zanzibar permission grant/revoke/check, SCIM v2
  user/group provisioning, export profile rendering, audit log query.
- Dual-layer rate limiting: per-IP (pre-auth) and per-tenant
  (post-auth) token buckets with configurable RPS and burst.
- Bearer token + JWT authentication middleware.
- SSE streaming for synthesis status polling with bounded lifetime.
- Prometheus metrics (`/metrics`) and subsystem health check
  (`/health`).
- In-memory fallback stores when `KNOWLEDGE_DATABASE_URL` is unset
  (zero-dep local development).
- NATS JetStream audit consumer with configurable per-tenant retention.
- 12-factor configuration via environment variables (see
  `docs/API_REFERENCE.md`).

### Added — Connector content fetching (Session B)

- `fetch_content` method on the `Connector` trait
  (`crates/connector_framework/src/connector.rs`) — connectors now
  fetch real document bodies, not just metadata events.
- `FetchedContent` type carrying body bytes, MIME type, title,
  source URL, and provider-specific metadata.
- Implemented for all 10 connectors: Google Drive, OneDrive, Notion,
  Jira, Confluence, Figma, HubSpot, Slack, Email, GitHub (unstable).
- Go gateway connector service wires `fetch_content` into the sync
  pipeline — each delta page triggers content fetching and evidence
  ingestion.

### Added — Multilingual lexicons (Session C)

- 7 new language lexicons: Hebrew (`he`), Indonesian (`id`),
  Italian (`it`), Tibetan (`bo`), Khmer (`km`), Lao (`lo`),
  Burmese (`my`).
- `LexiconRegistry` now covers 22 BCP-47 primary subtags (was 15).
- Per-language decision / task keywords, imperative verbs, stop-words,
  and interrogative tables validated via
  `multilingual_pipeline.rs` test suite.
- Tashkeel-tolerant Arabic normalisation in `normalize_for_lookup`.
- Bonsai-1.7B validation for synthesis + extraction across all 22
  languages (`multilingual_bonsai.rs`, gated by `live-integration`).

### Added — Benchmark suite (Session D)

- `crates/benchmarks/` Criterion.rs production benchmark suite:
  ingest throughput, FTS query (exact/phrase/boolean/prefix), hybrid
  retrieval, synthesis e2e, storage footprint, decay sweep,
  concept-graph traversal, crypto operations (AEAD + post-quantum
  KEM/sign), connector sync throughput, storage read-through/scan.
- `docs/BENCHMARKS.md` documenting methodology, reference hardware,
  and all measured results.
- `.github/workflows/benchmarks.yml` for CI-triggered benchmark runs.

### Added — Security tests and compliance (Session F)

- Comprehensive `crypto` crate test coverage: hybrid KEM round-trip,
  ML-DSA-65 sign/verify, key derivation, AEAD nonce uniqueness,
  and cryptographic forgetting (DEK destruction).
- `evidence_store` security tests: scope isolation, encrypted-at-rest
  verification, forgetting produces unrecoverable ciphertext.
- `docs/COMPLIANCE.md`: GDPR / SOC 2 / HIPAA control mapping with
  concrete code citations (cryptographic forgetting, data portability,
  proposal-only agents, audit trail).
- `docs/SUPPLY_CHAIN.md`: dependency policy, CycloneDX SBOM
  generation per commit, `cargo-deny` + `cargo-audit` gates.

<!--
  No tagged release exists yet, so the Keep-a-Changelog
  `compare/v<last>...HEAD` link cannot resolve. Until the first tag
  is cut, the `[Unreleased]` link points at the full commit history
  on the default branch — which is the only honest "everything that
  has happened" view in the pre-1.0 state. The first release should
  replace this with `compare/v<first-tag>...HEAD`.
-->
[Unreleased]: https://github.com/kennguy3n/knowledge/commits/main
