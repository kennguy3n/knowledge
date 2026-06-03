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

**No server required.** Because convergence is a property of the merge
function, devices can sync through *any* transport — a relay, a shared
folder, a peer connection — without that transport being a trusted
authority. The substrate does not depend on a central server to decide
truth. The [sync protocol spec](../docs/technical/sync-protocol.md)
explains the "why no server" reasoning in full.

**Delta serialization and compaction.** Sending the entire op log
forever would be wasteful, so the engine serializes **deltas** (just
what changed since a known point) via a `DeltaEnvelope`, and
**compacts** the log using epochs and snapshot checkpointing so storage
and transfer stay bounded as history grows. State persists to SQLCipher
between runs, encrypted like the rest of the substrate.

## Implementation Walk-through

Conceptually, each device runs an engine, records local changes, and
exchanges deltas:

```text
let mut engine = SyncEngine::<T>::new("replica-iphone");
engine.add(obj);                       // local op, logged
let delta = engine.delta_since(cursor); // serialize what's new
// transport delta to other devices (any channel)
engine.merge(remote_delta);            // deterministic convergence
```

The `PersistentSyncEngine` variant persists the op log to SQLCipher so
state survives restarts; `merge_logs` is the deterministic merge that
guarantees every replica reaches the same state. Because the merge is
order-independent, the week-offline laptop and the airplane-mode phone
both reconcile correctly once they exchange logs — no central
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

## What's Next

Sync, like the rest of the substrate, runs out on user devices where
you can't SSH in to debug. The next post closes Series 2 with
observability: how to monitor a system that runs on hardware you don't
control, without collecting the PII that would defeat the privacy
model.

---
*This is part 11 of the "Building Knowledge" series. [Previous: Cost Engineering: Zero Marginal](10-cost-engineering-zero-marginal.md) | [Next: Observability Without Ops](12-observability-without-ops.md)*
