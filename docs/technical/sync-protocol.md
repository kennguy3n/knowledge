# Sync Protocol

This document specifies how Knowledge synchronizes synthesis objects
across a user's devices without a central authority. It is the
reference companion to [design.md §3.2](design.md) and is implemented
by the `sync_engine` crate (`crates/sync_engine`).

## Model: an add-wins set over an op log

Every replica holds, per scope:

- an **`AddWinsSet`** of synthesis-object ids (the materialized state),
  and
- an append-only **`OpLog`** of `SyncOp` entries (`Add` / `Remove` /
  `Supersede`).

Replicas exchange their op logs over a transport (the built-in
untrusted relay below, or any host-provided channel — iCloud, a LAN
link). `merge_logs` / `OpLog::merge` produce a **deterministic merged
state regardless of arrival order**, which is the defining property of
a CRDT: no central coordinator, no locking, eventual convergence.

The high-level `SyncEngine` keeps a **cached** in-memory `AddWinsSet`
updated incrementally on every mutation, so reading current state is
`O(live elements)` rather than `O(total ops in history)`. Mutating the
raw op log invalidates the cache, which is rebuilt lazily on the next
read.

## Conflict resolution

Conflicts are resolved by the add-wins policy plus supersession:

- Concurrent `Add` and `Remove` of the same id → **add wins**.
- `Supersede(old, new)` records that `new` replaces `old`, so a later
  object version dominates an earlier one deterministically.

Because element identity is whatever `Eq` + `Hash` derive on the
element type, the CRDT machinery is **byte-exact and culture-neutral** —
there is no locale-dependent comparison in the merge path.

## Delta serialization

Full op-log exchange is wasteful once replicas are mostly in sync. The
`delta` module encodes/decodes/applies only the ops after a peer's
`since_seq` as a wire-format byte blob, guarded by the
**compaction epoch** (see below) so a stale delta cannot be applied
against a compacted log.

## Network transport (implemented)

Delta serialization defines *what* travels on the wire; the
`transport` module (`crates/sync_engine/src/transport.rs`) and the
`sync_relay` crate (`crates/sync_relay`) define *how*, end to end:

- **`SyncTransport` trait** — the store-and-forward contract: `push`
  appends opaque sealed blobs to a topic and returns the topic's new
  high-water cursor; `pull` returns every blob past a cursor. The crate
  ships an in-process `InMemoryTransport`; `sync_relay` ships the HTTP
  one.
- **`SyncClient`** — wraps a `SyncEngine` for a scope. It derives an
  opaque routing **`TopicId`** and a per-scope AEAD seal key from the
  master key (HKDF), then on `push` encodes its *own* new ops
  (`encode_own_delta_since` — so each op reaches the relay exactly once,
  authored by its originating replica, never re-forwarded and amplified
  by peers) into a `DeltaEnvelope`, seals it with XChaCha20-Poly1305
  (binding the `TopicId` into the AAD), and uploads the resulting
  `SealedDelta`. On `pull` it opens each blob and folds it in through
  the existing idempotent `apply_delta` path.
- **Untrusted relay (`sync_relay`)** — an authenticated axum server
  that stores `SealedDelta`s (nonce + ciphertext) keyed by
  `(tenant, topic)`. It holds **no** master key, so it cannot decrypt
  blobs, cannot link a `TopicId` to a scope, and cannot resolve or
  reorder CRDT state. A bearer token authenticates each request and
  selects the tenant namespace, isolating tenants from one another on a
  shared relay. The only relay-visible metadata is the topic routing
  key and the arrival offset it assigns — no replica id, scope id, op
  count, or sequence range.

Authentication and confidentiality are orthogonal here: the bearer
token controls *access* to the store-and-forward buffer, while the
per-scope seal key (which the relay never holds) controls *who can
read* the contents. The exactly-once upload model assumes each replica
eventually connects to the relay directly; a replica that only ever
gossips peer-to-peer re-uploads its own ops when it next reaches the
relay.

Convergence over the relay is covered end to end by
`crates/sync_relay/tests/three_replica_relay.rs` (≥3 replicas over a
real HTTP relay, including offline/partition, add-wins, supersession,
order-independence, and a "relay only ever holds ciphertext"
assertion), with a fast in-process tier in
`crates/sync_engine/tests/transport_convergence.rs`.

## Compaction

`SyncEngine::compact` rewrites the local op log into a minimal
`Add`-only form (dropping tombstoned and superseded history) and bumps a
`compaction_epoch` counter. Peers exchange the epoch; a mismatch tells a
peer its incremental position is no longer valid and it must
**bootstrap from a snapshot** instead of replaying deltas.

## Snapshot checkpointing

`SyncEngine::snapshot` / `restore_snapshot` serialize the materialized
set directly, bypassing log replay, so a brand-new replica (or one
behind a compaction boundary) can bootstrap without the full op
history.

## Persistence

`PersistentSyncEngine` mirrors the op log to a SQLCipher database with
per-scope AEAD on the payload column, following the same pattern as the
`concept_graph` crate. Synced data is encrypted at rest with the same
key hierarchy as the rest of the substrate (see
[crypto-spec.md](crypto-spec.md)).

## Why no server

Server-mediated sync would mean plaintext (or server-decryptable) user
data transiting and resting on infrastructure the user does not
control, and it would create a cross-border data-transfer surface. A
CRDT over an encrypted op log keeps **all** user content on the user's
devices: the transport only ever carries ciphertext deltas, and no
server is in a position to read or to be compelled to produce content.
The shipped `sync_relay` is exactly such a transport — an
authenticated buffer that store-and-forwards opaque `SealedDelta`s and
holds no key capable of reading them. It is not the central authority a
traditional sync server is; it is a dumb, replaceable relay, and a
deployment that prefers iCloud/a shared folder/a LAN link can drop it
entirely by implementing `SyncTransport` over that channel instead.

## Further reading

- [design.md](design.md) — substrate planes and where sync fits.
- [architecture.md](architecture.md) — data flow.
- [crypto-spec.md](crypto-spec.md) — at-rest encryption of synced data.
