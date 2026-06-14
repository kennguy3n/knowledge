# Knowledge

**Privacy-first, post-quantum secure memory for AI applications. On-device by default — $0 / user / month at any scale.**

Knowledge is a knowledge substrate: layered, decaying, scope-aware
memory that AI surfaces (chat, search, agents, exports) consume so they
never have to re-process raw data on every call. It runs on the user's
device by default, encrypts everything at rest with post-quantum
cryptography, and can forget — cryptographically, not by soft-delete.

---

## What is Knowledge?

- **Privacy-first** — data stays on the user's device by default. No
  server round-trip means no cross-border transfer and no central
  honeypot.
- **$0 / user / month** — on-device inference and storage make the
  marginal cost of an additional user effectively zero.
- **Post-quantum secure** — hybrid X25519 + ML-KEM-768 encryption and
  ML-DSA-65 signatures protect data against "harvest-now, decrypt-later"
  attacks.
- **Multilingual — and measured** — extraction works across 22 languages
  out of the box, with per-sentence language detection. Synthesis quality
  is graded by a reproducible, GPU-free offline eval harness (term
  coverage, faithfulness/grounding, in-language) and published on a
  [per-language leaderboard](docs/technical/multilingual-leaderboard.md)
  rather than asserted.
- **Reasoning, not just retrieval** — beyond similarity search, Knowledge
  answers *what contradicts X?*, *how has belief about X drifted?*, and
  *why was this retrieved?* privately and on-device, scope-isolated and
  bounded (256-node working set), surfaced over the gateway's
  `POST /api/v1/reasoning/{contradictions,drift,explain}` endpoints.
- **On-device acceleration** — the inference router selects the best
  available backend in order — Apple Neural Engine (Core ML) → ONNX
  Runtime (NNAPI / QNN-Hexagon NPU on Android, Core ML EP on iOS) → MLX →
  llama.cpp → managed cloud → deterministic fallback — with capability
  detection, graceful fallback, and **no native build dependencies** (the
  accelerator runtime is injected at load time). See
  [inference routing](docs/technical/inference-routing.md).
- **140 built-in connectors, honestly labelled** — pull knowledge from
  where it already lives, across file stores, docs/wikis, CRM and support,
  project tracking, chat and meetings, developer tools, design, and
  finance (Google Drive, Notion, Slack, Salesforce, Jira, GitHub, Stripe,
  and more), plus 100 region-focused sources across 10 markets — Vietnam,
  Singapore/Thailand/SEA, the GCC/Middle East, the UK, Germany, France,
  Switzerland, Australia, Latin America, and an expanded SEA batch (Zalo,
  MoMo, LINE, Grab, Careem, Talabat, Monzo, Qonto, Bexio, MYOB,
  MercadoLibre, GCash, …). Each connector carries an explicit maturity
  label: **5 are `live-verified`** (GitHub, Slack, Notion, MoMo, Stripe —
  driven end-to-end by committed cassette replays in CI) and the rest are
  **`contract-stable`** (full contract, unit-tested at the HTTP boundary).
  See the [connector maturity table](docs/product/roadmap.md#connector-maturity).
- **Browser-based admin** — manage connectors, tenants, synthesis, and
  audit from a web dashboard at `localhost:3001`, no CLI or PromQL
  required.
- **End-user reference UI** — a Next.js chat/search/memory app
  (`apps/knowledge-ui/`, served on `localhost:3002`) your users — not
  just operators — can open to chat with a scope, run hybrid search,
  browse decaying memory, and cryptographically forget a conversation.
- **Multi-device sync transport** — an add-wins CRDT with delta
  compaction and snapshot bootstrap, per-scope XChaCha20-Poly1305 sealing,
  and an untrusted `sync_relay` (bearer auth, per-tenant isolation) that
  only ever holds opaque ciphertext. The merge math, transport, and a
  ≥3-replica convergence test across offline/partition scenarios ship
  today. Current limitation: wiring it into a host app's background
  lifecycle (scheduling, retry/backoff) is integration work, and
  post-quantum key establishment for cross-device key transport is not the
  live path today — scope keys are distributed out of band.
- **Works offline** — the full pipeline (ingest → extract → remember →
  synthesize) runs with no network connection.

## Who is this for?

- **Product teams** building AI-powered apps — B2C chat, B2B knowledge
  tools, agent memory — who need user data to stay private without
  giving up retrieval quality.
- **Operators** deploying hybrid or enterprise knowledge infrastructure
  for SMEs and larger organizations who want connector-fed knowledge
  without running a heavyweight server tier.
- **Developers** embedding structured, decaying memory into AI agents
  and host apps on iOS, Android, or desktop (Electron).

## Quick Start

Pick the path that matches your role:

- **[For developers](docs/getting-started/for-developers.md)** — build
  from source, run the on-device demo, and embed the substrate in an app.
- **[For operators](docs/getting-started/for-operators.md)** — deploy
  the hybrid/enterprise server surface, configure it, and monitor it.
- **[For product teams](docs/getting-started/for-product-teams.md)** —
  understand what Knowledge enables and which integration pattern fits.

The fastest taste of the system (no server required):

```bash
git clone https://github.com/kennguy3n/knowledge.git
cd knowledge
cargo run -p demo --release
```

This drives a synthetic multi-scope dataset through every substrate API
and writes a reconciled report to `results/demo_results.md`. See the
**[Quick Start guide](docs/QUICKSTART.md)** for the full walkthrough
across all three deployment modes.

## One-command install

For a fresh host, the installer takes you from zero to a running stack —
it checks Docker + the Compose plugin, generates strong secrets into
`.env`, asks whether to enable on-device synthesis, pulls the published
images, waits for the gateway to report healthy, and prints the URLs to
open:

```bash
curl -fsSL https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.sh | bash
```

On Windows, run the PowerShell installer instead:

```powershell
irm https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.ps1 | iex
```

The bundled `llama-server` image ships the **Bonsai-1.7B** model baked
in, so on-device synthesis works with no manual model download. See the
[deployment guide](docs/operator/deployment-guide.md#one-command-installer)
for the installer's flags (`KNOWLEDGE_ASSUME_YES`,
`KNOWLEDGE_SLM_DEVICE_TIER`, …).

## Quick deploy with Docker

For the hybrid/enterprise server surface, deploy the gateway + substrate
from **pre-built, multi-arch images** — no local build required. Tagged
releases publish to GHCR (`ghcr.io/kennguy3n/knowledge-gateway` and
`…-substrate`).

```bash
git clone https://github.com/kennguy3n/knowledge.git
cd knowledge
cp .env.example .env            # set KNOWLEDGE_MASTER_KEY (openssl rand -hex 32)
export KNOWLEDGE_VERSION=latest # or a release tag, e.g. 1.2.0

docker compose \
  -f deploy/docker-compose.yml \
  -f deploy/docker-compose.images.yml \
  up -d                         # pulls images; no --build
```

On Kubernetes, install the Helm chart instead:

```bash
helm install knowledge deploy/helm/knowledge \
  --namespace knowledge --create-namespace \
  --set secrets.masterKey="$(openssl rand -hex 32)"
```

See the **[deployment guide](docs/operator/deployment-guide.md)** for the
pre-built-image compose override, Helm values, and the Terraform
starting points for EKS/GKE.

## Deployment Modes

| Mode | Use case | Infrastructure |
|---|---|---|
| **On-device** | B2C apps where each user's data stays on their own device (e.g. a private chat app). | None — the substrate runs entirely on iOS/Android/desktop. |
| **Hybrid** | SMEs connecting SaaS tools (Notion, Slack, Drive) with on-device synthesis. | A lightweight Go gateway + Rust substrate for connector sync; synthesis stays on-device or in a TEE. |
| **Enterprise** | Multi-tenant B2B knowledge platforms with central connectors, permissions, and audit. | Gateway + substrate + Postgres, with SCIM provisioning, Zanzibar permissions, and per-tenant keys. |

See **[deployment scenarios](docs/product/deployment-scenarios.md)** for
a decision tree, and the **[cost model](docs/operator/cost-model.md)**
for the per-user economics of each mode.

**High availability.** For the hybrid/enterprise tiers, the substrate
supports **active-passive failover**: a primary ships its SQLCipher WAL
frames to one or more read-only standbys over NATS JetStream, with leader
election via a NATS key-value lease. A standby promotes itself when the
primary's lease expires, giving **RPO = 0 for acknowledged WAL frames**
and **RTO ≤ 2 × lease TTL** (default lease TTL 15 s in production). This
is exercised by a chaos/integration test. Enable it with
`KNOWLEDGE_SUBSTRATE_ROLE` / `KNOWLEDGE_REPLICATION_NATS_URL` (or
`substrate.ha.enabled=true` in Helm, which renders a StatefulSet). See
[high availability](docs/operator/ha-failover.md) and the
[deployment guide](docs/operator/deployment-guide.md#high-availability-active-passive-failover).

## How Knowledge compares

Knowledge's defensible wedge is **on-device privacy at $0 marginal
cost**, paired with capabilities most alternatives don't ship together:
post-quantum crypto (ML-KEM-768 / ML-DSA-65), **cryptographic forgetting**
(irreversible key destruction, not soft delete), multilingual breadth
with a **published per-language eval board**, a **reasoning plane**
(contradiction / drift / explain), and honest, measured claims
(connector liveness, a device-benchmark matrix, synthesis quality).

- vs. **hosted memory layers** (Mem0, Zep, Letta/MemGPT): they run in the
  cloud and soft-delete; Knowledge runs on-device, forgets
  cryptographically, and adds reasoning beyond similarity recall.
- vs. **vector DBs** (Pinecone, Weaviate): they index embeddings you
  supply and retrieve in the cloud; Knowledge is the full on-device
  pipeline (evidence → observation → concept → synthesis) with crypto,
  permissions, connectors, and reasoning.
- vs. **enterprise assistants** (Glean, Dust, Microsoft 365 Copilot,
  Notion AI, Google NotebookLM): turnkey cloud products over vendor data;
  Knowledge is an **embeddable substrate** that runs in-region/offline and
  is cryptographically erasable.
- vs. **managed ETL** (Fivetran, Airbyte, Nango): cloud pipelines;
  Knowledge ships connectors on-device with an honest liveness/maturity
  distinction and regional (SEA/GCC) coverage.
- vs. **closed on-device AI** (Apple Intelligence, Rewind): single-vendor,
  single-platform; Knowledge is cross-platform, embeddable, and
  PQC-secured.

See the full table in
**[product/comparison.md](docs/product/comparison.md)**. Pricing claims
there are publicly-reported, order-of-magnitude figures — not vendor
quotes.

## Performance

Measured on reference hardware (AMD EPYC 7763, 8 vCPU, 31 GiB). See
**[benchmarks](docs/technical/benchmarks.md)** for the full suite and
methodology, and the
**[device benchmark matrix](docs/technical/benchmarks-device.md)** for the
portable, one-command harness that measures the real
`evidence_store`/`HybridRetriever` path per device (the Linux row is
filled; other device rows are marked pending real-device measurement
rather than estimated).

| Metric | Result |
|---|---|
| Ingest throughput (100K messages) | **~1,043 msgs/sec** |
| FTS phrase query (100K rows, 50 scopes) | p50 **13.56 ms** |
| Hybrid retrieval (10K rows) | **9.70 ms** |
| Decay sweep (100K objects) | **5.26 ms** (~19M rows/sec) |
| AEAD encrypt 64 KB | 80.4 µs (778 MiB/s) |
| Hybrid KEM encap (X25519 + ML-KEM-768) | 159.9 µs |
| ML-DSA-65 sign / verify | 320 µs / 77 µs |
| Storage per message (at 500K) | **612 bytes** |
| Connector sync (10K docs) | **~6,750 docs/sec** |

## Multilingual support

The observation engine ships lexicon tables for **22 languages**.
Each lexicon carries per-language decision keywords, task
keywords, imperative verbs, stop words, and an interrogative
table with a per-language matching strategy.

| BCP-47 | Language   | Script     | Decision strategy     | Notes |
|--------|-----------|------------|----------------------|-------|
| `ar`   | Arabic     | Arabic     | FirstToken + clitic strip | Arabic proclitics (`ال`, `و`, `ف`, `ب`, `ل`) stripped before match |
| `bo`   | Tibetan    | Tibetan    | Substring             | Tsheg (`་`) is a syllable separator, not a word boundary |
| `de`   | German     | Latin      | FirstToken            | |
| `en`   | English    | Latin      | FirstToken            | Fallback lexicon when detection returns `None` |
| `es`   | Spanish    | Latin      | FirstToken            | |
| `fr`   | French     | Latin      | FirstToken            | |
| `he`   | Hebrew     | Hebrew     | FirstToken + clitic strip | Hebrew proclitics (`ה`, `ו`, `ב`, `כ`, `ל`, `מ`, `ש`) stripped |
| `hi`   | Hindi      | Devanagari | FirstToken            | |
| `id`   | Indonesian | Latin      | FirstToken            | |
| `it`   | Italian    | Latin      | FirstToken            | |
| `ja`   | Japanese   | CJK        | Substring             | No whitespace word boundaries |
| `km`   | Khmer      | Khmer      | Substring             | No inter-word whitespace |
| `ko`   | Korean     | Hangul     | Substring             | |
| `lo`   | Lao        | Lao        | Substring             | No inter-word whitespace |
| `ms`   | Malay      | Latin      | FirstToken            | whatlang may merge with Indonesian (`id`) |
| `my`   | Myanmar    | Myanmar    | Substring             | No inter-word whitespace |
| `pt`   | Portuguese | Latin      | FirstToken            | |
| `ru`   | Russian    | Cyrillic   | FirstToken            | |
| `th`   | Thai       | Thai       | Substring             | No inter-word whitespace; entity extraction is substring-based |
| `tl`   | Tagalog    | Latin      | FirstToken            | VSO word order; interrogative pronouns front in questions |
| `vi`   | Vietnamese | Latin      | FirstBigram           | Syllable-per-token script; bigram match for multi-syllable keywords |
| `zh`   | Chinese    | CJK        | Substring             | No whitespace word boundaries |

### Quality-gate coverage

A 15-language subset (en, zh, es, hi, fr, ar, th, vi, ms, tl,
de, pt, ja, ko, ru) is validated by two test suites:

- **`multilingual_pipeline`** (`observation_engine`) — per-language
  decision / task / question extraction through the default
  pipeline, plus no-English-bleeding assertions.
- **`multilingual_bonsai`** (`inference_router`, gated on
  `live-integration` feature + `LLAMA_SERVER_BINARY` env var) —
  Bonsai-1.7B inference across summary generation, entity
  extraction, importance classification, and concept synthesis.

---

## Documentation

Documentation is organized by audience:

- **[Getting started](docs/getting-started/)** — role-based onboarding
  for developers, operators, and product teams.
- **[Technical](docs/technical/)** — architecture, design, crypto,
  sync, inference routing, connectors, permissions, API reference,
  benchmarks, and platform tuning.
- **[Operator](docs/operator/)** — deployment, configuration,
  monitoring, scaling, backup/recovery, troubleshooting, cost, and
  compliance.
- **[Product](docs/product/)** — use cases, deployment scenarios,
  comparisons, roadmap, and FAQ.
- **[Guides](docs/guides/)** — step-by-step integration and tutorials.
- **[Security](docs/security/)** — threat model, key management,
  supply chain, Electron hardening, and dependency policy.
- **[Blog](blog/00-series-index.md)** — the "Building Knowledge" series:
  the design, production operations, and real-world deployments.

## Community & Support

- **Contributing** — see [CONTRIBUTING.md](CONTRIBUTING.md) for build
  instructions, the contribution workflow, and the DCO sign-off
  requirement.
- **Code of Conduct** — see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- **Discussions** — open a GitHub Discussion for questions and ideas.
- **Issues** — use the bug report and feature request templates.
- **Security** — report vulnerabilities privately per
  [SECURITY.md](SECURITY.md) (contact **ken@uney.com**); please do not
  open public issues for security reports.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in
this project, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions.
