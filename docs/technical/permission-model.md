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

## Typed objects, subjects, and relations

Objects and subjects are both typed. The substrate distinguishes:

| Type | Role |
|---|---|
| `tenant` | Top of the B2B hierarchy; owns domains, channels, users |
| `domain` | Cross-channel workstream within a tenant |
| `channel` | Scope where messages and files land; the primary synthesis scope |
| `user` | A person; has devices and roles |
| `group` | A directory (SCIM-provisioned) set of users; used on the subject side via a `# member` userset rewrite |
| `device` | A user's endpoint; holds DEK delegations |
| `concept` / `summary` / `workflow` / `export_profile` / `agent` | Semantic-plane and write-contract objects |

The relations are:

| Relation | Meaning |
|---|---|
| `owner` | Full control, including delete and key destruction |
| `admin` | Configure policy, manage members, approve proposals |
| `editor` | Write canonical observations / concepts |
| `member` | Read and propose; cannot promote |
| `viewer` | Read-only |
| `synthesizer` | May publish synthesis objects to the scope |
| `proposer` | Agents only; propose, never promote |

`owner`/`admin`/`editor`/`member`/`viewer` form the inheritance chain
below. `synthesizer` and `proposer` are orthogonal ambient roles checked
directly — holding them implies nothing else, and nothing implies them.

## Namespace inheritance

The walk folds in namespace inheritance so higher relations imply lower
ones:

```
owner ⇒ admin ⇒ editor ⇒ member ⇒ viewer
```

So an `owner` automatically satisfies a `viewer` check without an
explicit tuple. This chain is registered by
`NamespaceRegistry::with_defaults()` for the scope-style object types
(`tenant`, `domain`, `channel`, `user`), and the substrate permission
store is constructed with that registry, so the implication holds at
runtime: granting a user `admin` on a tenant satisfies the `viewer`
gate on that tenant's read routes with no second tuple. `group` carries
no inheritance chain — only its `member` relation is meaningful — so a
group's role binding flows purely through the userset rewrite below.

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

## Directory groups and SCIM provisioning

Identity providers provision users and groups through the gateway's
SCIM v2 endpoints. Two kinds of tuple connect a directory to the
relation graph:

- **Membership.** Each `(group, user)` membership becomes
  `group:<gid># member @ user:<uid>`. The tuple exists iff the user is
  both a member of the group and active, so suspending a user removes
  group-derived access without dropping the directory record.
- **Role binding.** A group whose IdP-supplied `DisplayName` matches the
  convention `knowledge:tenant:<tenantUUID>:<role>` is bound to that
  tenant role via `tenant:<tenantUUID># <role> @ group:<gid># member`.
  Every member of the group inherits the role through the `# member`
  rewrite, so role assignment tracks group membership with no per-user
  grants. The bindable roles are `{admin, editor, member, viewer}`;
  `owner` is excluded (the tenant root is never bootstrappable from an
  IdP-controlled name), as are `synthesizer`/`proposer` (not
  tenant-hierarchy roles). Validation is structural — the tenant segment
  must parse as a UUID and the role must be allowed — and a group whose
  name does not match stays membership-only, so existing groups are
  unaffected. A rename re-points the binding (grant-new-before-revoke-old
  so members never lose the role mid-update); a delete revokes it.

Both kinds are reconciled atomically alongside the SCIM write through
the gateway's substrate-first apply path, which rolls back on failure.

## Control-plane authorization

The Go gateway authorizes the control plane before any call reaches the
substrate:

- **Platform-global lifecycle** (create tenant, list all tenants, delete
  tenant) and the `permission` and `scim` mounts are **service-only** —
  reachable only by the service principal (static API key), never by a
  tenant-scoped JWT.
- **Per-tenant routes** are ReBAC-authorized against the `{id}` tenant:
  reads (get tenant, list members, audit) require `viewer`; mutations
  (config, key rotation, member management, export) require `admin`.
  Because `admin` inherits `viewer` at runtime, an `admin` passes the
  read gates without a separate grant.
- The guards **fail closed**: if no permission service is wired, the
  per-tenant routes collapse to service-only rather than running
  ungated.

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
