# Contributing

Thank you for considering a contribution to Knowledge. This guide
covers the build environment, the quality gates the workspace
enforces, and the pull-request flow.

## Prerequisites

- **Rust** — a stable toolchain on or after the MSRV declared in
  `Cargo.toml` (`rust-version`). Install via
  [rustup](https://rustup.rs/) and add `clippy` and `rustfmt`
  with `rustup component add clippy rustfmt`.
- **C toolchain** for the bundled SQLCipher + OpenSSL sources
  built by `rusqlite`'s `bundled-sqlcipher-vendored-openssl`
  feature. On Debian / Ubuntu: `sudo apt install build-essential`.
- **`cargo-deny` and `cargo-audit`** — used in CI for license
  and advisory checks. Install with
  `cargo install cargo-deny cargo-audit`.

## Build, test, and lint

```bash
cargo build --all-targets
cargo build --release
cargo test --all                    # default features
cargo test --all --all-features     # includes test-support helpers
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
```

CI also runs `cargo audit` and `cargo deny check`. Run them
locally before opening a pull request:

```bash
cargo audit
cargo deny check
```

## Pull-request workflow

1. **Branch off `main`** with a descriptive name
   (`fix/body-store-forgetting`, `feat/fts-escape-helper`).
2. **Keep commits focused.** One logical change per commit; rebase
   the branch before requesting review if the history is noisy.
3. **Run the full quality gate locally** (fmt + clippy + test +
   audit + deny) before opening the pull request.
4. **Update the README's status section** when you add, remove,
   or change a public FFI function so the "wired" / "contract-only"
   lists stay accurate.
5. **Do not commit secrets**, `.env` files, fixture credentials,
   or local override configs.
6. **Request the full N-API cross-platform matrix when you need
   it.** PRs run a single ubuntu-latest leg of `napi-build` by
   default to keep the iteration loop fast. The full four-platform
   matrix (linux x64, macOS arm64/x64, windows x64) runs
   automatically on every push to `main` and on demand from the
   Actions UI (`workflow_dispatch`). To get the full matrix on a
   PR before merge, apply the `ci:full` label — the existing
   in-flight smoke run is replaced by the full matrix on the next
   `pull_request` event. Remove the label to switch back.

## Changelog discipline

Any pull request that adds, changes, deprecates, removes, or fixes a
**public API** item (types, functions, constants, feature flags, or FFI
entry points marked `// STABLE` or `// UNSTABLE` in `crates/*/src/lib.rs`)
must include a changelog entry in the PR description. Use the following
template:

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

- Follow the existing conventions in each file; match the
  surrounding code's tone, indentation, and naming.
- Prefer minimal, focused edits over large refactors.
- Public APIs are linted with `#[deny(missing_docs)]`. Every
  public item must have a doc comment that explains its
  contract, not just restates its name.
- Gate test-only types behind
  `cfg(any(test, feature = "test-support"))` and document them
  in the crate's top-level doc comment.
- New `unsafe` requires a `SAFETY:` comment justifying every
  invariant the call relies on.
- New public FFI entry points (in `crates/ffi/src/lib.rs` and
  the matching `crates/napi/src/{lib,bindings}.rs` wrappers)
  must be wired into the observability layer: wrap the body
  with `metrics::instrument(metrics::inc_<name>, || { … })`
  after adding the `<name>_total` counter to
  `crates/ffi/src/metrics.rs`, and probe the relevant
  subsystem from `crates/ffi/src/health.rs` if the call
  exercises a subsystem not already covered there. The
  `health_check` envelope is the single inspectable surface
  hosts use to introspect the substrate; do not bypass it.
- The `tracing-subscriber` feature on `ffi` /  `napi_addon`
  is opt-in. The substrate emits `tracing` events via the
  facade regardless of the feature, but no subscriber is
  installed by default. If you add a host-side log call,
  prefer `tracing::{info,warn,error,debug}!` over `eprintln!`
  / `println!` so the directive-filtered subscriber controls
  output.

## Security

If you discover a vulnerability, do not open a public issue.
Follow the responsible disclosure process in
[`SECURITY.md`](./SECURITY.md).
