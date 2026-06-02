# synthesis_pipeline

Channel / domain / tenant synthesis windows and encrypted
synthesis-object publication for the Knowledge substrate.

## Purpose

Manages synthesis windows (per-scope time ranges the synthesizer
aggregates over), synthesis objects (typed payloads with provenance),
GBNF schema types for SLM output, elected-device synthesis protocol,
and encrypted publish/consume paths.

## Public API summary

| Type / Function | Description |
|---|---|
| `SynthesisObject` / `SynthesisObjectType` | Typed synthesis payloads. |
| `SynthesisPipeline` (trait) | Synthesizer interface. |
| `SynthesizerElection` / `SynthesizerRole` | Elected-device protocol. |
| `HierarchyEnforcedWindowManager` | Scope-hierarchy window management. |
| `DomainSynthesisInput` / `TenantSynthesisInput` | Typed hierarchy inputs. |
| `publish_synthesis_object` / `consume_synthesis_object` | AEAD publish/consume. |

## Feature flags

| Feature | Description |
|---|---|
| `http-client` | Forwards to `inference_router/http-client`. |
| `test-support` | Enables `NoOpSynthesizer`. |

## Links

- [ARCHITECTURE.md](../../ARCHITECTURE.md) §2.1 — Synthesis pipeline.
- [docs/DESIGN.md](../../docs/DESIGN.md) §6 — Synthesis hierarchy.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
