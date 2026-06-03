# Permission Model

This document specifies the authorization model: a Zanzibar-style
relation graph. It is the reference companion to
[architecture.md §6](architecture.md) and [design.md §7.1](design.md)
and is implemented by the `permission_service` crate
(`crates/permission_service`).

## Every access decision is a reachability query

Authorization in Knowledge is not a per-row ACL column — it is a
**reachability query over a graph of relation tuples**:

```text
(object_type, object_id) # relation @ (subject_type, subject_id)
```

For example:

- `(Tenant, t-1) # owner @ (User, u-42)`
- `(Domain, d-9) # editor @ (Tenant, t-1) # admin`
- `(Channel, c-3) # viewer @ (User, u-7)`

`check(object, relation, subject)` walks the relation graph to decide
whether the subject can exercise the relation on the object.

## Namespace inheritance

The walk folds in namespace inheritance so higher relations imply lower
ones:

```
owner ⇒ admin ⇒ editor ⇒ member ⇒ viewer
```

So an `owner` automatically satisfies a `viewer` check without an
explicit tuple.

## Userset rewrites

The walk follows `RelationTuple::subject_relation` pointers — the
*userset rewrite* leg of the Zanzibar model. A tuple of the form:

```text
(Domain, d-9) # editor @ (Tenant, t-1) # admin
```

resolves by recursing into `(Tenant, t-1) # admin @ ?`, i.e. "anyone
who is admin of tenant t-1 is editor of domain d-9". This is what lets
permissions compose across tenants, domains, and channels without
materializing every transitive grant.

## Layered stores

The crate exposes layered stores so the relation graph can be held
in-memory for tests and persisted (SQLCipher) for production, with the
same query semantics over both.

## How connectors feed permissions

Source-system ACLs are projected into this relation graph by the
connector framework's `AclSyncEngine` (see
[connector-protocol.md](connector-protocol.md)), so a document's
reachability in Knowledge mirrors its access control in the source
system. Multi-tenant isolation, SCIM-provisioned users, and per-tenant
scoping all reduce to tuples in this same graph.

## Why Zanzibar

A reachability model gives **tenant isolation and fine-grained sharing
from the same primitive**. There is no separate code path for "is this
user in this tenant" versus "can this user read this channel" — both
are checks against the relation graph, which keeps the authorization
surface small and auditable.

## Further reading

- [architecture.md §6](architecture.md) — permission plane in the component map.
- [design.md §7.1](design.md) — permission design rationale.
- [connector-protocol.md](connector-protocol.md) — ACL projection from source systems.
- [../guides/build-b2b-knowledge.md](../guides/build-b2b-knowledge.md) — multi-tenant tutorial.
