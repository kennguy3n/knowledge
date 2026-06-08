# Knowledge for Healthcare

> **TL;DR:** Healthcare is the hardest privacy environment there is.
> Knowledge's on-device-by-default model, per-patient scope isolation,
> and cryptographic forgetting map directly onto HIPAA minimum-necessary
> access and the right to erasure — keeping protected health information
> off central servers by construction.

## The Business Problem

A healthcare provider wants an AI assistant that helps clinicians by
remembering context across patient interactions — prior visits, care
plans, open follow-ups. The clinical value is obvious. The compliance
exposure is equally obvious: every piece of that context is Protected
Health Information (PHI) under HIPAA, and the moment it lands on a
central server, the provider inherits a sprawling set of obligations —
access controls, audit trails, breach notification, business-associate
agreements with every vendor in the path.

The traditional cloud-AI answer multiplies that risk: PHI flows to an
inference provider, gets embedded into a vector store, and is retained
in logs and backups across multiple third parties. Each hop is a place
PHI can leak and a party that must be brought under a BAA. For many
providers, that risk surface is simply not worth the feature.

## The Technical Approach

Knowledge changes the risk surface by keeping PHI where it already is
and never sending it to a central corpus. The relevant building blocks,
applied to healthcare (see the [compliance doc](../docs/operator/compliance.md)
and [threat model](../docs/security/threat-model.md)):

- **On-device by default** ([post 1](01-why-on-device-memory.md)). PHI
  stays on the clinician's device or within the provider's controlled
  boundary; there is no central plaintext store to breach and no
  cross-vendor PHI flow for routine memory and retrieval.
- **Per-patient scope isolation** ([post 4](04-post-quantum-crypto-for-mortals.md)).
  One scope per patient, each with its own DEK, so a patient's data is
  cryptographically partitioned — aligning with HIPAA's
  minimum-necessary principle at the storage layer.
- **Cryptographic forgetting** ([post 3](03-memory-that-forgets.md)).
  A records-deletion or correction obligation is honored by destroying
  the patient scope's key — making the data unrecoverable, with a
  durable tombstone proving it was forgotten.
- **Audit logging** (the [`audit_service` crate](../crates/audit_service/)).
  Sensitive actions are recorded in an append-only audit log, supporting
  the access-tracking HIPAA expects.
- **On-device inference** ([post 5](05-on-device-inference-under-constraints.md)).
  Synthesis runs locally or in a TEE, so PHI is not shipped to a
  token-metered model API.

## Implementation Walk-through

A healthcare deployment maps cleanly onto the substrate's primitives:

```text
scope_id = patient_scope(patient_id)     // one scope per patient
ingest_message(scope_id, note, ...)      // PHI encrypted under patient DEK
query(scope_id, "recent care plan")      // minimum-necessary retrieval
forget(scope_id)                         // erasure / right to be forgotten
```

For multi-clinician settings, the
[permission model](../docs/technical/permission-model.md) restricts
which clinicians can reach which patient scopes, and SCIM
([post 9](09-multi-tenant-at-scale.md)) keeps access current as staff
change. The [compliance doc](../docs/operator/compliance.md) discusses
how these map to HIPAA controls. (Standard disclaimer: compliance is a
property of your whole deployment and processes, not of any single
library — Knowledge provides the technical controls; the provider owns
the program.)

## Performance & Cost Implications

The clinical workflow is interactive, and the on-device numbers from
[post 8](08-performance-at-device-scale.md) — ~9.7 ms hybrid retrieval —
keep "what's this patient's history?" instant at the point of care.
Cryptographic forgetting is constant-time at erasure
([post 3](03-memory-that-forgets.md)), so honoring a deletion request is
immediate rather than a batch reprocessing job.

The cost and risk reduction is the real win: by keeping PHI off central
servers, the provider shrinks both its breach surface and its
BAA-management burden, while paying zero marginal infrastructure cost
for on-device memory.

## What's Next

Healthcare optimizes for erasure and minimum-necessary access. Financial
services has almost the opposite pressure — *long* retention — which
makes the post-quantum story central. The next post looks at Knowledge
for financial services.

---
*This is part 13 of the "Building Knowledge" series. [Previous: Observability Without Ops](12-observability-without-ops.md) | [Next: Knowledge for Financial Services](14-knowledge-for-financial-services.md)*
