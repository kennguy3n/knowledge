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

## Supported versions

The project is pre-1.0 and does not yet have a stable release.
All security fixes target the `main` branch.
