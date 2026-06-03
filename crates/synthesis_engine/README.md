# synthesis_engine

Server-side synthesis engine for the Knowledge substrate.

## Purpose

The Rust side of the server-side synthesis service. Consumes
channel-level outputs and produces domain / tenant summary synthesis
objects via managed AI endpoints, with optional TEE attestation.

## Public API summary

| Type / Function | Description |
|---|---|
| `SynthesisEngine` (trait) | Synthesizer interface (`synthesize_domain`, `synthesize_tenant`). |
| `ManagedEndpointSynthesizer` | Deterministic test scaffold (stub module). |
| `HttpManagedEndpointSynthesizer` | Production synthesizer via managed HTTP endpoint. |
| `TeeWorker` | TEE-attested wrapper delegating to the production synthesizer. |
| `BlockingHttpClientAdapter` | Reqwest-backed HTTP (feature-gated: `http-client`). |

## Feature flags

| Feature | Description |
|---|---|
| `http-client` | Reqwest-backed HTTP adapter for managed AI endpoints. |
| `nitro-tee` | AWS Nitro Enclave attestation runtime. |
| `test-support` | Enables `MockTeeRuntime`. |

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §2.1 — Synthesis engine.
- [synthesis_pipeline](../synthesis_pipeline/) — Window management and publication.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
