# agent_contract

Agent proposal-only write contract for the Knowledge substrate.

## Purpose

Per `docs/technical/design.md` §7.3, software agents (LLM-driven workflows,
integrations, AI employees) **never** write canonical memory directly.
Instead they speak to the substrate through a proposal-only API.
Promotion to canonical requires explicit human action or a matching
tenant auto-promotion policy.

## Public API summary

| Type / Function | Description |
|---|---|
| `AgentProposal` | Envelope carrying one of four proposal kinds. |
| `AgentIdentity` | Identity of the proposing agent. |
| `ObservationProposal` | Propose an observation claim. |
| `ConceptProposal` | Propose a concept node. |
| `RelationProposal` | Propose a typed relation edge. |
| `SummaryProposal` | Propose a summary. |
| `validate_proposal` | Schema validation for proposals. |
| `ProposalStore` | In-memory lifecycle store for proposals. |
| `AutoPromotionPolicy` | Policy governing automatic promotion. |
| `CanonicalArtifact` | Output of successful promotion. |

## Usage example

```rust
use agent_contract::{AgentProposal, AgentIdentity, ObservationProposal, validate_proposal};

let identity = AgentIdentity::new("my-agent", "1.0.0");
let proposal = AgentProposal::observation(
    identity,
    scope_id,
    ObservationProposal { claim: "Rust is used for the rewrite".into(), /* … */ },
);
validate_proposal(&proposal)?;
```

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §6 — Permission model (`proposer` relation).
- [docs/technical/design.md](../../docs/technical/design.md) §7.3 — Agent write contract.
- [docs/getting-started/for-developers.md](../../docs/getting-started/for-developers.md) — Consumer integration guide.
