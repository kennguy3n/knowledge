# 70 Connectors: Connecting Knowledge to Every Tool Your Team Uses

> **TL;DR:** Knowledge now ships **70 stable connectors** across CRM,
> cloud storage, communication, finance, developer tools, and 30
> region-focused platforms across Vietnam, Singapore/Thailand/SEA, and
> the GCC/Middle East. They all sit behind one `Connector` contract, so
> the substrate ingests, deduplicates, and permission-scopes content
> from every source the same way — add a provider and the entire
> pipeline lights up for it.

## The Business Problem

Institutional knowledge does not live in one place. A growing company
keeps decisions in Slack and Teams, documents in Google Drive, Notion,
and SharePoint, customer context in Salesforce, HubSpot, and Zendesk,
issues in Jira, Linear, and GitHub, and contracts in DocuSign. The
answer to "what did we promise this customer?" is real — it is just
scattered across a dozen systems, each with its own API, auth model,
pagination quirks, and permission semantics.

The naive fix is a pile of bespoke ingestion scripts. That path leads
to a brittle data-engineering project that breaks every time a provider
rotates a token format or changes a cursor, and — worse — usually
flattens the careful access controls those source systems enforce. The
goal of the connector layer is the opposite: make adding a source a
*configuration* decision, not an engineering project, and carry the
source's permissions along with its data.

## One contract, seventy sources

Every connector implements the same `Connector` trait from the
`connector_framework` crate, covering four concerns:

1. **Authentication** — OAuth2 with token refresh through the
   framework's token vault, never hand-rolled per provider.
2. **Sync** — a `full → incremental → failure → recovery` cursor state
   machine, so a transient API error never loses progress.
3. **Content fetch** — discover changed documents, fetch full bodies
   under the provider's rate limits, and emit `DocumentCreated` /
   `DocumentUpdated` evidence.
4. **Push (optional)** — describe a webhook subscription and parse
   inbound events, with the source's ACLs projected into the permission
   graph so reachability in Knowledge mirrors the source.

Because the contract is uniform, the substrate downstream — extraction,
decay, synthesis, search — does not care whether a document came from
Dropbox or DocuSign. Add a provider and the entire pipeline lights up
for it.

## The catalog

All 70 connectors are **stable** — each meets the trait-impl and
test-coverage bar and is safe to build on, grouped here by domain:

- **Core / original** — Google Drive, OneDrive, Notion, Jira,
  Confluence, Figma, HubSpot, Slack, Email, GitHub.
- **CRM & productivity** — Salesforce, ServiceNow, Zendesk, Linear,
  Asana, Monday, ClickUp, Freshdesk, Intercom, Pipedrive.
- **Cloud storage & communication** — Dropbox, Box, SharePoint, Teams,
  Discord, Zoom, Google Calendar, Google Docs, Google Sheets,
  Google Meet.
- **Business & developer tools** — QuickBooks, Xero, Stripe, Shopify,
  Airtable, GitLab, Bitbucket, Trello, Miro, DocuSign.
- **Vietnam** — Zalo, VNPay, MoMo, Tiki, Shopee VN, Lazada VN,
  Viettel Post, KiotViet, Sapo, Base.vn.
- **Singapore / Thailand / SEA** — LINE, Grab, Gojek, Talenox,
  Odoo (SEA), Fastwork, TrueMoney, SCB Easy, PromptPay, Tokopedia.
- **GCC / Middle East** — Careem, Talabat, Noon, Amazon.ae, Tabby,
  Foodics, Zoho, Bayt, Fetchr, PayFort.

## How a connector earns "stable"

Stable is a bar, not a default. Every connector in the catalog
implements the full `Connector` contract — auth, the sync state
machine, content fetch, optional webhooks, and ACL projection — and
carries unit coverage against canned provider responses. The hard
lessons a live API teaches — undocumented rate-limit headers,
pagination that changes shape mid-stream, webhook payloads that differ
from the docs — are exactly what that contract and its tests are built
to absorb.

That is why brand-new *contributed* connectors still land **unstable**
and soak against the real API before they graduate — the same path
GitHub took before this release. The policy lives in
[add-a-connector.md](../docs/guides/add-a-connector.md#maturity-expectations),
and the current status of every provider is in the
[connector maturity table](../docs/product/roadmap.md#connector-maturity).

## What it means for you

For an SME, this is the difference between "Knowledge works with the
three tools we tried" and "Knowledge works with the stack we already
have." OAuth a handful of connectors from the
[admin dashboard](20-admin-without-ops.md), let the substrate sync and
synthesize, and ask questions across all of them — with each source's
permissions intact and at $0 marginal cost per user.

## Further reading

- [Connector Architecture](06-connector-architecture.md) — the design of
  the `Connector` contract this catalog is built on.
- [30 Connectors for Vietnam, SEA & the GCC](21-asia-gcc-connectors.md) —
  the regional catalog, data residency, and multilingual extraction.
- [add-a-connector.md](../docs/guides/add-a-connector.md) — write your
  own connector for a source we don't ship.
- [Managing Knowledge Without a DevOps Team](20-admin-without-ops.md) —
  wiring connectors from the browser.
