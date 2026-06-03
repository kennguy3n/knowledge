# Threat Model

This is the formal threat model for the Knowledge substrate. It expands
on the summary in [SECURITY.md](../../SECURITY.md) and explains what
Knowledge defends against, what it explicitly does not, and why. For the
disclosure process and supported versions, see
[SECURITY.md](../../SECURITY.md).

## What we protect

User data at rest on a personal device — evidence bodies, derived
observations, concepts, and synthesized memory — plus the integrity and
provenance of synthesis outputs.

## Assets

| Asset | Protection |
|---|---|
| Evidence bodies | XChaCha20-Poly1305 AEAD under per-scope keys, inside a SQLCipher (AES-256) database. |
| Master key | Held in the platform secure element; never a long-lived plaintext in process (resolver path). |
| Scope DEKs | Derived per scope/epoch; destruction = cryptographic forgetting. |
| Synthesis provenance | ML-DSA-65 signatures; SPHINCS+ co-signing for archival verifiability. |
| Session secrets (sync/transfer) | Hybrid X25519 + ML-KEM-768 KEM. |

## Trust assumptions

- The **host OS** provides process isolation and filesystem-level
  protection.
- An attacker who obtains a copy of the encrypted SQLCipher database
  does **not** have the master key.
- The platform secure store (Keychain / Keystore / OS keychain) behaves
  as documented.

## Adversaries and defenses

### Stolen device / stolen database file

An attacker with the encrypted database but not the master key cannot
read content: bodies are AEAD-encrypted under keys derived from a key
that lives in the secure element. See
[key-management.md](key-management.md).

### Harvest-now, decrypt-later (future quantum adversary)

An adversary who records ciphertext today hoping to decrypt it after a
quantum break. Defended by the **hybrid** X25519 + ML-KEM-768 KEM: the
session secret is at least as strong as the stronger half, so a future
break of X25519 alone does not recover harvested secrets. See
[crypto-spec.md](../technical/crypto-spec.md).

### Right-to-erasure / data subpoena over forgotten data

Once a scope is forgotten, its DEK is destroyed and the ciphertext is
unrecoverable — even the device owner cannot produce the plaintext.
This makes erasure enforceable rather than a soft-delete flag.

### Tampered or forged synthesis output

Synthesis provenance is signed (ML-DSA-65), so a consumer can verify a
synthesis came from the expected signer and was not altered.

### Malicious / compromised connector source

Connectors fetch from external systems; source ACLs are projected into
the permission graph so a document's reachability mirrors the source.
A connector cannot widen access beyond what its scope attachment and the
permission graph allow. See
[../technical/connector-protocol.md](../technical/connector-protocol.md).

## Explicit non-goals (known limitations)

Knowledge is honest about what it does **not** defend against:

- **A compromised running process.** An attacker who compromises the
  live process has access to data decrypted in memory. Zeroize-on-drop
  and scope-bound keys shrink the exposure window but do not eliminate
  it.
- **Host-OS snapshot retention.** If the host filesystem retains old
  snapshots beneath the SQLCipher layer, forgotten ciphertext could
  survive there — that is a host-OS concern outside the substrate's
  control.
- **Side channels.** Timing/cache side channels in the underlying
  cryptographic libraries are out of scope for the substrate's own
  guarantees.
- **A malicious host application.** The substrate trusts the app that
  embeds it; a hostile host can misuse the API surface.
- **No third-party audit yet.** As of 1.0 the design has not undergone an
  external security audit. Treat the guarantees as design intent backed
  by tests, not audited claims.

## Defense in depth

Beyond the primitives, the substrate uses zeroize-on-drop on long-lived
secret-key state, scope-bound key derivation, and (in Electron hosts)
the [renderer-process hardening checklist](electron-hardening.md).

## Further reading

- [SECURITY.md](../../SECURITY.md) — policy, disclosure, supported versions.
- [crypto-spec.md](../technical/crypto-spec.md) — primitives and key hierarchy.
- [key-management.md](key-management.md) — key storage and cold-boot.
- [electron-hardening.md](electron-hardening.md) — Electron threat model.
