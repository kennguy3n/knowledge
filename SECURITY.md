# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in this project, please
report it responsibly. **Do not open a public GitHub issue.**

Send an email to **ken@uney.com** with:

- A description of the vulnerability and its potential impact.
- Reproduction steps; a proof of concept is appreciated.
- Any suggested mitigation or fix.

We will acknowledge receipt within 72 hours and aim to share a
fix or mitigation plan within 14 days, depending on severity.

## Scope

This policy covers the Rust workspace in this repository: every
crate under `crates/`, the CI pipeline, and the build artifacts
they produce (UniFFI `.xcframework`, JNI `.so`, N-API addon).

It does **not** cover the Go gateway, the host UI shells, or
production deployment infrastructure — those live in other
repositories and have their own disclosure policies.

## Threat model

The substrate protects user data at rest on a personal device.
The threat model assumes:

- The host OS provides process isolation and filesystem-level
  encryption.
- An attacker who obtains a copy of the encrypted SQLCipher
  database does **not** have the master key.
- An attacker who compromises the running process has full
  access to decrypted data in memory. Defence-in-depth measures
  (zeroize-on-drop, scope-bound keys) shrink the exposure window
  but do not eliminate it.
- The cryptographic forgetting guarantee (key destruction) is
  honoured by the substrate; if the host filesystem retains old
  snapshots beneath the SQLCipher layer, that is a host-OS
  issue outside the substrate's control.

The substrate adopts a **harvest-now-decrypt-later** posture:
new key exchanges run a hybrid X25519 + ML-KEM-768 (Kyber)
construction so future quantum adversaries cannot recover
session secrets from harvested ciphertext. Signatures use
ML-DSA-65 (Dilithium); SPHINCS+ is reserved for archival
co-signing.

## Known security limitations

The following are honest gaps; the project is pre-1.0 and they
are tracked openly:

1. **No live connector traffic.** The connector implementations
   are fixture parsers — OAuth2 transport, webhook subscription,
   and incremental delta sync are contract-only at this stage.
2. **Host shells are out of scope.** Mobile and desktop UI
   shells live in sibling repositories and are not audited by
   this policy.

## Third-party audit

The project has not yet undergone an independent security audit.
The planned audit scope covers:

- `crates/crypto/` — hybrid KEM combiner (X25519 + ML-KEM-768),
  HKDF key derivation, XChaCha20-Poly1305 AEAD usage, zeroize
  discipline, ML-DSA-65 provenance signatures, SPHINCS+
  co-signing, and the `KeyStorage` trait surface that bounds
  hardware-backed key material (Keychain / Keystore / DPAPI /
  TEE) so the audit's threat model can include the host-shell
  storage boundary without re-deriving it from scratch.
- `crates/evidence_store/` — cryptographic forgetting via DEK
  destruction, FTS5 plaintext purge on tombstone, ring-buffer
  eviction, schema migrations (v1 → current).
- `crates/permission_service/` — Zanzibar-style reachability
  check, including the secondary-index path used on every
  permission lookup, and the audit-log integration that records
  every grant / revoke.

Candidate audit firms: NCC Group, Trail of Bits, Cure53. Audit
artefacts (engagement letter, scoping memo, reports, remediation
log) will be published alongside the corresponding release in
this repository once an engagement begins.

### Key storage

The substrate consumes a 32-byte master key as opaque bytes — see
[`crypto::MasterKey`](crates/crypto/src/kdf.rs). The *storage* of
that key is host-specific and must be hardware-backed wherever
the platform supports it:

| Platform        | Backing store                                              |
| --------------- | ---------------------------------------------------------- |
| iOS / macOS     | Keychain (`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`)  |
| Android         | Keystore / StrongBox (Pixel 6+)                            |
| Windows         | DPAPI; TPM via `NCryptOpenStorageProvider` on Win 11+      |
| Linux desktop   | `libsecret` (SecretService) where available                |
| Server / TEE    | Nitro / SEV-SNP sealed memory                              |

Host shells register an implementation of
[`crypto::KeyStorage`](crates/crypto/src/key_storage.rs) and the
matching FFI callback
[`ffi::KeyStorageResolver`](crates/ffi/src/key_storage.rs) at
startup. The substrate currently still receives the master key
through the FFI `open_store` call — the resolver registration is
a forward-compatibility plumbing hook for the migration that
removes the raw-bytes parameter from the public surface.

## Supported versions

The project is pre-1.0 and does not yet have a stable release.
All security fixes target the `main` branch.
