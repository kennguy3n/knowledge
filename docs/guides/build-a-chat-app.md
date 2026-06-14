# Build a Chat App (B2C, on-device)

An end-to-end walkthrough of building a KChat-style B2C chat app where
each user's memory lives encrypted on their own device. This is
[Mode 1: on-device](../product/deployment-scenarios.md).

## What you'll build

A chat client that:

- Stores every message as encrypted evidence on-device.
- Extracts observations and builds a concept graph as the user talks.
- Synthesizes a running memory the assistant can draw on.
- Lets the user forget a conversation irrecoverably.

No servers. $0 marginal cost per user. Works offline.

## 1. Embed the substrate

Pick your platform surface and follow its setup:

- iOS — [embed-in-ios.md](embed-in-ios.md)
- Android — [embed-in-android.md](embed-in-android.md)
- Electron/desktop — [embed-in-electron.md](embed-in-electron.md)

Each produces a stable FFI/N-API surface exposing the same logical
contract.

## 2. Provision a per-user master key

On first run, generate a 32-byte master key and store it in the
platform secure store (iOS Keychain / Android Keystore / OS keychain).
Open the store through the resolver path so the key is never a
long-lived plaintext string in your process — see
[key-management.md](../security/key-management.md).

## 3. Open a scope per conversation

Model each conversation (or channel) as a **scope**. Opening a store
gives you a handle; ingest messages into the conversation's scope as
they arrive:

```text
openStore(path, key) -> handle
ingestMessage(handle, { scope, sender, body })
```

(See [api-cookbook.md](api-cookbook.md) for the REST equivalents if you
front the substrate with the gateway.)

## 4. Query for retrieval

When the assistant needs context, query the scope for the most relevant
prior evidence and feed it into your prompt:

```text
query(handle, { text: userTurn, limit: 10 }) -> results[]
```

## 5. Synthesize memory

Trigger synthesis to roll observations into a durable summary. With no
model wired, the deterministic fallback runs (useful in dev); for real
summaries, wire an inference backend — see
[inference-routing.md](../technical/inference-routing.md) and
[custom-synthesis.md](custom-synthesis.md).

## 6. Forget on request

When the user deletes a conversation, forget the scope. This destroys
the scope's encryption key — the messages become unrecoverable, which
is how you honor a real "delete my data" request:

```text
forget(scopeId)
```

## 7. Verify

- Ingest, query, and confirm relevant results come back.
- Trigger synthesis and read the summary.
- Forget a scope and confirm its evidence no longer decrypts.

## Performance & cost

Retrieval stays in the low-millisecond range even on large histories
(see [../technical/benchmarks.md](../technical/benchmarks.md)), and
because inference is on-device your marginal cost per user is ~$0 (see
[../operator/cost-model.md](../operator/cost-model.md)).

## What's next

- Add multi-device sync — the add-wins CRDT merge math, delta transport,
  per-scope XChaCha20-Poly1305 sealing, and an untrusted relay (which only
  ever holds ciphertext) ship as a library-level capability with a
  ≥3-replica convergence test. Wiring it into your app's background
  lifecycle (scheduling, retry/backoff) is integration work you own — see
  [sync-protocol.md](../technical/sync-protocol.md).
- Building B2B instead? See
  [build-b2b-knowledge.md](build-b2b-knowledge.md).
