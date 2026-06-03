# export_plane

Portable concept profiles, export policies, and policy simulator for
the Knowledge substrate.

## Purpose

Per `docs/technical/design.md` §3.5, the export plane provides a narrow,
policy-gated interface for moving curated knowledge out of the
substrate into external surfaces (LLM tools, downstream apps,
integration partners). Never re-emits raw evidence by default.

## Public API summary

| Type / Function | Description |
|---|---|
| `PortableConceptProfile` | Exported concept bundle. |
| `ExportPolicy` / `PolicyEngine` | Policy evaluation. |
| `ExportDecision` | Allow / deny decision with rationale. |
| `ExportControlRegistry` | Per-concept / summary controls. |
| `ConceptApprovalWorkflow` | Bridges concept graph to export plane. |
| `PolicySimulator` | Read-only policy simulator. |

## Usage example

```rust
use export_plane::{PolicyEngine, ExportPolicy, ExportViewRequest};

let engine = PolicyEngine::new(policy);
let decision = engine.evaluate(&request)?;
```

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §4.1 — Export service.
- [docs/technical/design.md](../../docs/technical/design.md) §3.5 — Export plane.
- [docs/getting-started/for-developers.md](../../docs/getting-started/for-developers.md) — Consumer integration guide.
