# Knowledge

**Privacy-first, post-quantum secure memory for AI applications. On-device by default — $0 / user / month at any scale.**

Knowledge is a knowledge substrate: layered, decaying, scope-aware
memory that AI surfaces (chat, search, agents, exports) consume so they
never have to re-process raw data on every call. It runs on the user's
device by default, encrypts everything at rest with post-quantum
cryptography, and can forget — cryptographically, not by soft-delete.

---

## What is Knowledge?

- **Privacy-first** — data stays on the user's device by default. No
  server round-trip means no cross-border transfer and no central
  honeypot.
- **$0 / user / month** — on-device inference and storage make the
  marginal cost of an additional user effectively zero.
- **Post-quantum secure** — hybrid X25519 + ML-KEM-768 encryption and
  ML-DSA-65 signatures protect data against "harvest-now, decrypt-later"
  attacks.
- **Multilingual** — extraction works across 22 languages out of the
  box, with per-sentence language detection.
- **Works offline** — the full pipeline (ingest → extract → remember →
  synthesize) runs with no network connection.

## Who is this for?

- **Product teams** building AI-powered apps — B2C chat, B2B knowledge
  tools, agent memory — who need user data to stay private without
  giving up retrieval quality.
- **Operators** deploying hybrid or enterprise knowledge infrastructure
  for SMEs and larger organizations who want connector-fed knowledge
  without running a heavyweight server tier.
- **Developers** embedding structured, decaying memory into AI agents
  and host apps on iOS, Android, or desktop (Electron).

## Quick Start

Pick the path that matches your role:

- **[For developers](docs/getting-started/for-developers.md)** — build
  from source, run the on-device demo, and embed the substrate in an app.
- **[For operators](docs/getting-started/for-operators.md)** — deploy
  the hybrid/enterprise server surface, configure it, and monitor it.
- **[For product teams](docs/getting-started/for-product-teams.md)** —
  understand what Knowledge enables and which integration pattern fits.

The fastest taste of the system (no server required):

```bash
git clone https://github.com/kennguy3n/knowledge.git
cd knowledge
cargo run -p demo --release
```

This drives a synthetic multi-scope dataset through every substrate API
and writes a reconciled report to `results/demo_results.md`. See the
**[Quick Start guide](docs/QUICKSTART.md)** for the full walkthrough
across all three deployment modes.

## Deployment Modes

| Mode | Use case | Infrastructure |
|---|---|---|
| **On-device** | B2C apps where each user's data stays on their own device (e.g. a private chat app). | None — the substrate runs entirely on iOS/Android/desktop. |
| **Hybrid** | SMEs connecting SaaS tools (Notion, Slack, Drive) with on-device synthesis. | A lightweight Go gateway + Rust substrate for connector sync; synthesis stays on-device or in a TEE. |
| **Enterprise** | Multi-tenant B2B knowledge platforms with central connectors, permissions, and audit. | Gateway + substrate + Postgres, with SCIM provisioning, Zanzibar permissions, and per-tenant keys. |

See **[deployment scenarios](docs/product/deployment-scenarios.md)** for
a decision tree, and the **[cost model](docs/operator/cost-model.md)**
for the per-user economics of each mode.

## Performance

Measured on reference hardware (AMD EPYC 7763, 8 vCPU, 31 GiB). See
**[benchmarks](docs/technical/benchmarks.md)** for the full suite and
methodology.

| Metric | Result |
|---|---|
| Ingest throughput (100K messages) | **~1,043 msgs/sec** |
| FTS phrase query (100K rows, 50 scopes) | p50 **13.56 ms** |
| Hybrid retrieval (10K rows) | **9.70 ms** |
| Decay sweep (100K objects) | **5.26 ms** (~19M rows/sec) |
| AEAD encrypt 64 KB | 80.4 µs (778 MiB/s) |
| Hybrid KEM encap (X25519 + ML-KEM-768) | 159.9 µs |
| ML-DSA-65 sign / verify | 320 µs / 77 µs |
| Storage per message (at 500K) | **612 bytes** |
| Connector sync (10K docs) | **~6,750 docs/sec** |

## Documentation

Documentation is organized by audience:

- **[Getting started](docs/getting-started/)** — role-based onboarding
  for developers, operators, and product teams.
- **[Technical](docs/technical/)** — architecture, design, crypto,
  sync, inference routing, connectors, permissions, API reference,
  benchmarks, and platform tuning.
- **[Operator](docs/operator/)** — deployment, configuration,
  monitoring, scaling, backup/recovery, troubleshooting, cost, and
  compliance.
- **[Product](docs/product/)** — use cases, deployment scenarios,
  comparisons, roadmap, and FAQ.
- **[Guides](docs/guides/)** — step-by-step integration and tutorials.
- **[Security](docs/security/)** — threat model, key management,
  supply chain, Electron hardening, and dependency policy.

## Community & Support

- **Contributing** — see [CONTRIBUTING.md](CONTRIBUTING.md) for build
  instructions, the contribution workflow, and the DCO sign-off
  requirement.
- **Code of Conduct** — see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- **Discussions** — open a GitHub Discussion for questions and ideas.
- **Issues** — use the bug report and feature request templates.
- **Security** — report vulnerabilities privately per
  [SECURITY.md](SECURITY.md) (contact **ken@uney.com**); please do not
  open public issues for security reports.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in
this project, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions.
