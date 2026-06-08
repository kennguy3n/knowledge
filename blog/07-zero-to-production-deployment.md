# Zero to Production Deployment

> **TL;DR:** Knowledge has three deployment modes — on-device, hybrid,
> and enterprise — that share one substrate contract. You pick a mode
> based on where data lives and what you connect, not by rewriting your
> integration.

## The Business Problem

A SaaS startup wants to embed Knowledge in their product. The engineer
evaluating it has a practical question that the architecture diagrams
don't answer directly: *what do I actually have to deploy?* Is this a
library I link into my app, or a fleet of services I have to operate?
The answer determines whether adoption is an afternoon or a quarter.

The honest answer is "it depends on what you're building" — but that is
only useful if the options are clearly delineated. A B2C app that keeps
each user's data on their own device has radically different
infrastructure needs than a multi-tenant B2B platform syncing
connectors for a thousand organizations. Forcing both into one
deployment shape would over-burden the first and under-serve the
second.

## The Technical Approach

Knowledge defines three modes, documented in the
[deployment scenarios](../docs/product/deployment-scenarios.md) and the
[operator deployment guide](../docs/operator/deployment-guide.md):

**Mode 1 — On-device.** The substrate is embedded directly in your app
via the `ffi` (Swift/Kotlin) or `napi` (Electron) surface. There is no
backend: each user's data stays on their own device, encrypted under a
device-held master key. Infrastructure required: **none**. This is the
mode for privacy-first B2C — a private chat app, a personal assistant.

**Mode 2 — Hybrid.** A lightweight Go gateway fronts the substrate for
connector sync (pull from Notion/Slack/Drive), while synthesis stays
on-device or in a TEE. This suits SMEs who want to connect their SaaS
tools but keep the heavy, sensitive processing off a central server.
Infrastructure: a small gateway, no large model-serving fleet.

**Mode 3 — Enterprise.** The full multi-tenant deployment: gateway plus
substrate plus Postgres, with SCIM provisioning, Zanzibar permissions,
per-tenant keys, and audit. This is the mode for B2B knowledge
platforms serving many organizations with central connectors and
compliance requirements.

The crucial design property: **the substrate contract is the same
across modes.** The same ingest/query/forget API, the same scopes, the
same crypto. You can start on-device and grow into hybrid or enterprise
without re-architecting your integration — you add infrastructure
around the same core, you don't replace it.

## Implementation Walk-through

The path depends on your mode, and the
[getting-started guides](../docs/getting-started/README.md) route by
role:

- **On-device:** follow [for-developers](../docs/getting-started/for-developers.md),
  pick your platform embed guide
  ([iOS](../docs/guides/embed-in-ios.md),
  [Android](../docs/guides/embed-in-android.md),
  [Electron](../docs/guides/embed-in-electron.md)), provision a master
  key from the platform secure store, and you are running.
- **Hybrid / enterprise:** follow
  [for-operators](../docs/getting-started/for-operators.md), stand up
  the gateway via the Docker Compose topology, and configure through
  environment variables ([configuration](../docs/operator/configuration.md)).

A production rollout in the gateway modes runs through the
[deployment guide's production checklist](../docs/operator/deployment-guide.md):
master key sourced from a secret manager, authentication enabled, TLS
terminating proxy in front, the substrate's loopback port isolated,
monitoring scraping, and a backup drill rehearsed before go-live.

```text
# hybrid / enterprise, conceptually
gateway (Go, public)  ->  substrate (Rust, loopback only)  ->  Postgres
                         connectors sync via gateway
                         synthesis on-device / TEE
```

## Performance & Cost Implications

Mode selection is also a cost decision. On-device mode has **zero**
marginal infrastructure cost — the topic of
[post 10](10-cost-engineering-zero-marginal.md). Hybrid adds a small,
horizontally-scalable gateway. Enterprise adds Postgres and the
operational surface that multi-tenancy requires, but the substrate's
per-instance work remains bounded because heavy processing stays at the
edge.

Because the contract is stable across modes, the startup can ship
on-device first (cheapest, fastest to integrate), validate the product,
and adopt the gateway only when they need connectors or central
administration — without throwing away their integration.

## What's Next

Choosing a mode is the start; the next question is whether the
substrate is *fast enough* under real load. The next post digs into
performance at device scale: keeping retrieval interactive over hundreds
of thousands of messages.

---
*This is part 7 of the "Building Knowledge" series. [Previous: Connector Architecture](06-connector-architecture.md) | [Next: Performance at Device Scale](08-performance-at-device-scale.md)*
