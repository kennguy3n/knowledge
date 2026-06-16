# Sync Without Servers

> **TL;DR:** Multi-device sync usually means a central server that holds
> everyone's data. Knowledge uses an add-wins CRDT over an append-only
> op log, so devices converge to the same state regardless of message
> order — with no central authority to trust, pay for, or breach.

## The Business Problem

A user has an iPhone, a personal MacBook, and a work Windows laptop.
They expect the assistant's memory to be consistent everywhere: a
conversation captured on the phone should inform a query on the laptop.
The obvious way to deliver that is a sync server — a central service
that every device pushes to and pulls from.

But a central sync server reintroduces the exact problem
[on-device memory](01-why-on-device-memory.md) was built to avoid: a
single place that holds the plaintext (or at least the metadata) of
every user's knowledge, with all the privacy, cost, and breach-surface
implications. For a privacy-first product, "we keep your data on your
device — except for the sync server that has all of it" is not a
position you want to defend. And devices are not always online; sync
has to survive a laptop that was offline for a week and a phone in
airplane mode.

## The Technical Approach

The [`sync_engine` crate](../crates/sync_engine/) implements sync as a
**CRDT** (Conflict-free Replicated Data Type) problem rather than a
client-server one. See the [sync protocol spec](../docs/technical/sync-protocol.md)
and the [design document](../docs/technical/design.md) §3.2.

**Add-wins set over an op log.** Synthesis objects are synced as an
add-wins set backed by an append-only operation log. Each device
records operations locally; devices exchange op logs out-of-band; and
**merging logs produces a deterministic state regardless of arrival
order**. There is no "the server's version wins" — there is a
mathematically-defined merge that every device computes to the same
result. Conflicts resolve by the add-wins rule plus supersession, so
two devices that edited while disconnected converge cleanly when they
reconnect.

**No trusted server required.** Because convergence is a property of
the merge function, devices can sync through *any* transport — a relay,
a shared folder, a peer connection — without that transport being a
trusted authority. The substrate ships one such transport: the
`sync_relay` crate, an authenticated relay that store-and-forwards
encrypted delta blobs it cannot decrypt or resolve. It is a dumb buffer,
not a coordinator, and a deployment can swap it for any other channel by
implementing the `SyncTransport` trait. The
[sync protocol spec](../docs/technical/sync-protocol.md) explains the
"why no server" reasoning in full.

**Delta serialization and compaction.** Sending the entire op log
forever would be wasteful, so the engine serializes **deltas** (just
what changed since a known point) via a `DeltaEnvelope`, and
**compacts** the log using epochs and snapshot checkpointing so storage
and transfer stay bounded as history grows. State persists to SQLCipher
between runs, encrypted like the rest of the substrate.

## Implementation Walk-through

Each device runs an engine wrapped in a `SyncClient` that is bound to a
scope. The client derives an opaque routing topic and a per-scope
encryption key from the user's master key, then seals every delta
before it touches the transport:

```text
let mut engine = SyncEngine::<ObjId>::new();
let mut client = SyncClient::new(&master_key, scope)?; // derives topic + seal key

engine.add(obj);                       // local op, logged
client.sync(&mut engine, &transport)?; // push our sealed deltas, pull + merge peers'
```

`client.sync` does two things over the `SyncTransport`: it **push**es
this device's own new ops (encoded as a `DeltaEnvelope`, then
XChaCha20-Poly1305-sealed) and it **pull**s every sealed blob the relay
has accumulated, opening each and folding it into the local engine
through the same deterministic, idempotent merge. The transport is just
a buffer of opaque ciphertext blobs keyed by topic; the in-tree
`sync_relay` crate is one such transport — an authenticated HTTP relay
that store-and-forwards `SealedDelta`s it cannot read.

The `PersistentSyncEngine` variant persists the op log to SQLCipher so
state survives restarts; `merge_logs` is the deterministic merge that
guarantees every replica reaches the same state. Because the merge is
order-independent, the week-offline laptop and the airplane-mode phone
both reconcile correctly once they reach the relay — no central
coordinator, no "last write wins" data loss.

## Performance & Cost Implications

CRDT sync moves deltas, not whole datasets, and the merge is local
computation. Compaction keeps the op log from growing unbounded, so a
device that has synced for years does not carry a forever-growing log.

The cost implication is the recurring theme of Series 2: **no sync
server means no sync-server bill** and no central store to secure. The
transport can be as cheap as a relay that never sees plaintext, because
the relay is not trusted to resolve anything — it just shuttles
encrypted deltas. For a product serving millions of multi-device users,
eliminating the central sync tier removes both a major cost and a major
breach surface.

## What ships today (and what doesn't)

To be precise about the state of the code rather than the vision:

- **Shipped and tested:** the add-wins CRDT merge math; delta
  serialization, compaction, and snapshot bootstrap; SQLCipher
  persistence; **and now the transport layer** — the `SyncTransport`
  trait, the `SyncClient` push/pull API with per-scope AEAD sealing,
  and the `sync_relay` untrusted HTTP relay with bearer-token auth and
  per-tenant isolation. A ≥3-replica integration test exchanges deltas
  through a real relay across offline/partition scenarios and asserts
  both deterministic convergence and that the relay only ever holds
  opaque ciphertext.
- **Reference-grade, not yet production-hardened:** the relay's blob
  store is an in-memory reference implementation behind a `BlobStore`
  trait — a production deployment implements that trait over durable,
  replicated storage. TLS is terminated at the ingress, not by the
  relay process.
- **Not yet wired:** plumbing `SyncClient` into the host app's
  lifecycle and FFI surface (when a device syncs, retry/backoff policy,
  background scheduling) is the integration step that turns this
  library-level capability into an end-user feature.

This section exists so the post tracks the code exactly: the merge
math, transport, and untrusted relay are real and tested; the host-app
lifecycle wiring is the honest remaining gap.

## What's Next

Sync, like the rest of the substrate, runs out on user devices where
you can't SSH in to debug. The next post closes Series 2 with
observability: how to monitor a system that runs on hardware you don't
control, without collecting the PII that would defeat the privacy
model.

---
*This is part 11 of the "Building Knowledge" series. [Previous: Cost Engineering: Zero Marginal](10-cost-engineering-zero-marginal.md) | [Next: Observability Without Ops](12-observability-without-ops.md)*
