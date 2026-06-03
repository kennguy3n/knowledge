# substrate_server

Internal HTTP loopback server exposing the Knowledge substrate's FFI surface to the Go server tier.

## Purpose

Rust stays the library; Go is the server. This binary crate is the thin
bridge between them: it boots an `axum` service on `127.0.0.1:9090`
(loopback only — never exposed publicly) that wraps each relevant
`ffi::*` function behind a REST endpoint, plus a handful of endpoints
that call `permission_service`, `export_plane`, and `crypto` directly
where no FFI function exists. Synchronous FFI calls are dispatched on
the blocking thread pool via `spawn_blocking` so the async runtime is
never stalled by a SQLCipher round-trip.

## Key types

- `config` — loopback bind address and runtime configuration.
- `state` — shared `RuntimeHandle` and server state.
- `dto` — request/response wire types mirroring the FFI surface.
- `metrics` — Prometheus counters for the gateway-facing endpoints.

## Usage

Launched by the Go gateway as a child process; not invoked directly.
See the gateway topology in the deployment guide.

## Links

- [docs/technical/architecture.md](../../docs/technical/architecture.md) §3 — Platform integration plane.
- [docs/operator/deployment-guide.md](../../docs/operator/deployment-guide.md) — Gateway + substrate topology.
