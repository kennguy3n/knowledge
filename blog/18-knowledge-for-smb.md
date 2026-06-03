# Knowledge for SMB

> **TL;DR:** A 5–50 person business doesn't have a platform team. The
> hybrid "no-ops" deployment gives them a knowledge assistant over their
> existing SaaS tools — a lightweight gateway, OAuth a few connectors,
> on-device synthesis — without standing up AI infrastructure or paying
> per-seat AI prices.

## The Business Problem

A 30-person marketing agency has the same knowledge problem as the
enterprise from [post 9](09-multi-tenant-at-scale.md), minus the
resources to solve it the enterprise way. Their institutional memory
lives in Notion, Slack, and Google Drive, and nobody can find anything.
But they have no DevOps engineer, no security team, and no appetite for
a six-figure enterprise-search contract priced per seat.

What an SMB actually needs is narrow and specific: connect the handful
of tools they already use, get a useful assistant over the combined
knowledge, and *not* become accidental infrastructure operators in the
process. Most AI knowledge products are built for one of the two
extremes — a single-user app or a large enterprise — and serve the
"too big for a toy, too small for a platform" middle poorly.

## The Technical Approach

The [hybrid deployment mode](07-zero-to-production-deployment.md) is
designed for exactly this middle. It is the "no-ops" configuration:

- **A lightweight gateway, not a fleet.** Hybrid mode runs a small Go
  gateway for connector sync; there is no large model-serving cluster
  and no Postgres requirement of the full enterprise mode. The
  [deployment scenarios](../docs/product/deployment-scenarios.md) map
  this to the SME profile.
- **Connect what you already use** ([post 6](06-connector-architecture.md)).
  OAuth the relevant connectors — Notion, Slack, Google Drive for our
  agency — and the framework handles token refresh, delta sync, and ACL
  projection, so the assistant respects who-can-see-what from the source
  tools without manual permission setup.
- **Synthesis stays on-device or in a TEE** ([post 5](05-on-device-inference-under-constraints.md)),
  so the heavy, sensitive processing doesn't require the agency to run
  (and secure) an inference server.
- **Configuration over code** ([configuration](../docs/operator/configuration.md)).
  The gateway is configured through environment variables with sensible
  defaults; the [deployment guide](../docs/operator/deployment-guide.md)
  provides a production checklist a non-specialist can follow.

The result is a deployment a technically-comfortable generalist can
stand up — not a project that needs a platform team.

## Implementation Walk-through

The SMB path is short:

```text
1. Stand up the gateway   (Docker Compose topology from the deployment guide)
2. Connect sources        (OAuth Notion + Slack + Drive; framework syncs + projects ACLs)
3. Query                  (assistant answers over the combined, permission-aware corpus)
```

A connector-selection guide for picking the right sources is part of
the [for-operators](../docs/getting-started/for-operators.md) and
[deployment scenarios](../docs/product/deployment-scenarios.md) docs,
and the [FAQ](../docs/product/faq.md) answers the common "do I need a
server?" / "is this a product or a library?" questions an SMB evaluator
asks first. If the agency later outgrows hybrid, the same substrate
contract scales into [enterprise mode](09-multi-tenant-at-scale.md)
without re-integration ([post 7](07-zero-to-production-deployment.md)).

## Performance & Cost Implications

For an SMB, cost is the deciding factor, and this is where the
zero-marginal design ([post 10](10-cost-engineering-zero-marginal.md))
pays off most visibly. The only infrastructure is a small stateless
gateway; synthesis runs at the edge with no per-token bill; and there is
no per-seat enterprise license. The
[comparison](../docs/product/comparison.md) lays out the TCO honestly
against Copilot, Glean, and Notion AI — Knowledge is a substrate the
agency (or their vendor) builds into a product, and its economics favor
the small team that can't absorb per-seat AI pricing.

Performance is the same on-device profile as everywhere else: ~9.7 ms
retrieval ([post 8](08-performance-at-device-scale.md)) and multilingual
extraction ([post 2](02-multilingual-extraction-engine.md)), so a small
team gets the same substrate the enterprise does.

## What's Next

That closes the "Building Knowledge" series — from the first design
decision through production operations to real-world deployments across
industries and geographies. To go further:

- Build something: the [API cookbook](../docs/guides/api-cookbook.md)
  and [build-a-chat-app](../docs/guides/build-a-chat-app.md) /
  [build-b2b-knowledge](../docs/guides/build-b2b-knowledge.md) tutorials.
- Evaluate the fit: [use cases](../docs/product/use-cases.md),
  [comparison](../docs/product/comparison.md), and [FAQ](../docs/product/faq.md).
- Read the design: the [architecture](../docs/technical/architecture.md)
  and [design](../docs/technical/design.md) docs.
- Revisit the start: [the series index](00-series-index.md).

---
*This is part 18 of the "Building Knowledge" series. [Previous: Knowledge for Education](17-knowledge-for-education.md) | [Series index](00-series-index.md)*
