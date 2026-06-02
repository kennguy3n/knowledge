# Supply Chain Security

This document describes the Knowledge substrate's dependency management
policy, audit gates, and SBOM generation pipeline.

---

## 1. `deny.toml` Policy

The workspace uses [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny)
with the configuration in [`deny.toml`](/deny.toml) (schema v2):

### Advisories (`[advisories]`)

- **`version = 2`** — modern strict handling.
- **`yanked = "deny"`** — any yanked crate in the lockfile fails CI.
- **`ignore = []`** — no advisory exceptions. CI fails the moment a
  new advisory matches a transitive dependency.

### Licenses (`[licenses]`)

Accepted SPDX identifiers:

| License | Notes |
|---|---|
| `MIT` | Permissive |
| `Apache-2.0` | Permissive |
| `BSD-2-Clause` | Permissive |
| `BSD-3-Clause` | Permissive |
| `ISC` | Permissive |
| `MPL-2.0` | File-level copyleft; used by UniFFI. Linking does not taint the workspace. |
| `Unicode-3.0` | Unicode data files |
| `Unicode-DFS-2016` | Unicode data files |
| `Zlib` | Permissive |
| `CDLA-Permissive-2.0` | Used by `webpki-roots` for the Mozilla CA bundle. No copyleft. |

Any license outside this allow-list fails the build.

### Bans (`[bans]`)

- **`multiple-versions = "warn"`** — warns on duplicate crate versions
  in the dependency graph but does not fail CI. This is intentional
  because `rand_core 0.6` / `rand 0.10` and `digest 0.10` / `digest 0.11`
  coexist by design (see workspace `Cargo.toml` comments).

### Sources (`[sources]`)

- **`unknown-registry = "deny"`** — only crates.io is accepted.
- **`unknown-git = "deny"`** — no git dependencies from unknown sources.

---

## 2. Direct Dependencies

Extracted from the workspace `Cargo.toml` `[workspace.dependencies]`:

| Crate | Version | License | Purpose |
|---|---|---|---|
| `thiserror` | 2 | MIT / Apache-2.0 | Derive macro for `std::error::Error` |
| `uuid` | 1 | MIT / Apache-2.0 | UUID v4/v5 generation and serde |
| `blake3` | 1 | MIT / Apache-2.0 | BLAKE3 content hashing |
| `chacha20poly1305` | 0.10 | MIT / Apache-2.0 | XChaCha20-Poly1305 AEAD |
| `hkdf` | 0.13 | MIT / Apache-2.0 | HKDF-SHA256 key derivation |
| `hmac` | 0.13 | MIT / Apache-2.0 | HMAC-SHA256 (provenance signing) |
| `sha2` | 0.11 | MIT / Apache-2.0 | SHA-256 (HKDF, HMAC) |
| `zeroize` | 1 | MIT / Apache-2.0 | Secure memory zeroing with derive |
| `rand_core` | 0.6 | MIT / Apache-2.0 | OS RNG trait for PQ-crypto crates |
| `rand` | 0.10 | MIT / Apache-2.0 | Random number generation |
| `x25519-dalek` | 2 | BSD-3-Clause | X25519 Diffie-Hellman |
| `ml-kem` | 0.2 | MIT / Apache-2.0 | ML-KEM-768 post-quantum KEM |
| `ml-dsa` | 0.1.0 | MIT / Apache-2.0 | ML-DSA-65 post-quantum signatures |
| `pqcrypto-sphincsplus` | 0.7 | MIT / Apache-2.0 | SPHINCS+-SHAKE-128f-simple signatures |
| `pqcrypto-traits` | 0.3 | MIT / Apache-2.0 | Shared PQ signature trait surface |
| `rusqlite` | 0.36 | MIT | SQLCipher-backed storage |
| `chrono` | 0.4 | MIT / Apache-2.0 | Date/time handling |
| `libc` | 0.2 | MIT / Apache-2.0 | System FFI types |
| `serde` | 1 | MIT / Apache-2.0 | Serialization framework |
| `serde_json` | 1 | MIT / Apache-2.0 | JSON serialization |
| `whatlang` | 0.18 | MIT | Language detection |
| `unicode-normalization` | 0.1 | MIT / Apache-2.0 | Unicode NFC normalization |
| `reqwest` | 0.12 | MIT / Apache-2.0 | HTTP client (gated behind features) |
| `tokio` | 1 | MIT | Async runtime (gated behind features) |
| `async-trait` | 0.1 | MIT / Apache-2.0 | Async fn in traits |
| `axum` | 0.8 | MIT | HTTP framework for webhook receiver |
| `tower` | 0.5 | MIT | Middleware ecosystem |
| `hyper` | 1 | MIT | HTTP/1 implementation |
| `hyper-util` | 0.1 | MIT | Hyper utilities |
| `tracing` | 0.1 | MIT | Structured logging facade |
| `tracing-subscriber` | 0.3 | MIT | Tracing subscriber (optional) |
| `uniffi` | 0.31 | MPL-2.0 | UniFFI bindings for iOS/Android |

**Dev-only dependencies** (not shipped in production):

| Crate | Version | Purpose |
|---|---|---|
| `tempfile` | 3 | Temporary directories for tests |
| `proptest` | 1 | Property-based testing |
| `criterion` | 0.7 | Benchmarking harness |

---

## 3. CI Security Gates

The following CI jobs enforce supply-chain security on every PR and push:

### `cargo audit` (job: `audit`)

```yaml
- run: cargo install cargo-audit --locked
- run: cargo audit
```

Checks all transitive dependencies against the
[RustSec Advisory Database](https://rustsec.org/). Fails on any known
vulnerability.

### `cargo deny` (job: `deny`)

```yaml
- run: cargo install cargo-deny --locked
- run: cargo deny check
```

Enforces the `deny.toml` policy: license allow-list, advisory checks,
yanked-crate rejection, source restrictions.

### SBOM generation (job: `sbom`)

```yaml
- run: cargo install cargo-cyclonedx --locked
- run: cargo cyclonedx --all
```

Produces CycloneDX JSON SBOMs for every workspace crate. Uploaded as
a CI artefact (`sbom/*.cdx.json`).

### `unsafe_code` enforcement (job: `unsafe-code-guard`)

Two-pronged guard:

1. **Source scan:** `rg` scans every tracked `*.rs` file outside the
   napi allowlist for `#[allow(unsafe_code)]` attributes (including
   `cfg_attr`-gated forms). New relaxations fail CI.
2. **Cargo.toml pin:** Asserts that the workspace root declares
   `unsafe_code = "deny"` and `crates/crypto/Cargo.toml` declares
   `unsafe_code = "forbid"`. Downgrades fail CI.

---

## 4. CODEOWNERS Enforcement

The `.github/CODEOWNERS` file requires explicit review for changes to
security-sensitive crates:

| Path | Owner |
|---|---|
| `/crates/crypto/` | `@kennguy3n` |
| `/crates/ffi/` | `@kennguy3n` |
| `/crates/napi/` | `@kennguy3n` |
| `/crates/evidence_store/` | `@kennguy3n` |
| `/SECURITY.md` | `@kennguy3n` |

Combined with branch protection rules requiring at least one approving
review from a CODEOWNER, this ensures that changes to the cryptography
stack, the FFI surface, the durable storage layer, and the security
policy document cannot land without explicit review from the designated
maintainer.
