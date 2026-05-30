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

[Unreleased]: https://github.com/kennguy3n/knowledge/compare/HEAD...HEAD
