# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **30 new connectors.** The connector catalog grows from 10 to 40
  providers, all shipping as **stable** with full `Connector` trait
  implementations and unit coverage:
  - *Productivity & CRM* — Salesforce, ServiceNow, Zendesk, Linear,
    Asana, Monday, ClickUp, Freshdesk, Intercom, Pipedrive.
  - *Cloud storage & communication* — Dropbox, Box, SharePoint, Teams,
    Discord, Zoom, Google Calendar, Google Docs, Google Sheets,
    Google Meet.
  - *Business & developer tools* — QuickBooks, Xero, Stripe, Shopify,
    Airtable, GitLab, Bitbucket, Trello, Miro, DocuSign.
- **10 Asia-Pacific (Singapore / Thailand / SEA) connectors**, all
  shipping as **stable** with full `Connector` trait implementations
  and `MockHttpTransport` unit coverage: LINE, Grab, Gojek, Talenox,
  Odoo (SEA), Fastwork, TrueMoney, SCB Easy, PromptPay, Tokopedia.
- **Browser-based admin dashboard** (`admin/`) — a React + Vite SPA served
  on `:3001` for managing connectors, tenants, synthesis runs, the memory
  browser, and the audit log without the CLI or PromQL.
- **Pre-built container images & Helm chart.** Multi-arch images publish
  to GHCR/Docker Hub on tagged releases; `deploy/docker-compose.images.yml`
  runs the stack with no local build, and `deploy/helm/knowledge` plus
  starter Terraform modules (`deploy/terraform/{aws,gcp}`) deploy it to
  Kubernetes.
- **Release & auto-update automation** — a tag-triggered release workflow
  (binaries, images, Helm chart) and an optional substrate update-check
  endpoint that compares the running version against the latest release.

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
  `ConnectorKind::GitHub`. With the 30 connectors added this release, the
  catalog is now **40 stable** (was 9 stable + 1 unstable in 1.0.0).

### Fixed

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

[1.0.0]: https://github.com/kennguy3n/knowledge/releases/tag/v1.0.0
