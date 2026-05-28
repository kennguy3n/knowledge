//! `uniffi-bindgen` binary — drives the `uniffi` CLI for the
//! `crates/ffi` library.
//!
//! Re-exporting `uniffi::uniffi_bindgen_main` (gated behind the
//! `cli` feature on the `uniffi` workspace dependency) lets the
//! Android / iOS build scripts invoke
//! `cargo run -p ffi --bin uniffi-bindgen -- generate \
//!     --library target/release/libknowledge_ffi.so \
//!     --language kotlin \
//!     --out-dir crates/ffi/android \
//!     --no-format`
//! against the version of `uniffi` pinned by the workspace
//! `Cargo.toml`, which is the same version that produced the
//! scaffolding embedded in the compiled `cdylib` /
//! `staticlib`. Decoupling the bindgen binary from the
//! workspace's uniffi version would re-introduce the
//! "scaffolding version mismatch" failure mode upstream UniFFI
//! explicitly warns about — the bindgen CLI would parse a
//! `.so` produced by a different scaffolding version and emit
//! corrupted FFI vtables.
//!
//! Pattern mirrored from
//! `uneycom/kchat-rust-sdk/crates/kchat-mls-uniffi/uniffi-bindgen.rs`.
fn main() {
    uniffi::uniffi_bindgen_main();
}
