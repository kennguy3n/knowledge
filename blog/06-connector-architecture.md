# Connector Architecture

> **TL;DR:** Knowledge ships 140 connectors behind a single `Connector`
> contract: OAuth2 with token refresh, full-then-incremental delta sync,
> real content fetch, webhook subscriptions, and ACL projection so
> source-system permissions follow the data in.

## The Business Problem

A 50-person agency runs on Notion (docs and wikis), Slack (decisions
and discussion), and Google Drive (deliverables). Their institutional
knowledge is real but scattered: the answer to "what did we decide
about the Acme rebrand?" exists, but it is spread across three tools
and nobody can find it. They want one assistant that can draw on all
three — without a quarter-long data-engineering project to build and
maintain ingestion pipelines, and without flattening the careful
permission boundaries those tools enforce.

The permissions point is subtle and important. If you pull every
document from Drive into a shared index, you have just created a
system where anyone who can query the assistant can read documents they
were never granted access to in Drive. A connector that ignores source
permissions is a data-leak generator.

## The Technical Approach

The [`connector_framework` crate](../crates/connector_framework/)
defines the boundary; the [`connectors` crate](../crates/connectors/)
ships the concrete implementations. The
[connector protocol spec](../docs/technical/connector-protocol.md) is
the full reference. Every connector implements one contract with four
responsibilities:

1. **Authentication.** OAuth2 against the source, with tokens held in a
   vault and refreshed automatically as they expire. The host kicks off
   the OAuth flow once; the framework keeps the connection alive.

2. **Sync.** A state machine: an initial **full** sync, then
   **incremental** delta pulls using the cursor the previous sync
   returned, with explicit **failure** and **recovery** transitions and
   backoff so a flaky source does not wedge the pipeline.

3. **Content fetch.** Real document-content retrieval — not just
   metadata — so the substrate ingests the actual text to extract and
   index.

4. **ACL projection.** The connector projects the source system's
   access-control lists into the substrate's permission model via an
   `AclSyncEngine`, so a document's readership in Drive becomes the set
   of principals allowed to retrieve it through Knowledge. This is what
   keeps source permissions intact — covered in depth in
   [post 9](09-multi-tenant-at-scale.md) and the
   [permission model](../docs/technical/permission-model.md).

The catalog spans **140 connectors** across the common B2B sources —
file stores (Google Drive, OneDrive, Dropbox, Box, SharePoint), docs and
wikis (Notion, Confluence, Google Docs/Sheets), CRM and support
(Salesforce, HubSpot, Zendesk, Intercom, Freshdesk, ServiceNow, Pipedrive),
project tracking (Jira, Linear, Asana, Monday, ClickUp, Trello), chat and
meetings (Slack, Teams, Discord, Zoom, Google Meet/Calendar), developer
tools (GitHub, GitLab, Bitbucket), design (Figma, Miro), finance
(Stripe, QuickBooks, Xero, Shopify, Airtable, DocuSign), and email —
plus 100 region-focused platforms across 10 markets: Vietnam,
Singapore/Thailand/SEA, the GCC/Middle East, the UK, Germany, France,
Switzerland, Australia, Latin America, and an expanded SEA batch.

Every provider implements the full contract, but the catalog is honest
about how far each has been *verified* against a live API. Most are
**contract-stable** — full contract plus unit coverage against canned
provider responses — while five exemplars (GitHub, Slack, Notion, MoMo,
Stripe) are **live-verified** by a committed cassette that replays the
whole lifecycle against recorded real provider traffic in CI. The label
rides in catalog metadata via `ConnectorKind::maturity()`, so operators
reason about liveness programmatically rather than trusting a flat
"stable" count. The full list and per-provider status live in the
[roadmap](../docs/product/roadmap.md#connector-maturity).

Some connectors also support **webhook subscriptions** so the substrate
learns about changes as they happen rather than only on the next poll
(gated behind the `webhook-server` feature).

## Implementation Walk-through

For the agency, connecting a source is a create-then-sync flow:

```text
create_connector(kind, tenant_scope)   // returns a connector instance
authenticate(connector_id)             // OAuth2; tokens vaulted + refreshed
sync(connector_id)                     // full sync, then incremental
// documents are ingested, extracted, indexed, and ACL-projected
```

After the initial sync, incremental pulls keep the substrate current,
and ACL projection means a query only ever returns documents the
querying principal is allowed to see. Writing a *new* connector follows
the same contract: the [add-a-connector guide](../docs/guides/add-a-connector.md)
walks through implementing the `Connector` trait, projecting ACLs,
wiring the `http-client` / `webhook-server` feature flags, and handling
rate limits and failures — and explains why a fresh connector should
ship as unstable until it has soaked.

## Performance & Cost Implications

Connector sync is built to move volume. The
[benchmarks](../docs/technical/benchmarks.md) measure the sync
throughput path — processing connector events — at roughly **8 million
events/second** for the in-process pipeline (the real-world ceiling is
the source API's rate limits, not the substrate). The framework's
backoff and incremental cursors are designed to stay well inside those
limits.

Operationally, connectors run in the hybrid and enterprise
[deployment modes](../docs/product/deployment-scenarios.md) through a
lightweight gateway, while synthesis stays on-device or in a TEE. The
agency does not run a data-engineering team; they authorize three
connectors and let the framework handle refresh, delta sync, and
permission projection.

## What's Next

That completes the technical foundation — storage, extraction,
forgetting, crypto, inference, and connectors. Series 2 shifts from
*how it's built* to *how you run it*. The next post takes the substrate
from `cargo run` to a real production deployment across the three
deployment modes.

---
*This is part 6 of the "Building Knowledge" series. [Previous: On-Device Inference Under Constraints](05-on-device-inference-under-constraints.md) | [Next: Zero to Production Deployment](07-zero-to-production-deployment.md)*
