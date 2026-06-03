# uniffi-bindgen

UniFFI binding generator binary for the Knowledge substrate.

## Purpose

Dedicated binary crate that runs `uniffi_bindgen_main()` with the
`cli` feature enabled. Exists as a separate crate so the workspace-
wide build does not pull the `cli` feature (and its `clap` / `syn` /
`quote` / `proc-macro2` deps) into the `ffi` crate's static library.

## Usage

```bash
cargo run -p uniffi-bindgen -- generate \
    crates/ffi/src/knowledge.udl \
    --language swift \
    --out-dir generated/swift/
```

## Notes

- This is a **binary** crate; it does not export a library.
- Per-package builds (`cargo build -p ffi`) sidestep feature
  unification, so `ffi` sees a `cli`-feature-free `uniffi`.

## Links

- [ffi](../ffi/) — The UniFFI surface crate.
- [docs/getting-started/for-developers.md](../../docs/getting-started/for-developers.md) — Platform build instructions.
