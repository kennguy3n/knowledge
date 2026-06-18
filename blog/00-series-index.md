# The Building Knowledge Series

> **TL;DR:** A multi-part engineering blog series on building a
> privacy-first, on-device knowledge substrate for AI applications —
> from first principles, through production operations and real-world
> industry deployments, to the connected platform around it. Plus a
> screenshot-driven [field series](executive-personas/README.md) that
> drives the *running* system, and a
> [how-to-build companion](how-to-build/README.md) that pairs the
> engineering with the business decision behind every layer.

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

## Series 4 — The Connected Platform (Ecosystem & Operations)

Connecting Knowledge to the tools teams already use, and operating it
without a platform team.

19. [140 Connectors](19-connector-ecosystem.md) — connecting Knowledge to every tool your team uses.
20. [Managing Knowledge Without a DevOps Team](20-admin-without-ops.md) — the browser-based admin dashboard.
21. [30 Connectors for Vietnam, SEA & the GCC](21-asia-gcc-connectors.md) — regional platforms, data residency, and multilingual extraction.
22. [Zero to Running in One Command](22-zero-to-running.md) — the installer, bundled model, reference UI, and first-run wizard.
23. [High Availability for the Substrate](23-substrate-high-availability.md) — WAL replication over NATS, failover, and lag monitoring.

## Capstone — The Product Vision

How every subsystem above composes into one product that serves both
consumers and enterprises without forking the architecture.

24. [The AI Privacy Spectrum](24-the-ai-privacy-spectrum.md) — five trust postures on one substrate; the `User`/`Channel`/`Domain` scope ladder for B2C (per user / channel / community) and B2B (per user / channel / domain); and mixed-language memory across 140 connectors.

## Field Series — Executives on the Substrate (Real Run, Real Output)

A separate, screenshot-driven series that drives the *running* system —
gateway, encrypted substrate, and the Bonsai-1.7B model — through five
executive personas across five countries and seven languages, and
reports verbatim what happened, including where the model's output is
weak. See [the field-series index](executive-personas/README.md).

- [Five Executives, One Substrate](executive-personas/01-five-executives-one-substrate.md) — how the system works, via a CFO's month-end close.
- [Multilingual Recall, in Practice](executive-personas/02-multilingual-recall.md) — real FR/JA/PT/ES/HI queries and how `BR-2505`-style identifiers stay searchable through FTS5.
- [Synthesis Quality: A Deterministic Pipeline](executive-personas/03-synthesis-quality.md) — verbatim model output, good and bad, and why grammar guarantees shape not substance.
- [The UI, and What It Honestly Reveals](executive-personas/04-design-and-product-gaps.md) — the design system, the chat panel's two memory surfaces, and the live Memory page.

## How-to-Build Series — Rebuilding the Substrate (Engineering + Business)

A build-order companion that pairs the **engineering** ("what to build
and how", in dependency order) with the **business decision** ("the
scenario, the trade-off, and the competitor we're choosing differently
from") for every layer. Read this if you want to rebuild a substrate
like this one — and be able to defend each choice to a CFO. See
[the how-to-build index](how-to-build/README.md).

- [Architecture & Build Order](how-to-build/01-architecture-and-build-order.md) — the device-vs-cloud fork, the crate graph, and the network-free invariant. *Build-vs-buy: substrate vs. product.*
- [The Encrypted Store](how-to-build/02-the-encrypted-store.md) — `crypto` + `evidence_store`, the DEK/CEK hierarchy, cryptographic forgetting. *Erasure vs. soft-delete vendors.*
- [Observation & Extraction](how-to-build/03-observation-and-extraction.md) — lexicon-first 22-language extraction with published F1 floors. *On-device NLP vs. cloud APIs.*
- [Retrieval & the Memory Graph](how-to-build/04-retrieval-and-memory.md) — the `HybridRetriever`, decay, concept graph. *Power-user latency vs. server-side RAG.*
- [Inference Routing on Device](how-to-build/05-inference-routing.md) — device tiers + the accelerator chain. *1.7B-vs-4B, measured not asserted.*
- [Synthesis & Honest Eval](how-to-build/06-synthesis-and-eval.md) — deterministic synthesis + a public multilingual leaderboard. *Publishing a quality bar vs. vendors who don't.*
- [The Reasoning Plane](how-to-build/07-the-reasoning-plane.md) — contradiction / drift / explain, wired end-to-end. *Understanding vs. similarity-only tools.*
- [140 Connectors, Honestly](how-to-build/08-connectors.md) — the framework, cassette liveness, maturity labels, regional reach. *Contract-stable vs. live-verified; regional vs. US-centric ETL.*
- [Sync & Multi-Device](how-to-build/09-sync-and-multi-device.md) — CRDT over an untrusted ciphertext relay. *No-trusted-server sync vs. cloud-native.*
- [The Server & Multi-Tenancy](how-to-build/10-server-and-multitenancy.md) — Go gateway, Zanzibar permissions, audit, 5k-tenant fairness. *Self-host/in-region vs. SaaS.*
- [Packaging & Shipping](how-to-build/11-packaging-and-shipping.md) — FFI/N-API, reference UI, installer, device benchmark. *Zero-to-running, no-ops SMB.*
- [The Decision Playbook](how-to-build/12-decision-playbook.md) — the rebuild checklist, the cost model, and the head-to-head decision matrix on one page.

## Where to go next

- New to the project? Start with the [getting-started guides](../docs/getting-started/README.md).
- Evaluating it for a product? See [use cases](../docs/product/use-cases.md) and the [comparison](../docs/product/comparison.md).
- Ready to build? The [API cookbook](../docs/guides/api-cookbook.md) and [build-a-chat-app](../docs/guides/build-a-chat-app.md) tutorials are the fast path.
