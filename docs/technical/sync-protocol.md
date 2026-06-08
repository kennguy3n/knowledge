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

Replicas exchange their op logs out-of-band (over whatever transport
the host app already has — iCloud, a relay, a LAN link). `merge_logs` /
`OpLog::merge` produce a **deterministic merged state regardless of
arrival order**, which is the defining property of a CRDT: no central
coordinator, no locking, eventual convergence.

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

## Further reading

- [design.md](design.md) — substrate planes and where sync fits.
- [architecture.md](architecture.md) — data flow.
- [crypto-spec.md](crypto-spec.md) — at-rest encryption of synced data.
