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
| Connectors | 140 stable | M365 + Graph-connected sources | 100+ enterprise sources | Notion + limited integrations | None (BYO embeddings) | SaaS integrations + browser ext. | Notion + limited integrations | Upload + Google Drive/Docs | Limited integrations |
| Regional connector coverage | Yes — UK, DE, FR, CH, AU, LATAM, SEA (7+ regions) | Global SaaS (US-centric) | Global SaaS (US-centric) | Limited | None | Global SaaS | Limited | None (upload-based) | Limited |
| File/media ingest | Yes (text, files, media refs, API payloads) | Files via M365 | Files via connectors | Files in Notion | No (BYO vectors) | Cards + attachments | Files in Notion | Yes (PDF, web, audio) | Notes + some files |
| Works offline | Yes | No | No | No | No | No | No | No | No |
| Post-quantum crypto | Yes (ML-KEM-768, ML-DSA-65) | No (classical TLS) | No | No | No | No | No | No | No |
| Cryptographic forgetting | Yes (key destruction) | Soft delete | Soft delete | Soft delete | Vector delete | Soft delete | Soft delete | Soft delete | Soft delete |
| Multilingual extraction | 22 languages on-device | Cloud models | Cloud models | Cloud models | BYO embeddings | Cloud models | Cloud models | Cloud models | Cloud models |
| End-user UI | Yes (reference web UI you can ship) | Yes (product) | Yes (product) | Yes (product) | No (BYO) | Yes (product) | Yes (product) | Yes (product) | Yes (product) |
| High availability | Yes (self-hosted active-passive failover) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) | Managed (vendor SLA) |
| You embed it | Yes (substrate) | No (product) | No (product) | No (product) | Yes (vector DB) | No (product) | No (product) | No (product) | No (product) |

## How to read this

- **vs. Copilot / Glean / Notion AI** — these are finished products with
  great UX. Knowledge is a substrate you build *into your own* product.
  Choose them if you want a turnkey assistant over Microsoft/Google/
  Notion data; choose Knowledge if you're building a product and need
  private, embeddable memory.
- **vs. Pinecone** — Pinecone is a hosted vector database; you bring
  embeddings and run retrieval in the cloud. Knowledge is a full
  on-device knowledge pipeline (evidence → observation → concept →
  synthesis) with crypto, permissions, and connectors, not just vector
  search. If all you need is a managed ANN index, Pinecone is simpler;
  if you need private end-to-end memory, Knowledge does more.
- **vs. Guru / Notion AI Q&A / Google NotebookLM / Mem.ai** — these are
  hosted question-answering layers over a cloud knowledge base (a wiki,
  a Notion workspace, uploaded documents, or personal notes). They are
  excellent turnkey products, but the content lives in a vendor cloud,
  there is no offline mode, and "forgetting" is a soft delete rather
  than key destruction. Choose them for a fast hosted answer engine;
  choose Knowledge when the content must stay on-device, work offline,
  and be cryptographically erasable — and when you need regional
  connector coverage (140 connectors across 7+ regions) rather than a
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
