# Post-Quantum Crypto for Mortals

> **TL;DR:** "Harvest now, decrypt later" means data you encrypt today
> with classical crypto may be readable by a future quantum computer.
> Knowledge uses a hybrid X25519 + ML-KEM-768 scheme so today's data
> stays protected against both classical *and* quantum adversaries —
> with a key hierarchy designed for clean per-scope erasure.

## The Business Problem

A financial services firm keeps client records for ten years to satisfy
retention rules. Some of that data — account histories, identifiers,
correspondence — is sensitive for the entire decade and beyond. The
firm's security team is asked a pointed question: *if an adversary
records our encrypted traffic and storage today, and a
cryptographically-relevant quantum computer arrives in eight years,
what happens to the data we encrypted this morning?*

With classical public-key cryptography alone, the honest answer is
uncomfortable: it becomes readable. This is the "harvest now, decrypt
later" threat. An adversary does not need a quantum computer *today* to
threaten *today's* data — they only need to store the ciphertext and
wait. For any data with a long confidentiality lifetime — financial
records, health data, legal matters, government information — the
quantum threat is a present-tense problem, not a future one.

## The Technical Approach

The [`crypto` crate](../crates/crypto/) implements a small, opinionated
cryptographic surface so the rest of the substrate never touches raw
key material. The [crypto spec](../docs/technical/crypto-spec.md) is
the full specification; the essentials:

**Hybrid key encapsulation.** Key exchange uses a *hybrid* of X25519
(classical elliptic-curve) and **ML-KEM-768** (FIPS 203,
lattice-based, post-quantum). Hybrid means the shared secret depends on
*both*: an attacker must break X25519 *and* ML-KEM-768 to recover it.
This is the conservative posture recommended during the PQC transition
— you do not bet everything on a young post-quantum scheme, and you do
not ignore the quantum threat. You require both to fail.

**Post-quantum signatures.** Provenance and integrity use **ML-DSA-65**
(FIPS 204) lattice signatures, with **SPHINCS+-SHAKE** stateless
hash-based signatures available for archival co-signing where a
different security assumption is wanted.

**Symmetric and hashing layers.** Content is encrypted with
XChaCha20-Poly1305 AEAD; content hashing uses BLAKE3; key derivation
uses HKDF-SHA256. Symmetric primitives at these sizes are not
meaningfully threatened by quantum search, so the post-quantum effort
goes where it matters — the public-key layer.

**The key hierarchy.** This is what ties the cryptography to the
product. A per-user **master key** sits at the root. From it, the
substrate derives a **Data Encryption Key (DEK) per scope**. All
evidence in a scope is encrypted under its scope DEK. The hierarchy is
deliberately shaped so that:

- A scope is the unit of both *isolation* and *erasure*: destroying one
  scope DEK erases exactly one scope (the foundation of
  [cryptographic forgetting](03-memory-that-forgets.md)).
- The master key never encrypts content directly, so rotating or
  protecting it is separable from the bulk data.
- Keys are zeroized from memory after use, limiting exposure if the
  process is compromised.

## Implementation Walk-through

Host applications do not call the cryptographic primitives directly —
that is the point of the opinionated surface. A host provides a master
key at `open_store` (resolved from the platform secure store — Keychain,
Keystore, TPM/DPAPI), and the substrate derives scope DEKs, encrypts on
write, and decrypts on read transparently:

```text
open_store(db_path, master_key)   // master key from secure store
ingest_message(scope_id, ...)     // encrypted under derived scope DEK
forget(scope_id)                  // destroy scope DEK -> unrecoverable
```

Where the master key actually lives is the most important operational
decision, and it is the host's responsibility: a hardware-backed secure
element wherever possible. The
[key-management guide](../docs/security/key-management.md) covers secure
storage, resolver paths, and rotation constraints; the
[threat model](../docs/security/threat-model.md) covers what the crypto
does and does not defend against (it protects data at rest on a
personal device; it cannot save you if the attacker holds the master
key).

## Performance & Cost Implications

Post-quantum does not mean slow. From the
[benchmarks](../docs/technical/benchmarks.md): hybrid KEM (X25519 +
ML-KEM-768) encapsulation runs in about **159.9 µs** and decapsulation
in **156.8 µs**; ML-DSA-65 signing in **320.3 µs** and verification in
**77.4 µs**. AEAD encryption sustains hundreds of MiB/s and over 1 GiB/s
on decrypt for larger payloads. SPHINCS+ signing is the expensive
outlier at **17.36 ms**, which is why it is reserved for archival
co-signing rather than the hot path.

These numbers are comfortably within a device's budget for
interactive use. The firm protecting ten-year records pays a sub-
millisecond cryptographic tax per operation to close the harvest-now-
decrypt-later threat — a trade any long-retention business should take.

## What's Next

Strong cryptography on a device is moot if the AI workload can't
actually run there. The next post is about on-device inference:
fitting small language models into 2–8 GB of RAM and routing across
the runtimes that real devices expose.

---
*This is part 4 of the "Building Knowledge" series. [Previous: Memory That Forgets](03-memory-that-forgets.md) | [Next: On-Device Inference Under Constraints](05-on-device-inference-under-constraints.md)*
