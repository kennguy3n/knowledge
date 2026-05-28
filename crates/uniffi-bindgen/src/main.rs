//! `uniffi-bindgen` binary — drives the `uniffi` CLI for the
//! `crates/ffi` library.
//!
//! Re-exporting `uniffi::uniffi_bindgen_main` (gated behind the
//! `cli` feature on this crate's *local* `uniffi` dependency) lets
//! the Android / iOS build scripts invoke
//! `cargo run -p uniffi-bindgen -- generate \
//!     --library target/release/libffi.so \
//!     --language kotlin \
//!     --out-dir crates/ffi/android \
//!     --no-format`
//! against the version of `uniffi` pinned to match the workspace
//! `Cargo.toml`, which is the same version that produced the
//! scaffolding embedded in the compiled `cdylib` / `staticlib`.
//! Decoupling the bindgen binary from the workspace's uniffi
//! version would re-introduce the "scaffolding version mismatch"
//! failure mode upstream UniFFI explicitly warns about — the
//! bindgen CLI would parse a `.so` produced by a different
//! scaffolding version and emit corrupted FFI vtables.
//!
//! # Why a separate crate
//!
//! Was originally a `[[bin]]` target inside `crates/ffi/Cargo.toml`,
//! but Cargo features are per-crate (not per-target), so enabling
//! `cli` on the workspace `uniffi` dependency compiled `clap` /
//! `syn` / `quote` / `proc-macro2` into the dependency graph for
//! ALL `ffi` build outputs — including the `staticlib` (iOS
//! `.xcframework`) and `cdylib` (Android `jniLibs/`). Splitting the
//! binary into this dedicated workspace crate keeps the `ffi`
//! library's dependency graph feature-lean: only this binary's
//! dependency graph carries the CLI-only deps. The mobile build
//! scripts call `cargo run -p uniffi-bindgen`, which builds only
//! this crate's dependencies — never the `staticlib` or `cdylib`.
//!
//! Pattern mirrored from
//! `uneycom/kchat-rust-sdk/crates/kchat-mls-uniffi/uniffi-bindgen.rs`
//! (which lives in a sibling crate next to the `staticlib`/`cdylib`
//! producer for the same reason).
fn main() {
    uniffi::uniffi_bindgen_main();
}
