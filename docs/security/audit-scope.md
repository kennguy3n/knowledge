# Third-party security audit — scope

This document defines the formal scope for an external security audit of
the Knowledge substrate. It is the authoritative statement of what is
in scope, what is out of scope, and what deliverables we expect. Auditor
onboarding (how to build, where things live) is in
[audit-guide.md](audit-guide.md); the threat model the audit validates
is in [threat-model.md](threat-model.md).

## Audit objective

Validate that the substrate's encryption-at-rest, cryptographic
forgetting, authorization, and key-handling actually deliver the
guarantees stated in the [threat model](threat-model.md) — and identify
any deviations, weaknesses, or implementation defects.

## In scope

| Area | Crate / surface | What to examine |
|---|---|---|
| Cryptographic primitives | `crates/crypto/src/` | AEAD (`aead.rs`, XChaCha20-Poly1305), HKDF derivation (`kdf.rs`), hybrid KEM (`hybrid_kem.rs`, X25519 + ML-KEM-768), ML-KEM backend (`kem.rs`), provenance signing (`provenance.rs`, `signer_backend.rs` ML-DSA-65), SPHINCS+ backup signer (`sphincs.rs`), MLS group keying (`mls.rs`), hybrid policy enforcement (`hybrid_enforcement.rs`), BLAKE3 hashing (`hash.rs`). |
| Cryptographic forgetting | `crates/crypto/src/forgetting.rs`, `crates/evidence_store/` | DEK destruction, epoch rotation, irrecoverability of forgotten scopes, tombstoning. |
| Encryption at rest | `crates/evidence_store/` | SQLCipher page-key derivation, per-scope DEK wrapping, body sealing (inline + body table), master-key rotation (`store.rs::rotate_master_key`). |
| Authorization (Zanzibar) | `crates/permission_service/` | Relation-tuple model, permission checks (`check.rs`), namespace config, encrypted tuple persistence (`persist.rs`), tuple re-encryption on rotation. |
| FFI key handling | `crates/ffi/` | Key material crossing the FFI boundary: `generate_keypair`, `encrypt`/`decrypt`, keypair endpoints; zeroization; error mapping that must not leak plaintext or key bytes. |
| Sync transport + MLS keying | `crates/sync_engine/`, `crates/sync_relay/`, `crates/crypto/src/mls.rs` | Wire-format delta/op-log handling (`delta.rs`, `op_log.rs`), CRDT merge integrity (`crdt.rs`), per-scope XChaCha20-Poly1305 AEAD sealing on the client transport (`transport.rs`), untrusted-relay tenant isolation (`crates/sync_relay/`), and hybrid-KEM session-secret establishment on the MLS group-keying path (`mls.rs`). |
| Key storage contract | `crates/crypto/src/key_storage.rs`, `docs/security/key-management.md` | The `KeyStorageResolver` trust contract and per-platform secure-element integration patterns. |

## Out of scope

- **Host shells / desktop integration** — the Electron/Tauri host
  application, renderer process, and OS-level packaging. The
  [renderer hardening checklist](electron-hardening.md) documents these
  separately; they are the host author's responsibility.
- **Admin web UI** (`admin/`) — the operator SPA is not part of the
  substrate's trust boundary.
- **Connectors** (`crates/connectors/`, connector framework) — external
  data-source adapters. Their *projection into the permission graph* is
  in scope (an audit concern), but the third-party services they talk to
  are not.
- **Deployment infrastructure** — Docker/Helm/Terraform manifests in
  `deploy/`, beyond confirming that credential handling and the
  documented key-rotation procedure are sound.
- **Physical / supply-chain attacks** on the build pipeline — covered by
  [supply-chain.md](supply-chain.md) and [dependency-policy.md](dependency-policy.md);
  not part of this code audit.

## Explicit non-goals (carried from the threat model)

The audit should treat the following as accepted limitations, not
findings (see [threat-model.md](threat-model.md#explicit-non-goals-known-limitations)):

- A compromised running process (data is decrypted in memory).
- Host-OS filesystem snapshot retention beneath the SQLCipher layer.
- Side channels in underlying cryptographic libraries.
- A malicious host application embedding the substrate.

## Expected deliverables

1. **Findings report** — one entry per issue using
   [finding-template.md](finding-template.md): description, severity,
   CWE, affected code, reproduction steps, recommended fix.
2. **Severity ratings** — CVSS v3.1 base score plus a qualitative
   rating (Critical / High / Medium / Low / Informational) for each
   finding, with rationale.
3. **Remediation timeline** — recommended priority and a suggested
   remediation window per severity (e.g. Critical: 7 days; High: 30
   days; Medium: 90 days; Low/Informational: best-effort).
4. **Attestation summary** — a short statement of what was reviewed,
   the methodology (manual review, fuzzing, property testing), and
   residual risk.

## Methodology expectations

- Manual source review of the in-scope crates.
- Exercising the existing security test suites and property tests (see
  [audit-guide.md](audit-guide.md)).
- Optionally extending the `cargo-fuzz` harnesses under
  `crates/crypto/fuzz/` and `crates/evidence_store/fuzz/`.
- Review of trust-boundary inputs: the FFI surface, the substrate HTTP
  surface (loopback), and the sync-engine wire protocol.
```
