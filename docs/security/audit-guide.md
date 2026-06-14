# Auditor onboarding guide

This guide gets a third-party auditor productive on the Knowledge
substrate quickly: how to build and exercise the code, where the
security-relevant code and tests live, and which trust boundaries to
focus on. For the formal scope and deliverables see
[audit-scope.md](audit-scope.md); for the guarantees under review see
[threat-model.md](threat-model.md).

## 1. Build and run the test suite

The substrate is a Rust workspace. Toolchain: stable Rust (MSRV
**1.88.0**). SQLCipher and its OpenSSL are vendored and compiled from
source by `rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature, so
no system SQLite/OpenSSL packages are required — only a C toolchain and
`pkg-config`.

```sh
# Build everything.
cargo build --all-targets --all-features

# Run the full test suite.
cargo test --all --all-features

# Lint / format gates the project holds itself to.
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Note: the `demo` crate enables `crypto`'s `test-support` feature, which
`compile_error!`s in release builds. CI builds the workspace with
`--workspace --exclude demo` for release profiles; `cargo test --all`
in the default (dev) profile is fine.

## 2. Where the crypto primitives live

All primitives are in `crates/crypto/src/`:

| File | Primitive |
|---|---|
| `aead.rs` | XChaCha20-Poly1305 AEAD (body/payload encryption). |
| `kdf.rs` | HKDF-SHA256 derivation (SQLCipher master key, scope keys, context strings). |
| `hybrid_kem.rs` | Hybrid X25519 + ML-KEM-768 KEM (harvest-now-decrypt-later defense). |
| `kem.rs` | ML-KEM-768 (Kyber) with a swappable backend trait. |
| `provenance.rs` | PROV bundle data model for observations/synthesis. |
| `signer_backend.rs` | ML-DSA-65 signing backend (synthesis provenance). |
| `sphincs.rs` | SPHINCS+-SHAKE-128f backup signer (archival co-signing). |
| `mls.rs` | MLS-style group keying for sync. |
| `hybrid_enforcement.rs` | Policy enforcement that every key exchange is hybrid. |
| `hash.rs` | BLAKE3 content hashing. |
| `forgetting.rs` | Cryptographic forgetting + epoch rotation. |
| `key_storage.rs` | `KeyStorageResolver` hardware-backed master-key contract. |
| `attestation.rs` | Attestation reports bound to synthesizer keys. |

## 3. Where the security tests live

Start with these; they encode the properties the audit should
challenge.

| Test file | Focus |
|---|---|
| `crates/crypto/tests/proptest_audit.rs` | Property tests over the crypto primitives (round-trips, tamper-rejection, invariants). |
| `crates/crypto/tests/security_hardening.rs` | Hardening assertions (e.g. rejecting malformed inputs, enforcing hybrid use). |
| `crates/crypto/tests/api.rs` | Public-API behavior of the crypto crate. |
| `crates/crypto/tests/provenance.rs` | Provenance signing/verification. |
| `crates/permission_service/tests/adversarial_tests.rs` | Adversarial authorization scenarios (privilege escalation attempts, tuple tampering). |
| `crates/permission_service/tests/permission_tests.rs` | Functional Zanzibar permission checks. |
| `crates/evidence_store/tests/recovery_hardening.rs` | Recovery/corruption hardening of the encrypted store. |
| `crates/evidence_store/tests/privacy_redteam.rs` | Red-team privacy scenarios against the store. |
| `crates/evidence_store/tests/forgetting_fts.rs` | Forgetting interaction with the FTS index. |
| `crates/evidence_store/tests/key_rotation.rs` | Master-key rotation round-trips and old-key rejection. |
| `crates/substrate_server/tests/key_rotation_tool.rs` | End-to-end offline rotation orchestration (swap + backups). |
| `crates/sync_engine/tests/crdt_tests.rs` | CRDT merge integrity for the sync wire format. |

Run a single security suite, e.g.:

```sh
cargo test -p crypto --test proptest_audit
cargo test -p permission_service --test adversarial_tests
cargo test -p evidence_store --test recovery_hardening
```

## 4. Trust boundaries to focus on

The substrate is a library; three boundaries take untrusted or
semi-trusted input and deserve the most scrutiny.

### 4.1 FFI surface (`crates/ffi/`)

The Rust library is exposed to the host application over a synchronous
FFI surface (`ingest_message`, `query`, `get_evidence`, `encrypt`,
`decrypt`, `generate_keypair`, `forget`, `forget_scope`, …). Audit
focus: key material crossing the boundary, zeroization, and that error
mapping (`FfiError`) never leaks plaintext or key bytes. FTS query input
is sanitized via `escape_fts_query`.

### 4.2 Substrate HTTP surface (`crates/substrate_server/`)

A loopback-only (`127.0.0.1:9090`, never publicly exposed) axum service
wraps the FFI surface for the Go server tier. Routes include `/ingest`,
`/query`, `/evidence/{id}`, `/forget`, `/forget_scope`,
`/permission/{grant,revoke,check}`, `/crypto/{hybrid_keypair,signing_keypair}`,
and `/export/evaluate`. Audit focus: request validation, that the
loopback-only assumption holds, and that handlers dispatch blocking
SQLCipher work without leaking secrets into logs (the server logs only
scope ids and operation names).

### 4.3 Sync-engine wire protocol (`crates/sync_engine/`, `crates/sync_relay/`)

Deltas and op-log entries (`delta.rs`, `op_log.rs`) are exchanged
between devices and merged via CRDTs (`crdt.rs`). The client transport
(`transport.rs`) seals every delta envelope with a per-scope
XChaCha20-Poly1305 AEAD key derived from the master key and ships it
through an untrusted relay (`crates/sync_relay/`) that only ever holds
opaque ciphertext. The hybrid KEM (`crypto/src/mls.rs` + `hybrid_kem.rs`)
backs the MLS-style group-keying path; establishing the per-scope sync
key across devices over that KEM is a current limitation. Audit focus:
deserialization of untrusted wire data, AEAD sealing and AAD binding on
the transport, merge-integrity invariants, relay tenant isolation, and
that a malicious peer or relay cannot forge or replay state.

## 5. Threat-model summary and non-goals

The substrate protects user data at rest on a personal device (evidence
bodies, observations, concepts, synthesized memory) and the integrity
and provenance of synthesis outputs. Bodies are AEAD-encrypted under
per-scope DEKs inside a SQLCipher (AES-256) database; the master key
lives in the platform secure element.

Explicit non-goals (do **not** file as findings): a compromised running
process, host-OS snapshot retention beneath SQLCipher, side channels in
underlying crypto libraries, and a malicious host application. See
[threat-model.md](threat-model.md#explicit-non-goals-known-limitations)
for the full statement.

## 6. Reporting

File each issue with [finding-template.md](finding-template.md). See
[audit-scope.md](audit-scope.md#expected-deliverables) for the full set
of expected deliverables and severity conventions.
```
