//! Substrate-wide metrics counters and gauges (Phase 6).
//!
//! Every public [`crate::*`] entry point increments the counters in this
//! module so platform hosts can observe the substrate's lifetime
//! activity through [`snapshot`] / [`MetricsSnapshot`]. The counters
//! live in a process-singleton [`Metrics`] struct (`OnceLock`) so the
//! substrate can increment them without threading a `&Metrics`
//! through every call site, and so `[snapshot]` can return a stable
//! view at any moment.
//!
//! # Counter semantics — "calls initiated", not "calls completed"
//!
//! [`instrument`] increments the per-function call counter BEFORE
//! running the body closure. This is deliberate: it means every
//! counter reads as "calls initiated" rather than "calls completed".
//! A call that panics out of the closure (uncaught — exceptional)
//! still increments the counter; the per-kind error counter is
//! incremented on the `Err` path via [`Result::inspect_err`], so a
//! call that completes with `Err` shows up in BOTH
//! `<name>_total` and `errors_by_kind.<kind>`. A successful call
//! shows up only in `<name>_total`; subtracting `errors_by_kind`
//! per-FFI-function isn't possible because errors aren't tagged by
//! origin function. This is by design — the per-kind error counters
//! are a separate axis ("what's failing across the substrate") from
//! the per-function call counters ("what's the substrate doing").
//!
//! # Design
//!
//! * **Lock-free on the hot path.** Every counter is an
//!   [`AtomicU64`] incremented with [`Ordering::Relaxed`]. The
//!   per-call cost is one atomic add — no allocations, no locks.
//!   `Relaxed` is sufficient because the host never makes a
//!   correctness decision on a single counter read (a slightly
//!   stale read just means the reported number is a few calls
//!   behind reality, which is acceptable for a diagnostic surface).
//!
//! * **Stable wire shape.** [`MetricsSnapshot`] is the serde-flat
//!   structure platform hosts deserialize. The field order and
//!   names are part of the FFI contract — adding a new counter must
//!   be additive (new optional field with `#[serde(default)]`).
//!
//! * **Error counters keyed by [`FfiError::kind`].** One per
//!   discriminant tag, plus a `total` sum, lets hosts plot
//!   error-rate by kind without parsing every error string.
//!
//! Uptime tracking is anchored on a `boot_unix_secs` value that is
//! captured the first time [`metrics`] is initialised — i.e. the
//! first time any counter is touched. The `init` FFI helper exposes
//! `prime_metrics` so hosts can force this stamp at known-good
//! boot points (otherwise it lands at the first ingest / query /
//! health check).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::FfiError;

/// Process-singleton metrics block. Internal — hosts call
/// [`snapshot`] to read.
#[derive(Default, Debug)]
pub(crate) struct Metrics {
    // Counters (monotone, set once never reset).
    pub(crate) ingest_total: AtomicU64,
    pub(crate) query_total: AtomicU64,
    pub(crate) synthesis_triggered_total: AtomicU64,
    pub(crate) decay_sweeps_total: AtomicU64,
    pub(crate) forgets_total: AtomicU64,
    pub(crate) forget_scopes_total: AtomicU64,
    pub(crate) get_evidence_total: AtomicU64,
    pub(crate) get_user_memory_total: AtomicU64,
    pub(crate) get_channel_memory_total: AtomicU64,
    pub(crate) list_memories_total: AtomicU64,
    pub(crate) pin_total: AtomicU64,
    pub(crate) unpin_total: AtomicU64,
    pub(crate) open_store_total: AtomicU64,
    pub(crate) close_store_total: AtomicU64,
    pub(crate) encrypt_total: AtomicU64,
    pub(crate) decrypt_total: AtomicU64,
    pub(crate) generate_keypair_total: AtomicU64,
    /// Total `escape_fts_query` calls. Pure string transform, no
    /// `Err` path — a non-zero value here is a signal of host
    /// search activity volume, useful when correlating against
    /// `query_total` to see escape-helper-to-query ratio.
    pub(crate) escape_fts_query_total: AtomicU64,
    /// Total `metrics_snapshot` calls. Pure read of the singleton
    /// counters (no `Err` path) — a non-zero value here means a host
    /// is actively polling the diagnostic surface (e.g. an Electron
    /// status panel, an iOS / Android observability tile). Useful
    /// when correlating against the host shell's polling cadence to
    /// catch a runaway-polling regression. The counter is read —
    /// and incremented — by [`snapshot`] itself, so the value seen
    /// in any given `MetricsSnapshot` always lags its own counter
    /// by exactly one (this read).
    //
    // `clippy::struct_field_names` flags `metrics_snapshot_total`
    // because the field shares the struct's `metrics_` prefix.
    // Every counter on `Metrics` follows the `<function_name>_total`
    // convention (`ingest_total`, `query_total`, `escape_fts_query_total`,
    // `health_check_total`, …) and the renaming the lint suggests
    // (`snapshot_total`) would (a) lose the parallel with the
    // UniFFI export name `metrics_snapshot`, and (b) be ambiguous
    // with the local `snapshot()` function in this same module. The
    // allow is scoped to this single field rather than the whole
    // struct so accidental shadowings on future fields still surface.
    #[allow(clippy::struct_field_names)]
    pub(crate) metrics_snapshot_total: AtomicU64,
    /// Total `health_check` calls initiated. Counted on both the
    /// bridge-only (no-handle) path and the full-probe (valid-handle)
    /// path. The `Err` path (unknown / closed handle) still
    /// increments this counter, then also feeds `errors_total` /
    /// `errors_by_kind.unavailable` via the `metrics::instrument`
    /// wrapper. Without this the substrate would have no visibility
    /// into health-probe traffic — a regression where hosts spin
    /// on `health_check` would be invisible to operators.
    pub(crate) health_check_total: AtomicU64,
    /// Total `try_init_tracing` calls (counted whether the underlying
    /// `tracing-subscriber` feature is compiled in or not — the
    /// counter exists in [`Metrics`] unconditionally so the wire
    /// shape of [`MetricsSnapshot`] does not vary by feature flag).
    /// On non-feature builds the counter stays at `0` because the
    /// only call site is in the feature-gated `tracing_init`
    /// module.
    pub(crate) init_tracing_total: AtomicU64,

    // Per-kind error counters. The set mirrors `FfiError::kind`
    // exactly so adding a new error variant is a compile error
    // here (`inc_error` won't exhaustively match without an arm).
    pub(crate) errors_unimplemented: AtomicU64,
    pub(crate) errors_invalid_id: AtomicU64,
    pub(crate) errors_not_found: AtomicU64,
    pub(crate) errors_evidence: AtomicU64,
    pub(crate) errors_memory: AtomicU64,
    pub(crate) errors_synthesis: AtomicU64,
    pub(crate) errors_crypto: AtomicU64,
    pub(crate) errors_unavailable: AtomicU64,
    pub(crate) errors_inference_failure: AtomicU64,
    /// Sum of every per-kind error counter, maintained alongside the
    /// individual counters so [`snapshot`] does not have to fan out
    /// nine reads to compute the total.
    pub(crate) errors_total: AtomicU64,

    // Gauges (mutable up-and-down).
    pub(crate) open_handles: AtomicU64,
    pub(crate) tombstone_count: AtomicU64,

    /// Unix-epoch seconds at which the metrics block was first
    /// initialised. Captured inside `metrics()`'s `OnceLock` so it
    /// has a well-defined value the first time any counter is
    /// touched.
    pub(crate) boot_unix_secs: AtomicU64,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Borrow the process-singleton metrics block, initialising it on
/// the first call. Internal — hosts should use [`snapshot`] /
/// [`prime`].
#[inline]
pub(crate) fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let m = Metrics::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        m.boot_unix_secs.store(now, Ordering::Relaxed);
        m
    })
}

/// Force the metrics block to initialise — sets `boot_unix_secs`
/// to *now* if this is the first call. Subsequent calls are
/// no-ops (the boot stamp is set exactly once per process).
///
/// Hosts call this from their early-boot path (Electron's
/// `app.whenReady`, Swift `application:didFinishLaunching`) so the
/// uptime counter is anchored at a known-good moment rather than
/// at the first `ingest_message` / `query` call. Calling this is
/// optional — the metrics block initialises lazily on any counter
/// touch — but doing so improves the accuracy of the
/// `uptime_secs` field on the health envelope.
pub fn prime() {
    let _ = metrics();
}

// ─── Counter helpers ────────────────────────────────────────────────
//
// Each public entry point in `crate::*` calls one of these
// helpers at most once per call. The `Relaxed` ordering is
// sufficient because metrics are a diagnostic surface — a host
// reader that observes an in-flight count is still observing a
// monotone snapshot.

macro_rules! counter_inc {
    ($vis:vis fn $name:ident => $field:ident) => {
        #[inline]
        $vis fn $name() {
            metrics().$field.fetch_add(1, Ordering::Relaxed);
        }
    };
}

counter_inc!(pub(crate) fn inc_ingest => ingest_total);
counter_inc!(pub(crate) fn inc_query => query_total);
counter_inc!(pub(crate) fn inc_synthesis_triggered => synthesis_triggered_total);
counter_inc!(pub(crate) fn inc_decay_sweep => decay_sweeps_total);
counter_inc!(pub(crate) fn inc_forget => forgets_total);
counter_inc!(pub(crate) fn inc_forget_scope => forget_scopes_total);
counter_inc!(pub(crate) fn inc_get_evidence => get_evidence_total);
counter_inc!(pub(crate) fn inc_get_user_memory => get_user_memory_total);
counter_inc!(pub(crate) fn inc_get_channel_memory => get_channel_memory_total);
counter_inc!(pub(crate) fn inc_list_memories => list_memories_total);
counter_inc!(pub(crate) fn inc_pin => pin_total);
counter_inc!(pub(crate) fn inc_unpin => unpin_total);
counter_inc!(pub(crate) fn inc_open_store => open_store_total);
counter_inc!(pub(crate) fn inc_close_store => close_store_total);
counter_inc!(pub(crate) fn inc_encrypt => encrypt_total);
counter_inc!(pub(crate) fn inc_decrypt => decrypt_total);
counter_inc!(pub(crate) fn inc_generate_keypair => generate_keypair_total);
counter_inc!(pub(crate) fn inc_escape_fts_query => escape_fts_query_total);
counter_inc!(pub(crate) fn inc_metrics_snapshot => metrics_snapshot_total);
counter_inc!(pub(crate) fn inc_health_check => health_check_total);
// Feature-gated to match the only call site
// (`crate::tracing_init::try_init_tracing`). The counter *field*
// in `MetricsSnapshot` stays unconditional so the wire shape does
// not drift across feature flags; the helper that increments it
// only exists when there is something that could call it.
#[cfg(feature = "tracing-subscriber")]
counter_inc!(pub(crate) fn inc_init_tracing => init_tracing_total);

/// Increment the error counter that matches `err.kind()`, and the
/// `errors_total` summary. Called from the error-mapping shims in
/// `crate::*` after each call that produced a non-`Ok` result.
///
/// Exhaustively matches every [`FfiError`] variant — adding a new
/// variant in `error.rs` without a matching counter field above is
/// a compile error, which is intentional (the metrics surface must
/// keep up with the error contract).
pub(crate) fn inc_error(err: &FfiError) {
    let m = metrics();
    let counter = match err {
        FfiError::Unimplemented { .. } => &m.errors_unimplemented,
        FfiError::InvalidId { .. } => &m.errors_invalid_id,
        FfiError::NotFound { .. } => &m.errors_not_found,
        FfiError::Evidence { .. } => &m.errors_evidence,
        FfiError::Memory { .. } => &m.errors_memory,
        FfiError::Synthesis { .. } => &m.errors_synthesis,
        FfiError::Crypto { .. } => &m.errors_crypto,
        FfiError::Unavailable { .. } => &m.errors_unavailable,
        FfiError::InferenceFailure { .. } => &m.errors_inference_failure,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    m.errors_total.fetch_add(1, Ordering::Relaxed);
}

// ─── Gauge helpers ──────────────────────────────────────────────────

/// Set the `open_handles` gauge. Called from
/// `crate::runtime::insert_runtime` and `crate::runtime::remove_runtime`
/// so the value mirrors the actual handle-registry size.
pub(crate) fn set_open_handles(n: u64) {
    metrics().open_handles.store(n, Ordering::Relaxed);
}

/// Set the `tombstone_count` gauge to the current size of the
/// destroyed-DEK tombstone set. Re-read on every `forget` /
/// `forget_scope` and on every [`snapshot`] call when a runtime
/// handle is in play.
pub(crate) fn set_tombstone_count(n: u64) {
    metrics().tombstone_count.store(n, Ordering::Relaxed);
}

// ─── Snapshot API ───────────────────────────────────────────────────

/// Public wire-flat view of every counter and gauge in the
/// process-singleton metrics block.
///
/// All fields are monotone u64 except [`Self::open_handles`] /
/// [`Self::tombstone_count`] (gauges). New fields MUST be added as
/// optional with `#[serde(default)]` to keep the wire contract
/// additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct MetricsSnapshot {
    /// Total `open_store` calls initiated. See the module docs for
    /// why the counter reads as "initiated" and not "completed".
    pub open_store_total: u64,
    /// Total `close_store` calls initiated.
    pub close_store_total: u64,
    /// Total `ingest_message` calls initiated.
    pub ingest_total: u64,
    /// Total `query` calls initiated.
    pub query_total: u64,
    /// Total `get_evidence` calls initiated.
    pub get_evidence_total: u64,
    /// Total `get_user_memory` calls initiated.
    pub get_user_memory_total: u64,
    /// Total `get_channel_memory` calls initiated.
    pub get_channel_memory_total: u64,
    /// Total `list_memories` calls initiated.
    pub list_memories_total: u64,
    /// Total `pin` calls initiated.
    pub pin_total: u64,
    /// Total `unpin` calls initiated.
    pub unpin_total: u64,
    /// Total `trigger_synthesis` calls initiated (counted before the
    /// actual dispatch, so this includes `InferenceFailure` and
    /// `Unavailable` returns).
    pub synthesis_triggered_total: u64,
    /// Total `run_decay_sweep` calls initiated.
    pub decay_sweeps_total: u64,
    /// Total `forget` calls initiated.
    pub forgets_total: u64,
    /// Total `forget_scope` calls initiated.
    pub forget_scopes_total: u64,
    /// Total `encrypt` calls initiated.
    pub encrypt_total: u64,
    /// Total `decrypt` calls initiated.
    pub decrypt_total: u64,
    /// Total `generate_keypair` calls initiated.
    pub generate_keypair_total: u64,
    /// Total `escape_fts_query` calls. Pure string transform, no
    /// error counter sibling.
    pub escape_fts_query_total: u64,
    /// Total `metrics_snapshot` calls. Pure read of the counter
    /// block, no error counter sibling. The counter is incremented
    /// by [`snapshot`] itself; the value in any one snapshot is
    /// therefore always one less than the post-call counter.
    //
    // `#[serde(default)]` per the struct-level wire-contract note
    // above. The pre-existing fields predate that rule and stay as
    // they are (changing them now would re-flow the wire contract
    // they were shipped under), but every new field added from this
    // PR onward MUST default to `0` on deserialise so an older
    // emitter's snapshot still round-trips through a newer reader.
    #[serde(default)]
    pub metrics_snapshot_total: u64,
    /// Total `health_check` calls initiated — every probe (bridge
    /// only and full) increments this, including the `Err` path for
    /// an unknown / closed handle (the `Err` path also feeds
    /// `errors_by_kind.unavailable`).
    pub health_check_total: u64,
    /// Total `try_init_tracing` calls initiated. Always present in
    /// the snapshot — when the `tracing-subscriber` feature is off,
    /// the substrate exposes no entry point that touches this
    /// counter so it stays at `0`. The field stays in the snapshot
    /// shape unconditionally to keep the wire contract stable
    /// across features.
    pub init_tracing_total: u64,
    /// Per-kind error counter snapshot.
    pub errors_by_kind: ErrorCounters,
    /// Total errors across all kinds (sum of `errors_by_kind`'s
    /// counters, maintained alongside them rather than computed
    /// at read time).
    pub errors_total: u64,
    /// Number of currently-open runtime handles. Gauge.
    pub open_handles: u64,
    /// Number of tombstones in the destroyed-DEK registry on the
    /// most-recently-observed handle. Gauge — last write wins.
    pub tombstone_count: u64,
    /// Unix-epoch seconds at which the metrics block was first
    /// initialised. Used to compute `uptime_secs` on the health
    /// envelope.
    pub boot_unix_secs: u64,
}

/// Per-kind error counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct ErrorCounters {
    /// `FfiError::Unimplemented`.
    pub unimplemented: u64,
    /// `FfiError::InvalidId`.
    pub invalid_id: u64,
    /// `FfiError::NotFound`.
    pub not_found: u64,
    /// `FfiError::Evidence`.
    pub evidence: u64,
    /// `FfiError::Memory`.
    pub memory: u64,
    /// `FfiError::Synthesis`.
    pub synthesis: u64,
    /// `FfiError::Crypto`.
    pub crypto: u64,
    /// `FfiError::Unavailable`.
    pub unavailable: u64,
    /// `FfiError::InferenceFailure`.
    pub inference_failure: u64,
}

/// Return a wire-flat snapshot of every counter and gauge. Reads
/// each `AtomicU64` with [`Ordering::Relaxed`] — see the module
/// docs for why that's sufficient.
///
/// The UniFFI export name is renamed to `metrics_snapshot` so the
/// generated Swift / Kotlin surface (`metricsSnapshot()`) matches
/// the N-API surface (`metricsSnapshot()` — see
/// `crates/ffi/src/lib.rs` where `snapshot` is re-exported as
/// `metrics_snapshot`). The bare Rust name stays `snapshot` because
/// every call site already lives inside the `metrics::` module so
/// the longer name would be redundant.
#[must_use]
#[uniffi::export(name = "metrics_snapshot")]
pub fn snapshot() -> MetricsSnapshot {
    // Pure counter read — no `FfiResult<T>` to route through
    // `metrics::instrument`. Bump the per-function call counter
    // first so the wire surface matches every other infallible
    // entry point (`escape_fts_query` follows the same pattern at
    // `crates/ffi/src/lib.rs:296`). The snapshot value of
    // `metrics_snapshot_total` therefore lags its own counter by
    // exactly one read — this is documented on the field.
    inc_metrics_snapshot();
    let m = metrics();
    MetricsSnapshot {
        open_store_total: m.open_store_total.load(Ordering::Relaxed),
        close_store_total: m.close_store_total.load(Ordering::Relaxed),
        ingest_total: m.ingest_total.load(Ordering::Relaxed),
        query_total: m.query_total.load(Ordering::Relaxed),
        get_evidence_total: m.get_evidence_total.load(Ordering::Relaxed),
        get_user_memory_total: m.get_user_memory_total.load(Ordering::Relaxed),
        get_channel_memory_total: m.get_channel_memory_total.load(Ordering::Relaxed),
        list_memories_total: m.list_memories_total.load(Ordering::Relaxed),
        pin_total: m.pin_total.load(Ordering::Relaxed),
        unpin_total: m.unpin_total.load(Ordering::Relaxed),
        synthesis_triggered_total: m.synthesis_triggered_total.load(Ordering::Relaxed),
        decay_sweeps_total: m.decay_sweeps_total.load(Ordering::Relaxed),
        forgets_total: m.forgets_total.load(Ordering::Relaxed),
        forget_scopes_total: m.forget_scopes_total.load(Ordering::Relaxed),
        encrypt_total: m.encrypt_total.load(Ordering::Relaxed),
        decrypt_total: m.decrypt_total.load(Ordering::Relaxed),
        generate_keypair_total: m.generate_keypair_total.load(Ordering::Relaxed),
        escape_fts_query_total: m.escape_fts_query_total.load(Ordering::Relaxed),
        metrics_snapshot_total: m.metrics_snapshot_total.load(Ordering::Relaxed),
        health_check_total: m.health_check_total.load(Ordering::Relaxed),
        init_tracing_total: m.init_tracing_total.load(Ordering::Relaxed),
        errors_by_kind: ErrorCounters {
            unimplemented: m.errors_unimplemented.load(Ordering::Relaxed),
            invalid_id: m.errors_invalid_id.load(Ordering::Relaxed),
            not_found: m.errors_not_found.load(Ordering::Relaxed),
            evidence: m.errors_evidence.load(Ordering::Relaxed),
            memory: m.errors_memory.load(Ordering::Relaxed),
            synthesis: m.errors_synthesis.load(Ordering::Relaxed),
            crypto: m.errors_crypto.load(Ordering::Relaxed),
            unavailable: m.errors_unavailable.load(Ordering::Relaxed),
            inference_failure: m.errors_inference_failure.load(Ordering::Relaxed),
        },
        errors_total: m.errors_total.load(Ordering::Relaxed),
        open_handles: m.open_handles.load(Ordering::Relaxed),
        tombstone_count: m.tombstone_count.load(Ordering::Relaxed),
        boot_unix_secs: m.boot_unix_secs.load(Ordering::Relaxed),
    }
}

/// Wrap one public FFI entry point with the metrics call /
/// error-counter pair. `inc_call` is the per-function call counter
/// helper (e.g. [`inc_ingest`]); `body` is a closure that returns
/// the [`crate::error::FfiResult`] the function would otherwise
/// return directly. On `Err` the helper routes the error through
/// [`inc_error`]; on `Ok` it returns the value untouched.
///
/// Used at the top of every public FFI entry point in
/// `crates/ffi/src/lib.rs` so the metrics surface is wired in
/// exactly one place per function and the diff stays minimal.
#[inline]
pub(crate) fn instrument<T, F>(inc_call: fn(), body: F) -> crate::error::FfiResult<T>
where
    F: FnOnce() -> crate::error::FfiResult<T>,
{
    inc_call();
    body().inspect_err(inc_error)
}

/// Tracking flag for [`crate::tracing_init`] — flips to `true` the
/// first time a subscriber install attempt succeeds, and stays
/// `true` for the rest of the process. Read by the health envelope
/// so hosts can confirm tracing is wired without trying to install
/// a competing subscriber.
static TRACING_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Internal — mark the tracing subscriber as installed. Called
/// from [`crate::tracing_init::try_init_tracing`] on success.
/// Feature-gated to match the only call site; without the
/// `tracing-subscriber` feature there is no path that can flip
/// the flag, and the gauge stays `false` for the process lifetime.
#[cfg(feature = "tracing-subscriber")]
pub(crate) fn mark_tracing_initialized() {
    TRACING_INITIALIZED.store(true, Ordering::Relaxed);
}

/// Whether a tracing subscriber has been installed via
/// [`crate::tracing_init::try_init_tracing`] in this process.
/// Surfaced on [`crate::health::HealthStatus`].
#[must_use]
pub fn tracing_initialized() -> bool {
    TRACING_INITIALIZED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests in this module mutate the process-singleton metrics
    /// block. Other tests in the same test binary (the `lib.rs` unit
    /// tests that drive `open_store` / `ingest_message` / `close_store`
    /// through the real FFI surface) run **in parallel** with these
    /// tests under `cargo test`'s default scheduler, and those calls
    /// also increment the same counters via the `metrics::instrument`
    /// wrapper. Exact-delta assertions (`after == before + N`) are
    /// therefore inherently flaky — a concurrent test bumping
    /// `close_store_total` between our two snapshots breaks the
    /// equality.
    ///
    /// The architecturally correct assertion for a singleton-state
    /// counter under parallel test execution is a **monotonic lower
    /// bound**: "my call incremented the counter by at least N".
    /// That property catches every real wiring bug — `inc_X` writing
    /// to the wrong field, a counter that's accidentally a no-op,
    /// a snapshot reader that drops a field — without depending on
    /// the absence of concurrent activity.
    #[test]
    fn snapshot_reflects_counter_increments() {
        let before = snapshot();

        inc_ingest();
        inc_query();
        inc_query();
        inc_synthesis_triggered();
        inc_decay_sweep();
        inc_forget();
        inc_forget_scope();
        inc_get_evidence();
        inc_get_user_memory();
        inc_get_channel_memory();
        inc_list_memories();
        inc_pin();
        inc_unpin();
        inc_open_store();
        inc_close_store();
        inc_encrypt();
        inc_decrypt();
        inc_generate_keypair();
        inc_escape_fts_query();

        // `snapshot()` itself bumps `metrics_snapshot_total`, so we
        // capture the lower bound by calling `snapshot()` here too
        // and asserting the `after` snapshot is strictly greater than
        // `before` for that field. This catches both wiring directions
        // (a `snapshot()` that forgot to call `inc_metrics_snapshot`
        // and a snapshot reader that dropped the new field).
        let after = snapshot();
        assert!(after.ingest_total > before.ingest_total);
        assert!(after.query_total >= before.query_total + 2);
        assert!(after.synthesis_triggered_total > before.synthesis_triggered_total);
        assert!(after.decay_sweeps_total > before.decay_sweeps_total);
        assert!(after.forgets_total > before.forgets_total);
        assert!(after.forget_scopes_total > before.forget_scopes_total);
        assert!(after.get_evidence_total > before.get_evidence_total);
        assert!(after.get_user_memory_total > before.get_user_memory_total);
        assert!(after.get_channel_memory_total > before.get_channel_memory_total);
        assert!(after.list_memories_total > before.list_memories_total);
        assert!(after.pin_total > before.pin_total);
        assert!(after.unpin_total > before.unpin_total);
        assert!(after.open_store_total > before.open_store_total);
        assert!(after.close_store_total > before.close_store_total);
        assert!(after.encrypt_total > before.encrypt_total);
        assert!(after.decrypt_total > before.decrypt_total);
        assert!(after.generate_keypair_total > before.generate_keypair_total);
        assert!(after.escape_fts_query_total > before.escape_fts_query_total);
        // `before = snapshot()` and `after = snapshot()` both bump
        // this counter, so the after value must be at least
        // `before + 1` (it's `before + 2` minus whatever concurrent
        // snapshot calls observe, but the lower bound is enough to
        // prove the new wiring is live).
        assert!(after.metrics_snapshot_total > before.metrics_snapshot_total);
    }

    #[test]
    fn inc_error_routes_to_matching_kind_and_updates_total() {
        // Same singleton-parallel-test caveat as
        // `snapshot_reflects_counter_increments` — assert monotonic
        // lower bounds rather than exact deltas.
        let before = snapshot();

        inc_error(&FfiError::InvalidId {
            message: "bad".into(),
        });
        inc_error(&FfiError::Evidence {
            message: "fts".into(),
        });
        inc_error(&FfiError::Evidence {
            message: "aead".into(),
        });
        inc_error(&FfiError::Unavailable {
            subsystem: "inference_router".into(),
        });
        inc_error(&FfiError::InferenceFailure {
            message: "timeout".into(),
        });

        let after = snapshot();
        assert!(after.errors_by_kind.invalid_id > before.errors_by_kind.invalid_id);
        assert!(after.errors_by_kind.evidence >= before.errors_by_kind.evidence + 2);
        assert!(after.errors_by_kind.unavailable > before.errors_by_kind.unavailable);
        assert!(after.errors_by_kind.inference_failure > before.errors_by_kind.inference_failure);
        assert!(after.errors_total >= before.errors_total + 5);
    }

    #[test]
    fn gauges_overwrite_rather_than_increment() {
        // Gauges are "last write wins" and use `AtomicU64::store`,
        // which is linearizable per-atom — but a snapshot two-step
        // (set + later snapshot) still races against concurrent
        // `open_store` / `close_store` tests that also call
        // `set_open_handles` (via the runtime registry). We therefore
        // verify gauge writes by sampling the atom directly
        // immediately after the store, AND verifying snapshot reads
        // the atom by sampling the atom on both sides of the snapshot
        // and asserting the snapshot value falls in that window.
        let handles_uniq: u64 = 0xDEAD_BEEF_AAAA_0007;
        let tombs_uniq: u64 = 0xDEAD_BEEF_BBBB_002A;

        set_open_handles(handles_uniq);
        let pre_open = metrics().open_handles.load(Ordering::Relaxed);
        set_tombstone_count(tombs_uniq);
        let pre_tomb = metrics().tombstone_count.load(Ordering::Relaxed);
        let a = snapshot();
        let post_open = metrics().open_handles.load(Ordering::Relaxed);
        let post_tomb = metrics().tombstone_count.load(Ordering::Relaxed);

        // Set→load round-trip: the store *was* observed at some
        // moment. (Either we still see our unique value, or someone
        // else overwrote it after — which itself proves the field is
        // writable.)
        assert!(
            pre_open == handles_uniq || post_open != pre_open,
            "open_handles atomic store was not observable"
        );
        assert!(
            pre_tomb == tombs_uniq || post_tomb != pre_tomb,
            "tombstone_count atomic store was not observable"
        );

        // Snapshot reads the atom's current value: must fall between
        // the pre-snapshot and post-snapshot loads (or equal one of
        // them under sequential consistency).
        let (open_lo, open_hi) = (pre_open.min(post_open), pre_open.max(post_open));
        let (tomb_lo, tomb_hi) = (pre_tomb.min(post_tomb), pre_tomb.max(post_tomb));
        assert!(
            (open_lo..=open_hi).contains(&a.open_handles),
            "snapshot.open_handles={} not in [{}, {}]",
            a.open_handles,
            open_lo,
            open_hi
        );
        assert!(
            (tomb_lo..=tomb_hi).contains(&a.tombstone_count),
            "snapshot.tombstone_count={} not in [{}, {}]",
            a.tombstone_count,
            tomb_lo,
            tomb_hi
        );
    }

    #[test]
    fn boot_unix_secs_is_set_after_first_access() {
        prime();
        let snap = snapshot();
        // The boot stamp may be any non-zero positive integer
        // (post-1970); we only check that it's been initialised.
        assert!(snap.boot_unix_secs > 0);
    }
}
