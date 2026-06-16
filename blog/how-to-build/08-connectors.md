# 140 Connectors, Honestly

> **TL;DR:** Knowledge ingests from where data already lives — 140
> built-in connectors across 10 markets. But "140 connectors" is a claim
> that's easy to inflate, so this post builds two things alongside the
> framework: an **explicit maturity label** (`Unstable` /
> `ContractStable` / `LiveVerified`) on every connector, and a
> **cassette/VCR liveness harness** that proves the live-verified ones
> against recorded real traffic. The differentiator isn't the count —
> it's the honesty and the regional coverage.

## What you are building

- **`connector_framework`** — the OAuth2 token vault, incremental +
  webhook sync state, channel-scoped attachment, and ACL sync that every
  connector shares.
- **`connectors`** — 140 vendor implementations across file stores,
  docs/wikis, CRM, support, chat/meetings, developer tools, finance, and
  **region-focused platforms** (Vietnam, SEA, GCC, and more).

## Build it: the framework before the connectors

Resist the urge to write connector #1 against a vendor SDK directly.
Build the framework first so all 140 share:

1. **A secret-safe transport seam.** Connectors talk to the network
   through a `Transport` abstraction. In tests you swap in
   `ReplayTransport` (plays recorded cassettes) and `RecordingTransport`
   (captures real traffic, **auto-redacting secrets**). This is what
   makes connectors testable without live credentials in CI.
2. **Incremental + webhook sync state.** Connectors page through a
   source and emit `DocumentCreated`/updated events; the framework tracks
   cursors so re-syncs are incremental, and accepts webhooks for push
   updates. The `bench_connector_sync_throughput` harness isolates this
   parse + event-emission cost (~8 M events/sec over a mock transport) so
   you can see the framework overhead separate from network latency.
3. **ACL sync.** Source-side permissions are mirrored so a connected
   document inherits the right scope isolation — connectors don't get to
   leak across the boundary the crypto layer (post 2) enforces.

## Build it: the maturity contract

The honesty mechanism is a type, not a footnote. Every connector carries
a `ConnectorMaturity` enum (`crates/connector_framework/src/config.rs`):

- **`ContractStable`** — the default for most of the 140. The
  request/response contract is implemented and tested against cassettes,
  but not continuously verified against the live API.
- **`LiveVerified`** — proven against recorded real traffic *and* a
  weekly live workflow. Five exemplars are live-verified today: **GitHub,
  Slack, Notion, MoMo, Stripe** (note the regional one — MoMo is a
  Vietnamese wallet, not a US SaaS).
- **`Unstable`** — explicitly flagged as not ready.

So "140 connectors" is always qualified by "5 live-verified, the rest
contract-stable" — in the docs, the [comparison](../../docs/product/comparison.md),
and the type system. See [`add-a-connector.md`](../../docs/guides/add-a-connector.md).

## The business decision: count vs. credibility, and where they live

**Scenario.** A buyer in Ho Chi Minh City or Riyadh asks whether you
connect to the platforms *their* business actually runs on — not just
Google Workspace and Salesforce.

- **US-centric ETL/connector vendors (Fivetran, Airbyte, Nango) and
  cloud assistants (Glean, Copilot).** Deep catalogues of mostly US/EU
  SaaS, run as managed *cloud* pipelines that centralize data in a
  warehouse. Regional platforms (MoMo, regional banks, SEA/GCC tools) are
  thin or absent.
- **Knowledge.** 140 connectors that run **on-device / in-region** with
  explicit regional coverage (10 markets), an honest liveness label, and
  ingestion that never has to leave the device. You trade some catalogue
  depth in mainstream US SaaS for regional reach and a privacy posture
  the managed pipelines can't offer.

The strategic point: a connector count is a vanity metric unless it's
(a) labelled by how real each one is and (b) covering the markets your
customers operate in. Building the maturity enum and the regional set is
how you make the number mean something.

## How a competitor would build this

A managed ETL vendor runs connectors as cloud jobs against a central
warehouse — operationally simpler, with a big SaaS catalogue, and the
right tool when you *want* data centralized. It cannot keep ingestion on
the device or in a specific region, and its catalogue follows the US SaaS
market. The on-device, regionally-aware, honestly-labelled approach is
the choice you make when residency and privacy are the constraint.

## What's next

One device with connectors is useful; a user has several devices. Next:
syncing memory across them through a relay that can't read it.

---
*Part 8 of "How to Build Knowledge." [Previous: The Reasoning Plane](07-the-reasoning-plane.md) | [Next: Sync & Multi-Device](09-sync-and-multi-device.md) | [Series index](README.md)*
