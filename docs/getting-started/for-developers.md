# Getting Started for Developers

You want to embed structured, private memory into an app or AI agent.
This page takes you from a fresh clone to an embedded substrate.

## 1. Prerequisites

- **Rust 1.85+** (`rustup install stable && rustup component add clippy rustfmt`)
- A **C toolchain** for the bundled SQLCipher + OpenSSL
  (`sudo apt install build-essential` on Debian/Ubuntu)
- **Go 1.23+** only if you'll run the server gateway

## 2. Build and run the demo

```bash
git clone https://github.com/kennguy3n/knowledge.git
cd knowledge
cargo run -p demo --release
```

The demo opens an encrypted store, ingests evidence, extracts
observations, builds a concept graph, synthesizes a summary (using the
deterministic fallback — no model required), and demonstrates
cryptographic forgetting. Output lands in `results/demo_results.md`.

**Expected outcome:** the run completes without errors and the report
shows each stage populated and a forgotten scope whose bytes are
unrecoverable.

## 3. Understand the shape

Before embedding, read these in order:

1. [technical/architecture.md](../technical/architecture.md) — the
   component map and data flow.
2. [technical/design.md](../technical/design.md) — the memory model
   (evidence → observation → concept → memory planes).
3. [technical/api-reference.md](../technical/api-reference.md) — the
   surface you'll call.

## 4. Choose your integration surface

Depend on the smallest surface that fits — for native apps, prefer the
stable `ffi` / `napi` surfaces over the internal crates:

| Use case | Surface |
|---|---|
| iOS app | `ffi` via UniFFI — [embed-in-ios.md](../guides/embed-in-ios.md) |
| Android app | `ffi` via JNI — [embed-in-android.md](../guides/embed-in-android.md) |
| Electron/desktop | `napi` — [embed-in-electron.md](../guides/embed-in-electron.md) |
| Web backend / multi-tenant | Go gateway REST — [Quickstart Mode 2/3](../QUICKSTART.md) |

## 5. Build something

- [build-a-chat-app.md](../guides/build-a-chat-app.md) — an end-to-end
  KChat-style B2C app.
- [build-b2b-knowledge.md](../guides/build-b2b-knowledge.md) — a
  multi-tenant B2B knowledge tool.
- [api-cookbook.md](../guides/api-cookbook.md) — common API patterns.

## 6. Contribute

Found a bug or want to add a connector? See
[CONTRIBUTING.md](../../CONTRIBUTING.md) and
[add-a-connector.md](../guides/add-a-connector.md).

## What's next

Operators taking this to production should continue with
[for-operators.md](for-operators.md).
