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
   this policy. However, explicit host-shell key handling
   guidance — including per-platform code examples (iOS, Android,
   macOS, Windows) and a threat model for master-key leakage — is
   now provided in [`docs/HOST_KEY_HANDLING.md`](docs/HOST_KEY_HANDLING.md).

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

### Audit preparation — property-based and adversarial tests

To reduce the surface area an auditor must verify manually, the
following test suites have been added:

**`crates/crypto/tests/proptest_audit.rs`** — property-based tests
(via `proptest`) exercising:

- Hybrid KEM round-trip: `hybrid_keypair` → `hybrid_kem_encap` →
  `hybrid_kem_decap` yields matching shared secrets for every
  generated keypair.
- Distinct-recipient isolation: encapsulating to two different
  public keys produces different shared secrets.
- ML-DSA-65 signature round-trip: `sign` → `verify` succeeds
  for arbitrary `ProvenanceBundle` inputs; tampered bundles fail.
- SPHINCS+-SHAKE-128f-simple signature round-trip: same
  sign/verify/tamper discipline as ML-DSA-65.
- AEAD boundary inputs: empty plaintext, large plaintext (up to
  64 KiB), wrong-key rejection, tampered-ciphertext rejection,
  and full round-trip with arbitrary key/nonce/plaintext/AAD.
- `ZeroizeOnDrop` structural verification: `HybridSecretKey`
  derives `Zeroize` with `#[zeroize(drop)]`.

**`crates/permission_service/tests/adversarial_tests.rs`** —
adversarial tests exercising:

- Privilege escalation: indirect userset-rewrite paths cannot
  grant relations above what is explicitly assigned; orthogonal
  relations (`Synthesizer`, `Proposer`) do not cross-contaminate
  with the inheritance chain.
- Cycle detection: self-loops, 2-node cycles, and 3-node cycles
  terminate without hanging; cycles that *do* contain a valid
  grant still return `true`.
- Performance: deep chains (500 hops), wide fan-outs (1 000
  tuples), and combined deep+wide graphs (50 × 10) complete
  within bounded time without stack overflow.

These test suites run in CI (`cargo test --all --all-features`)
and are designed to be re-run by an auditor with `proptest`'s
seed-replay capability for full reproducibility.

### Host key handling

Per-platform guidance for securely storing and passing the master
key across the FFI boundary is documented in
[`docs/HOST_KEY_HANDLING.md`](docs/HOST_KEY_HANDLING.md). It
covers iOS Keychain, Android Keystore, macOS Secure Enclave,
Windows DPAPI + TPM 2.0, anti-patterns, and the first-run
provisioning flow.

### Random number generation

All cryptographic randomness in the substrate is sourced from the
operating system's secure RNG (`getrandom(2)` on Linux,
`getentropy(3)` on macOS, `BCryptGenRandom` on Windows, the kernel
CSPRNG on iOS / Android). Concretely, two classes of bytes are
generated:

| Generated bytes | Source | Sites |
| --------------- | ------ | ----- |
| Long-lived keys (master keys, scope DEKs, ML-KEM/X25519 key pairs, ML-DSA signing keys) | OS RNG | `crypto/{kem,hybrid_kem,kdf}.rs`, `evidence_store::store::random_dek` |
| Per-encryption AEAD nonces (XChaCha20-Poly1305) | OS RNG | every `random_nonce()` in `audit_service`, `concept_graph`, `evidence_store`, `ffi`, `permission_service`, `sync_engine`, `synthesis_pipeline`, `tenant_service` |

The OS RNG is reached through two import surfaces in the workspace,
both backed by the same `getrandom`-family syscall:

* Workspace crates that consume the `rand 0.10` API import
  `rand::rngs::SysRng` (the rand-0.10 rename of the older `OsRng`).
  `SysRng` impls `rand::TryRng` — the fallible RNG trait — and
  every callsite uses `.try_fill_bytes(...).expect("OS RNG failure")`.
* PQ-crypto callsites (`x25519-dalek 2`, `ml-kem 0.2`) consume the
  `rand_core 0.6` trait surface, which still exposes `OsRng` under
  the `rand_core` namespace. Those callsites (`crypto/{kem,hybrid_kem}.rs`)
  keep importing `rand_core::OsRng` because the PQ-crypto crates'
  trait bounds are pinned to `rand_core 0.6` and cannot consume a
  rand-0.10 `SysRng`. The two surfaces are kept side-by-side
  intentionally; see the `[workspace.dependencies]` comment in the
  workspace `Cargo.toml` for the full rationale.

Userspace CSPRNGs (e.g. `rand::ThreadRng`, a ChaCha12 generator
periodically reseeded from the OS pool) are also cryptographically
suitable for nonce generation, but the substrate deliberately
chooses the OS RNG at every production callsite for three reasons:

1. **Uniform audit story.** A future independent audit (planned per
   the "Third-party audit" section above) can verify "every
   cryptographic byte comes from the OS RNG" by `grep`-ing the
   workspace once for `SysRng` / `rand_core::OsRng`, rather than
   reasoning per-callsite about whether `ThreadRng`'s reseed cadence
   is adequate for that particular nonce counter / collision budget.
2. **No userspace state to protect.** The OS RNG carries no
   per-process state that needs to be defended against
   fork-without-exec, memory-disclosure side channels, or
   uninitialised-thread-local races.
3. **Negligible cost on the hot path.** The hottest production
   site (per-row evidence inserts in `evidence_store::store`) is
   bottlenecked by SQLCipher AES-256 + disk I/O, so the
   sub-microsecond `getrandom` syscall per row is unmeasurable
   against the millisecond-scale write.

`#[cfg(test)]` and integration-test helpers
(`fresh_key()` / `fresh_nonce()` in `synthesis_pipeline/tests/`,
`publish::tests::fresh_key`) use `rand::rng()` (`ThreadRng`) for
convenience — it impls the infallible `rand::Rng` trait (the
rand-0.10 rename of `RngCore`), so test code doesn't have to
thread the fallible `try_fill_bytes(...).expect(...)` shape
through helpers. This is intentional and does not affect the
production posture — the test-only helpers are not reachable
from any FFI surface and the substrate's own production code
never imports them.

Under the hood, rand 0.10 keeps the OS RNG as a fallible-only
backend: `SysRng` impls `TryRng` (formerly `TryRngCore`), so
every production callsite must reach for `try_fill_bytes(...)`
and handle a hypothetical `Result` failure. The substrate panics
via `.expect("OS RNG failure")` rather than propagating the
error, because a substrate that cannot draw OS entropy cannot
continue safely. The `Result` would have to be propagated to
the FFI boundary without any sensible recovery path on the host
side anyway — there is no "retry later" for a missing entropy
pool.

For anyone auditing the migration history: prior to the rand-0.10
bump the same posture was expressed via the
`OsRng.unwrap_err().fill_bytes(...)` adapter (rand 0.9, which had
renamed `OsRng` to a fallible-only type). The current
`SysRng.try_fill_bytes(...).expect(...)` form is semantically
identical — both panic on OS RNG failure — but is one less
adapter layer and surfaces the panic message `"OS RNG failure"`
rather than the rand-internal `UnwrapErr` panic format.

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
startup. Two cold-boot entry points are supported:

* [`ffi::open_store`](crates/ffi/src/runtime.rs) takes the
  32-byte master key as a 64-char hex string directly. Hosts
  that hold the key in-process (e.g. a desktop with a
  passphrase-derived key) can use this path; the master key
  is zeroized when [`ffi::close_store`] tears the runtime
  down.

* [`ffi::open_store_with_resolver`](crates/ffi/src/runtime.rs)
  takes a `key_id` opaque to the substrate and looks up the
  hex via the host-registered
  [`ffi::KeyStorageResolver::load_key`]. The resolver is
  stashed on the returned runtime so subsequent operations
  (key rotation, future migration paths) reach the same
  backing store without a second
  [`ffi::set_key_storage_resolver`] call. **This is the path
  hardware-backed hosts must use** — the master key never
  enters the host's address space as a long-lived plaintext
  string; the resolver pulls it from Keychain / Keystore /
  DPAPI / TEE on demand, the substrate consumes it, and it is
  zeroized on `close_store`.

The resolver registration is therefore no longer a
forward-compatibility plumbing hook — it is the substrate-side
consumer that hardware-backed hosts hit on every cold boot, and
the `open_store_with_resolver_total` metric counter exposes how
many cold boots went through the resolver-driven path vs the
direct-hex path.

## Supported versions

The project is pre-1.0 and does not yet have a stable release.
All security fixes target the `main` branch.
