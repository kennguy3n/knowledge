//! Build script for the `ffi` crate.
//!
//! Emits the `http_client_wired` cfg, which gates the reqwest-backed
//! llama.cpp loopback adapter wiring in
//! [`runtime::build_inference_router`]. The on-device synthesis path
//! (`trigger_synthesis`) dispatches `SynthSummary` through the
//! `InferenceRouter`; for that to reach a real `llama-server` sidecar
//! the `HttpLlamaServerClient` transport (and its `reqwest` + `tokio`
//! dependency chain) has to be compiled in.
//!
//! That transport must be present in server / desktop / hybrid builds
//! so a `docker compose up` deployment gets synthesis out of the box,
//! but it must stay OUT of the mobile UniFFI `staticlib` / `cdylib`
//! artefacts (iOS / Android), which the workspace deliberately keeps
//! free of `tokio` / `reqwest` (see the root `Cargo.toml` notes on the
//! `tokio` / `reqwest` workspace deps). Mobile shells drive synthesis
//! through the MLX adapter instead.
//!
//! `http_client_wired` is therefore on when EITHER:
//!   * the `http-client` Cargo feature is explicitly enabled (server
//!     CLI builds, `substrate_server`, `--all-features` CI runs), OR
//!   * the target OS is not a mobile platform (iOS / Android).
//!
//! This predicate is kept in exact lock-step with the target-gated
//! `inference_router` dependency in `Cargo.toml`, which enables
//! `inference_router/http-client` (hence `HttpLlamaServerClient`)
//! under the identical condition. The two MUST agree, otherwise the
//! cfg'd source would reference a type that was not compiled.
fn main() {
    // Register the custom cfg so `-D warnings` / `unexpected_cfgs`
    // does not flag `#[cfg(http_client_wired)]` in the source.
    println!("cargo::rustc-check-cfg=cfg(http_client_wired)");

    let feature_on = std::env::var_os("CARGO_FEATURE_HTTP_CLIENT").is_some();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let is_mobile = matches!(target_os.as_str(), "ios" | "android");

    if feature_on || !is_mobile {
        println!("cargo::rustc-cfg=http_client_wired");
    }
}
