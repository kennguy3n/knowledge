# Multi-Tenant at Scale

> **TL;DR:** Serving thousands of organizations from one deployment
> demands hard isolation. Knowledge combines per-scope encryption keys,
> Zanzibar-style relationship permissions, SCIM provisioning, and
> connector ACL projection so each tenant's data — and each user's
> access within a tenant — stays exactly where it should.

## The Business Problem

A B2B SaaS company runs one Knowledge-powered platform for 1,000
tenant organizations. Each tenant connects its own mix of sources —
this one uses Notion and Slack, that one Jira and Confluence — and each
has its own employees with their own access levels. The platform must
guarantee three things simultaneously: tenant A can never see tenant
B's data; within a tenant, an employee can only retrieve documents
they're permitted to see; and when someone leaves, their access
disappears.

Get any of these wrong and the failure is catastrophic, not cosmetic. A
cross-tenant leak is a breach-notification event. An intra-tenant
permission bug surfaces a confidential document to someone who should
never have seen it. "We'll add permissions later" is how knowledge
tools become liabilities.

## The Technical Approach

Knowledge layers four mechanisms, each covered in depth in the docs:

**Per-scope keys for tenant isolation.** Every tenant maps to its own
scope (or set of scopes), and each scope has its own DEK derived from
the master key (see [post 4](04-post-quantum-crypto-for-mortals.md)).
Tenant A's evidence is encrypted under tenant A's keys; a query in
tenant B's scope literally cannot decrypt it. Isolation is
cryptographic, not just a `WHERE tenant_id = ?` clause that one bad
query can bypass.

**Zanzibar-style permissions.** The
[`permission_service` crate](../crates/permission_service/) implements
Google-Zanzibar-style authorization: access is expressed as **relation
tuples** over objects and principals, and a permission check is a
**reachability query** across those relations. Namespaces support role
inheritance (owner ⇒ admin ⇒ editor ⇒ member ⇒ viewer) and userset
rewrites, so "can Alice read document X?" is answered by traversing the
relationship graph. The [permission model](../docs/technical/permission-model.md)
is the full reference.

**SCIM provisioning.** The [`tenant_service` crate](../crates/tenant_service/)
models tenant lifecycle and member provisioning, integrating with SCIM
so identity changes in the customer's IdP — new hires, role changes,
departures — flow into the platform automatically. When someone leaves,
deprovisioning removes their access without manual intervention.

**Connector ACL projection.** As covered in
[post 6](06-connector-architecture.md), connectors project the source
system's ACLs into the permission model. A document's readership in
Drive becomes the principals allowed to retrieve it through Knowledge,
so connecting a source does not flatten its permissions.

Together: tenants are isolated by cryptography, intra-tenant access is
governed by relationship permissions, identity is kept current by SCIM,
and source permissions are preserved by ACL projection.

## Implementation Walk-through

The multi-tenant flow, end to end, is the subject of the
[build-b2b-knowledge tutorial](../docs/guides/build-b2b-knowledge.md):

```text
create tenant            // tenant_service
provision members        // SCIM from the customer IdP
connect sources          // OAuth2 + sync + ACL projection (per tenant)
grant / check / revoke   // permission_service relation tuples
query(scope, text)       // returns only ACL-permitted, in-tenant results
```

A permission check is explicit and auditable:

```text
grant(tuple: user:alice  editor  doc:rebrand-brief)
check(user:alice, read, doc:rebrand-brief) -> true   // via reachability
revoke(tuple: ...)
```

Every sensitive action also lands in the audit log (the
[`audit_service` crate](../crates/audit_service/)), so isolation and
access decisions are not just enforced but recorded.

## Performance & Cost Implications

Permission checks are the operation that runs on every retrieval, so
their cost matters. The [benchmarks](../docs/technical/benchmarks.md)
measure reachability checks across graph sizes — microseconds for small
relation sets, scaling predictably as the graph grows — so authorization
is not the bottleneck.

Operationally, multi-tenant deployments run in
[enterprise mode](07-zero-to-production-deployment.md): gateway plus
substrate plus Postgres, scaled per the
[scaling guide](../docs/operator/scaling.md). The gateway tier is
stateless and scales horizontally; per-tenant substrate work stays
bounded because each tenant's data and keys are partitioned. Adding the
1,001st tenant does not require re-architecting the first 1,000.

## What's Next

Isolation and permissions are what make multi-tenant safe. The next
post tackles the economics that make it *viable*: how the substrate's
zero-marginal-cost design plays out when you're serving 100,000 users,
and where that model has limits.

---
*This is part 9 of the "Building Knowledge" series. [Previous: Performance at Device Scale](08-performance-at-device-scale.md) | [Next: Cost Engineering: Zero Marginal](10-cost-engineering-zero-marginal.md)*
