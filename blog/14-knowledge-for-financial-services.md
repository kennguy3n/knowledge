# Knowledge for Financial Services

> **TL;DR:** Financial firms must retain records for years while
> protecting them against a decade-long threat horizon. Knowledge's
> hybrid post-quantum cryptography defends long-lived data against
> harvest-now-decrypt-later, while audit logging and scoped retention
> support SOX/PCI-style controls.

## The Business Problem

A financial services firm retains client records — account histories,
transaction context, correspondence — for ten years or more to satisfy
regulatory retention rules (SOX-era recordkeeping, PCI-DSS for
cardholder data, and sector-specific mandates). Two pressures pull in
opposite directions. Retention says *keep it, intact, for a long time*.
Security says *that long-lived, sensitive data is a prime target*.

The combination creates a specific cryptographic problem. Data
encrypted today must stay confidential for its entire retention life.
An adversary who records encrypted data now and waits for a
cryptographically-relevant quantum computer — the "harvest now, decrypt
later" threat from [post 4](04-post-quantum-crypto-for-mortals.md) —
threatens *today's* records even if that computer is years away. For
ten-year data, that is not a hypothetical; it is in scope today.

## The Technical Approach

Knowledge's design lines up with the financial-services profile —
strong long-horizon cryptography, tamper-evident records, and bounded
access:

- **Post-quantum confidentiality** ([post 4](04-post-quantum-crypto-for-mortals.md)).
  The hybrid X25519 + ML-KEM-768 KEM means an attacker must break both
  a classical and a post-quantum scheme to read harvested data. For
  ten-year records, this directly addresses the harvest-now-decrypt-
  later risk. ML-DSA-65 signatures provide post-quantum provenance.
- **Encryption at rest, everywhere.** All evidence is encrypted under
  per-scope DEKs in a SQLCipher store; sensitive data is never written
  in plaintext (see the [crypto spec](../docs/technical/crypto-spec.md)).
- **Tamper-evident audit log** (the [`audit_service` crate](../crates/audit_service/)).
  An append-only audit trail of sensitive actions supports the
  access-tracking and recordkeeping SOX-style controls expect.
- **Scoped retention.** Scopes are the unit of both isolation and
  lifecycle: matter/account-scoped data can be retained for its
  required period and then cryptographically forgotten
  ([post 3](03-memory-that-forgets.md)) once the retention clock
  expires — retention and erasure governed by the same primitive.
- **Permissioned access** ([post 9](09-multi-tenant-at-scale.md)) so
  client data is reachable only by authorized principals, with changes
  tracked via SCIM.

## Implementation Walk-through

A retention-and-protection flow uses the same primitives, with the
lifecycle made explicit:

```text
scope_id = account_scope(account_id)     // per-account isolation
ingest_message(scope_id, record, ...)    // PQ-encrypted at rest
// ... retained for the mandated period, audit-logged on access ...
forget(scope_id)                          // cryptographic erasure at end of life
```

The [compliance doc](../docs/operator/compliance.md) discusses mapping
these controls to financial-sector requirements, and the
[key-management guide](../docs/security/key-management.md) covers the
master-key custody decisions that long-retention data makes critical —
because for ten-year records, key management *is* the security program.
(As always: the firm owns the compliance program; Knowledge provides
the technical controls.)

## Performance & Cost Implications

Post-quantum protection is not a performance tax at financial-workflow
scale: from the [benchmarks](../docs/technical/benchmarks.md), hybrid
KEM operations run in ~160 µs and ML-DSA-65 signing in ~320 µs — sub-
millisecond per operation. Encrypting and signing every record costs a
trivial amount of compute relative to the value of closing the quantum
threat.

Retention has a storage cost, but because the substrate deduplicates
content and routes by size ([post 8](08-performance-at-device-scale.md)),
ten years of records are stored efficiently, and end-of-life erasure is
a constant-time key destruction rather than an expensive bulk purge.

## What's Next

Financial services centers on long retention and cryptographic
durability. Legal practice adds a different constraint: privilege and
matter-scoped confidentiality, plus the need to export cleanly for
discovery. The next post covers Knowledge for legal.

---
*This is part 14 of the "Building Knowledge" series. [Previous: Knowledge for Healthcare](13-knowledge-for-healthcare.md) | [Next: Knowledge for Legal](15-knowledge-for-legal.md)*
