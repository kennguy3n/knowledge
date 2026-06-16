# Sync & Multi-Device

> **TL;DR:** A user has a phone, a personal laptop, and a work laptop,
> and expects memory to be consistent across all of them. The obvious
> answer — a central sync server — reintroduces the exact data-in-the-
> cloud problem the whole substrate avoids. This post builds sync as a
> **CRDT** problem instead: an add-wins set over an append-only op log,
> synced through an **untrusted relay that only ever holds ciphertext**.
> No trusted server to pay for or breach.

## What you are building

- **`sync_engine`** — the add-wins CRDT, delta serialization/compaction,
  snapshot bootstrap, SQLCipher-persisted op log, and the
  `SyncTransport` trait + `SyncClient` push/pull API.
- **`sync_relay`** — an axum HTTP relay (bearer-token auth, per-tenant
  isolation) that store-and-forwards sealed delta blobs it cannot
  decrypt.

## Build it: convergence as a math property

The core idea: make convergence a property of the *merge function*, not
of a server's authority.

1. **Add-wins set over an op log.** Each device records operations
   locally; devices exchange op logs out-of-band; **merging logs produces
   a deterministic state regardless of arrival order.** There is no "the
   server's version wins" — `merge_logs` is a deterministic, idempotent
   function every replica computes to the same result. Conflicts resolve
   by the add-wins rule plus supersession.
2. **Deltas, not the whole log.** Sending the full op log forever is
   wasteful, so the engine serializes `DeltaEnvelope`s (what changed
   since a known point) and **compacts** with epochs + snapshot
   checkpointing, so a device that has synced for years doesn't carry an
   unbounded log.
3. **Seal before the transport sees it.** A `SyncClient` is bound to a
   scope; it derives an opaque routing topic and a per-scope key from the
   master key, then **XChaCha20-Poly1305-seals every delta** before it
   touches the wire:

   ```text
   let mut engine = SyncEngine::<ObjId>::new();
   let mut client = SyncClient::new(&master_key, scope)?;
   engine.add(obj);                       // local op, logged
   client.sync(&mut engine, &transport)?; // push sealed deltas, pull + merge peers'
   ```

4. **An untrusted relay.** Because convergence is in the merge, the
   transport need not be trusted. `sync_relay` is a *dumb buffer* of
   opaque ciphertext blobs keyed by topic — swap it for a shared folder
   or peer connection by implementing `SyncTransport`. A ≥3-replica
   integration test exchanges deltas through a real relay across
   offline/partition scenarios and asserts both deterministic convergence
   **and** that the relay only ever holds ciphertext.

## What ships, and what doesn't (build it honestly)

Carry the same candor as [post 11 of the main series](../11-sync-without-servers.md):

- **Shipped + tested:** the CRDT merge, delta serialization/compaction/
  snapshot, SQLCipher persistence, the `SyncTransport`/`SyncClient` API
  with per-scope AEAD, and the `sync_relay` untrusted relay.
- **Reference-grade:** the relay's `BlobStore` is in-memory behind a
  trait — production implements it over durable/replicated storage; TLS
  terminates at ingress.
- **Not yet wired:** plumbing `SyncClient` into the host-app lifecycle
  (background scheduling, retry/backoff) and post-quantum key
  establishment for cross-device key transport. Naming this gap is part
  of the build.

## The business decision: who do you trust with the merge?

**Scenario.** Multi-device consistency for millions of privacy-sensitive
users. Where does the merge happen?

- **Cloud-native sync (every comparison-table competitor).** A central
  server is the source of truth — it sees the data (or at least the
  metadata), it's a per-user cost, and it's the single richest breach
  target you own. "We keep your data on your device, except for the sync
  server that has all of it" is not a position you want to defend.
- **CRDT + untrusted relay (this).** The merge is local computation; the
  relay is a ciphertext buffer that can't resolve or read anything.
  Eliminating the trusted sync tier removes both a major recurring cost
  and a major breach surface at once.

## How a competitor would build this

Cloud sync is genuinely simpler: a server with last-write-wins (or OT)
and a database is well-trodden, and the server-as-authority model makes
features like server-side search trivial. The price is the trust and
cost of that central tier. The CRDT-over-untrusted-relay model is more
math up front, but it's the only way to keep "your data stays yours"
true across devices.

## What's next

The device side is complete. For B2B you also need a server: a gateway,
multi-tenancy, permissions, and an audit trail — without becoming the
centralized cloud you set out to avoid. Next.

---
*Part 9 of "How to Build Knowledge." [Previous: 140 Connectors, Honestly](08-connectors.md) | [Next: The Server & Multi-Tenancy](10-server-and-multitenancy.md) | [Series index](README.md)*
