# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`connector_framework::WatermarkCursor` (STABLE).** A backward-compatible
  incremental-sync cursor that stores the high-water `updated_at` instant
  together with the set of source ids observed at that instant, so records
  sharing the exact boundary second are no longer dropped (see _Fixed_).
  Legacy bare-timestamp cursors parse transparently as that watermark with an
  empty id set.
- **70 new regional connectors (140 stable total) across 7 regions.**
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

### Fixed

- **Incremental sync no longer drops records at the watermark boundary
  (repo-wide).** Every connector built from the timestamp-watermark template
  persisted only a bare RFC-3339 high-water timestamp and skipped each record
  with `updated_at <= cursor` on the next run. Any record sharing the exact
  boundary second that was not part of the previous page (e.g. written in the
  same second just after the prior snapshot, or split across a page boundary)
  was dropped permanently. All 90 affected connectors now persist and consume a
  `connector_framework::WatermarkCursor` (timestamp + boundary id-set), so a
  brand-new boundary-second record is surfaced while already-emitted ids are
  not duplicated. The cursor wire format is backward compatible with existing
  persisted bare-timestamp cursors. This expands the original 10 UK-connector
  fix (Monzo Business, Revolut Business, FreeAgent, GoCardless, Royal Mail,
  Deliveroo, Just Eat, Companies House, HMRC MTD, Starling) to the remaining
  ~80 template connectors across all regions that shared the same pattern.
  Connectors with bespoke cursors that were never affected (Figma, Stripe,
  Slack, HubSpot) are unchanged, as are Zoom and Google Meet which already
  used a boundary-id cursor.
- **`connectors::bexio::DEFAULT_API_BASE_URL` no longer double-versions
  the request path.** The default was `https://api.bexio.com/2.0` while
  every endpoint path also carries a `/v1` segment (`/v1/invoices`),
  so requests resolved to `https://api.bexio.com/2.0/v1/invoices` — two
  API-version segments. The default is now `https://api.bexio.com`, so
  requests resolve to `https://api.bexio.com/v1/invoices`, matching the
  bare-host + `/v1` convention used by the other nine Switzerland
  connectors. Overridable as before via `auth_config_json.api_base_url`.

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

[Unreleased]: https://github.com/kennguy3n/knowledge/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/kennguy3n/knowledge/releases/tag/v1.1.0
[1.0.0]: https://github.com/kennguy3n/knowledge/releases/tag/v1.0.0
