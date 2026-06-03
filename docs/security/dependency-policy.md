# Dependency Policy

This document is the reference for Knowledge's Rust toolchain
requirements, version-pinning rationale, automated dependency
management, and supply-chain auditing. Consuming products should
reference it when deciding which Rust version to target and when
evaluating supply-chain compatibility.

For the broader supply-chain posture (SBOM, advisory gates, license
policy), see [supply-chain.md](supply-chain.md).

## Minimum Supported Rust Version (MSRV)

| Property | Value |
|---|---|
| Workspace MSRV | **1.85** |
| Declared in | [`Cargo.toml`](../../Cargo.toml) `rust-version = "1.85"` |
| Enforced by | CI job `MSRV (1.85.0)` in [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) via `dtolnay/rust-toolchain@1.85.0` |
| Reason | `ml-dsa 0.1.0` (post-quantum signatures) declares `edition = "2024"`, which requires Rust 1.85+. |

**Exception:** the N-API addon crate (`crates/napi`) carries its own
`rust-version = "1.88"` because `napi-rs 3.x` requires `rustc >= 1.88`.
The MSRV CI gate excludes this crate; the addon is built separately by
the host shell's toolchain.

### What this means for consumers

- Your product's Rust toolchain must be **>= 1.85** to compile the
  workspace (excluding the `napi` crate).
- If you consume the N-API surface, your Electron / Node build
  toolchain must be **>= 1.88**.
- The MSRV is bumped conservatively. Each bump follows the checklist
  below and is gated on the corresponding CI job update.

## Version-pinning rationale

Several workspace dependencies are deliberately pinned behind their
latest published line because newer releases raise their own MSRV above
the workspace floor. The pins are documented inline next to each
`version = "…"` declaration in the relevant `Cargo.toml`; those
comments are authoritative if the two ever drift.

| Dependency | Pinned line | Blocked by | Unblocks at MSRV |
|---|---|---|---|
| `rusqlite` | `0.36.x` | `libsqlite3-sys 0.36` uses `cfg_select!` (Rust 1.94) | 1.94 |
| `ort` | `=2.0.0-rc.10` | Upstream build break + MSRV 1.88 | 1.88 + upstream fix |
| `criterion` | `0.7.x` | `criterion 0.8` requires Rust 1.86 | 1.86 |
| `aws-nitro-enclaves-nsm-api` | `0.4.x` | `0.5` requires Rust 1.92 | 1.92 |

> **For consumers:** these pins are internal to the workspace. They do
> not constrain your product's dependency tree unless you share
> workspace members via path deps.

### `rusqlite = "0.36"`

`rusqlite 0.37`+ depends on `libsqlite3-sys 0.36`+, which uses the
`cfg_select!` macro that did not stabilise until Rust 1.94. The bundled
SQLite version (via `bundled-sqlcipher-vendored-openssl`) is `3.46.1`,
shipped inside the SQLCipher 4.6.1 vendored fork. A canary test
(`crates/evidence_store/tests/bundled_sqlite_canary.rs`) asserts on the
literal `sqlite_version()` / `sqlite_source_id()` so any future bump
that moves the bundled SQLite forward forces a deliberate ack and a
re-run of the FTS5 tokeniser and cross-lingual recall tests.

### `ort = "=2.0.0-rc.10"` (feature-gated)

Exact-version pin behind the `onnx-runtime` feature. Double-gated:
`rc.11`+ both introduced an upstream build break (a Xilinx Vitis AI
execution-provider field referencing a non-existent `OrtApi` member)
and raised the MSRV to `1.88`.

### `criterion = "0.7"`

`criterion 0.8`+ declares `rust-version = "1.86"`. The `0.5` → `0.6`
transition deprecated `criterion::black_box`; the workspace bench files
now import `black_box` from `std::hint::` directly.

### `aws-nitro-enclaves-nsm-api = "0.4"`

`0.5`+ declares `rust-version = "1.92"`. Used only by the optional
`nitro-tee` feature; production builds without that feature do not pull
this crate.

## Text-handling dependencies (no pins)

These crates handle human-language text in the multilingual paths. All
track their latest published line; no MSRV gating applies.

- **`whatlang = "0.18"`** — pure-Rust trigram language detector used by
  `observation_engine::detect_language` and the embedding-routing
  classifier. It does not ship classifiers for Tibetan or Lao, so the
  FTS5 CJK / bigram routing keys on **script presence** (Unicode
  code-point ranges) rather than the language tag, so detection refusal
  does not drop those scripts out of the recall lane.
- **`unicode-normalization = "0.1"`** — NFC normalisation, applied once
  at extractor time before the lexicon lookup. `no_std`-compatible.
- **`tokenizers = "0.23"`** — HuggingFace tokenisation for the optional
  ONNX embedding lane (feature-gated behind `evidence_store/onnx-runtime`),
  built with the pure-Rust `fancy-regex` backend.

## Dependabot

Automated updates are managed via
[`.github/dependabot.yml`](../../.github/dependabot.yml):

- **Cargo** — weekly, 10 concurrent PRs. Ignore rules mirror the
  version pins above (`rusqlite >=0.37`, `ort >=2.0.0-rc.11`,
  `criterion >=0.8`, `aws-nitro-enclaves-nsm-api >=0.5`); patch and
  intermediate bumps within pinned lines still surface. Security
  updates dispatch immediately.
- **GitHub Actions** — weekly, 5 concurrent PRs, keeping workflow
  actions current.
- PRs are auto-labelled `dependencies` + (`rust` | `github-actions`).

## Supply-chain auditing

CI runs two supply-chain checks on every push:

- **`cargo audit`** — checks the RustSec advisory database for known
  vulnerabilities in resolved dependencies.
- **`cargo deny check`** — enforces the license allowlist, bans
  duplicate crate versions where configured, and validates advisory
  compliance.

Consuming products should run the same checks against their own
`Cargo.lock` to surface any transitive advisories introduced by the
substrate.

## When the workspace MSRV is bumped

Walk this checklist in order:

1. **`1.86`** — unlocks `criterion 0.8`. No multilingual surface
   changes.
2. **`1.88`** — unlocks `ort 2.0.0-rc.11`+ (still gated on the upstream
   `vitis` field fix) and brings `napi-rs 3.x` into the workspace MSRV
   gate's surface.
3. **`1.91`** — drops the workspace's `async-trait` dep (regular
   `async fn` in traits).
4. **`1.94`** — unlocks `rusqlite 0.37`+ / `libsqlite3-sys 0.36`+. The
   bundled SQLite version advances, refreshing the `unicode61` /
   `trigram` tokenisers; verify the multilingual lexical-lane tests
   still pass and re-run the cross-lingual recall benchmark to detect
   tokenisation regressions.

## Further reading

- [supply-chain.md](supply-chain.md) — SBOM, advisory gates, license
  policy.
- [../../CONTRIBUTING.md](../../CONTRIBUTING.md) — build, test, lint, PR
  flow.
- [../guides/](../guides/) — step-by-step consumer integration.
