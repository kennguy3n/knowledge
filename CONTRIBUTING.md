# Contributing

Thank you for considering a contribution to Knowledge. This guide
covers the code of conduct, the build environment, the quality gates
the workspace enforces, where to start reading the code, and the
pull-request flow.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
By participating you agree to uphold it. Report unacceptable behavior
to **ken@uney.com**.

## Developer Certificate of Origin (DCO)

Contributions are accepted under the
[Developer Certificate of Origin](https://developercertificate.org/).
You certify that you wrote the patch (or otherwise have the right to
submit it under the project's dual MIT/Apache-2.0 license) by adding a
`Signed-off-by` line to each commit:

```
Signed-off-by: Jane Developer <jane@example.com>
```

Add it automatically with `git commit -s`. The name and email must
match a real identity. There is no separate CLA.

## Where to start reading

New to the codebase? A good reading order:

1. **[docs/technical/architecture.md](docs/technical/architecture.md)** —
   the component map, service topology, and data flow. Start here.
2. **[docs/technical/design.md](docs/technical/design.md)** — the
   memory model, planes, and decay rationale.
3. **`crates/evidence_store`** — encrypted storage, FTS5, and hybrid
   retrieval; the foundation everything else builds on.
4. **`crates/observation_engine`** and **`crates/memory_manager`** —
   how raw input becomes structured, decaying memory.

Issues labeled **`good first issue`** are scoped to be approachable
without deep familiarity with the whole workspace — they touch a single
crate and have clear acceptance criteria.

## Prerequisites

- **Rust** — a stable toolchain on or after the MSRV declared in
  `Cargo.toml` (`rust-version`). Install via
  [rustup](https://rustup.rs/) and add `clippy` and `rustfmt` with
  `rustup component add clippy rustfmt`.
- **C toolchain** for the bundled SQLCipher + OpenSSL sources built by
  `rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature. On
  Debian / Ubuntu: `sudo apt install build-essential`.
- **Go 1.23+** — for the API gateway in `server/`.
- **`cargo-deny` and `cargo-audit`** — used in CI for license and
  advisory checks. Install with `cargo install cargo-deny cargo-audit`.

## Build, test, and lint

### Rust workspace

```bash
cargo build --all-targets --all-features
cargo test --all                    # default features
cargo test --all --all-features     # includes test-support helpers
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo audit
cargo deny check
```

The MSRV gate is enforced in CI; you can reproduce it locally with
`cargo +<msrv> build --all-features` using the version from
`Cargo.toml`.

### Go gateway

```bash
cd server
gofmt -l .
go vet ./...
golangci-lint run ./...
go test -race -count=1 ./...
```

### Running tests across platforms

Most tests are platform-independent and run on the host toolchain. The
host-binding crates have additional matrices:

- **iOS / Android (`ffi`, UniFFI)** — exercised via the cross-compile
  matrix in CI. Locally you can run the crate's own tests with
  `cargo test -p ffi --all-features`.
- **Electron / Node (`napi`)** — PRs run a single `ubuntu-latest` leg
  of `napi-build` by default. The full four-platform matrix (linux x64,
  macOS arm64/x64, windows x64) runs on every push to `main` and on
  demand via `workflow_dispatch`. To get the full matrix on a PR before
  merge, apply the `ci:full` label; remove it to switch back.

### Benchmarks

The Criterion suite (~30 min) lives in `crates/benchmarks`:

```bash
cargo bench -p benchmarks
```

## Pull-request workflow

1. **Branch off `main`** with a descriptive name
   (`fix/body-store-forgetting`, `feat/fts-escape-helper`).
2. **Sign off every commit** (`git commit -s`) per the DCO above.
3. **Keep commits focused** — one logical change per commit.
4. **Run the full quality gate locally** (fmt + clippy + test + audit +
   deny, plus the Go checks if you touched `server/`) before opening
   the PR.
5. **Update [docs/technical/architecture.md](docs/technical/architecture.md)
   §"Cross-platform FFI"** when you add, remove, or change a public FFI
   function so the wired-surface documentation stays accurate.
6. **Fill out the PR template** and include a changelog entry for any
   public-API change (see below).
7. **Do not commit secrets**, `.env` files, fixture credentials, or
   local override configs.

### Issue and PR templates

Use the templates under `.github/`: **Bug Report** and
**Feature Request** for issues, and the **pull request template** for
PRs. Filling them out completely speeds up triage and review.

## Changelog discipline

Any pull request that adds, changes, deprecates, removes, or fixes a
**public API** item (types, functions, constants, feature flags, or FFI
entry points marked `// STABLE` or `// UNSTABLE` in
`crates/*/src/lib.rs`) must include a changelog entry in the PR
description. Use the following template:

```markdown
### Changelog

#### Added
- `crate_name::NewType` — brief description.

#### Changed
- `crate_name::existing_fn` — what changed and why.

#### Deprecated
- `crate_name::OldType` — use `NewType` instead.

#### Removed
- `crate_name::removed_fn` — reason for removal.

#### Fixed
- `crate_name::buggy_fn` — what was wrong.

#### Security
- `crate_name::crypto_fn` — security-relevant change.
```

Omit categories that have no entries. The categories follow
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) conventions.

Items marked `// UNSTABLE` or `#[doc(hidden)]` may change without a
changelog entry, but a courtesy note in the PR description is
appreciated. Items marked `// STABLE` **must** have a changelog entry
for any change.

## Code style

- Follow the existing conventions in each file; match the surrounding
  code's tone, indentation, and naming.
- Prefer minimal, focused edits over large refactors.
- Public APIs are linted with `#[deny(missing_docs)]`. Every public
  item must have a doc comment that explains its contract, not just
  restates its name.
- Gate test-only types behind `cfg(any(test, feature = "test-support"))`
  and document them in the crate's top-level doc comment.
- New `unsafe` requires a `SAFETY:` comment justifying every invariant
  the call relies on.
- New public FFI entry points (in `crates/ffi/src/lib.rs` and the
  matching `crates/napi/src/{lib,bindings}.rs` wrappers) must be wired
  into the observability layer: wrap the body with
  `metrics::instrument(metrics::inc_<name>, || { … })` after adding the
  `<name>_total` counter to `crates/ffi/src/metrics.rs`, and probe the
  relevant subsystem from `crates/ffi/src/health.rs` if the call
  exercises a subsystem not already covered there. The `health_check`
  envelope is the single inspectable surface hosts use to introspect
  the substrate; do not bypass it.
- The `tracing-subscriber` feature on `ffi` / `napi_addon` is opt-in.
  The substrate emits `tracing` events via the facade regardless of the
  feature, but no subscriber is installed by default. Prefer
  `tracing::{info,warn,error,debug}!` over `eprintln!` / `println!` so
  the directive-filtered subscriber controls output.

## Release process

Releases follow [Semantic Versioning](https://semver.org/). The release
flow is:

1. Land all changes for the release on `main` with green CI.
2. Update [CHANGELOG.md](CHANGELOG.md) with the new version, date, and
   categorized entries.
3. Tag the release (`vX.Y.Z`). The tag-triggered release workflow
   (`.github/workflows/release.yml`) runs the full test suite and
   publishes a GitHub release with notes.

## Security

If you discover a vulnerability, do not open a public issue. Follow the
responsible disclosure process in [SECURITY.md](SECURITY.md).
