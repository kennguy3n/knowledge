# Dependency Policy

This document is the consumer-facing reference for the Knowledge
substrate's Rust toolchain requirements, version-pinning rationale,
and automated dependency management. Consuming products should
reference this when deciding which Rust version to target and when
evaluating supply-chain compatibility.

For the full internal dependency audit (individual crate pin
rationales, text-handling deps, MSRV bump checklist), see
[docs/DEPENDENCIES.md](./DEPENDENCIES.md).

---

## Minimum Supported Rust Version (MSRV)

| Property | Value |
|---|---|
| Workspace MSRV | **1.85** |
| Declared in | [`Cargo.toml`](../Cargo.toml) `rust-version = "1.85"` |
| Enforced by | CI job `MSRV (1.85.0)` in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) via `dtolnay/rust-toolchain@1.85.0` |
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
- The MSRV is bumped conservatively. Each bump follows a
  [documented checklist](./DEPENDENCIES.md#when-the-workspace-msrv-is-bumped)
  and is gated on the corresponding CI job update.

---

## Version-pinning rationale

Several workspace dependencies are deliberately pinned behind their
latest published version because newer releases raise their own MSRV
above the workspace floor. The pins are documented inline in the
relevant `Cargo.toml` files and summarised in
[docs/DEPENDENCIES.md § Notable dependency pins](./DEPENDENCIES.md#notable-dependency-pins).

Current pins:

| Dependency | Pinned line | Blocked by | Unblocks at MSRV |
|---|---|---|---|
| `rusqlite` | `0.36.x` | `libsqlite3-sys 0.36` uses `cfg_select!` (Rust 1.94) | 1.94 |
| `ort` | `=2.0.0-rc.10` | Upstream build break + MSRV 1.88 | 1.88 + upstream fix |
| `criterion` | `0.7.x` | `criterion 0.8` requires Rust 1.86 | 1.86 |
| `aws-nitro-enclaves-nsm-api` | `0.4.x` | `0.5` requires Rust 1.92 | 1.92 |

> **For consumers:** these pins are internal to the workspace. They
> do not constrain your product's dependency tree unless you share
> workspace members via path deps. If you hit version conflicts,
> check whether the pin has been lifted in a newer commit.

---

## Dependabot configuration

Automated dependency updates are managed by GitHub Dependabot via
[`.github/dependabot.yml`](../.github/dependabot.yml).

### Cargo ecosystem

- **Schedule:** weekly.
- **Open PR limit:** 10 concurrent.
- **Ignore rules:** mirror the version pins above — Dependabot will
  not open PRs for the gated version ranges. Patch and intermediate
  bumps within pinned lines still surface.
- **Security updates:** dispatched immediately regardless of the
  weekly schedule.

### GitHub Actions ecosystem

- **Schedule:** weekly.
- **Open PR limit:** 5 concurrent.
- Ensures workflow actions (`actions/checkout`, `dtolnay/rust-toolchain`,
  `actions/cache`, etc.) stay current.

### Labels

Dependabot PRs are auto-labelled:
- `dependencies` + `rust` for Cargo bumps.
- `dependencies` + `github-actions` for Actions bumps.

---

## Supply-chain auditing

CI runs two supply-chain checks on every push:

- **`cargo audit`** — checks the `RustSec` advisory database for
  known vulnerabilities in resolved dependencies.
- **`cargo deny check`** — enforces license allowlists, bans
  duplicate crate versions (where configured), and validates
  advisory compliance.

Consuming products should run the same checks against their own
`Cargo.lock` to surface any transitive advisories introduced by the
substrate.

---

## Further reading

- [docs/DEPENDENCIES.md](./DEPENDENCIES.md) — full internal audit.
- [CONTRIBUTING.md](../CONTRIBUTING.md) — build, test, lint, PR flow.
- [docs/INTEGRATION_GUIDE.md](./INTEGRATION_GUIDE.md) — step-by-step
  consumer integration.
