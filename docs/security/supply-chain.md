# Supply-Chain Security

This document describes how the workspace controls its dependency
supply chain: the `cargo-deny` policy, the direct dependencies and why
each is present, the CI gates that enforce the policy on every commit,
and the `CODEOWNERS` review requirement for security-critical crates.

See also: [`SECURITY.md`](../../SECURITY.md) and
[`compliance.md`](../operator/compliance.md) (SOC 2 CC8/CC9, change
management).

---

## `deny.toml` policy

The policy lives in [`deny.toml`](../../deny.toml) at the workspace root
and is enforced by `cargo deny check` (see CI gates below). It uses
cargo-deny's modern `version = 2` schema.

### Advisories — no known vulnerabilities, no yanked crates

```toml
[advisories]
version = 2
yanked = "deny"
ignore = []
```

- Any crate in `Cargo.lock` with a matching RustSec advisory fails the
  build (the `version = 2` default).
- **Yanked** releases are treated as a hard failure, not a warning —
  a yank almost always signals a known-bad release.
- `ignore = []` is intentionally empty: the moment a new advisory
  matches a shipped crate, CI fails. Exceptions must be added as
  explicit, version-pinned entries and reviewed.

### Licenses — permissive allow-list, no copyleft surprises

```toml
[licenses]
version = 2
allow = [
    "MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC",
    "MPL-2.0", "Unicode-3.0", "Unicode-DFS-2016", "Zlib",
    "CDLA-Permissive-2.0",
]
```

Any license **outside** this allow-list fails the build (the
`version = 2` default), so a transitive dependency under **GPL/AGPL or
any other strong-copyleft license cannot enter the tree** without an
explicit, reviewed policy change.

Two entries carry deliberate rationale (documented inline in
`deny.toml`):

- **`MPL-2.0`** — file-level copyleft only. The UniFFI crate family
  (`uniffi`, `uniffi_bindgen`, `uniffi_core`, …) is MPL-2.0 and is a
  core dependency of the FFI substrate (Swift/Kotlin bindings).
  Linking does not taint the workspace.
- **`CDLA-Permissive-2.0`** — the Community Data License Agreement
  used by Mozilla's trusted-CA bundle in `webpki-roots`, pulled in
  transitively via `hyper-rustls → reqwest` when the `http-client`
  feature is active. No copyleft/share-alike clause.

### Bans & sources

```toml
[bans]
multiple-versions = "warn"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

- **`multiple-versions = "warn"`** surfaces duplicate-version drift
  (e.g. two majors of the same crate) without failing the build. For
  the security-critical crates the duplicate-free expectation is
  enforced by review (see CODEOWNERS) rather than a hard ban, so a
  benign transitive duplicate elsewhere does not block unrelated work.
- **`unknown-registry`/`unknown-git = "deny"`** — every dependency
  must come from crates.io (or an explicitly allow-listed source). A
  crate pulled from an arbitrary git URL or private registry fails the
  build.

---

## Direct dependencies

Direct (first-party-declared) dependencies are centralised in the
root `Cargo.toml` under `[workspace.dependencies]` so every crate
shares one version and feature set. Licenses below are the SPDX
expressions resolved by `cargo deny`; every one resolves into the
allow-list above. The machine-readable source of truth is the
CycloneDX SBOM (see below).

### Cryptography

| Crate | License | Purpose |
| ----- | ------- | ------- |
| `blake3` | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | Content hashing (resolves under Apache-2.0). |
| `chacha20poly1305` | Apache-2.0 OR MIT | XChaCha20-Poly1305 AEAD for evidence/ring-buffer/DEK sealing. |
| `hkdf`, `hmac`, `sha2` | MIT OR Apache-2.0 | HKDF-SHA256 key derivation and the hybrid-KEM combiner. |
| `x25519-dalek` | BSD-3-Clause | Classical half of the hybrid KEM. |
| `ml-kem` | Apache-2.0 OR MIT | ML-KEM-768 (post-quantum) half of the hybrid KEM. |
| `ml-dsa` | Apache-2.0 OR MIT | ML-DSA-65 provenance signatures. |
| `pqcrypto-sphincsplus`, `pqcrypto-traits` | MIT OR Apache-2.0 | SPHINCS+ archival co-signing. |
| `zeroize` | Apache-2.0 OR MIT | Wipe-on-drop for secret key material. |
| `rand`, `rand_core` | MIT OR Apache-2.0 | OS-RNG access (`SysRng` / `rand_core::OsRng`). |

### Storage & data

| Crate | License | Purpose |
| ----- | ------- | ------- |
| `rusqlite` | MIT | SQLCipher-backed encrypted store (bundled SQLCipher + vendored OpenSSL), FTS5, blobs. |
| `chrono` | MIT OR Apache-2.0 | Timestamps for decay TTLs, tombstones, audit entries. |
| `uuid` | Apache-2.0 OR MIT | Scope/evidence identifiers (v4/v5). |
| `serde`, `serde_json` | MIT OR Apache-2.0 | Serialisation for export plane, proposals, config. |
| `whatlang`, `unicode-normalization` | MIT / MIT OR Apache-2.0 | Language detection + normalisation for the multilingual lexicon. |

### Runtime, FFI & networking

| Crate | License | Purpose |
| ----- | ------- | ------- |
| `uniffi` | MPL-2.0 | Swift/Kotlin binding generation for the FFI surface. |
| `tokio`, `async-trait` | MIT / MIT OR Apache-2.0 | Async runtime for the service crates. |
| `reqwest` | MIT OR Apache-2.0 | HTTP client (rustls-TLS) behind the optional `http-client` feature. |
| `axum`, `tower`, `hyper`, `hyper-util` | MIT | HTTP server scaffolding for the service crates. |
| `tracing`, `tracing-subscriber` | MIT | Structured diagnostics. |
| `thiserror` | MIT OR Apache-2.0 | Error enums across crates. |
| `libc` | MIT OR Apache-2.0 | Low-level platform glue. |

### Dev / test only

| Crate | License | Purpose |
| ----- | ------- | ------- |
| `tempfile` | MIT OR Apache-2.0 | Throwaway databases in integration tests. |
| `proptest` | MIT OR Apache-2.0 | Property-based crypto/permission tests. |
| `criterion` | Apache-2.0 OR MIT | Benchmark harness (`benches/`). |

---

## CI gates

All gates run on every push / pull request via
[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml).

### `cargo-audit` (job `audit`)

```yaml
- run: cargo install cargo-audit --locked
- run: cargo audit
```

Scans `Cargo.lock` against the RustSec Advisory Database and fails on
any known vulnerability.

### `cargo-deny` (job `deny`)

```yaml
- run: cargo install cargo-deny --locked
- run: cargo deny check
```

Enforces the full `deny.toml` policy above: advisories, license
allow-list, bans, and source allow-list.

### SBOM (job `sbom`)

```yaml
- run: cargo install cargo-cyclonedx --locked   # skipped on cache hit
- run: cargo cyclonedx --all --format json
# collects crates/**/*.cdx.json into sbom/ and uploads them
```

Generates a **CycloneDX** Software Bill of Materials — one
`<crate>.cdx.json` per workspace member — and uploads them as the
`knowledge-sbom-cyclonedx` build artifact. Every commit therefore has
an auditable, machine-readable dependency manifest attached to its CI
run, which is the source of truth for the license/version data
summarised in this document.

---

## CODEOWNERS enforcement

[`CODEOWNERS`](../../.github/CODEOWNERS) requires explicit review from
`@kennguy3n` on every change to the security-critical surfaces. The
file deliberately has **no catch-all** entry, so the codeowner
contract never silently expands to crates the maintainers have not
opted into:

| Path | Why it is gated |
| ---- | --------------- |
| `/crates/crypto/` | Hybrid KEM, HKDF, AEAD, zeroize discipline, signatures. |
| `/crates/ffi/`, `/crates/napi/` | The public contract against host platforms (Swift/Kotlin/Node). |
| `/crates/evidence_store/` | Encrypted storage, FTS5, cryptographic forgetting, migrations. |
| `/SECURITY.md` | Threat model, audit scope, disclosure process. |

Combined with the advisory/license/SBOM gates, this means a change
that touches cryptography, the FFI boundary, or data-at-rest cannot
merge without (a) a green supply-chain gate and (b) review by the
security codeowner.
