# permission_service

Zanzibar-style permission service for the Knowledge substrate.

## Purpose

Every access decision in the substrate is a reachability query over
a graph of relation tuples. Supports namespace inheritance
(Owner => Admin => Editor => Member => Viewer) and userset rewrites.

## Public API summary

| Type / Function | Description |
|---|---|
| `check_permission` | Main reachability check. |
| `TupleStore` | In-memory `HashSet`-backed relation store. |
| `PersistentTupleStore` | SQLCipher-backed persistent store. |
| `RelationTuple` | `(object, relation, subject)` triple. |
| `NamespaceConfig` / `NamespaceRegistry` | Inheritance configuration. |
| `ObjectRef` / `SubjectRef` / `Relation` | Typed references. |

## Usage example

```rust
use permission_service::{TupleStore, RelationTuple, check_permission};

let mut store = TupleStore::new();
store.insert(RelationTuple::new(obj, Relation::Owner, subject));
assert!(check_permission(&store, &namespace, obj, Relation::Viewer, subject));
```

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §6 — Permission model.
- [docs/technical/design.md](../../docs/technical/design.md) §7.1 — Permission model.
- [docs/getting-started/for-developers.md](../../docs/getting-started/for-developers.md) — Consumer integration guide.
