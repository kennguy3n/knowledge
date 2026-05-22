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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Total `open_store` calls completed.
    pub open_store_total: u64,
    /// Total `close_store` calls completed.
    pub close_store_total: u64,
    /// Total `ingest_message` calls.
    pub ingest_total: u64,
    /// Total `query` calls.
    pub query_total: u64,
    /// Total `get_evidence` calls.
    pub get_evidence_total: u64,
    /// Total `get_user_memory` calls.
    pub get_user_memory_total: u64,
    /// Total `get_channel_memory` calls.
    pub get_channel_memory_total: u64,
    /// Total `list_memories` calls.
    pub list_memories_total: u64,
    /// Total `pin` calls.
    pub pin_total: u64,
    /// Total `unpin` calls.
    pub unpin_total: u64,
    /// Total `trigger_synthesis` calls (counted before the actual
    /// dispatch, so this includes `InferenceFailure` and
    /// `Unavailable` returns).
    pub synthesis_triggered_total: u64,
    /// Total `run_decay_sweep` calls.
    pub decay_sweeps_total: u64,
    /// Total `forget` calls.
    pub forgets_total: u64,
    /// Total `forget_scope` calls.
    pub forget_scopes_total: u64,
    /// Total `encrypt` calls.
    pub encrypt_total: u64,
    /// Total `decrypt` calls.
    pub decrypt_total: u64,
    /// Total `generate_keypair` calls.
    pub generate_keypair_total: u64,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
#[must_use]
pub fn snapshot() -> MetricsSnapshot {
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
    /// block, so they cannot run in parallel against each other.
    /// `cargo test` already serialises tests within a module by
    /// default *only* when they touch `static mut` — atomics don't
    /// trigger that. We work around this by using a single combined
    /// test that exercises every counter sequentially against a
    /// known baseline.
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

        let after = snapshot();
        assert_eq!(after.ingest_total, before.ingest_total + 1);
        assert_eq!(after.query_total, before.query_total + 2);
        assert_eq!(
            after.synthesis_triggered_total,
            before.synthesis_triggered_total + 1
        );
        assert_eq!(after.decay_sweeps_total, before.decay_sweeps_total + 1);
        assert_eq!(after.forgets_total, before.forgets_total + 1);
        assert_eq!(after.forget_scopes_total, before.forget_scopes_total + 1);
        assert_eq!(after.get_evidence_total, before.get_evidence_total + 1);
        assert_eq!(
            after.get_user_memory_total,
            before.get_user_memory_total + 1
        );
        assert_eq!(
            after.get_channel_memory_total,
            before.get_channel_memory_total + 1
        );
        assert_eq!(after.list_memories_total, before.list_memories_total + 1);
        assert_eq!(after.pin_total, before.pin_total + 1);
        assert_eq!(after.unpin_total, before.unpin_total + 1);
        assert_eq!(after.open_store_total, before.open_store_total + 1);
        assert_eq!(after.close_store_total, before.close_store_total + 1);
        assert_eq!(after.encrypt_total, before.encrypt_total + 1);
        assert_eq!(after.decrypt_total, before.decrypt_total + 1);
        assert_eq!(
            after.generate_keypair_total,
            before.generate_keypair_total + 1
        );
    }

    #[test]
    fn inc_error_routes_to_matching_kind_and_updates_total() {
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
        assert_eq!(
            after.errors_by_kind.invalid_id,
            before.errors_by_kind.invalid_id + 1
        );
        assert_eq!(
            after.errors_by_kind.evidence,
            before.errors_by_kind.evidence + 2
        );
        assert_eq!(
            after.errors_by_kind.unavailable,
            before.errors_by_kind.unavailable + 1
        );
        assert_eq!(
            after.errors_by_kind.inference_failure,
            before.errors_by_kind.inference_failure + 1
        );
        assert_eq!(after.errors_total, before.errors_total + 5);
    }

    #[test]
    fn gauges_overwrite_rather_than_increment() {
        set_open_handles(7);
        set_tombstone_count(42);
        let a = snapshot();
        assert_eq!(a.open_handles, 7);
        assert_eq!(a.tombstone_count, 42);

        set_open_handles(3);
        set_tombstone_count(9);
        let b = snapshot();
        assert_eq!(b.open_handles, 3);
        assert_eq!(b.tombstone_count, 9);
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
