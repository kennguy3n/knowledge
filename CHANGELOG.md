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

<!--
  No tagged release exists yet, so the Keep-a-Changelog
  `compare/v<last>...HEAD` link cannot resolve. Until the first tag
  is cut, the `[Unreleased]` link points at the full commit history
  on the default branch — which is the only honest "everything that
  has happened" view in the pre-1.0 state. The first release should
  replace this with `compare/v<first-tag>...HEAD`.
-->
[Unreleased]: https://github.com/kennguy3n/knowledge/commits/main
