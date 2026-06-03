# Cost Engineering: Zero Marginal

> **TL;DR:** "$0/user/month" is not marketing — it's a consequence of
> moving memory, retrieval, and synthesis onto the user's device. This
> post explains exactly how that works, what costs remain in the
> gateway modes, and the honest cases where the model breaks down.

## The Business Problem

A product leader is comparing the total cost of ownership of an
AI-memory feature for 100,000 users against the alternatives — a
Copilot-style assistant, a Glean-style enterprise search, or a
roll-your-own stack on a managed vector database. With server-side
architectures, the cost line is brutal and linear: every user's
embeddings, vector storage, retrieval compute, and inference tokens are
a recurring bill that grows with adoption. Success makes the bill
bigger.

For a venture-funded consumer app chasing thin margins, or an
enterprise buyer wary of per-seat AI pricing, the question is blunt:
*what does this actually cost to run at scale, and what's the catch?*

## The Technical Approach

The zero-marginal claim follows directly from the architecture in
Series 1. The [cost model](../docs/operator/cost-model.md) is the full
breakdown; the logic:

**On-device mode: genuinely zero marginal infra cost.** When the
substrate runs on the user's device ([post 1](01-why-on-device-memory.md)):

- **Storage** is the user's disk, not your database.
- **Retrieval** is the user's CPU ([post 8](08-performance-at-device-scale.md)),
  not your retrieval fleet.
- **Extraction** is lexicon-driven on-device
  ([post 2](02-multilingual-extraction-engine.md)), not a cloud model.
- **Synthesis** runs on-device via the inference router
  ([post 5](05-on-device-inference-under-constraints.md)), not a metered
  token API.

There is no per-user line item because there is no per-user server-side
work. You pay once to build and ship the software; the 100,000th user's
memory costs the same as the first user's — nothing.

**Where costs remain (hybrid / enterprise).** The gateway modes are not
free, and pretending otherwise would be dishonest. They add:

- A **Go gateway** tier — stateless and horizontally scalable, so cost
  scales with request volume, not raw user count.
- **Postgres** (enterprise) for tenant/permission metadata.
- **Connector sync** traffic, bounded by source-API rate limits.

These are deliberately the *light* parts of the system. The expensive
work — storing and processing the actual knowledge — stays at the edge.

**Cost-control mechanisms.** Even in the gateway modes, the substrate
includes rate limiters and synthesis batching to keep resource use
bounded and predictable, so a burst of activity does not translate into
an unbounded bill. These are described in the
[cost model](../docs/operator/cost-model.md) and tuned via
[configuration](../docs/operator/configuration.md).

## Implementation Walk-through

The cost posture is chosen by deployment mode, not by configuration
gymnastics:

```text
On-device   -> $0 marginal infra; you ship a library
Hybrid      -> small stateless gateway; cost ~ request volume
Enterprise  -> gateway + Postgres; cost ~ tenants + activity
```

A team optimizing for cost starts on-device and only adopts the gateway
when a feature (connectors, central admin, multi-device sync) requires
it — paying for infrastructure exactly when it delivers value, and not
before. The [deployment scenarios](../docs/product/deployment-scenarios.md)
decision tree maps business needs to the cheapest mode that satisfies
them.

## Performance & Cost Implications

The performance numbers from [post 8](08-performance-at-device-scale.md)
and the [benchmarks](../docs/technical/benchmarks.md) are what make the
cost model real: retrieval at ~9.7 ms and ingest at ~1,043 msgs/sec on
the device mean the edge can actually carry the load that would
otherwise be a server bill.

**The honest limits.** Zero-marginal is a property of on-device mode,
not a universal law:

- If your product *requires* a central server view (cross-user
  analytics, server-side global search), you are back to server-side
  economics for that feature.
- Hybrid/enterprise modes have real, if modest, costs — the savings are
  relative to a fully server-side stack, not absolute zero.
- Devices vary; the lightest tiers run lighter workloads
  ([post 5](05-on-device-inference-under-constraints.md)), which is a
  capability trade, not a hidden cost.

The [comparison](../docs/product/comparison.md) lays out these
trade-offs against Copilot, Glean, Notion AI, and Pinecone without
spin: Knowledge is a substrate you build into your own product, and its
cost advantage is real precisely where the data can stay on-device.

## What's Next

Zero-marginal cost assumes data lives on individual devices — but users
have several devices. The next post is about sync without servers:
keeping a user's iPhone, MacBook, and work laptop consistent using
CRDTs, with no central authority to pay for or trust.

---
*This is part 10 of the "Building Knowledge" series. [Previous: Multi-Tenant at Scale](09-multi-tenant-at-scale.md) | [Next: Sync Without Servers](11-sync-without-servers.md)*
