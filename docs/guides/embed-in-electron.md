# Embed Knowledge in Electron (Node / N-API)

This guide walks through embedding the Knowledge substrate in an
Electron or Node.js desktop app. Desktop hosts consume the
`crates/napi` crate, which produces a native `.node` addon exposing the
same logical contract as the iOS/Android FFI surface.

## Prerequisites

- Rust **1.88+** — `napi-rs 3.x` requires `rustc >= 1.88` (higher than
  the workspace MSRV).
- A C toolchain for the bundled SQLCipher + OpenSSL sources.
- Node.js and npm.

## 1. Add the workspace

For a monorepo, add Knowledge as a submodule:

```bash
git submodule add https://github.com/kennguy3n/knowledge.git deps/knowledge
```

## 2. Build the N-API addon

```bash
cd crates/napi
npm install
npx napi build --platform --release
```

The resulting `.node` file is loaded by Node via
`require('./<platform>.node')`. For distribution, build the full
platform matrix (linux x64, macOS arm64/x64, windows x64); CI does this
on every push to `main`.

## 3. Use it from JavaScript

```js
const knowledge = require('./knowledge.node');

// Initialize
knowledge.init(JSON.stringify({ dataDir: './data' }));
const handle = knowledge.openStore('/path/to/store.db', masterKeyHex);

// Ingest
knowledge.ingestMessage(handle, JSON.stringify({
  scope: 'channel-1',
  sender: 'Alice',
  body: 'We decided to use Rust for the rewrite.',
}));

// Query
const results = knowledge.query(handle, JSON.stringify({
  text: 'Rust rewrite',
  limit: 10,
}));

// Back up the live store without closing it. `destPath` must not
// already exist; write to a fresh temp path, then atomically rename
// it into place. The copy keeps the same master key (a backup, not a
// rekey), so it re-opens with `openStore(destPath, masterKeyHex)`.
knowledge.snapshotStoreTo(handle, '/path/to/store.db.bak.tmp');

// Cleanup (zeroizes the master key)
knowledge.closeStore(handle);
```

For hardware-backed key storage, use `openStoreWithResolver` with a
`setKeyStorageResolver`-registered resolver instead of passing the hex
directly — see [key management](../security/key-management.md).

## 4. Feature flags

The N-API addon forwards the relevant `ffi` features:

- **`http-client`** (via `ffi`) — enables inference, connectors, and
  server synthesis. Without it, network-dependent subsystems return
  `FfiError::Unavailable`.
- **`tracing-subscriber`** — installs a `tracing` subscriber forwarded
  into `ffi`.

## 5. Harden the Electron host

Embedding native code in Electron has a specific threat model. Before
shipping, work through the
[Electron hardening checklist](../security/electron-hardening.md):
`BrowserWindow` settings, CSP, IPC allowlist, preload isolation, and
the main-process posture.

## Further reading

- [Electron hardening](../security/electron-hardening.md)
- [Platform tuning](../technical/platforms.md)
- [Architecture](../technical/architecture.md)
