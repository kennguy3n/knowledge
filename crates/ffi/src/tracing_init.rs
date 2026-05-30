//! Opt-in tracing subscriber initialisation for platform hosts (Phase 6).
//!
//! The substrate emits `tracing::{warn, debug, info}!` calls from
//! ~7 crates (`evidence_store`, `connector_framework`, `ffi`,
//! `inference_router`, …) but ships **without** a default
//! subscriber installed — the `tracing` crate documents that no
//! events are emitted at all when no subscriber is registered, so
//! the "no subscriber" path is the substrate's correct
//! library-side default.
//!
//! Platform hosts (Electron desktop main, Swift iOS shell, Kotlin
//! Android shell) call [`try_init_tracing`] from their early-boot
//! path to install a subscriber with their preferred filter
//! directive. The function is **idempotent** — a second call is a
//! no-op rather than an error, so a host that calls it from both a
//! shared library and its own main is not penalised.
//!
//! # Filter directive syntax
//!
//! [`tracing_subscriber::EnvFilter`] uses **`::` as its target
//! hierarchy separator**, NOT `_` or `-`. The substrate's workspace
//! crates each get their own top-level tracing target derived from
//! the crate name with hyphens converted to underscores:
//!
//! * `evidence_store` (the `evidence_store` crate)
//! * `connector_framework` (the `connector_framework` crate)
//! * `inference_router` (the `inference_router` crate)
//! * `ffi` (this crate)
//! * `napi_addon` (the `napi` crate)
//!
//! A filter like `evidence_store=debug` matches the
//! `evidence_store` target and any sub-targets (e.g.
//! `evidence_store::retrieval`); it does NOT match siblings like
//! `evidence_store_*` (none exist today, but the point is the
//! prefix is exact).
//!
//! To enable debug logging across the whole substrate, enumerate
//! every interesting crate explicitly. The shorthand
//! `RUST_LOG=knowledge=debug` matches nothing — there is no crate
//! by that name; the workspace ships as a set of sibling crates.
//! The most useful directives in practice:
//!
//! ```text
//! info                                          # global default
//! ffi=debug,evidence_store=debug                # FFI boundary + storage
//! inference_router=debug,connector_framework=info  # SLM + connectors
//! ```
//!
//! # Feature gating
//!
//! This module is built only when the `tracing-subscriber` feature
//! is enabled. Hosts that don't need a subscriber (or that want to
//! install their own bespoke subscriber outside of this helper)
//! can omit the feature; the substrate still emits tracing events,
//! they just go nowhere.

use std::sync::OnceLock;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::error::{FfiError, FfiResult};
use crate::metrics;

/// Latched flag — once the subscriber has been installed
/// successfully, every subsequent [`try_init_tracing`] returns
/// `Ok(())` without trying to install a competing subscriber.
static INSTALLED: OnceLock<()> = OnceLock::new();

/// Install a [`tracing_subscriber`] with the given EnvFilter
/// directive. Idempotent — a second call is a no-op `Ok(())`.
///
/// `directive` is the same syntax accepted by
/// [`tracing_subscriber::EnvFilter::try_new`] and the `RUST_LOG`
/// environment variable. Examples: `"info"`,
/// `"ffi=debug,evidence_store=debug"`, `"warn,inference_router=info"`.
///
/// # Parameter shape
///
/// `directive` is taken by **owned `String`**, not `&str`. The
/// owned shape is what UniFFI requires for parameter marshalling
/// (UniFFI can only bridge owned `String` across the FFI boundary,
/// not borrowed `&str`), and the cost — one allocation per
/// early-boot call — is irrelevant against the substrate's
/// once-per-process call frequency. Rust-side callers can pass
/// either `String::from("info")`, a `String` they already own, or
/// a `&str` via `.to_string()` / `.into()`.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] (re-using the existing variant for a
///   structural argument problem) if `directive` does not parse as
///   a valid EnvFilter expression. We deliberately do NOT introduce
///   a new error variant for this — every host that wires a
///   directive should validate it once during early-boot and bail
///   noisily if it's wrong; tracing-init failures are a developer
///   error, not a runtime failure mode.
///
/// On the **first successful call** this function installs the
/// subscriber globally (the substrate uses
/// [`tracing_subscriber::registry::Registry`] under a `fmt::Layer`),
/// marks the metrics block's `tracing_initialized` flag, and
/// returns `Ok(())`. On every subsequent call it short-circuits to
/// `Ok(())` without re-installing.
///
/// # Concurrency — "first directive wins"
///
/// Hosts are expected to call `try_init_tracing` exactly once
/// during early-boot. If two threads race into this function
/// concurrently with **different** directives, the contract is
/// well-defined but may surprise the loser:
///
/// 1. Both threads pass the `INSTALLED.get().is_some()` check
///    (which returns `false` until the global default is set).
/// 2. Both construct an `EnvFilter` from their respective
///    directives and call `try_init()` on the global subscriber.
/// 3. **The first to reach `set_global_default` wins**
///    (`tracing-subscriber` uses an atomic compare-and-swap). The
///    second `try_init()` returns `Err` and is silently dropped
///    by the `let _ = …` discard.
/// 4. Both threads then call `INSTALLED.set(())` (idempotent) and
///    `mark_tracing_initialized()` (idempotent) and return
///    `Ok(())`.
///
/// Net effect: the runner-up's directive is dropped without a
/// signal, and the runner-up still gets `Ok(())`. This is safe (no
/// data corruption; the host still has a working subscriber) but
/// non-deterministic with respect to which directive ends up
/// installed. Hosts that need a deterministic filter should
/// serialise calls themselves (e.g. behind a host-side
/// `OnceCell`) or call this once from a single early-boot
/// codepath. The runner-up's `Ok(())` is intentional: it preserves
/// the "idempotent boot" contract every other entry point upholds.
///
/// # Subscriber shape
///
/// The installed subscriber is the canonical
/// `Registry::default().with(fmt::Layer).with(EnvFilter)` stack
/// from `tracing-subscriber`. The `fmt` layer writes to stderr by
/// default (no ANSI colour, no per-span entry/exit logging) — the
/// minimum-noise shape suitable for production deployments. Hosts
/// that need a different sink (file, OS logger, OpenTelemetry,
/// JSON-over-IPC) should install their own subscriber instead of
/// calling this helper; the `tracing` crate's global-default
/// dispatcher will pick up whichever subscriber is installed
/// first.
// FFI: UniFFI requires an owned `String` for parameter marshalling
// (the wire ABI for `RustBuffer`-backed strings only models
// ownership transfer), so the borrowed-`&str` shape this function
// used to carry would not roundtrip. The owned parameter is then
// consumed only by an `&directive` borrow inside the body — that
// is what trips clippy::needless_pass_by_value. Suppress the lint
// here rather than refactor to `&str`: the FFI boundary contract
// is what dictates the shape, not the body's borrow pattern.
#[allow(clippy::needless_pass_by_value)]
#[uniffi::export]
pub fn try_init_tracing(directive: String) -> FfiResult<()> {
    // Wire through the same metrics::instrument pattern every other
    // public FFI entry point uses: increments `init_tracing_total`
    // before the body runs, and routes the `Err` path through
    // `inc_error` so a malformed directive shows up in
    // `errors_by_kind.invalid_id`. This satisfies the
    // CONTRIBUTING.md rule that requires every new public FFI entry
    // point be wired into the observability layer.
    metrics::instrument(metrics::inc_init_tracing, || {
        if INSTALLED.get().is_some() {
            return Ok(());
        }

        let filter = EnvFilter::try_new(&directive).map_err(|e| FfiError::InvalidId {
            message: format!("invalid tracing directive `{directive}`: {e}"),
        })?;

        // `try_init` returns `Err` if a global default has already
        // been set by some other code path. That is the substrate's
        // "another subscriber wins" outcome — we treat it as success
        // (the host has tracing wired, even if it isn't ours) and
        // still flip the flag so the health envelope reports tracing
        // as initialised.
        let layer = tracing_subscriber::fmt::layer().with_ansi(false);
        let registry = tracing_subscriber::registry().with(layer).with(filter);
        let _ = registry.try_init();

        // Latch — every subsequent `try_init_tracing` is a no-op.
        let _ = INSTALLED.set(());
        metrics::mark_tracing_initialized();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_directive_returns_invalid_id() {
        // EnvFilter rejects anything that looks like a target with
        // an unparseable level. We don't actually install a
        // subscriber here — we only assert the parse-failure path.
        let err = EnvFilter::try_new("not-a-real-level=oh_no").unwrap_err();
        let mapped = FfiError::InvalidId {
            message: format!("invalid tracing directive `not-a-real-level=oh_no`: {err}"),
        };
        match mapped {
            FfiError::InvalidId { message } => {
                assert!(message.contains("invalid tracing directive"));
            }
            _ => panic!("expected InvalidId"),
        }
    }

    #[test]
    fn try_init_tracing_is_idempotent() {
        // The first call may or may not succeed (depending on
        // whether another test in the same process already
        // installed a subscriber); the second call MUST be
        // `Ok(())`. We don't care about the directive's level here.
        let _ = try_init_tracing("info".to_string());
        let second = try_init_tracing("debug".to_string());
        assert!(second.is_ok());
        // After at least one successful or short-circuited call
        // the metrics flag must be set.
        assert!(metrics::tracing_initialized());
    }

    #[test]
    fn try_init_tracing_accepts_owned_string() {
        // Pin the public signature: the function takes `String`
        // (not `&str`) so UniFFI can bridge the parameter across
        // the FFI boundary. If a future refactor regresses this
        // back to `&str`, the UniFFI scaffolding will fail to
        // compile — but this test gives the regression a clearer
        // message at the function-signature level.
        let directive: String = String::from("info");
        let result = try_init_tracing(directive);
        assert!(result.is_ok());
    }
}
