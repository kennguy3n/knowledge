# Knowledge for Legal

> **TL;DR:** Legal work demands strict matter-by-matter confidentiality,
> defense of privilege, and clean export for discovery. Knowledge's
> per-matter scope isolation, permission model, audit trail, and
> policy-gated export plane map onto exactly those needs.

## The Business Problem

A law firm wants an assistant that remembers the context of each
matter — filings, correspondence, research, decisions — so attorneys
can pick up a case without re-reading the whole file. But legal data
carries obligations that make a naive knowledge tool dangerous:

- **Privilege.** Attorney-client privileged material must not leak, and
  certainly must not be commingled with other matters or exposed to a
  third-party AI vendor in a way that could be argued to waive
  privilege.
- **Matter confidentiality and ethical walls.** Information from one
  matter must be walled off from attorneys working a conflicting
  matter.
- **Discovery.** When material is responsive to a discovery request,
  the firm must be able to identify and export *exactly* the relevant
  scope — no more, no less.

A shared, server-side index that mixes matters and ships content to an
external model is incompatible with all three.

## The Technical Approach

Knowledge's scope model is a natural fit for matter-centric work:

- **Per-matter scope isolation.** One scope per matter, each with its
  own DEK ([post 4](04-post-quantum-crypto-for-mortals.md)). Matter A's
  material is cryptographically separated from matter B's — the
  technical substrate for an ethical wall.
- **Permissioned access and walls** ([post 9](09-multi-tenant-at-scale.md)).
  The Zanzibar-style [permission model](../docs/technical/permission-model.md)
  controls which attorneys can reach which matter scopes, so conflict
  walls are enforced by reachability, not by policy alone.
- **On-device / no third-party exposure** ([post 1](01-why-on-device-memory.md)).
  Privileged content stays within the firm's controlled boundary rather
  than flowing to an external inference provider — reducing the
  privilege-waiver surface.
- **Audit trail** (the [`audit_service` crate](../crates/audit_service/))
  for a defensible record of who accessed what.
- **Policy-gated export for discovery.** The
  [`export_plane` crate](../crates/export_plane/) provides a narrow,
  policy-gated interface for moving curated knowledge out of the
  substrate, with an export policy engine and a policy simulator (per
  the [design document](../docs/technical/design.md) §3.5). Export is
  scoped and governed, so producing exactly one matter's responsive
  material for discovery is a controlled operation, not a database
  dump.

## Implementation Walk-through

Matter lifecycle maps onto the substrate primitives, with export as a
first-class, policy-checked step:

```text
scope_id = matter_scope(matter_id)        // one scope per matter
grant(attorney, member, matter_scope)     // conflict-walled access
ingest_message(scope_id, filing, ...)     // privileged content, encrypted
query(scope_id, "what did we argue re: jurisdiction?")
export(scope_id, policy)                  // policy-gated discovery export
```

The export plane's policy engine lets the firm define what may leave
the substrate and simulate a policy before applying it — so a discovery
export can be validated to include the responsive matter and exclude
everything else. The [api cookbook](../docs/guides/api-cookbook.md) and
the [build-b2b-knowledge tutorial](../docs/guides/build-b2b-knowledge.md)
show the scope/permission/export wiring.

## Performance & Cost Implications

Matter scopes are independent, so a firm with thousands of matters pays
no cross-matter retrieval penalty — each query runs against one scoped
index at the ~9.7 ms hybrid-retrieval latency from
[post 8](08-performance-at-device-scale.md). Export is bounded to the
target scope, so producing a matter's material is proportional to that
matter, not the whole store.

Keeping privileged content on-device also avoids per-seat external AI
costs and, more importantly, avoids the harder-to-quantify cost of a
privilege dispute over third-party data handling. When a matter closes
and retention expires, cryptographic forgetting
([post 3](03-memory-that-forgets.md)) erases it cleanly.

## What's Next

Legal centers on confidentiality and controlled export. The next post
shifts from industry to geography: deploying across APAC, where CJK
language support, data-residency rules, and device constraints in
emerging markets all come together.

---
*This is part 15 of the "Building Knowledge" series. [Previous: Knowledge for Financial Services](14-knowledge-for-financial-services.md) | [Next: Knowledge Across APAC](16-knowledge-across-apac.md)*
