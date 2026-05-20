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

## Security

If you discover a vulnerability, do not open a public issue.
Follow the responsible disclosure process in
[`SECURITY.md`](./SECURITY.md).
