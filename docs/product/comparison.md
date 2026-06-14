# Comparison

An honest look at how Knowledge differs from adjacent tools. These are
good products; they make different trade-offs. Pricing figures are
publicly reported list prices and move over time — treat them as
order-of-magnitude, not quotes. Knowledge optimizes for
**on-device privacy and zero marginal cost**, which is the right choice
for some products and the wrong one for others.

## At a glance

| | Knowledge | Microsoft 365 Copilot | Glean | Notion AI | Pinecone | Guru | Notion AI Q&A | Google NotebookLM | Mem.ai |
|---|---|---|---|---|---|---|---|---|---|
| Where data lives | On user's device (or your in-region infra) | Microsoft cloud | Vendor cloud | Notion cloud | Vendor cloud (vectors) | Vendor cloud | Notion cloud | Google cloud | Vendor cloud |
| Marginal cost/user | ~$0 on-device | Per-seat license | Per-seat license | Per-seat add-on | Per-usage (vectors/queries) | Per-seat license | Per-seat add-on | Bundled/consumer | Per-seat license |
| List price (reported) | $0/user (self-hosted) | ~$30/user/mo | ~$10–15/user/mo | ~$10/user/mo add-on | Usage-based (vectors/queries) | ~$10–15/user/mo | ~$10/user/mo add-on | Free (consumer) / Workspace | ~$10/user/mo |
| Connectors | 140 built-in (5 live-verified, rest contract-stable) | M365 + Graph-connected sources | 100+ enterprise sources | Notion + limited integrations | None (BYO embeddings) | SaaS integrations + browser ext. | Notion + limited integrations | Upload + Google Drive/Docs | Limited integrations |
| Regional connector coverage | Yes — 10 markets (VN, Singapore/Thailand/SEA, GCC, UK, DE, FR, CH, AU, LATAM, SEA-expanded) | Global SaaS (US-centric) | Global SaaS (US-centric) | Limited | None | Global SaaS | Limited | None (upload-based) | Limited |
| File/media ingest | Yes (text, files, media refs, API payloads) | Files via M365 | Files via connectors | Files in Notion | No (BYO vectors) | Cards + attachments | Files in Notion | Yes (PDF, web, audio) | Notes + some files |
| Works offline | Yes | No | No | No | No | No | No | No | No |
| Post-quantum crypto | Yes (ML-KEM-768, ML-DSA-65) | No (classical TLS) | No | No | No | No | No | No | No |
| Cryptographic forgetting | Yes (key destruction) | Soft delete | Soft delete | Soft delete | Vector delete | Soft delete | Soft delete | Soft delete | Soft delete |
| Multilingual extraction | 22 languages on-device | Cloud models | Cloud models | Cloud models | BYO embeddings | Cloud models | Cloud models | Cloud models | Cloud models |
| Published multilingual eval | Yes — per-language leaderboard, reproducible from one command | No | No | No | No | No | No | No | No |
| Reasoning plane (contradiction / drift / explain) | Yes — on-device | No | No | No | No (similarity only) | No | No | No | No |
| On-device NPU/ANE inference | Yes — CoreML/ANE + ONNX NPU adapters, capability-detected | No (cloud) | No (cloud) | No (cloud) | No (cloud) | No (cloud) | No (cloud) | No (cloud) | No (cloud) |
| Multi-device sync | CRDT + untrusted relay (relay sees only ciphertext); host-app wiring is a current limitation | Cloud-native (server is source of truth) | Cloud-native | Cloud-native | Cloud-native | Cloud-native | Cloud-native | Cloud-native | Cloud-native |
| End-user UI | Yes (reference web UI you can ship) | Yes (product) | Yes (product) | Yes (product) | No (BYO) | Yes (product) | Yes (product) | Yes (product) | Yes (product) |
| High availability | Yes (self-hosted active-passive failover) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) |
| You embed it | Yes (substrate) | No (product) | No (product) | No (product) | Yes (vector DB) | No (product) | No (product) | No (product) | No (product) |

A few of the new capability rows are worth unpacking:

- **Reasoning plane** — most retrieval tools answer *"what is similar to
  X"*. Knowledge additionally answers *"what contradicts X"*, *"how has
  belief about X drifted as evidence changed"*, and *"why was this answer
  retrieved"*, privately and on-device, via the
  `/api/v1/reasoning/{contradictions,drift,explain}` endpoints.
- **Published multilingual eval** — quality is graded by a deterministic,
  offline harness rolled up into a per-language leaderboard that
  regenerates byte-for-byte from one command. The honest current state
  (the default 1.7B model is weak on some CJK/Arabic recaps; the opt-in
  4B model is recommended for non-Latin deployments) is published, not
  hidden.
- **Multi-device sync** — the add-wins CRDT merge math, delta transport,
  per-scope XChaCha20-Poly1305 sealing, and an untrusted relay (which
  only ever holds opaque ciphertext) ship as a library-level capability
  with a ≥3-replica convergence test. Wiring it into the host-app
  lifecycle (background scheduling, retry/backoff) and post-quantum key
  establishment for cross-device key transport are current limitations,
  not shipping end-user features.

## How to read this

- **vs. Copilot / Glean / Dust / Notion AI** — these are finished
  products with great UX. Knowledge is a substrate you build *into your
  own* product. Choose them if you want a turnkey assistant over
  Microsoft/Google/Notion data (or, with Dust, a hosted multi-agent
  workspace); choose Knowledge if you're building a product and need
  private, embeddable memory that runs on-device or in-region.
- **vs. Pinecone / Weaviate** — these are hosted vector databases; you
  bring embeddings and run ANN retrieval in the cloud. Knowledge is a
  full on-device knowledge pipeline (evidence → observation → concept →
  synthesis) with crypto, permissions, connectors, and a reasoning plane,
  not just vector search. If all you need is a managed ANN index, a
  vector DB is simpler; if you need private end-to-end memory, Knowledge
  does more.
- **vs. agent memory layers — Mem0 / Zep / Letta (MemGPT)** — these are
  hosted/cloud memory APIs for AI agents, and they publish eval
  leaderboards. They are a close functional analogue to Knowledge's
  memory plane, so the differences are sharp: Knowledge runs **on-device
  and private**, offers **cryptographic forgetting** (key destruction,
  not soft delete), publishes **multilingual quality per language**, and
  adds a **reasoning plane** (contradiction/drift/explain) beyond
  similarity recall. Knowledge also publishes its own reproducible eval
  board. Choose a hosted memory API for the lowest-friction cloud
  integration; choose Knowledge when memory must stay on the device and
  be cryptographically erasable.
- **vs. connector/ETL — Fivetran / Airbyte / Nango** — these are managed
  cloud data pipelines. Knowledge ships 140 connectors **on-device** with
  an honest liveness distinction (contract-stable vs live-verified) and
  **regional coverage** (SEA/GCC and other markets) rather than a
  US-centric SaaS catalogue. Choose a managed pipeline when you want data
  centralised in a cloud warehouse; choose Knowledge when ingestion must
  stay local to the device or region.
- **vs. on-device AI — Apple Intelligence / Rewind** — these are closed,
  single-vendor, single-platform. Knowledge is cross-platform,
  embeddable, and PQC-secured, with measured quality and an open
  reasoning/eval story.
- **vs. Guru / Notion AI Q&A / Google NotebookLM / Mem.ai** — these are
  hosted question-answering layers over a cloud knowledge base (a wiki,
  a Notion workspace, uploaded documents, or personal notes). They are
  excellent turnkey products, but the content lives in a vendor cloud,
  there is no offline mode, and "forgetting" is a soft delete rather
  than key destruction. Choose them for a fast hosted answer engine;
  choose Knowledge when the content must stay on-device, work offline,
  and be cryptographically erasable — and when you need regional
  connector coverage (140 connectors across 10 markets) rather than a
  US-centric SaaS set.

## When *not* to choose Knowledge

- You want a hosted product your users log into, not a library you
  embed.
- You need centralized analytics across all users' content (Knowledge
  keeps content on-device by design).
- Your team has no appetite for shipping native code into mobile/
  desktop apps.

## Further reading

- [use-cases.md](use-cases.md) — what Knowledge is good at.
- [cost-model.md](../operator/cost-model.md) — the $0/user claim,
  honestly bounded.
- [faq.md](faq.md).
