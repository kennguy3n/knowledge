# Cryptographic Specification

This document specifies the cryptographic design of Knowledge: the
primitive inventory, the key hierarchy, and the cryptographic
forgetting protocol. It is the reference companion to
[architecture.md §8](architecture.md#8-post-quantum-crypto-layer) and
[design.md §9](design.md#9-post-quantum-cryptography). The
implementation lives in the `crypto` crate (`crates/crypto`).

## Primitive inventory

| Purpose | Primitive | Notes |
|---|---|---|
| Content hashing | **BLAKE3** | Evidence-body integrity framing. |
| Symmetric AEAD | **XChaCha20-Poly1305** | Per-scope, per-epoch encryption of evidence bodies and cold-archive segments. 192-bit nonces are safe to sample randomly. |
| Key derivation | **HKDF-SHA256** | Derives context-labelled subkeys from the per-user master key. |
| Key encapsulation | **Hybrid X25519 + ML-KEM-768** | Concatenate-then-KDF combiner (HKDF-SHA256). Both halves are real; the PQC half is FIPS 203 ML-KEM-768 via the `ml-kem` RustCrypto crate. |
| Provenance signatures | **ML-DSA-65** | FIPS 204 lattice signatures for per-synthesis provenance. `ZeroizeOnDrop` on long-lived secret keys. |
| Archival co-signing | **SPHINCS+-SHAKE-128f-simple** | Stateless hash-based signatures for the archival AND-combiner path (PQClean via `pqcrypto-sphincsplus`). |

All long-lived secret-key state is wrapped in `ZeroizeOnDrop` so it is
scrubbed from memory on destruction. See
[random-number generation](#random-number-generation) for entropy
sourcing.

## Why post-quantum, and why hybrid

The threat is **harvest-now, decrypt-later**: an adversary who captures
ciphertext today can store it until a cryptographically relevant
quantum computer exists. For data with a long confidentiality horizon
(financial records, health records, legal matters), that horizon is
already inside the window of concern.

Knowledge uses a **hybrid** KEM rather than a pure-PQC one: the shared
secret is derived from both an X25519 (classical) and an ML-KEM-768
(post-quantum) encapsulation, combined through HKDF-SHA256. The result
is at least as strong as the stronger of the two — a break of either
primitive alone does not compromise the session key. This hedges
against both a future quantum break of X25519 *and* an
implementation/analysis flaw in the comparatively young lattice
schemes.

## Key hierarchy

```
Master key (per user, 32 bytes)
    │  HKDF-SHA256(context label)   ── derives the keys below; never stores them
    ├── DEK wrapping key  (HKDF "scope-dek-wrap:v1")
    │       └── AEAD-wraps each Scope DEK at rest (scope_deks.wrapped_dek)
    │               └── Scope DEK   (per scope, RANDOM from OS RNG — not derived)
    │                       └── AEAD-wraps per-row CEKs (body_store_key_wraps.wrapped_cek)
    │                               └── Content Encryption Key (per body row, RANDOM)
    │                                       └── XChaCha20-Poly1305 over the evidence body
    ├── Archive segment key  (per cold segment)     ── AEAD cold archive
    └── Wrapping keys        (for KEM-encapsulated transfer)
```

The Scope DEK is **random and stored only in wrapped form**, not
HKDF-derived from the master key — see the note below for why this
matters to the forgetting guarantee.

- The **master key** never leaves the device's secure element in
  plaintext beyond the live process; see
  [key-management.md](../security/key-management.md) for the
  resolver-driven cold-boot flow.
- **Scope DEKs** are the unit of cryptographic forgetting, rotated per
  epoch. New scopes generate a **random** DEK from the OS RNG and
  persist it **only** as a blob AEAD-wrapped under a master-derived
  wrapping key (`HKDF-SHA256("scope-dek-wrap:v1")`), in the
  `scope_deks` table. This is deliberate: a random, wrap-only DEK
  cannot be re-derived from `master_key + context`, so destroying the
  wrap makes the scope unrecoverable **even if the master key is later
  compromised**. (A legacy HKDF-derived DEK path exists for
  pre-existing scopes.) See `crates/evidence_store/src/schema.rs`
  (`scope_deks`, `body_store_key_wraps`) and `store_scope_dek` /
  `dek_wrapping_key` in `store.rs`.
- Other context-labelled subkeys (the SQLCipher page key, the
  permission-tuple key, the DEK wrapping key) **are** HKDF-derived
  deterministically from the master key plus a context label, so they
  reproduce on cold boot without being stored.
- For the full HNDL threat model, the per-scope DEK lifecycle, and an
  honest account of what forgetting does and does not guarantee, see
  the [PQC threat-model whitepaper](../security/pqc-threat-model.md).

## Cryptographic forgetting protocol

Forgetting in Knowledge is not a soft-delete flag — it is **key
destruction**. Each scope's evidence bodies are encrypted under a scope
DEK. To forget a scope:

1. Destroy (zeroize) the scope DEK and any wrapped copies.
2. Purge the FTS5 index rows for the scope.
3. Replay tombstones so derived structures (concept graph, memory
   objects) drop the scope's contributions.

Once the DEK is gone, the ciphertext bodies are unrecoverable even by
the device owner — this is what makes GDPR Article 17 / "right to
erasure" enforceable rather than aspirational. The forgetting path is
exercised end-to-end by the [quickstart demo](../QUICKSTART.md) and the
`crypto::forgetting` module.

## Random number generation

All key material and nonces are sampled from the operating system CSPRNG
via the `rand` / `getrandom` stack. The substrate does not implement its
own PRNG and does not seed from low-entropy sources. See
[SECURITY.md](../../SECURITY.md#random-number-generation) for the full
RNG posture and platform notes.

## Further reading

- [architecture.md §8](architecture.md#8-post-quantum-crypto-layer) — where crypto sits in the component map.
- [design.md §9](design.md#9-post-quantum-cryptography) — design rationale and threat model.
- [../security/key-management.md](../security/key-management.md) — key storage and cold-boot handling.
- [../security/threat-model.md](../security/threat-model.md) — formal threat model.
- [../security/pqc-threat-model.md](../security/pqc-threat-model.md) — certifiable PQC threat-model whitepaper (HNDL, forgetting guarantees/limits, HIPAA/SOX/FERPA mapping, external-review checklist).
