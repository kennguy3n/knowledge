# napi (napi_addon)

N-API addon for macOS / Windows Electron desktop integration.

## Purpose

Mirrors the iOS / Android UniFFI surface (see the sibling `ffi`
crate) but speaks JSON-over-N-API for Electron / Node hosts. The
`#[napi]` proc-macros produce a `.node` cdylib loaded via
`require('./*.node')`.

## Building

```bash
cd crates/napi
npm install
npx napi build --platform --release
```

## Public API (JS-facing)

The `bindings` module exposes `camelCase` JS names mirroring the
`ffi` crate's `snake_case` Rust API:

- `init(configJson)` — bootstrap (N-API only).
- `openStore(path, masterKeyHex)` / `closeStore(handle)`.
- `ingestMessage(handle, json)` / `query(handle, json)`.
- `triggerSynthesis(handle, scopeId)`.
- `healthCheck(handle?)` / `coreVersion()`.
- `encrypt(…)` / `decrypt(…)` / `generateKeypair()`.
- And all connector, memory, sync-scheduler, webhook, and synthesis
  entry points.

## Feature flags

| Feature | Description |
|---|---|
| `tracing-subscriber` | Forwards to `ffi/tracing-subscriber`. |

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §3 — Platform integration plane.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
- [ffi](../ffi/) — Sibling UniFFI surface for mobile.
