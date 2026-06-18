# The Server & Multi-Tenancy

> **TL;DR:** Consumer apps run the substrate entirely on-device. B2B
> wants a server too — for connector pipelines, cross-tenant synthesis,
> and an admin surface — without becoming the centralized cloud you set
> out to avoid. This post builds the Go gateway plus the
> `tenant_service`, `permission_service`, and `audit_service`, and shows
> the fairness controls that let one deployment serve 5,000 tenants
> without one noisy tenant starving the rest.

## What you are building

A **self-hostable** server surface (run it in *your* region, not a
vendor's cloud):

- **API gateway** (Go, `server/cmd/gateway`) — the mTLS/HTTP front door
  exposing `/api/v1/{ingest,query,memories,synthesis,reasoning,forget,
  connectors}`.
- **`tenant_service`** — tenant lifecycle, per-tenant keys, member
  provisioning.
- **`permission_service`** — a Zanzibar-style relation graph with
  reachability checks.
- **`audit_service`** — an append-only audit log of canonical
  promotions, exports, agent proposals, and policy changes.

The server is a *peer* of the device surface, not a replacement: it
shares the same observation/semantic/reasoning/export schema and reuses
the same Rust core via FFI. The gateway is a thin, fair, auditable shell
around the substrate — not a new source of truth for user content.

## Build it: permissions as a reachability graph

Model authorization as a **Zanzibar-style relation graph** rather than
per-endpoint role checks. A tuple is `object_type:id#relation@subject`,
where the subject is either a single user or a *userset* like
`group:<gid>#member` (every member of the group inherits the relation).
Object types are `tenant`, `domain`, `channel`, `user`, and `group`; a
permission check is a bounded reachability walk over those tuples. The
`bench_permission_check` harness keeps it honest:

| Check | p50 | Rate |
|---|---|---|
| allowed (depth-5 chain) | 6.51 µs | ~152K checks/sec |
| denied (worst case: walks the reachable closure) | 112.3 µs | ~8.8K checks/sec |

Note the asymmetry — the *denied* path is the expensive one because it
must exhaust the reachable set before returning `false`. Knowing that
shape is what lets you size the gateway's auth budget.

Roles imply one another along a fixed chain —
`owner ⇒ admin ⇒ editor ⇒ member ⇒ viewer` — registered for the
`tenant`, `domain`, `channel`, and `user` namespaces, so a single
`admin` grant satisfies a `viewer` check without a second tuple. (Groups
carry no inheritance chain: only their `member` relation is meaningful.)
Wire that chain in at construction time
(`NamespaceRegistry::with_defaults()`) — an empty registry silently
disables every implication.

## Build it: gate the control plane

The relation graph is only useful if the gateway actually consults it.
Two classes of route, two gates:

- **Service-only** — tenant lifecycle, `/permission/*`, and `/scim/v2/*`
  are infrastructure surfaces; they require a service principal and are
  closed to tenant-user tokens outright.
- **Per-tenant ReBAC** — tenant reads (get tenant, list members, audit)
  gate on `viewer`; mutations (config, key rotation, member management,
  export) gate on `admin`. With the inheritance chain above, an
  `admin` passes the `viewer` gate automatically. Non-service principals
  are denied until a role is provisioned — deny-by-default closes the
  cross-tenant read hole.

Provision those roles from your IdP without a bespoke endpoint: SCIM
syncs group membership as `group:<gid>#member@user:<uid>`, and a group
whose `DisplayName` matches `knowledge:tenant:<tenantUUID>:<role>` is
bound to that tenant role via `tenant:<id>#<role>@group:<gid>#member`.
Every member of the group then inherits the role through the `#member`
rewrite — role assignment tracks group membership with no per-user
grants. Bindable roles are `{admin, editor, member, viewer}` (`owner` is
excluded so a tenant root is never bootstrappable from an IdP-controlled
name).

## Build it: an audit trail you can hand an auditor

Every canonical promotion, export, agent proposal, and policy change
appends to `audit_service`. Make it **append-only and tamper-evident**
(the sibling security platform hash-chains its journal; the same pattern
applies). For regulated buyers this is the difference between "trust us"
and "here is the immutable record."

## Build it: multi-tenant fairness

The dominant risk at scale is the **shared, CPU-bound synthesis path** —
one tenant triggering many syntheses can occupy the whole `llama-server`
pool and starve the other 4,999. Three layered controls bound the blast
radius (see [`multitenant-5k.md`](../../docs/operator/multitenant-5k.md)):

1. **Synthesis fair-share** — a per-tenant concurrency cap + bounded FIFO
   queue under a global cap that matches the real pool; over the cap it
   sheds with `429` + `Retry-After` instead of piling up.
2. **Per-tenant quotas** — requests/min, syntheses/day, and an advisory
   storage soft cap, with per-tenant overrides.
3. **Per-tenant SLOs** — p50/p95/p99 latency + error-rate metrics with
   tenant-cardinality protection and error-budget recording rules.

Sizing rule: `globalConcurrency ≈ replicaCount × per-replica
parallelism`, with `tenantConcurrency` kept small (1–2) so no tenant
holds more than a fraction of capacity (at `2`/`8`, at most 25%).

## The business decision: self-host/in-region vs. SaaS

**Scenario.** A bank in the GCC or a hospital network in the EU will buy
a knowledge product only if the data — and the keys — stay in their
region, under their control, with an audit trail their regulator
accepts.

- **SaaS (Copilot, Glean, hosted memory layers).** Fastest to deploy,
  managed SLA, no ops — but the data lives in the vendor's cloud, often
  the vendor's region, under the vendor's keys. For some regulated buyers
  that's a hard no regardless of price.
- **Self-host the server (this).** You run the gateway and workers in
  your own region/VPC, hold your own keys, and get the fairness + audit
  controls out of the box. More ops responsibility, but it's the only
  shape that satisfies strict residency and key-custody requirements —
  and the [HA failover guide](../../docs/operator/ha-failover.md) plus
  the [5k-tenant guide](../../docs/operator/multitenant-5k.md) make the
  ops tractable.

## How a competitor would build this

A SaaS vendor runs one global multi-tenant control plane with row-level
security and a managed SLA — efficient and operationally simple, and the
right answer when residency isn't a constraint. It can't, however, hand a
regulated customer a deployment in their own region with their own keys.
The self-hostable server is the choice you make to sell into regulated,
residency-bound markets — accepting that you ship software customers
operate rather than a service you operate for them.

## What's next

The substrate, the device side, and the server are all built. The last
mile is turning a Rust workspace into something a developer can `install`
and a user can open: FFI/N-API bindings, the reference UI, and a
one-command installer. Next.

---
*Part 10 of "How to Build Knowledge." [Previous: Sync & Multi-Device](09-sync-and-multi-device.md) | [Next: Packaging & Shipping](11-packaging-and-shipping.md) | [Series index](README.md)*
