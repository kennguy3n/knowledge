# sync_engine

CRDT-based delta sync of synthesis objects.

## Purpose

Implements add-wins set CRDT with an append-only op log for
multi-replica sync of synthesis objects. Replicas exchange op logs
out-of-band; merge produces a deterministic state regardless of
arrival order.

## Public API summary

| Type / Function | Description |
|---|---|
| `SyncEngine<T>` | High-level engine wrapping `AddWinsSet` + `OpLog`. |
| `AddWinsSet<T>` | Add-wins set CRDT. |
| `OpLog` / `SyncOp` | Append-only operation log. |
| `merge_logs` | Deterministic log merge. |
| `PersistentSyncEngine` | SQLCipher-backed persistence. |
| `DeltaEnvelope` | Wire-format delta serialisation. |

## Usage example

```rust
use sync_engine::SyncEngine;

let mut engine = SyncEngine::<String>::new("replica-1");
engine.add("synthesis-obj-1".to_string());
let delta = engine.delta_since(0);
```

## Links

- [docs/DESIGN.md](../../docs/DESIGN.md) §3.2 — Delta sync.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
