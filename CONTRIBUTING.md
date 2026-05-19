# Contributing to Knowledge

Thank you for considering a contribution to the Knowledge substrate.

## Getting Started

### Prerequisites

- **Rust** — the workspace's `rust-version` field specifies the MSRV.
  Install via [rustup](https://rustup.rs/).
- **cargo-deny** and **cargo-audit** — used by CI for license and
  advisory checks. Install with `cargo install cargo-deny cargo-audit`.

### Building

```bash
cargo build          # default features
cargo build --release
```

### Running Tests

```bash
cargo test --all               # default features
cargo test --all --all-features  # includes test-support mocks
```

### Linting

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
```

## Pull Request Process

1. **Branch from `main`.** Use a descriptive branch name
   (e.g. `fix/body-store-forgetting`, `feat/fts-escape-helper`).
2. **Keep commits focused.** One logical change per commit; rebase
   before merging if the history is noisy.
3. **Run the full CI locally** before opening the PR:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all --all-features
   cargo audit
   cargo deny check
   ```
4. **Update documentation** when you change behaviour:
   - If you add or change a public FFI function, update the README's
     "What is partially wired" list.
5. **Do not commit secrets**, `.env` files, or credentials.

## Code Style

- Follow the existing conventions in each file.
- Prefer minimal, focused edits over large refactors.
- Use `#[deny(missing_docs)]` — every public API must have doc
  comments.
- Gate test-only types behind `cfg(any(test, feature = "test-support"))`
  and document them in the per-crate Notes column of MODULE_STATUS.md.

## Security

If you discover a vulnerability, please follow the responsible
disclosure process in [`SECURITY.md`](./SECURITY.md) rather than
opening a public issue.
