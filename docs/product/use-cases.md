# Use Cases

What you can build on Knowledge, and why the on-device, privacy-first
model fits each one.

## B2C chat with memory

A consumer assistant or chat app (the KChat pattern) that remembers what
the user told it — preferences, facts, ongoing threads — without ever
shipping that memory to a server.

- **Why Knowledge:** each user's memory is encrypted on their device, so
  there's no central store to breach or subpoena, and your inference
  cost doesn't scale per token with your user base.
- **Mode:** [on-device](deployment-scenarios.md).
- **Build it:** [build-a-chat-app.md](../guides/build-a-chat-app.md).

## B2B knowledge base

A team tool that pulls knowledge from where it already lives — Notion,
Slack, Google Drive — and answers questions over it with proper access
control.

- **Why Knowledge:** connectors do real content fetching, source-system
  ACLs are projected into the permission graph, and multi-tenant
  isolation is a first-class primitive.
- **Mode:** [hybrid or enterprise](deployment-scenarios.md).
- **Build it:** [build-b2b-knowledge.md](../guides/build-b2b-knowledge.md).

## Agent long-term memory

Structured, durable memory for an LLM agent: evidence → observations →
concepts → synthesized memory, with decay so stale facts fade.

- **Why Knowledge:** the memory model is explicit and inspectable, not a
  vector blob, and forgetting is cryptographic, not best-effort.
- **Mode:** on-device or hybrid.

## Vertical / compliance-driven apps

Healthcare, financial services, legal, and education apps where data
residency and retention rules dominate the design.

- **Why Knowledge:** on-device means no cross-border transfer of user
  content; post-quantum crypto protects long-horizon records;
  cryptographic forgetting makes erasure enforceable.
- **Mode:** depends on the regulation; see
  [compliance.md](../operator/compliance.md).

## Where Knowledge is *not* the right fit

- You need a shared, server-side analytics warehouse over all users'
  data — Knowledge deliberately keeps content on the user's device.
- You want a turnkey hosted SaaS with no engineering — Knowledge is a
  substrate you embed, not a finished product.

## Further reading

- [deployment-scenarios.md](deployment-scenarios.md) — pick a mode.
- [comparison.md](comparison.md) — vs. Copilot, Glean, Notion AI,
  Pinecone.
- [faq.md](faq.md) — common questions.
