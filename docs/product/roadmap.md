# Roadmap

This is a public, directional roadmap — not a commitment or a dated
plan. Priorities shift with community input; see
[CONTRIBUTING.md](../../CONTRIBUTING.md) for how to influence them.

## Shipped in 1.0

- 24-crate Rust workspace: full on-device knowledge substrate.
- Post-quantum cryptography (ML-KEM-768, ML-DSA-65).
- 22-language multilingual extraction.
- 10 production connectors (including GitHub).
- Go API gateway with the full REST surface.
- Three deployment modes: on-device, hybrid, enterprise.
- Criterion.rs benchmark suite with documented results.

## Connector maturity

| Connector | Status |
|---|---|
| Google Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot, Slack, Email, GitHub | Production |

See [connector-protocol.md](../technical/connector-protocol.md).

## Areas we're exploring

These are directions, not promises:

- Broadening the on-device inference adapter set beyond MLX / llama.cpp.
- Additional first-party connectors.
- Richer sync transports on top of the CRDT
  [sync protocol](../technical/sync-protocol.md).
- Expanded language coverage in the extraction lexicon.

## Where to contribute

Good places to start:

- New connectors — [add-a-connector.md](../guides/add-a-connector.md).
- Inference adapters —
  [custom-synthesis.md](../guides/custom-synthesis.md).
- Docs, examples, and platform integration guides.
- Issues labelled **good first issue**.

## Further reading

- [CONTRIBUTING.md](../../CONTRIBUTING.md) — how to get involved.
- [faq.md](faq.md).
