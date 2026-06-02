# ffi

UniFFI surface for iOS / Android platform bindings.

## Purpose

The primary consumer API for mobile hosts. Exposes the Knowledge
substrate's lifecycle, evidence store, memory manager, synthesis
pipeline, crypto, connectors, and observability through UniFFI-
generated Swift (iOS) and Kotlin (Android) bindings.

## Public API summary (entry points)

- **Lifecycle:** `open_store`, `close_store`, `open_store_with_resolver`.
- **Evidence:** `ingest_message`, `query`, `get_evidence`.
- **Memory:** `get_user_memory`, `pin`, `unpin`, `forget`, `list_memories`, `run_decay_sweep`.
- **Synthesis:** `trigger_synthesis`, `get_channel_memory`, `trigger_server_synthesis`.
- **Crypto:** `generate_keypair`, `encrypt`, `decrypt`.
- **Connectors:** `create_connector`, `authenticate_connector`, `sync_connector`, etc.
- **Observability:** `health_check`, `metrics_snapshot`, `try_init_tracing`.

## Feature flags

| Feature | Description |
|---|---|
| `http-client` | Enables real reqwest-backed HTTP for inference, connectors, and server synthesis. |
| `tracing-subscriber` | Installs a `tracing` subscriber via `try_init_tracing`. |

## Links

- [ARCHITECTURE.md](../../ARCHITECTURE.md) §3 — Platform integration plane.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
- [napi](../napi/) — Sibling N-API surface for desktop (Electron).
