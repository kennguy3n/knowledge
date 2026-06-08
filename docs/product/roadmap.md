# Roadmap

This is a public, directional roadmap — not a commitment or a dated
plan. Priorities shift with community input; see
[CONTRIBUTING.md](../../CONTRIBUTING.md) for how to influence them.

## Shipped in 1.0

- 24-crate Rust workspace: full on-device knowledge substrate.
- Post-quantum cryptography (ML-KEM-768, ML-DSA-65).
- 22-language multilingual extraction.
- 10 production connectors (including GitHub).
- Go API gateway with the full REST surface.
- Three deployment modes: on-device, hybrid, enterprise.
- Criterion.rs benchmark suite with documented results.

## Shipped since 1.0

Cumulative since the 1.0 release (see [CHANGELOG.md](../../CHANGELOG.md)
for the full v1.1.0 entry):

- **Substrate high availability** — active-passive failover via WAL
  shipping over NATS JetStream, leader election through a NATS key-value
  lease, and replication-lag monitoring.
- **End-user reference UI** — a Next.js chat/search/memory app
  (`apps/knowledge-ui/`, served on `:3002`) alongside the operator
  `admin/` dashboard.
- **Bundled SLM** — the Bonsai-1.7B GGUF is baked into the published
  `llama-server` image, so `docker compose up` has synthesis working
  with zero manual model download.
- **30 Asia & GCC connectors** — 10 Vietnam, 10 Singapore/Thailand/SEA,
  and 10 GCC/Middle East providers, bringing the catalog to 70 stable.
- **70 regional connectors** — 10 each for the UK, Germany, France,
  Switzerland, Australia, Latin America, and an expanded SEA batch,
  doubling the catalog to **140 stable** across 7+ regions.
- **Security-audit prep** — audit scope/guide/finding-template docs,
  hardened default credentials (no-default passwords), an offline
  master-key rotation tool, and a crypto fuzz harness.
- **One-command setup** — `scripts/install.sh` / `install.ps1`, an
  admin first-run wizard, and a managed-cloud inference adapter.

## Connector maturity

The catalog spans **140 stable providers** across 7+ regions. Each implements the full
`Connector` contract — OAuth2 with refresh, full-then-incremental sync,
content fetch, optional webhooks, and ACL projection — with unit
coverage. New contributed connectors still follow the
[maturity path](../guides/add-a-connector.md#maturity-expectations)
(land unstable, graduate once soaked) before joining this list.

| Domain | Connectors |
|---|---|
| Core / original (10) | Google Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, Email, GitHub |
| CRM & productivity (10) | Salesforce, ServiceNow, Zendesk, Linear, Asana, Monday, ClickUp, Freshdesk, Intercom, Pipedrive |
| Cloud storage & communication (10) | Dropbox, Box, SharePoint, Teams, Discord, Zoom, Google Calendar, Google Docs, Google Sheets, Google Meet |
| Business & developer tools (10) | QuickBooks, Xero, Stripe, Shopify, Airtable, GitLab, Bitbucket, Trello, Miro, DocuSign |
| Vietnam (10) | Zalo, VNPay, MoMo, Tiki, Shopee VN, Lazada VN, Viettel Post, KiotViet, Sapo, Base.vn |
| Singapore / Thailand / SEA (10) | LINE, Grab, Gojek, Talenox, Odoo (SEA), Fastwork, TrueMoney, SCB Easy, PromptPay, Tokopedia |
| GCC / Middle East (10) | Careem, Talabat, Noon, Amazon.ae, Tabby, Foodics, Zoho, Bayt, Fetchr, PayFort |
| United Kingdom (10) | Monzo Business, Revolut Business, FreeAgent, GoCardless, Royal Mail, Deliveroo, Just Eat, Companies House, HMRC MTD, Starling |
| Germany (10) | N26 Business, DATEV, lexoffice, DHL Business, Otto, Zalando, Deutsche Post, Personio, sevDesk, Billomat |
| France (10) | Qonto, Pennylane, PayFit, Colissimo, Cdiscount, MangoPay, Brevo (Sendinblue), OVHcloud, Alan, Swile |
| Switzerland (10) | PostFinance, TWINT, Swiss Post, Bexio, Abacus, ricardo.ch, Digitec Galaxus, SIX Payment, Klara, Beem |
| Australia (10) | MYOB, Afterpay, Australia Post, Employment Hero, Deputy, Tyro, Prospa, SEEK, Campaign Monitor, Pinch |
| Latin America (10) | MercadoLibre, Rappi, Nubank Business, PagSeguro, iFood, VTEX, Clip, Ualá, Falabella, Correos de México |
| SEA expanded (10) | Shopee (regional), Lazada (regional), SeaMoney/ShopeePay, GrabPay, Bukalapak, Blibli, Traveloka, AirAsia, MyEG, GCash |

See [connector-protocol.md](../technical/connector-protocol.md).

## Areas we're exploring

These are directions, not promises:

- Broadening the on-device inference adapter set beyond MLX / llama.cpp.
- Additional first-party connectors.
- Richer sync transports on top of the CRDT
  [sync protocol](../technical/sync-protocol.md).
- Expanded language coverage in the extraction lexicon.

## Where to contribute

Good places to start:

- New connectors — [add-a-connector.md](../guides/add-a-connector.md).
- Inference adapters —
  [custom-synthesis.md](../guides/custom-synthesis.md).
- Docs, examples, and platform integration guides.
- Issues labelled **good first issue**.

## Further reading

- [CONTRIBUTING.md](../../CONTRIBUTING.md) — how to get involved.
- [faq.md](faq.md).
