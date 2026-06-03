# The Building Knowledge Series

> **TL;DR:** A three-part engineering blog series on building a
> privacy-first, on-device knowledge substrate for AI applications —
> from first principles, through production operations, to real-world
> industry deployments.

[Knowledge](../README.md) is a privacy-first, post-quantum-secure
knowledge substrate for AI applications. It is on-device by default,
costs $0/user/month at the infrastructure layer, works offline, and
extracts structured memory across 22 languages. This series explains
how it is built and how to deploy it — grounded in the actual code,
the [architecture](../docs/technical/architecture.md), and the
published [benchmarks](../docs/technical/benchmarks.md).

The series is written for three audiences at once: engineers who want
to understand the design, operators who run it in production, and
product teams deciding whether it fits their problem. Each post stands
alone, but they build on one another.

## Series 1 — Building Knowledge From Scratch (Technical Foundation)

The design decisions that make an on-device knowledge substrate
possible, one subsystem at a time.

1. [Why On-Device Memory](01-why-on-device-memory.md) — the case against server-side RAG for privacy-conscious apps.
2. [The Multilingual Extraction Engine](02-multilingual-extraction-engine.md) — structured observations across 22 languages.
3. [Memory That Forgets](03-memory-that-forgets.md) — decay as a feature and cryptographic forgetting.
4. [Post-Quantum Crypto for Mortals](04-post-quantum-crypto-for-mortals.md) — why PQC matters now, and the key hierarchy.
5. [On-Device Inference Under Constraints](05-on-device-inference-under-constraints.md) — running SLMs on 2–8 GB devices.
6. [Connector Architecture](06-connector-architecture.md) — pulling knowledge from where it already lives.

## Series 2 — Scaling Knowledge (Production & Operations)

Taking the substrate from `cargo run` to a production deployment that
serves real organizations.

7. [Zero to Production Deployment](07-zero-to-production-deployment.md) — the three deployment modes.
8. [Performance at Device Scale](08-performance-at-device-scale.md) — sub-15 ms retrieval over 500K messages.
9. [Multi-Tenant at Scale](09-multi-tenant-at-scale.md) — thousands of organizations from one deployment.
10. [Cost Engineering: Zero Marginal](10-cost-engineering-zero-marginal.md) — how $0/user actually works.
11. [Sync Without Servers](11-sync-without-servers.md) — CRDT multi-device sync with no central authority.
12. [Observability Without Ops](12-observability-without-ops.md) — monitoring a substrate that runs on user devices.

## Series 3 — Knowledge in the Real World (Industry & Geography)

What the substrate looks like in regulated industries and across
geographies with different constraints.

13. [Knowledge for Healthcare](13-knowledge-for-healthcare.md) — HIPAA, patient data on-device, cryptographic erasure.
14. [Knowledge for Financial Services](14-knowledge-for-financial-services.md) — SOX/PCI, long-term retention, PQC.
15. [Knowledge for Legal](15-knowledge-for-legal.md) — privilege, matter-scoped memory, discovery export.
16. [Knowledge Across APAC](16-knowledge-across-apac.md) — CJK extraction, data residency, device constraints.
17. [Knowledge for Education](17-knowledge-for-education.md) — FERPA/COPPA, offline-first, multilingual classrooms.
18. [Knowledge for SMB](18-knowledge-for-smb.md) — the no-ops deployment for a 5–50 person team.

## Where to go next

- New to the project? Start with the [getting-started guides](../docs/getting-started/README.md).
- Evaluating it for a product? See [use cases](../docs/product/use-cases.md) and the [comparison](../docs/product/comparison.md).
- Ready to build? The [API cookbook](../docs/guides/api-cookbook.md) and [build-a-chat-app](../docs/guides/build-a-chat-app.md) tutorials are the fast path.
