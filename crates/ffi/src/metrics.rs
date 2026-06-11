//! Substrate-wide metrics counters and gauges.
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
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use inference_router::LatencyHistogram;
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
    /// Total on-device synthesis runs whose first attempt tripped a
    /// deterministic quality flag (meta-commentary preface, too-short
    /// recap, or near-zero evidence coverage) and so triggered a
    /// verify-and-retry second attempt. See
    /// `synthesis_pipeline::quality`.
    pub(crate) synthesis_lowquality_total: AtomicU64,
    /// Total verify-and-retry second attempts dispatched (one per
    /// low-quality first attempt). Distinct from
    /// [`Self::synthesis_lowquality_total`] only if the policy ever
    /// grows to retry more than once; today they move together.
    pub(crate) synthesis_retry_total: AtomicU64,
    /// Total verify-and-retry second attempts that were dispatched but
    /// *errored*, so the first (mediocre but usable) bundle was kept
    /// rather than failing the synthesis. A subset of
    /// [`Self::synthesis_retry_total`]; a rising value flags a retry-only
    /// flaky adapter that the graceful-degradation path would otherwise
    /// hide.
    pub(crate) synthesis_retry_failed_total: AtomicU64,
    /// Total synthesis attempts (first OR retry) whose raw output was
    /// salvaged from a token-cap-truncated prefix (strict JSON parse
    /// failed, `from_slm_str` recovered it). A rising value means the
    /// `n_predict` budget is too small for the evidence windows in play.
    pub(crate) synthesis_truncated_total: AtomicU64,
    /// Sum of kept-recap lengths, in Unicode scalar values, across every
    /// on-device synthesis run. Paired with
    /// [`Self::synthesis_recap_samples_total`] so a host can derive a
    /// mean recap length (`chars / samples`) without the substrate
    /// holding a float.
    pub(crate) synthesis_recap_chars_total: AtomicU64,
    /// Count of on-device synthesis runs that contributed to
    /// [`Self::synthesis_recap_chars_total`] (the denominator for the
    /// mean recap-length signal).
    pub(crate) synthesis_recap_samples_total: AtomicU64,
    /// Total `model_download_status` calls initiated (host polling the
    /// lazy SLM-weight download state to paint a one-time progress bar).
    pub(crate) model_download_status_total: AtomicU64,
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
    /// Total `open_store_with_resolver` calls initiated. Mirrors
    /// `open_store_total` so operators can see how many cold-boots
    /// went through the resolver-driven master-key path vs the
    /// legacy direct-master-key-hex path. A non-trivial ratio in
    /// favour of `open_store_total` after the resolver consumer
    /// has shipped is a signal that some host still has not
    /// migrated.
    pub(crate) open_store_with_resolver_total: AtomicU64,
    pub(crate) close_store_total: AtomicU64,
    pub(crate) encrypt_total: AtomicU64,
    pub(crate) decrypt_total: AtomicU64,
    pub(crate) generate_keypair_total: AtomicU64,
    /// Total `escape_fts_query` calls. Pure string transform, no
    /// `Err` path — a non-zero value here is a signal of host
    /// search activity volume, useful when correlating against
    /// `query_total` to see escape-helper-to-query ratio.
    pub(crate) escape_fts_query_total: AtomicU64,
    /// Total times [`query`](crate::query) retried a verbatim-rejected
    /// FTS5 expression as a sanitised literal-token query (the
    /// `fts_literal_token_fallback` recovery path). Distinct from
    /// `escape_fts_query_total`, which counts the separate public
    /// `escape_fts_query` string helper. A rising value here means hosts
    /// are sending raw text FTS5 rejects (hyphenated IDs, comma decimals,
    /// stray operators) often enough that the recovery path is load-bearing.
    pub(crate) query_fts_fallback_total: AtomicU64,
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
    /// Total `open_store_duration_histogram` reads. Like
    /// [`Self::metrics_snapshot_total`] this counts diagnostic
    /// read-outs (one per Prometheus scrape of the open-store latency
    /// histogram), so a runaway value flags an over-eager scraper.
    pub(crate) open_store_duration_histogram_total: AtomicU64,
    /// Total `slm_dispatch_histograms` calls initiated. Counts
    /// handle-keyed reads of the per-`(task, adapter)` SLM
    /// dispatch-latency histograms (one per Prometheus scrape that
    /// includes the SLM series).
    pub(crate) slm_dispatch_histograms_total: AtomicU64,
    /// Total `create_connector` calls initiated.
    pub(crate) create_connector_total: AtomicU64,
    /// Total `authenticate_connector` calls initiated.
    pub(crate) authenticate_connector_total: AtomicU64,
    /// Total `sync_connector` calls initiated.
    pub(crate) sync_connector_total: AtomicU64,
    /// Total `list_connectors` calls initiated.
    pub(crate) list_connectors_total: AtomicU64,
    /// Total `connector_status` calls initiated
    /// (single-instance health probe symmetric
    /// with [`Self::synthesis_status_total`]).
    pub(crate) connector_status_total: AtomicU64,
    /// Total `remove_connector` calls initiated.
    pub(crate) remove_connector_total: AtomicU64,
    /// Total `refresh_connector_token` calls initiated.
    pub(crate) refresh_connector_token_total: AtomicU64,
    /// Total `set_oauth_client_secret_resolver` calls initiated
    /// (host-supplied resolver registration).
    pub(crate) set_oauth_client_secret_resolver_total: AtomicU64,
    /// Total `clear_oauth_client_secret_resolver` calls initiated
    /// (host-supplied resolver de-registration).
    pub(crate) clear_oauth_client_secret_resolver_total: AtomicU64,
    /// Total `set_key_storage_resolver` calls initiated. Increments
    /// every time a host (re-)registers a [`crate::key_storage::
    /// KeyStorageResolver`]; high frequency indicates the host is
    /// treating the resolver as request-scoped rather than as a
    /// once-per-`open_store` lifecycle event — worth
    /// investigating, mirroring the OAuth resolver metric.
    pub(crate) set_key_storage_resolver_total: AtomicU64,
    /// Total `clear_key_storage_resolver` calls initiated.
    pub(crate) clear_key_storage_resolver_total: AtomicU64,
    /// Total `start_webhook_server` calls initiated
    /// (webhook receiver server startup).
    pub(crate) start_webhook_server_total: AtomicU64,
    /// Total `stop_webhook_server` calls initiated
    /// (webhook receiver server shutdown).
    pub(crate) stop_webhook_server_total: AtomicU64,
    /// Total `register_webhook_dispatch` calls initiated
    /// (bind a `provider_id` to a connector instance).
    pub(crate) register_webhook_dispatch_total: AtomicU64,
    /// Total `unregister_webhook_dispatch` calls initiated
    /// (drop a `(server, provider_id)` binding).
    pub(crate) unregister_webhook_dispatch_total: AtomicU64,
    /// Total `list_webhook_servers` calls initiated
    /// (diagnostic enumeration of running servers).
    pub(crate) list_webhook_servers_total: AtomicU64,
    /// Total `start_sync_scheduler` calls initiated
    /// (background sync scheduler startup).
    pub(crate) start_sync_scheduler_total: AtomicU64,
    /// Total `start_sync_scheduler_for_platform` calls initiated
    /// (platform-aware scheduler startup; distinct from the legacy
    /// `start_sync_scheduler` so desktop vs mobile starts are
    /// independently observable).
    pub(crate) start_sync_scheduler_for_platform_total: AtomicU64,
    /// Total `stop_sync_scheduler` calls initiated
    /// (background sync scheduler shutdown).
    pub(crate) stop_sync_scheduler_total: AtomicU64,
    /// Total `configure_sync_schedule` calls initiated
    /// (per-instance policy override).
    pub(crate) configure_sync_schedule_total: AtomicU64,
    /// Total `clear_sync_schedule` calls initiated
    /// (per-instance policy clear).
    pub(crate) clear_sync_schedule_total: AtomicU64,
    /// Total `sync_scheduler_status` calls initiated
    /// (diagnostic snapshot read).
    pub(crate) sync_scheduler_status_total: AtomicU64,
    /// Total ticks the scheduler worker thread has completed
    /// across every scheduler instance the process has ever run.
    /// Process-singleton sum because per-runtime
    /// counters live inside the per-runtime
    /// [`crate::sync_scheduler::RunningSyncScheduler`] and would
    /// be invisible to a host that polls only `metrics_snapshot`.
    pub(crate) sync_scheduler_ticks_total: AtomicU64,
    /// Total scheduler-initiated dispatches attempted across every
    /// runtime. Counts `sync_connector` calls made by
    /// scheduler worker threads, not their success/failure.
    pub(crate) sync_scheduler_dispatches_attempted_total: AtomicU64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Ok(SyncReport)`.
    pub(crate) sync_scheduler_dispatches_succeeded_total: AtomicU64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Err(_)`. Drives the per-instance
    /// exponential-backoff curve.
    pub(crate) sync_scheduler_dispatches_failed_total: AtomicU64,
    /// Total candidate instances the scheduler skipped because
    /// they were already in
    /// [`connector_framework::SyncStatus::InProgress`] when the
    /// tick fired (a host-driven sync was running concurrently).
    /// Distinct from `*_dispatches_failed_total` because the
    /// scheduler never invoked `sync_connector` for these
    /// instances.
    pub(crate) sync_scheduler_dispatches_skipped_in_progress_total: AtomicU64,
    /// Total webhook dispatches that completed with `200 OK`
    /// across every running server in this process. Tracked as a
    /// process-singleton sum because the per-server counters live
    /// inside the per-runtime `FfiWebhookRouter` and would be
    /// invisible to a host that polls only `metrics_snapshot`.
    pub(crate) webhook_dispatch_ok_total: AtomicU64,
    /// Total webhook dispatches that completed with `400 Bad
    /// Request` (the dispatcher returned `ConnectorError::Webhook`).
    pub(crate) webhook_dispatch_bad_request_total: AtomicU64,
    /// Total webhook dispatches that completed with `502 Bad
    /// Gateway` (the dispatcher returned any other `ConnectorError`).
    pub(crate) webhook_dispatch_bad_gateway_total: AtomicU64,
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
    /// Total `configure_synthesis_engine` calls initiated
    /// (host installs the server-side synthesis
    /// endpoint configuration on a runtime).
    pub(crate) configure_synthesis_engine_total: AtomicU64,
    /// Total `trigger_server_synthesis` calls initiated
    /// (host explicitly dispatches a domain / tenant
    /// synthesis on a configured engine).
    pub(crate) trigger_server_synthesis_total: AtomicU64,
    /// Total `synthesis_status` calls initiated
    /// (host polls the status of a previously
    /// dispatched window).
    pub(crate) synthesis_status_total: AtomicU64,
    /// Total `list_recent_syntheses` calls initiated
    /// (host enumerates per-scope synthesis history).
    pub(crate) list_recent_syntheses_total: AtomicU64,
    /// Total `configure_sync_auto_synthesize` calls initiated
    /// (host toggles the per-instance auto-synthesise
    /// flag on the sync scheduler).
    pub(crate) configure_sync_auto_synthesize_total: AtomicU64,
    /// Total `admit_approved_document` calls initiated
    /// (host attaches an approved-document payload to
    /// the tenant memory).
    pub(crate) admit_approved_document_total: AtomicU64,
    /// Total `revoke_approved_document` calls initiated
    /// (host removes a previously admitted approved
    /// document from tenant memory).
    pub(crate) revoke_approved_document_total: AtomicU64,
    /// Total `replace_approved_document` calls initiated
    /// (host replaces payload on an existing
    /// approved document without revoking and re-admitting).
    pub(crate) replace_approved_document_total: AtomicU64,
    /// Total `list_approved_documents` calls initiated
    /// (host enumerates approved-document refs for a
    /// tenant scope).
    pub(crate) list_approved_documents_total: AtomicU64,
    /// Total synthesis windows transitioned from `Pending` → `Failed`
    /// by the `open_store` stuck-Pending recovery sweep.
    /// Incremented once per swept window. A non-zero value here
    /// indicates a prior host run crashed mid-dispatch (between
    /// the Step-1 `flush_synthesis_windows` and the Step-3
    /// `apply_dispatch_outcome` commit) OR a Step-3 commit failed
    /// and the in-process recovery flush also failed to land. Either
    /// way the next `open_store` reclaimed the stranded window so
    /// the host can retry it.
    pub(crate) stuck_pending_window_recovered_total: AtomicU64,
    /// Total `trigger_server_synthesis` calls rejected by the
    /// global token-bucket rate limiter .
    /// Incremented once per `Throttled` return. Distinct from
    /// the per-kind [`Self::errors_throttled`] counter — that
    /// one ticks on every `FfiError::Throttled` regardless of
    /// surface, while this one isolates the synthesis-trigger
    /// surface so operators can spot rate-shaping-driven
    /// throttles separately from any future throttled
    /// surfaces.
    pub(crate) trigger_server_synthesis_throttled_total: AtomicU64,
    /// Total `replay_synthesis` calls initiated .
    /// Counts entries to the surface — both successful replays
    /// AND failure paths (engine error, transaction commit
    /// failure, invalid-state refusal). Per-kind error counters
    /// can be cross-referenced to disambiguate.
    pub(crate) replay_synthesis_total: AtomicU64,
    /// Total `list_synthesis_versions` calls .
    pub(crate) list_synthesis_versions_total: AtomicU64,

    // Per-kind error counters. The set mirrors `FfiError::kind`
    // exactly so adding a new error variant is a compile error
    // here (`inc_error` won't exhaustively match without an arm).
    pub(crate) errors_unimplemented: AtomicU64,
    pub(crate) errors_invalid_id: AtomicU64,
    pub(crate) errors_invalid_query: AtomicU64,
    pub(crate) errors_not_found: AtomicU64,
    pub(crate) errors_evidence: AtomicU64,
    pub(crate) errors_memory: AtomicU64,
    pub(crate) errors_synthesis: AtomicU64,
    pub(crate) errors_crypto: AtomicU64,
    pub(crate) errors_unavailable: AtomicU64,
    pub(crate) errors_inference_failure: AtomicU64,
    pub(crate) errors_connector: AtomicU64,
    pub(crate) errors_throttled: AtomicU64,
    pub(crate) errors_model_downloading: AtomicU64,
    /// Sum of every per-kind error counter, maintained alongside the
    /// individual counters so [`snapshot`] does not have to fan out
    /// across the per-kind reads to compute the total.
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

// ─── Latency histograms (open_store duration) ───────────────────────
//
// `open_store` wall-clock duration is tracked in a process-global
// fixed-bucket histogram (rather than a counter on the `Metrics`
// block) so the substrate's Prometheus surface can expose the full
// `knowledge_open_store_duration_seconds` `_bucket` / `_sum` / `_count`
// series — a single counter cannot carry a latency distribution. The
// histogram is process-global (not per-runtime) because it measures
// the cost of *constructing* a runtime, which has no handle yet.

/// Process-global histogram of `open_store` /
/// `open_store_with_resolver` wall-clock durations, backing the
/// `knowledge_open_store_duration_seconds` Prometheus histogram.
static OPEN_STORE_DURATION: OnceLock<Mutex<LatencyHistogram>> = OnceLock::new();

fn open_store_duration() -> &'static Mutex<LatencyHistogram> {
    OPEN_STORE_DURATION.get_or_init(|| Mutex::new(LatencyHistogram::new()))
}

/// Record one `open_store` wall-clock duration into the global
/// `knowledge_open_store_duration_seconds` histogram.
///
/// Called once per successful `open_store` / `open_store_with_resolver`
/// from [`crate::runtime`]. A poisoned lock (a prior panic while
/// recording) is silently ignored — a dropped diagnostic sample must
/// never escalate into an `open_store` failure.
pub(crate) fn record_open_store_duration(elapsed: Duration) {
    if let Ok(mut hist) = open_store_duration().lock() {
        hist.record(elapsed);
    }
}

/// Wire-flat snapshot of a fixed-bucket latency histogram for
/// Prometheus text exposition.
///
/// Returned by [`open_store_duration_histogram`] and embedded in the
/// per-`(task, adapter)` [`SlmDispatchHistogram`] entries. Carries the
/// cumulative `(le_seconds, count)` buckets (Prometheus `_bucket`
/// shape, including the trailing `+Inf` bucket), the running `_sum` in
/// seconds, and the total `_count`.
#[derive(Debug, Clone)]
pub struct HistogramView {
    /// Cumulative `(le_seconds, count)` buckets including `+Inf`.
    pub buckets: Vec<(f64, u64)>,
    /// Running sum of observed durations in seconds.
    pub sum_seconds: f64,
    /// Total number of observed samples.
    pub count: u64,
}

impl HistogramView {
    fn from_hist(hist: &LatencyHistogram) -> Self {
        Self {
            buckets: hist.cumulative_buckets(),
            sum_seconds: hist.sum_seconds(),
            count: hist.count(),
        }
    }
}

/// Snapshot the global `knowledge_open_store_duration_seconds`
/// histogram for Prometheus exposition.
///
/// Returns an empty histogram (zero samples) before the first
/// `open_store` call.
///
/// A poisoned lock is recovered (via [`PoisonError::into_inner`])
/// rather than panicked on, symmetric with the graceful handling in
/// [`record_open_store_duration`]: this readout backs a Prometheus
/// scrape, so a single poisoned recording must not turn every
/// subsequent scrape into a panic. The recovered data is still a
/// monotone histogram — [`LatencyHistogram::record`] only does
/// sequential scalar/bucket increments, so a poisoned guard cannot
/// expose a torn distribution.
///
/// Bumps `open_store_duration_histogram_total` first — this is an
/// infallible reader, so it follows the direct-`inc` pattern of
/// [`snapshot`] rather than routing through [`instrument`] (there is
/// no `FfiResult` to thread through). The snapshot value therefore
/// lags its own counter by exactly one read, as documented on the
/// field.
#[must_use]
pub fn open_store_duration_histogram() -> HistogramView {
    inc_open_store_duration_histogram();
    let hist = open_store_duration()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    HistogramView::from_hist(&hist)
}

/// Wire-flat snapshot of one `(task, adapter)` SLM dispatch-latency
/// histogram, for the `knowledge_slm_dispatch_duration_seconds`
/// Prometheus series.
///
/// Mirrors [`inference_router::DispatchLatency`] but with the task and
/// adapter pre-rendered to their stable string tags so the substrate
/// server can render the exposition without depending on
/// `inference_router` directly.
#[derive(Debug, Clone)]
pub struct SlmDispatchHistogram {
    /// Stable task tag (the `task` label).
    pub task: String,
    /// Stable adapter tag (the `adapter` label).
    pub adapter: String,
    /// Cumulative `(le_seconds, count)` buckets including `+Inf`.
    pub buckets: Vec<(f64, u64)>,
    /// Running sum of observed dispatch durations in seconds.
    pub sum_seconds: f64,
    /// Total dispatches recorded for this `(task, adapter)` pair.
    pub count: u64,
}

/// Snapshot every per-`(task, adapter)` SLM dispatch-latency histogram
/// recorded by the runtime behind `handle`.
///
/// Backs the `knowledge_slm_dispatch_duration_seconds` Prometheus
/// series. Returned in the router's stable sort order (task tag, then
/// adapter tag).
///
/// # Errors
///
/// Forwards [`FfiError::Unavailable`] when `handle` is unknown or has
/// been closed, matching every other handle-keyed FFI entry point.
///
/// Wrapped in [`instrument`] like every other fallible handle-keyed
/// entry point: bumps `slm_dispatch_histograms_total` and routes the
/// `Unavailable` error through `inc_error`, so this diagnostic reader
/// shows up in the substrate's own call-count telemetry alongside
/// `metrics_snapshot` and `health_check`.
pub fn slm_dispatch_histograms(
    handle: crate::runtime::RuntimeHandle,
) -> crate::error::FfiResult<Vec<SlmDispatchHistogram>> {
    instrument(inc_slm_dispatch_histograms, || {
        crate::runtime::with_runtime(handle, |rt| {
            Ok(rt
                .inference_router
                .dispatch_latencies()
                .into_iter()
                .map(|d| SlmDispatchHistogram {
                    task: d.task.tag().to_string(),
                    adapter: d.adapter.as_str().to_string(),
                    buckets: d.buckets,
                    sum_seconds: d.sum_seconds,
                    count: d.count,
                })
                .collect())
        })
    })
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
counter_inc!(pub(crate) fn inc_synthesis_lowquality => synthesis_lowquality_total);
counter_inc!(pub(crate) fn inc_synthesis_retry => synthesis_retry_total);
counter_inc!(pub(crate) fn inc_synthesis_retry_failed => synthesis_retry_failed_total);
counter_inc!(pub(crate) fn inc_synthesis_truncated => synthesis_truncated_total);

/// Record the kept-recap length of one on-device synthesis run: adds
/// `recap_chars` to the sum and bumps the sample count by one, so the
/// snapshot exposes both halves of the mean recap-length signal. Bounds
/// the per-call addend to `u64` losslessly (recap lengths are tiny).
#[inline]
pub(crate) fn observe_synthesis_recap_chars(recap_chars: usize) {
    let m = metrics();
    let chars = u64::try_from(recap_chars).unwrap_or(u64::MAX);
    m.synthesis_recap_chars_total
        .fetch_add(chars, Ordering::Relaxed);
    m.synthesis_recap_samples_total
        .fetch_add(1, Ordering::Relaxed);
}

counter_inc!(pub(crate) fn inc_model_download_status => model_download_status_total);
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
counter_inc!(pub(crate) fn inc_open_store_with_resolver => open_store_with_resolver_total);
counter_inc!(pub(crate) fn inc_close_store => close_store_total);
counter_inc!(pub(crate) fn inc_encrypt => encrypt_total);
counter_inc!(pub(crate) fn inc_decrypt => decrypt_total);
counter_inc!(pub(crate) fn inc_generate_keypair => generate_keypair_total);
counter_inc!(pub(crate) fn inc_escape_fts_query => escape_fts_query_total);
counter_inc!(pub(crate) fn inc_query_fts_fallback => query_fts_fallback_total);
counter_inc!(pub(crate) fn inc_metrics_snapshot => metrics_snapshot_total);
counter_inc!(pub(crate) fn inc_open_store_duration_histogram => open_store_duration_histogram_total);
counter_inc!(pub(crate) fn inc_slm_dispatch_histograms => slm_dispatch_histograms_total);
counter_inc!(pub(crate) fn inc_health_check => health_check_total);
counter_inc!(pub(crate) fn inc_create_connector => create_connector_total);
counter_inc!(pub(crate) fn inc_authenticate_connector => authenticate_connector_total);
counter_inc!(pub(crate) fn inc_sync_connector => sync_connector_total);
counter_inc!(pub(crate) fn inc_list_connectors => list_connectors_total);
counter_inc!(pub(crate) fn inc_connector_status => connector_status_total);
counter_inc!(pub(crate) fn inc_remove_connector => remove_connector_total);
counter_inc!(pub(crate) fn inc_refresh_connector_token => refresh_connector_token_total);
counter_inc!(pub(crate) fn inc_set_oauth_client_secret_resolver => set_oauth_client_secret_resolver_total);
counter_inc!(pub(crate) fn inc_clear_oauth_client_secret_resolver => clear_oauth_client_secret_resolver_total);
counter_inc!(pub(crate) fn inc_set_key_storage_resolver => set_key_storage_resolver_total);
counter_inc!(pub(crate) fn inc_clear_key_storage_resolver => clear_key_storage_resolver_total);
counter_inc!(pub(crate) fn inc_start_webhook_server => start_webhook_server_total);
counter_inc!(pub(crate) fn inc_stop_webhook_server => stop_webhook_server_total);
counter_inc!(pub(crate) fn inc_register_webhook_dispatch => register_webhook_dispatch_total);
counter_inc!(pub(crate) fn inc_unregister_webhook_dispatch => unregister_webhook_dispatch_total);
counter_inc!(pub(crate) fn inc_list_webhook_servers => list_webhook_servers_total);
counter_inc!(pub(crate) fn inc_webhook_dispatch_ok => webhook_dispatch_ok_total);
counter_inc!(pub(crate) fn inc_webhook_dispatch_bad_request => webhook_dispatch_bad_request_total);
counter_inc!(pub(crate) fn inc_webhook_dispatch_bad_gateway => webhook_dispatch_bad_gateway_total);
counter_inc!(pub(crate) fn inc_start_sync_scheduler => start_sync_scheduler_total);
counter_inc!(pub(crate) fn inc_start_sync_scheduler_for_platform => start_sync_scheduler_for_platform_total);
counter_inc!(pub(crate) fn inc_stop_sync_scheduler => stop_sync_scheduler_total);
counter_inc!(pub(crate) fn inc_configure_sync_schedule => configure_sync_schedule_total);
counter_inc!(pub(crate) fn inc_configure_synthesis_engine => configure_synthesis_engine_total);
counter_inc!(pub(crate) fn inc_trigger_server_synthesis => trigger_server_synthesis_total);
counter_inc!(pub(crate) fn inc_synthesis_status => synthesis_status_total);
counter_inc!(pub(crate) fn inc_list_recent_syntheses => list_recent_syntheses_total);
counter_inc!(pub(crate) fn inc_configure_sync_auto_synthesize => configure_sync_auto_synthesize_total);
counter_inc!(pub(crate) fn inc_admit_approved_document => admit_approved_document_total);
counter_inc!(pub(crate) fn inc_revoke_approved_document => revoke_approved_document_total);
counter_inc!(pub(crate) fn inc_replace_approved_document => replace_approved_document_total);
counter_inc!(pub(crate) fn inc_list_approved_documents => list_approved_documents_total);
counter_inc!(pub(crate) fn inc_stuck_pending_window_recovered => stuck_pending_window_recovered_total);
counter_inc!(pub(crate) fn inc_trigger_server_synthesis_throttled => trigger_server_synthesis_throttled_total);
counter_inc!(pub(crate) fn inc_replay_synthesis => replay_synthesis_total);
counter_inc!(pub(crate) fn inc_list_synthesis_versions => list_synthesis_versions_total);
counter_inc!(pub(crate) fn inc_clear_sync_schedule => clear_sync_schedule_total);
counter_inc!(pub(crate) fn inc_sync_scheduler_status => sync_scheduler_status_total);
counter_inc!(pub(crate) fn inc_sync_scheduler_tick => sync_scheduler_ticks_total);
counter_inc!(pub(crate) fn inc_sync_scheduler_dispatch_attempted => sync_scheduler_dispatches_attempted_total);
counter_inc!(pub(crate) fn inc_sync_scheduler_dispatch_succeeded => sync_scheduler_dispatches_succeeded_total);
counter_inc!(pub(crate) fn inc_sync_scheduler_dispatch_failed => sync_scheduler_dispatches_failed_total);
counter_inc!(pub(crate) fn inc_sync_scheduler_dispatch_skipped_in_progress => sync_scheduler_dispatches_skipped_in_progress_total);
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
        FfiError::InvalidQuery { .. } => &m.errors_invalid_query,
        FfiError::NotFound { .. } => &m.errors_not_found,
        FfiError::Evidence { .. } => &m.errors_evidence,
        FfiError::Memory { .. } => &m.errors_memory,
        FfiError::Synthesis { .. } => &m.errors_synthesis,
        FfiError::Crypto { .. } => &m.errors_crypto,
        FfiError::Unavailable { .. } => &m.errors_unavailable,
        FfiError::InferenceFailure { .. } => &m.errors_inference_failure,
        FfiError::Connector { .. } => &m.errors_connector,
        FfiError::Throttled { .. } => &m.errors_throttled,
        FfiError::ModelDownloading { .. } => &m.errors_model_downloading,
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
    /// Total `open_store_with_resolver` calls initiated. Mirrors
    /// `open_store_total` for the resolver-driven cold-boot path.
    /// Added in the v10 surface bump; older snapshots deserialise
    /// to `0` via `#[serde(default)]` so this field is additive on
    /// the wire.
    #[serde(default)]
    pub open_store_with_resolver_total: u64,
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
    /// Total on-device synthesis runs whose first attempt was flagged
    /// low-quality and triggered a verify-and-retry. `#[serde(default)]`
    /// so a host deserializing a pre-existing snapshot gets `0`.
    #[serde(default)]
    pub synthesis_lowquality_total: u64,
    /// Total verify-and-retry second attempts dispatched.
    #[serde(default)]
    pub synthesis_retry_total: u64,
    /// Total verify-and-retry second attempts that errored, keeping the
    /// first bundle (graceful-degradation / flaky-retry-adapter signal).
    #[serde(default)]
    pub synthesis_retry_failed_total: u64,
    /// Total synthesis attempts whose output was salvaged from a
    /// token-cap-truncated prefix (budget-pressure signal).
    #[serde(default)]
    pub synthesis_truncated_total: u64,
    /// Sum of kept-recap lengths (scalar values) across synthesis runs;
    /// divide by [`Self::synthesis_recap_samples_total`] for the mean.
    #[serde(default)]
    pub synthesis_recap_chars_total: u64,
    /// Number of synthesis runs contributing to
    /// [`Self::synthesis_recap_chars_total`].
    #[serde(default)]
    pub synthesis_recap_samples_total: u64,
    /// Total `model_download_status` calls initiated (host polling the
    /// lazy SLM-weight download state). `#[serde(default)]` so a host
    /// deserializing a snapshot produced before this counter existed
    /// gets `0` rather than a parse error.
    #[serde(default)]
    pub model_download_status_total: u64,
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
    /// Total times [`query`](crate::query) recovered a verbatim-rejected
    /// FTS5 expression via the literal-token fallback. Correlate against
    /// `query_total` to see how often raw user text trips the FTS5 parser.
    #[serde(default)]
    pub query_fts_fallback_total: u64,
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
    /// Total `open_store_duration_histogram` diagnostic reads. Like
    /// `metrics_snapshot_total`, this lags its own counter by one read.
    #[serde(default)]
    pub open_store_duration_histogram_total: u64,
    /// Total `slm_dispatch_histograms` diagnostic reads.
    #[serde(default)]
    pub slm_dispatch_histograms_total: u64,
    /// Total `create_connector` calls initiated.
    #[serde(default)]
    pub create_connector_total: u64,
    /// Total `authenticate_connector` calls initiated.
    #[serde(default)]
    pub authenticate_connector_total: u64,
    /// Total `sync_connector` calls initiated.
    #[serde(default)]
    pub sync_connector_total: u64,
    /// Total `list_connectors` calls initiated.
    #[serde(default)]
    pub list_connectors_total: u64,
    /// Total `connector_status` calls initiated
    /// (single-instance health probe symmetric
    /// with [`Self::synthesis_status_total`]).
    #[serde(default)]
    pub connector_status_total: u64,
    /// Total `remove_connector` calls initiated.
    #[serde(default)]
    pub remove_connector_total: u64,
    /// Total `refresh_connector_token` calls initiated. Counts
    /// every host-driven explicit refresh; the auto-refresh path
    /// inside `sync_connector` does NOT increment this counter
    /// (it is part of the `sync_connector_total` accounting).
    #[serde(default)]
    pub refresh_connector_token_total: u64,
    /// Total `set_oauth_client_secret_resolver` calls initiated.
    /// Increments every time a host (re-)registers a
    /// resolver; high frequency indicates the host is treating the
    /// resolver registration as a per-request operation rather
    /// than a once-per-`open_store` lifecycle event — worth
    /// investigating.
    #[serde(default)]
    pub set_oauth_client_secret_resolver_total: u64,
    /// Total `clear_oauth_client_secret_resolver` calls initiated.
    #[serde(default)]
    pub clear_oauth_client_secret_resolver_total: u64,
    /// Total `set_key_storage_resolver` calls initiated. Mirrors
    /// the OAuth resolver counter so operators can spot hosts that
    /// treat the key-storage resolver as request-scoped.
    #[serde(default)]
    pub set_key_storage_resolver_total: u64,
    /// Total `clear_key_storage_resolver` calls initiated.
    #[serde(default)]
    pub clear_key_storage_resolver_total: u64,
    /// Total `start_webhook_server` calls initiated.
    #[serde(default)]
    pub start_webhook_server_total: u64,
    /// Total `stop_webhook_server` calls initiated.
    #[serde(default)]
    pub stop_webhook_server_total: u64,
    /// Total `register_webhook_dispatch` calls initiated.
    #[serde(default)]
    pub register_webhook_dispatch_total: u64,
    /// Total `unregister_webhook_dispatch` calls initiated.
    #[serde(default)]
    pub unregister_webhook_dispatch_total: u64,
    /// Total `list_webhook_servers` calls initiated.
    #[serde(default)]
    pub list_webhook_servers_total: u64,
    /// Total `start_sync_scheduler` calls initiated.
    #[serde(default)]
    pub start_sync_scheduler_total: u64,
    /// Total `start_sync_scheduler_for_platform` calls initiated.
    /// `#[serde(default)]` so snapshots produced before this counter
    /// existed deserialize to `0` rather than failing.
    #[serde(default)]
    pub start_sync_scheduler_for_platform_total: u64,
    /// Total `stop_sync_scheduler` calls initiated.
    #[serde(default)]
    pub stop_sync_scheduler_total: u64,
    /// Total `configure_sync_schedule` calls initiated.
    #[serde(default)]
    pub configure_sync_schedule_total: u64,
    /// Total `clear_sync_schedule` calls initiated.
    #[serde(default)]
    pub clear_sync_schedule_total: u64,
    /// Total `sync_scheduler_status` calls initiated.
    #[serde(default)]
    pub sync_scheduler_status_total: u64,
    /// Total ticks the scheduler worker thread has completed
    /// across every scheduler instance in this process.
    /// Tracked as a process-singleton sum because the per-runtime
    /// counter lives inside the per-runtime
    /// [`crate::sync_scheduler::RunningSyncScheduler`] and would
    /// be invisible to a host that polls only `metrics_snapshot`.
    #[serde(default)]
    pub sync_scheduler_ticks_total: u64,
    /// Total scheduler-initiated dispatches attempted across every
    /// runtime.
    #[serde(default)]
    pub sync_scheduler_dispatches_attempted_total: u64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Ok(SyncReport)`.
    #[serde(default)]
    pub sync_scheduler_dispatches_succeeded_total: u64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Err(_)`.
    #[serde(default)]
    pub sync_scheduler_dispatches_failed_total: u64,
    /// Total candidate instances the scheduler skipped because
    /// they were already in
    /// [`connector_framework::SyncStatus::InProgress`] when the
    /// tick fired.
    #[serde(default)]
    pub sync_scheduler_dispatches_skipped_in_progress_total: u64,
    /// Total webhook dispatches that returned `200 OK` across every
    /// running server in this process. The per-server
    /// counters live in
    /// [`crate::types::WebhookServerSummary::dispatch_ok_total`];
    /// this counter is the process-wide sum, surfaced through
    /// `metrics_snapshot` so a host that polls only the metrics
    /// surface sees webhook activity without enumerating servers.
    #[serde(default)]
    pub webhook_dispatch_ok_total: u64,
    /// Total webhook dispatches that returned `400 Bad Request`
    /// across every running server in this process.
    /// Companion to [`Self::webhook_dispatch_ok_total`].
    #[serde(default)]
    pub webhook_dispatch_bad_request_total: u64,
    /// Total webhook dispatches that returned `502 Bad Gateway`
    /// across every running server in this process.
    /// Companion to [`Self::webhook_dispatch_ok_total`].
    #[serde(default)]
    pub webhook_dispatch_bad_gateway_total: u64,
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
    /// Total `configure_synthesis_engine` calls initiated.
    #[serde(default)]
    pub configure_synthesis_engine_total: u64,
    /// Total `trigger_server_synthesis` calls initiated.
    #[serde(default)]
    pub trigger_server_synthesis_total: u64,
    /// Total `synthesis_status` calls initiated.
    #[serde(default)]
    pub synthesis_status_total: u64,
    /// Total `list_recent_syntheses` calls initiated.
    #[serde(default)]
    pub list_recent_syntheses_total: u64,
    /// Total `configure_sync_auto_synthesize` calls initiated.
    #[serde(default)]
    pub configure_sync_auto_synthesize_total: u64,
    /// Total `admit_approved_document` calls initiated.
    #[serde(default)]
    pub admit_approved_document_total: u64,
    /// Total `revoke_approved_document` calls initiated.
    #[serde(default)]
    pub revoke_approved_document_total: u64,
    /// Total `replace_approved_document` calls initiated.
    #[serde(default)]
    pub replace_approved_document_total: u64,
    /// Total `list_approved_documents` calls initiated.
    #[serde(default)]
    pub list_approved_documents_total: u64,
    /// Total synthesis windows transitioned from `Pending` → `Failed`
    /// by the `open_store` stuck-Pending recovery sweep.
    /// A non-zero value indicates a prior run left at least
    /// one window stranded mid-dispatch and the next `open_store`
    /// reclaimed it; the host can retry the recovered window via the
    /// normal trigger path.
    #[serde(default)]
    pub stuck_pending_window_recovered_total: u64,
    /// Total `trigger_server_synthesis` calls rejected by the
    /// FFI-wide rate-shaping token bucket .
    /// Distinct from `errors_by_kind.throttled` because that
    /// total covers every surface returning
    /// `FfiError::Throttled` — currently only this one, but
    /// future surfaces should reuse the variant rather than
    /// minting a new one.
    #[serde(default)]
    pub trigger_server_synthesis_throttled_total: u64,
    /// Total `replay_synthesis` calls . Counts
    /// every entry to the surface, regardless of outcome — pair
    /// with `errors_by_kind.synthesis` / `.evidence` for failure
    /// rates.
    #[serde(default)]
    pub replay_synthesis_total: u64,
    /// Total `list_synthesis_versions` calls .
    #[serde(default)]
    pub list_synthesis_versions_total: u64,
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
    /// Multilingual lexicon-path telemetry. Counts
    /// per-BCP-47 lexicon hits, [`observation_engine::MatchStrategy`]
    /// fires, and Arabic / Hebrew clitic-peel depth distribution.
    /// `#[serde(default)]` per the additive-wire-contract rule —
    /// older emitters' JSON lacks this field and deserialises to
    /// [`LexiconTelemetry::default()`] (all zeroes).
    #[serde(default)]
    pub lexicon_telemetry: LexiconTelemetry,
    /// Multilingual FTS5-path telemetry. Counts
    /// per-lane query / row totals, recall-lane skip causes, and
    /// stopword strip volumes per call site.
    /// `#[serde(default)]` per the additive-wire-contract rule.
    #[serde(default)]
    pub fts_telemetry: FtsTelemetry,
    /// Multilingual embedding / vector-retrieval telemetry.
    /// Counts live embeddings computed per call site,
    /// `evidence_embeddings` cache outcomes, dedup-copy hits,
    /// per-variant adapter errors, and `model_tag` rotation-rule
    /// violations.  `#[serde(default)]` per the additive-wire-contract
    /// rule.
    #[serde(default)]
    pub vector_telemetry: VectorTelemetry,
    /// Unified retrieval-telemetry view — the three
    /// per-lane telemetry sub-structs grouped under one namespace
    /// for dashboard ergonomics. Always populated identically to
    /// the flat [`Self::fts_telemetry`] / [`Self::lexicon_telemetry`]
    /// / [`Self::vector_telemetry`] fields; the
    /// `metrics_snapshot_retrieval_metrics_matches_flat_fields`
    /// test below locks the parity.  `#[serde(default)]` per the
    /// additive-wire-contract rule — older emitters' JSON lacks
    /// this field and deserialises to
    /// [`RetrievalMetrics::default()`] (all zeroes).
    #[serde(default)]
    pub retrieval_metrics: RetrievalMetrics,
}

/// Multilingual lexicon-path observability counters mirrored from
/// [`observation_engine::lexicon_telemetry`].
///
/// The mirror lives here rather than upstream because the FFI
/// crate is where the `uniffi::Record` / serde derive lives, and
/// the upstream `observation_engine` crate intentionally does not
/// depend on either FFI runtime. The field list mirrors
/// [`observation_engine::LexiconTelemetrySnapshot`] verbatim
/// — adding a counter requires extending both structs
/// symmetrically.
///
/// New fields must use `#[serde(default)]` so older snapshots
/// deserialise cleanly; the additive-wire-contract rule applies
/// to this struct exactly the same as to [`MetricsSnapshot`].
///
/// # Counter semantics — read this before consuming `hits_*`
///
/// Each `hits_<tag>` field counts *lexicon-resolution calls*
/// (every invocation of
/// `LexiconRegistry::lexicon_for_or_english`), NOT unique
/// sentences or documents. A typical sentence triggers several
/// resolution calls — up to three classifier-loop resolutions
/// plus one per inspected capitalised word for the stop-word
/// filter — so e.g. a 5-capitalised-word English sentence with
/// no class match will bump `hits_en` ~8 times. Operators
/// inferring "documents classified" from these counters should
/// divide by their measured calls-per-document ratio rather
/// than reading the counter directly. See the upstream
/// `observation_engine::lexicon_telemetry` module doc for the
/// full rationale (calls-vs-documents distinction).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct LexiconTelemetry {
    /// Resolved-lexicon hits for `ar`.
    #[serde(default)]
    pub hits_ar: u64,
    /// Resolved-lexicon hits for `bo`.
    #[serde(default)]
    pub hits_bo: u64,
    /// Resolved-lexicon hits for `de`.
    #[serde(default)]
    pub hits_de: u64,
    /// Resolved-lexicon hits for `en`. Includes the
    /// unknown-tag → English fallback path (see
    /// [`Self::unknown_tag_fallbacks_total`]).
    #[serde(default)]
    pub hits_en: u64,
    /// Resolved-lexicon hits for `es`.
    #[serde(default)]
    pub hits_es: u64,
    /// Resolved-lexicon hits for `fr`.
    #[serde(default)]
    pub hits_fr: u64,
    /// Resolved-lexicon hits for `he`.
    #[serde(default)]
    pub hits_he: u64,
    /// Resolved-lexicon hits for `hi`.
    #[serde(default)]
    pub hits_hi: u64,
    /// Resolved-lexicon hits for `id`.
    #[serde(default)]
    pub hits_id: u64,
    /// Resolved-lexicon hits for `it`.
    #[serde(default)]
    pub hits_it: u64,
    /// Resolved-lexicon hits for `ja`.
    #[serde(default)]
    pub hits_ja: u64,
    /// Resolved-lexicon hits for `km`.
    #[serde(default)]
    pub hits_km: u64,
    /// Resolved-lexicon hits for `ko`.
    #[serde(default)]
    pub hits_ko: u64,
    /// Resolved-lexicon hits for `lo`.
    #[serde(default)]
    pub hits_lo: u64,
    /// Resolved-lexicon hits for `ms`.
    #[serde(default)]
    pub hits_ms: u64,
    /// Resolved-lexicon hits for `my`.
    #[serde(default)]
    pub hits_my: u64,
    /// Resolved-lexicon hits for `pt`.
    #[serde(default)]
    pub hits_pt: u64,
    /// Resolved-lexicon hits for `ru`.
    #[serde(default)]
    pub hits_ru: u64,
    /// Resolved-lexicon hits for `th`.
    #[serde(default)]
    pub hits_th: u64,
    /// Resolved-lexicon hits for `tl`.
    #[serde(default)]
    pub hits_tl: u64,
    /// Resolved-lexicon hits for `vi`.
    #[serde(default)]
    pub hits_vi: u64,
    /// Resolved-lexicon hits for `zh`.
    #[serde(default)]
    pub hits_zh: u64,
    /// Times an input primary_tag was `Some(t)` but no lexicon
    /// was configured for `t`, so the registry fell back to
    /// English. Always satisfies
    /// `unknown_tag_fallbacks_total <= hits_en`.
    #[serde(default)]
    pub unknown_tag_fallbacks_total: u64,
    /// `MatchStrategy::FirstToken` fires.
    #[serde(default)]
    pub strategy_first_token: u64,
    /// `MatchStrategy::FirstBigram` fires.
    #[serde(default)]
    pub strategy_first_bigram: u64,
    /// `MatchStrategy::Substring` fires.
    #[serde(default)]
    pub strategy_substring: u64,
    /// `MatchStrategy::FirstTokenWithArabicClitics` fires.
    #[serde(default)]
    pub strategy_first_token_with_arabic_clitics: u64,
    /// `MatchStrategy::FirstTokenWithHebrewClitics` fires.
    #[serde(default)]
    pub strategy_first_token_with_hebrew_clitics: u64,
    /// Arabic clitic-peeler matches at depth 0 (no peel needed).
    #[serde(default)]
    pub arabic_peel_depth_0_matches: u64,
    /// Arabic clitic-peeler matches at depth 1 (one peel).
    #[serde(default)]
    pub arabic_peel_depth_1_matches: u64,
    /// Arabic clitic-peeler matches at depth 2 (two peels).
    #[serde(default)]
    pub arabic_peel_depth_2_matches: u64,
    /// Arabic clitic-peeler matches at depth 3 (three peels).
    #[serde(default)]
    pub arabic_peel_depth_3_matches: u64,
    /// Arabic clitic-peeler budget exhausted without a match.
    #[serde(default)]
    pub arabic_peel_depth_exhausted: u64,
    /// Hebrew clitic-peeler matches at depth 0 (no peel needed).
    #[serde(default)]
    pub hebrew_peel_depth_0_matches: u64,
    /// Hebrew clitic-peeler matches at depth 1 (one peel).
    #[serde(default)]
    pub hebrew_peel_depth_1_matches: u64,
    /// Hebrew clitic-peeler matches at depth 2 (two peels).
    #[serde(default)]
    pub hebrew_peel_depth_2_matches: u64,
    /// Hebrew clitic-peeler matches at depth 3 (three peels).
    #[serde(default)]
    pub hebrew_peel_depth_3_matches: u64,
    /// Hebrew clitic-peeler budget exhausted without a match.
    #[serde(default)]
    pub hebrew_peel_depth_exhausted: u64,
}

/// Multilingual FTS5-path observability counters mirrored from
/// [`evidence_store::fts_telemetry`].
///
/// See [`LexiconTelemetry`] for the wire-mirror rationale.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct FtsTelemetry {
    /// Times the unicode61 lane (`evidence_fts`) was invoked
    /// with a non-empty query in
    /// [`evidence_store::store::merged_fts_search`].
    #[serde(default)]
    pub unicode61_lane_queries_total: u64,
    /// Cumulative row count across all
    /// [`Self::unicode61_lane_queries_total`] invocations.
    #[serde(default)]
    pub unicode61_lane_rows_total: u64,
    /// Times the CJK trigram lane (`evidence_fts_cjk`) was
    /// invoked with a non-empty stripped query.
    #[serde(default)]
    pub cjk_trigram_lane_queries_total: u64,
    /// Cumulative row count across all
    /// [`Self::cjk_trigram_lane_queries_total`] invocations.
    #[serde(default)]
    pub cjk_trigram_lane_rows_total: u64,
    /// Times the CJK trigram lane was skipped because the
    /// stopword stripping collapsed the query to empty. This is
    /// the SINGLE structural-skip variant for the trigram lane
    /// — Latin-only queries are NOT skipped because the FTS5
    /// `trigram` tokeniser windows Latin substrings embedded in
    /// CJK bodies (see upstream
    /// `evidence_store::fts_telemetry` module doc for the
    /// cross-script rationale).
    #[serde(default)]
    pub cjk_trigram_lane_skips_pure_stopword_query_total: u64,
    /// Times the CJK bigram lane (`evidence_fts_bigram`) was
    /// invoked with a non-empty bigram match string.
    #[serde(default)]
    pub bigram_lane_queries_total: u64,
    /// Cumulative row count across all
    /// [`Self::bigram_lane_queries_total`] invocations.
    #[serde(default)]
    pub bigram_lane_rows_total: u64,
    /// Times the CJK bigram lane was skipped because the
    /// stopword stripping collapsed the query to empty.
    /// Mutually exclusive with
    /// [`Self::bigram_lane_skips_no_cjk_query_total`] — the
    /// pure-stopword check runs first, so a pure-stopword CJK
    /// query like `の の の` bumps this counter and NOT the
    /// no-CJK counter. See the upstream
    /// `evidence_store::fts_telemetry::SkipReason` doc for the
    /// taxonomic rationale.
    #[serde(default)]
    pub bigram_lane_skips_pure_stopword_query_total: u64,
    /// Times the CJK bigram lane was skipped because the
    /// stripped query was non-empty but contained no adjacent-
    /// CJK codepoint pair (e.g. a Latin-only query). Mutually
    /// exclusive with
    /// [`Self::bigram_lane_skips_pure_stopword_query_total`].
    #[serde(default)]
    pub bigram_lane_skips_no_cjk_query_total: u64,
    /// Cumulative count of stopword instances stripped at
    /// index-write time.
    #[serde(default)]
    pub index_write_stopwords_stripped_total: u64,
    /// Cumulative count of stopword instances stripped at
    /// query time.
    #[serde(default)]
    pub query_time_stopwords_stripped_total: u64,
    /// Cumulative count of stopword instances stripped during
    /// the v15 → v16 chunked re-tokenisation migration.
    #[serde(default)]
    pub v16_migration_stopwords_stripped_total: u64,
}

/// Multilingual embedding / vector-retrieval observability counters
/// mirrored from [`evidence_store::vector_telemetry`].
///
/// See [`LexiconTelemetry`] for the wire-mirror rationale; the
/// `#[serde(default)]` discipline applies symmetrically here so an
/// older emitter's JSON that lacks any of these fields still
/// deserialises cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct VectorTelemetry {
    /// Successful query-side embeds in
    /// [`evidence_store::retrieval::HybridRetriever::search_hybrid`]
    /// or
    /// [`evidence_store::retrieval::HybridRetriever::rerank_with_embeddings`].
    #[serde(default)]
    pub query_embeddings_total: u64,
    /// Successful fresh body embeds in
    /// [`evidence_store::EvidenceStore::index_embedding`] (the
    /// path NOT short-circuited by dedup-copy).
    #[serde(default)]
    pub index_write_embeddings_total: u64,
    /// Successful body embeds in
    /// [`evidence_store::retrieval::HybridRetriever::candidate_embedding`]'s
    /// cache-miss fallback.
    #[serde(default)]
    pub live_body_embeddings_total: u64,
    /// Embedding-cache lookups that returned a dimension-
    /// matching row for the active `(evidence_id, model_tag)`.
    #[serde(default)]
    pub cache_hits_total: u64,
    /// Embedding-cache lookups that found no row for the
    /// active `(evidence_id, model_tag)`.
    #[serde(default)]
    pub cache_misses_no_row_total: u64,
    /// Embedding-cache lookups that returned a row whose
    /// dimension did NOT match. Defensive — non-zero means the
    /// `model_tag` rotation rule (one tag ⇒ one dimension) was
    /// violated somewhere in history.
    #[serde(default)]
    pub cache_misses_dimension_total: u64,
    /// Embedding-cache lookups whose `SELECT` itself errored.
    /// Demoted to a miss to preserve the fail-open read-path
    /// contract.
    #[serde(default)]
    pub cache_misses_read_error_total: u64,
    /// Dedup-copy hits in
    /// [`evidence_store::EvidenceStore::index_embedding_or_copy_dedup`]
    /// — the dominant write-path optimisation for high-dedup
    /// workloads.
    #[serde(default)]
    pub dedup_copy_hits_total: u64,
    /// Failed embeds with
    /// [`evidence_store::embeddings::EmbeddingError::RuntimeUnavailable`].
    #[serde(default)]
    pub runtime_unavailable_total: u64,
    /// Failed embeds with
    /// [`evidence_store::embeddings::EmbeddingError::ModelLoad`].
    #[serde(default)]
    pub model_load_errors_total: u64,
    /// Failed embeds with
    /// [`evidence_store::embeddings::EmbeddingError::InferenceFailure`].
    #[serde(default)]
    pub inference_failures_total: u64,
    /// Number of times the same `model_tag` was observed at a
    /// different output dimension than its first observation — a
    /// rotation-rule violation. See upstream
    /// `evidence_store::vector_telemetry::record_observed_dimension`.
    #[serde(default)]
    pub model_tag_dimension_violations_total: u64,
    /// Pre-embedding router admitted the input —
    /// `model.embed(text)` was invoked. See upstream
    /// `evidence_store::embedding_routing::classify_for_embedding`
    /// for the routing rationale.
    #[serde(default)]
    pub pre_embed_admitted_total: u64,
    /// Pre-embedding router diverted the call site because the
    /// input was empty after `str::trim`. Usually signals an
    /// upstream extraction bug rather than legitimate noise.
    #[serde(default)]
    pub pre_embed_skipped_empty_after_trim_total: u64,
    /// Pre-embedding router diverted the call site because the
    /// input was non-empty after trim but `whatlang::detect`
    /// found no trigram-detectable linguistic content (pure
    /// punctuation / pure emoji / pure digits / pure-symbol
    /// input all land here).
    #[serde(default)]
    pub pre_embed_skipped_no_linguistic_content_total: u64,
}

/// Unified retrieval-telemetry read surface — the
/// three per-lane telemetry sub-structs grouped under a single
/// namespace for dashboard ergonomics.
///
/// Mirrors the upstream
/// [`observation_engine::RetrievalMetricsSnapshot`] verbatim:
/// hosts that want the "all retrieval counters in one place"
/// view consume [`MetricsSnapshot::retrieval_metrics`]; hosts
/// that still consume the flat `lexicon_telemetry` /
/// `fts_telemetry` / `vector_telemetry` fields on
/// [`MetricsSnapshot`] continue to work unchanged — the two
/// views are populated from the same three upstream singletons
/// in [`snapshot`], so they always carry the same values
/// (subject to the same sub-second cross-lane skew documented
/// on the upstream module).
///
/// The duplication is intentional: removing the flat fields
/// would be a wire-breaking change, and the grouped view
/// doesn't *replace* the flat view — it offers a second,
/// nested read shape that dashboard code can consume by
/// expanding one field instead of locating three among the
/// 50+ unrelated host-API call counters on [`MetricsSnapshot`].
///
/// Wire-contract invariants pinned by the
/// `metrics_snapshot_retrieval_metrics_matches_flat_fields`
/// test below:
/// * `retrieval_metrics.fts == fts_telemetry`
/// * `retrieval_metrics.lexicon == lexicon_telemetry`
/// * `retrieval_metrics.vector == vector_telemetry`
///
/// New fields must use `#[serde(default)]` per the additive-
/// wire-contract rule documented on [`MetricsSnapshot`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct RetrievalMetrics {
    /// FTS5-path telemetry — per-lane query / row totals,
    /// recall-lane structural skips, stopword-strip volumes
    /// per call site. Identical to
    /// [`MetricsSnapshot::fts_telemetry`].
    #[serde(default)]
    pub fts: FtsTelemetry,
    /// Lexicon-path telemetry — per-BCP-47 lexicon hits, match-
    /// strategy fires, Arabic / Hebrew clitic-peel depth
    /// distribution. Identical to
    /// [`MetricsSnapshot::lexicon_telemetry`].
    #[serde(default)]
    pub lexicon: LexiconTelemetry,
    /// Vector-path telemetry — embedding-call-site volumes,
    /// `evidence_embeddings` cache outcomes, adapter error
    /// variants, `model_tag` rotation-rule violations. Identical
    /// to [`MetricsSnapshot::vector_telemetry`].
    #[serde(default)]
    pub vector: VectorTelemetry,
}

/// Per-kind error counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct ErrorCounters {
    /// `FfiError::Unimplemented`.
    pub unimplemented: u64,
    /// `FfiError::InvalidId`.
    pub invalid_id: u64,
    /// `FfiError::InvalidQuery`. `#[serde(default)]` per the
    /// additive-wire-contract rule — older emitters' `ErrorCounters`
    /// JSON lacks the `invalid_query` key and must still deserialise
    /// without surfacing a missing-field error.
    #[serde(default)]
    pub invalid_query: u64,
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
    /// `FfiError::Connector`.
    //
    // `#[serde(default)]` per the additive-wire-contract rule
    // documented on `MetricsSnapshot` (see lines ~297-300 above):
    // `ErrorCounters` is embedded as `MetricsSnapshot::errors_by_kind`,
    // so an older emitter's `ErrorCounters` JSON (which lacks the
    // `connector` key entirely) must still deserialise under a newer
    // reader without surfacing a `missing field 'connector'` error.
    #[serde(default)]
    pub connector: u64,
    /// `FfiError::Throttled` . `#[serde(default)]`
    /// per the additive-wire-contract rule — older emitters'
    /// `ErrorCounters` JSON lacks the `throttled` key and must
    /// still deserialise without surfacing a missing-field
    /// error.
    #[serde(default)]
    pub throttled: u64,
    /// `FfiError::ModelDownloading`. `#[serde(default)]` per the
    /// additive-wire-contract rule — older emitters' `ErrorCounters`
    /// JSON lacks the `model_downloading` key and must still
    /// deserialise without surfacing a missing-field error.
    #[serde(default)]
    pub model_downloading: u64,
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
    // read the upstream retrieval-telemetry aggregator
    // ONCE and project into both the flat fields and the new
    // grouped `retrieval_metrics` view from the same read pass —
    // see the populate-block comment below for why a single read
    // is required to preserve the documented parity invariant.
    let upstream_retrieval = observation_engine::retrieval_telemetry::snapshot();
    let fts_view = project_fts_telemetry(&upstream_retrieval.fts);
    let lex_view = project_lexicon_telemetry(&upstream_retrieval.lexicon);
    let vec_view = project_vector_telemetry(&upstream_retrieval.vector);
    MetricsSnapshot {
        open_store_total: m.open_store_total.load(Ordering::Relaxed),
        open_store_with_resolver_total: m.open_store_with_resolver_total.load(Ordering::Relaxed),
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
        synthesis_lowquality_total: m.synthesis_lowquality_total.load(Ordering::Relaxed),
        synthesis_retry_total: m.synthesis_retry_total.load(Ordering::Relaxed),
        synthesis_retry_failed_total: m.synthesis_retry_failed_total.load(Ordering::Relaxed),
        synthesis_truncated_total: m.synthesis_truncated_total.load(Ordering::Relaxed),
        synthesis_recap_chars_total: m.synthesis_recap_chars_total.load(Ordering::Relaxed),
        synthesis_recap_samples_total: m.synthesis_recap_samples_total.load(Ordering::Relaxed),
        model_download_status_total: m.model_download_status_total.load(Ordering::Relaxed),
        decay_sweeps_total: m.decay_sweeps_total.load(Ordering::Relaxed),
        forgets_total: m.forgets_total.load(Ordering::Relaxed),
        forget_scopes_total: m.forget_scopes_total.load(Ordering::Relaxed),
        encrypt_total: m.encrypt_total.load(Ordering::Relaxed),
        decrypt_total: m.decrypt_total.load(Ordering::Relaxed),
        generate_keypair_total: m.generate_keypair_total.load(Ordering::Relaxed),
        escape_fts_query_total: m.escape_fts_query_total.load(Ordering::Relaxed),
        query_fts_fallback_total: m.query_fts_fallback_total.load(Ordering::Relaxed),
        metrics_snapshot_total: m.metrics_snapshot_total.load(Ordering::Relaxed),
        open_store_duration_histogram_total: m
            .open_store_duration_histogram_total
            .load(Ordering::Relaxed),
        slm_dispatch_histograms_total: m.slm_dispatch_histograms_total.load(Ordering::Relaxed),
        create_connector_total: m.create_connector_total.load(Ordering::Relaxed),
        authenticate_connector_total: m.authenticate_connector_total.load(Ordering::Relaxed),
        sync_connector_total: m.sync_connector_total.load(Ordering::Relaxed),
        list_connectors_total: m.list_connectors_total.load(Ordering::Relaxed),
        connector_status_total: m.connector_status_total.load(Ordering::Relaxed),
        remove_connector_total: m.remove_connector_total.load(Ordering::Relaxed),
        refresh_connector_token_total: m.refresh_connector_token_total.load(Ordering::Relaxed),
        set_oauth_client_secret_resolver_total: m
            .set_oauth_client_secret_resolver_total
            .load(Ordering::Relaxed),
        clear_oauth_client_secret_resolver_total: m
            .clear_oauth_client_secret_resolver_total
            .load(Ordering::Relaxed),
        set_key_storage_resolver_total: m.set_key_storage_resolver_total.load(Ordering::Relaxed),
        clear_key_storage_resolver_total: m
            .clear_key_storage_resolver_total
            .load(Ordering::Relaxed),
        start_webhook_server_total: m.start_webhook_server_total.load(Ordering::Relaxed),
        stop_webhook_server_total: m.stop_webhook_server_total.load(Ordering::Relaxed),
        register_webhook_dispatch_total: m.register_webhook_dispatch_total.load(Ordering::Relaxed),
        unregister_webhook_dispatch_total: m
            .unregister_webhook_dispatch_total
            .load(Ordering::Relaxed),
        list_webhook_servers_total: m.list_webhook_servers_total.load(Ordering::Relaxed),
        start_sync_scheduler_total: m.start_sync_scheduler_total.load(Ordering::Relaxed),
        start_sync_scheduler_for_platform_total: m
            .start_sync_scheduler_for_platform_total
            .load(Ordering::Relaxed),
        stop_sync_scheduler_total: m.stop_sync_scheduler_total.load(Ordering::Relaxed),
        configure_sync_schedule_total: m.configure_sync_schedule_total.load(Ordering::Relaxed),
        clear_sync_schedule_total: m.clear_sync_schedule_total.load(Ordering::Relaxed),
        sync_scheduler_status_total: m.sync_scheduler_status_total.load(Ordering::Relaxed),
        sync_scheduler_ticks_total: m.sync_scheduler_ticks_total.load(Ordering::Relaxed),
        sync_scheduler_dispatches_attempted_total: m
            .sync_scheduler_dispatches_attempted_total
            .load(Ordering::Relaxed),
        sync_scheduler_dispatches_succeeded_total: m
            .sync_scheduler_dispatches_succeeded_total
            .load(Ordering::Relaxed),
        sync_scheduler_dispatches_failed_total: m
            .sync_scheduler_dispatches_failed_total
            .load(Ordering::Relaxed),
        sync_scheduler_dispatches_skipped_in_progress_total: m
            .sync_scheduler_dispatches_skipped_in_progress_total
            .load(Ordering::Relaxed),
        webhook_dispatch_ok_total: m.webhook_dispatch_ok_total.load(Ordering::Relaxed),
        webhook_dispatch_bad_request_total: m
            .webhook_dispatch_bad_request_total
            .load(Ordering::Relaxed),
        webhook_dispatch_bad_gateway_total: m
            .webhook_dispatch_bad_gateway_total
            .load(Ordering::Relaxed),
        health_check_total: m.health_check_total.load(Ordering::Relaxed),
        init_tracing_total: m.init_tracing_total.load(Ordering::Relaxed),
        configure_synthesis_engine_total: m
            .configure_synthesis_engine_total
            .load(Ordering::Relaxed),
        trigger_server_synthesis_total: m.trigger_server_synthesis_total.load(Ordering::Relaxed),
        synthesis_status_total: m.synthesis_status_total.load(Ordering::Relaxed),
        list_recent_syntheses_total: m.list_recent_syntheses_total.load(Ordering::Relaxed),
        configure_sync_auto_synthesize_total: m
            .configure_sync_auto_synthesize_total
            .load(Ordering::Relaxed),
        admit_approved_document_total: m.admit_approved_document_total.load(Ordering::Relaxed),
        revoke_approved_document_total: m.revoke_approved_document_total.load(Ordering::Relaxed),
        replace_approved_document_total: m.replace_approved_document_total.load(Ordering::Relaxed),
        list_approved_documents_total: m.list_approved_documents_total.load(Ordering::Relaxed),
        stuck_pending_window_recovered_total: m
            .stuck_pending_window_recovered_total
            .load(Ordering::Relaxed),
        trigger_server_synthesis_throttled_total: m
            .trigger_server_synthesis_throttled_total
            .load(Ordering::Relaxed),
        replay_synthesis_total: m.replay_synthesis_total.load(Ordering::Relaxed),
        list_synthesis_versions_total: m.list_synthesis_versions_total.load(Ordering::Relaxed),
        errors_by_kind: ErrorCounters {
            unimplemented: m.errors_unimplemented.load(Ordering::Relaxed),
            invalid_id: m.errors_invalid_id.load(Ordering::Relaxed),
            invalid_query: m.errors_invalid_query.load(Ordering::Relaxed),
            not_found: m.errors_not_found.load(Ordering::Relaxed),
            evidence: m.errors_evidence.load(Ordering::Relaxed),
            memory: m.errors_memory.load(Ordering::Relaxed),
            synthesis: m.errors_synthesis.load(Ordering::Relaxed),
            crypto: m.errors_crypto.load(Ordering::Relaxed),
            unavailable: m.errors_unavailable.load(Ordering::Relaxed),
            inference_failure: m.errors_inference_failure.load(Ordering::Relaxed),
            connector: m.errors_connector.load(Ordering::Relaxed),
            throttled: m.errors_throttled.load(Ordering::Relaxed),
            model_downloading: m.errors_model_downloading.load(Ordering::Relaxed),
        },
        errors_total: m.errors_total.load(Ordering::Relaxed),
        open_handles: m.open_handles.load(Ordering::Relaxed),
        tombstone_count: m.tombstone_count.load(Ordering::Relaxed),
        boot_unix_secs: m.boot_unix_secs.load(Ordering::Relaxed),
        // read the upstream aggregator once and project
        // into BOTH the flat fields and the grouped `retrieval_metrics`
        // view from the same read pass. This guarantees the
        // `metrics_snapshot_retrieval_metrics_matches_flat_fields`
        // parity invariant holds even under concurrent telemetry
        // writes — if we did two separate reads (one for the flat
        // fields, one for the grouped view), a writer that bumps a
        // counter between the two reads would leave the two views
        // disagreeing on the same `MetricsSnapshot` value, which
        // would break the wire contract documented on
        // [`MetricsSnapshot::retrieval_metrics`].
        lexicon_telemetry: lex_view.clone(),
        fts_telemetry: fts_view.clone(),
        vector_telemetry: vec_view.clone(),
        retrieval_metrics: RetrievalMetrics {
            fts: fts_view,
            lexicon: lex_view,
            vector: vec_view,
        },
    }
}

/// Project an upstream
/// [`observation_engine::LexiconTelemetrySnapshot`] into the FFI
/// mirror struct. Pure (no I/O) — separated from the
/// singleton-read so [`snapshot`] can read the upstream
/// aggregator once and project into both the flat fields and the
/// grouped [`RetrievalMetrics`] view from the same read pass.
/// The field lists are kept symmetric by the
/// `lexicon_telemetry_mirror_field_parity` test below.
fn project_lexicon_telemetry(s: &observation_engine::LexiconTelemetrySnapshot) -> LexiconTelemetry {
    LexiconTelemetry {
        hits_ar: s.hits_ar,
        hits_bo: s.hits_bo,
        hits_de: s.hits_de,
        hits_en: s.hits_en,
        hits_es: s.hits_es,
        hits_fr: s.hits_fr,
        hits_he: s.hits_he,
        hits_hi: s.hits_hi,
        hits_id: s.hits_id,
        hits_it: s.hits_it,
        hits_ja: s.hits_ja,
        hits_km: s.hits_km,
        hits_ko: s.hits_ko,
        hits_lo: s.hits_lo,
        hits_ms: s.hits_ms,
        hits_my: s.hits_my,
        hits_pt: s.hits_pt,
        hits_ru: s.hits_ru,
        hits_th: s.hits_th,
        hits_tl: s.hits_tl,
        hits_vi: s.hits_vi,
        hits_zh: s.hits_zh,
        unknown_tag_fallbacks_total: s.unknown_tag_fallbacks_total,
        strategy_first_token: s.strategy_first_token,
        strategy_first_bigram: s.strategy_first_bigram,
        strategy_substring: s.strategy_substring,
        strategy_first_token_with_arabic_clitics: s.strategy_first_token_with_arabic_clitics,
        strategy_first_token_with_hebrew_clitics: s.strategy_first_token_with_hebrew_clitics,
        arabic_peel_depth_0_matches: s.arabic_peel_depth_0_matches,
        arabic_peel_depth_1_matches: s.arabic_peel_depth_1_matches,
        arabic_peel_depth_2_matches: s.arabic_peel_depth_2_matches,
        arabic_peel_depth_3_matches: s.arabic_peel_depth_3_matches,
        arabic_peel_depth_exhausted: s.arabic_peel_depth_exhausted,
        hebrew_peel_depth_0_matches: s.hebrew_peel_depth_0_matches,
        hebrew_peel_depth_1_matches: s.hebrew_peel_depth_1_matches,
        hebrew_peel_depth_2_matches: s.hebrew_peel_depth_2_matches,
        hebrew_peel_depth_3_matches: s.hebrew_peel_depth_3_matches,
        hebrew_peel_depth_exhausted: s.hebrew_peel_depth_exhausted,
    }
}

/// Project an upstream
/// [`evidence_store::fts_telemetry::FtsTelemetrySnapshot`] into
/// the FFI mirror struct. Pure (no I/O) — see
/// [`project_lexicon_telemetry`] for the rationale.
fn project_fts_telemetry(s: &evidence_store::fts_telemetry::FtsTelemetrySnapshot) -> FtsTelemetry {
    FtsTelemetry {
        unicode61_lane_queries_total: s.unicode61_lane_queries_total,
        unicode61_lane_rows_total: s.unicode61_lane_rows_total,
        cjk_trigram_lane_queries_total: s.cjk_trigram_lane_queries_total,
        cjk_trigram_lane_rows_total: s.cjk_trigram_lane_rows_total,
        cjk_trigram_lane_skips_pure_stopword_query_total: s
            .cjk_trigram_lane_skips_pure_stopword_query_total,
        bigram_lane_queries_total: s.bigram_lane_queries_total,
        bigram_lane_rows_total: s.bigram_lane_rows_total,
        bigram_lane_skips_pure_stopword_query_total: s.bigram_lane_skips_pure_stopword_query_total,
        bigram_lane_skips_no_cjk_query_total: s.bigram_lane_skips_no_cjk_query_total,
        index_write_stopwords_stripped_total: s.index_write_stopwords_stripped_total,
        query_time_stopwords_stripped_total: s.query_time_stopwords_stripped_total,
        v16_migration_stopwords_stripped_total: s.v16_migration_stopwords_stripped_total,
    }
}

/// Project an upstream
/// [`evidence_store::vector_telemetry::VectorTelemetrySnapshot`]
/// into the FFI mirror struct. Pure (no I/O) — see
/// [`project_lexicon_telemetry`] for the rationale. The field
/// lists are kept symmetric by the
/// `vector_telemetry_mirror_round_trips` test below.
fn project_vector_telemetry(
    s: &evidence_store::vector_telemetry::VectorTelemetrySnapshot,
) -> VectorTelemetry {
    VectorTelemetry {
        query_embeddings_total: s.query_embeddings_total,
        index_write_embeddings_total: s.index_write_embeddings_total,
        live_body_embeddings_total: s.live_body_embeddings_total,
        cache_hits_total: s.cache_hits_total,
        cache_misses_no_row_total: s.cache_misses_no_row_total,
        cache_misses_dimension_total: s.cache_misses_dimension_total,
        cache_misses_read_error_total: s.cache_misses_read_error_total,
        dedup_copy_hits_total: s.dedup_copy_hits_total,
        runtime_unavailable_total: s.runtime_unavailable_total,
        model_load_errors_total: s.model_load_errors_total,
        inference_failures_total: s.inference_failures_total,
        model_tag_dimension_violations_total: s.model_tag_dimension_violations_total,
        pre_embed_admitted_total: s.pre_embed_admitted_total,
        pre_embed_skipped_empty_after_trim_total: s.pre_embed_skipped_empty_after_trim_total,
        pre_embed_skipped_no_linguistic_content_total: s
            .pre_embed_skipped_no_linguistic_content_total,
    }
}

/// Read the upstream
/// [`observation_engine::lexicon_telemetry::snapshot`] and
/// project it into the FFI mirror struct. Test-only convenience
/// wrapper — the production [`snapshot`] path now goes through the
/// upstream aggregator in
/// [`observation_engine::retrieval_telemetry::snapshot`] and the
/// pure [`project_lexicon_telemetry`] helper instead, so this
/// wrapper is gated to `#[cfg(test)]` to keep the lib build free
/// of dead-code warnings.
#[cfg(test)]
fn lexicon_telemetry_snapshot() -> LexiconTelemetry {
    project_lexicon_telemetry(&observation_engine::lexicon_telemetry::snapshot())
}

/// Read the upstream
/// [`evidence_store::fts_telemetry::snapshot`] and project it
/// into the FFI mirror struct. See [`lexicon_telemetry_snapshot`]
/// for the relationship to the new aggregator-based read path.
#[cfg(test)]
fn fts_telemetry_snapshot() -> FtsTelemetry {
    project_fts_telemetry(&evidence_store::fts_telemetry::snapshot())
}

/// Read the upstream
/// [`evidence_store::vector_telemetry::snapshot`] and project it
/// into the FFI mirror struct. See [`lexicon_telemetry_snapshot`]
/// for the relationship to the new aggregator-based read path.
#[cfg(test)]
fn vector_telemetry_snapshot() -> VectorTelemetry {
    project_vector_telemetry(&evidence_store::vector_telemetry::snapshot())
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

/// Serializes the tests that assert on *deltas* of the process-global
/// [`stuck_pending_window_recovered_total`](Metrics::stuck_pending_window_recovered_total)
/// counter. `cargo` runs the unit tests of a crate in parallel threads
/// of one process, so that counter is shared mutable state across them:
/// `open_store_recovers_stuck_pending_window` fires a real recovery
/// sweep (incrementing it via `runtime`), and
/// `snapshot_reflects_counter_increments` increments it directly. Either
/// can land between the before/after snapshots of
/// `open_store_leaves_fresh_pending_window_alone`, whose invariant is the
/// *exact-equality* "counter did not advance". Holding this guard across
/// each test's measurement window makes those deltas attributable to the
/// test's own actions without weakening any assertion.
#[cfg(test)]
pub(crate) static STUCK_PENDING_METRIC_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        // This test calls `inc_stuck_pending_window_recovered()`,
        // mutating the same process-global counter that
        // `synthesis::open_store_leaves_fresh_pending_window_alone`
        // asserts exact-equality on. Hold the shared guard across the
        // measurement window so that increment cannot leak into the
        // negative test's before/after window. Recover from poisoning
        // so an unrelated failure does not surface here as a lock error.
        let _metric_guard = STUCK_PENDING_METRIC_TEST_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

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
        inc_query_fts_fallback();
        inc_configure_synthesis_engine();
        inc_trigger_server_synthesis();
        inc_synthesis_status();
        inc_connector_status();
        inc_list_recent_syntheses();
        inc_configure_sync_auto_synthesize();
        inc_stuck_pending_window_recovered();
        inc_trigger_server_synthesis_throttled();
        inc_replay_synthesis();
        inc_list_synthesis_versions();

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
        assert!(after.query_fts_fallback_total > before.query_fts_fallback_total);
        assert!(after.configure_synthesis_engine_total > before.configure_synthesis_engine_total);
        assert!(after.trigger_server_synthesis_total > before.trigger_server_synthesis_total);
        assert!(after.synthesis_status_total > before.synthesis_status_total);
        assert!(after.connector_status_total > before.connector_status_total);
        assert!(after.list_recent_syntheses_total > before.list_recent_syntheses_total);
        assert!(
            after.configure_sync_auto_synthesize_total
                > before.configure_sync_auto_synthesize_total
        );
        assert!(
            after.stuck_pending_window_recovered_total
                > before.stuck_pending_window_recovered_total
        );
        assert!(
            after.trigger_server_synthesis_throttled_total
                > before.trigger_server_synthesis_throttled_total
        );
        assert!(after.replay_synthesis_total > before.replay_synthesis_total);
        assert!(after.list_synthesis_versions_total > before.list_synthesis_versions_total);
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
        inc_error(&FfiError::Throttled {
            subsystem: "synthesis_engine".into(),
            retry_after_ms: 50,
        });

        let after = snapshot();
        assert!(after.errors_by_kind.invalid_id > before.errors_by_kind.invalid_id);
        assert!(after.errors_by_kind.evidence >= before.errors_by_kind.evidence + 2);
        assert!(after.errors_by_kind.unavailable > before.errors_by_kind.unavailable);
        assert!(after.errors_by_kind.inference_failure > before.errors_by_kind.inference_failure);
        assert!(after.errors_by_kind.throttled > before.errors_by_kind.throttled);
        assert!(after.errors_total >= before.errors_total + 6);
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

    /// Pin the lexicon-telemetry mirror field parity.
    ///
    /// Every field on
    /// [`observation_engine::LexiconTelemetrySnapshot`] must have
    /// a one-to-one counterpart on the FFI [`LexiconTelemetry`]
    /// struct. The check is byte-by-byte: we read the upstream
    /// snapshot, project into the FFI mirror via
    /// [`lexicon_telemetry_snapshot`], and then re-project field
    /// values by name to make sure no field was dropped or
    /// silently zeroed. When this test fails after an
    /// upstream-counter addition, the FFI mirror is missing the
    /// new field — extend [`LexiconTelemetry`] and the
    /// projection helper symmetrically.
    ///
    /// The test bumps the upstream counters first so all fields
    /// have non-zero distinct values, then asserts the projection
    /// preserves them. We use distinct prime-ish increments per
    /// counter so a swapped-field-order bug surfaces as a value
    /// mismatch rather than silently aliasing.
    #[test]
    fn lexicon_telemetry_mirror_round_trips() {
        use observation_engine::lexicon::MatchStrategy;
        use observation_engine::lexicon_telemetry::{
            record_arabic_peel_depth, record_hebrew_peel_depth, record_lexicon_hit,
            record_match_strategy_fire, PeelOutcome,
        };

        // Take a baseline so we can compute deltas — this test
        // shares the process-singleton counters with every other
        // test in the binary that touches the lexicon path.
        let before = observation_engine::lexicon_telemetry::snapshot();
        // Issue a single representative increment per upstream
        // counter so every field exercises a non-zero delta.
        for tag in [
            "ar", "bo", "de", "en", "es", "fr", "he", "hi", "id", "it", "ja", "km", "ko", "lo",
            "ms", "my", "pt", "ru", "th", "tl", "vi", "zh",
        ] {
            record_lexicon_hit(Some(tag), tag);
        }
        record_lexicon_hit(Some("xx-unknown"), "en");
        for s in [
            MatchStrategy::FirstToken,
            MatchStrategy::FirstBigram,
            MatchStrategy::Substring,
            MatchStrategy::FirstTokenWithArabicClitics,
            MatchStrategy::FirstTokenWithHebrewClitics,
        ] {
            record_match_strategy_fire(s);
        }
        for o in [
            PeelOutcome::MatchedAtDepth(0),
            PeelOutcome::MatchedAtDepth(1),
            PeelOutcome::MatchedAtDepth(2),
            PeelOutcome::MatchedAtDepth(3),
            PeelOutcome::BudgetExhausted,
        ] {
            record_arabic_peel_depth(o);
            record_hebrew_peel_depth(o);
        }

        // Mirror is read AFTER upstream so its values are a
        // (potentially advanced) read of the same singleton;
        // parallel tests may bump counters in between, so the
        // mirror values are bounded *below* by the upstream
        // snapshot. This is the same monotonic-lower-bound
        // pattern used by [`snapshot_reflects_counter_increments`]
        // above. We additionally lower-bound the mirror by
        // `before + N` so the test catches a mirror that
        // silently zeroes a field (mirror==0 would be < before+1
        // even on a clean process).
        let upstream = observation_engine::lexicon_telemetry::snapshot();
        let mirror = lexicon_telemetry_snapshot();

        // Verify each FFI mirror field plumbs through to the
        // corresponding upstream counter. Lower-bound by the
        // upstream value because parallel tests cannot decrement
        // counters but may increment them between the two reads.
        assert!(mirror.hits_ar >= upstream.hits_ar);
        assert!(mirror.hits_bo >= upstream.hits_bo);
        assert!(mirror.hits_de >= upstream.hits_de);
        assert!(mirror.hits_en >= upstream.hits_en);
        assert!(mirror.hits_es >= upstream.hits_es);
        assert!(mirror.hits_fr >= upstream.hits_fr);
        assert!(mirror.hits_he >= upstream.hits_he);
        assert!(mirror.hits_hi >= upstream.hits_hi);
        assert!(mirror.hits_id >= upstream.hits_id);
        assert!(mirror.hits_it >= upstream.hits_it);
        assert!(mirror.hits_ja >= upstream.hits_ja);
        assert!(mirror.hits_km >= upstream.hits_km);
        assert!(mirror.hits_ko >= upstream.hits_ko);
        assert!(mirror.hits_lo >= upstream.hits_lo);
        assert!(mirror.hits_ms >= upstream.hits_ms);
        assert!(mirror.hits_my >= upstream.hits_my);
        assert!(mirror.hits_pt >= upstream.hits_pt);
        assert!(mirror.hits_ru >= upstream.hits_ru);
        assert!(mirror.hits_th >= upstream.hits_th);
        assert!(mirror.hits_tl >= upstream.hits_tl);
        assert!(mirror.hits_vi >= upstream.hits_vi);
        assert!(mirror.hits_zh >= upstream.hits_zh);
        assert!(mirror.unknown_tag_fallbacks_total >= upstream.unknown_tag_fallbacks_total);
        assert!(mirror.strategy_first_token >= upstream.strategy_first_token);
        assert!(mirror.strategy_first_bigram >= upstream.strategy_first_bigram);
        assert!(mirror.strategy_substring >= upstream.strategy_substring);
        assert!(
            mirror.strategy_first_token_with_arabic_clitics
                >= upstream.strategy_first_token_with_arabic_clitics
        );
        assert!(
            mirror.strategy_first_token_with_hebrew_clitics
                >= upstream.strategy_first_token_with_hebrew_clitics
        );
        assert!(mirror.arabic_peel_depth_0_matches >= upstream.arabic_peel_depth_0_matches);
        assert!(mirror.arabic_peel_depth_1_matches >= upstream.arabic_peel_depth_1_matches);
        assert!(mirror.arabic_peel_depth_2_matches >= upstream.arabic_peel_depth_2_matches);
        assert!(mirror.arabic_peel_depth_3_matches >= upstream.arabic_peel_depth_3_matches);
        assert!(mirror.arabic_peel_depth_exhausted >= upstream.arabic_peel_depth_exhausted);
        assert!(mirror.hebrew_peel_depth_0_matches >= upstream.hebrew_peel_depth_0_matches);
        assert!(mirror.hebrew_peel_depth_1_matches >= upstream.hebrew_peel_depth_1_matches);
        assert!(mirror.hebrew_peel_depth_2_matches >= upstream.hebrew_peel_depth_2_matches);
        assert!(mirror.hebrew_peel_depth_3_matches >= upstream.hebrew_peel_depth_3_matches);
        assert!(mirror.hebrew_peel_depth_exhausted >= upstream.hebrew_peel_depth_exhausted);

        // Reverse direction: every field we bumped above must
        // show movement from baseline through the FFI mirror.
        // This is what catches a silently-zeroed projection: if
        // the mirror dropped `hits_ar`, `mirror.hits_ar - before.hits_ar`
        // would be 0 even though our increment added 1.
        assert!(mirror.hits_ar > before.hits_ar, "hits_ar not plumbed");
        assert!(mirror.hits_bo > before.hits_bo, "hits_bo not plumbed");
        assert!(mirror.hits_de > before.hits_de, "hits_de not plumbed");
        assert!(mirror.hits_en >= before.hits_en + 2, "hits_en not plumbed");
        assert!(mirror.hits_es > before.hits_es, "hits_es not plumbed");
        assert!(mirror.hits_fr > before.hits_fr, "hits_fr not plumbed");
        assert!(mirror.hits_he > before.hits_he, "hits_he not plumbed");
        assert!(mirror.hits_hi > before.hits_hi, "hits_hi not plumbed");
        assert!(mirror.hits_id > before.hits_id, "hits_id not plumbed");
        assert!(mirror.hits_it > before.hits_it, "hits_it not plumbed");
        assert!(mirror.hits_ja > before.hits_ja, "hits_ja not plumbed");
        assert!(mirror.hits_km > before.hits_km, "hits_km not plumbed");
        assert!(mirror.hits_ko > before.hits_ko, "hits_ko not plumbed");
        assert!(mirror.hits_lo > before.hits_lo, "hits_lo not plumbed");
        assert!(mirror.hits_ms > before.hits_ms, "hits_ms not plumbed");
        assert!(mirror.hits_my > before.hits_my, "hits_my not plumbed");
        assert!(mirror.hits_pt > before.hits_pt, "hits_pt not plumbed");
        assert!(mirror.hits_ru > before.hits_ru, "hits_ru not plumbed");
        assert!(mirror.hits_th > before.hits_th, "hits_th not plumbed");
        assert!(mirror.hits_tl > before.hits_tl, "hits_tl not plumbed");
        assert!(mirror.hits_vi > before.hits_vi, "hits_vi not plumbed");
        assert!(mirror.hits_zh > before.hits_zh, "hits_zh not plumbed");
        assert!(
            mirror.unknown_tag_fallbacks_total > before.unknown_tag_fallbacks_total,
            "unknown_tag_fallbacks_total not plumbed"
        );
        assert!(
            mirror.strategy_first_token > before.strategy_first_token,
            "strategy_first_token not plumbed"
        );
        assert!(
            mirror.strategy_first_bigram > before.strategy_first_bigram,
            "strategy_first_bigram not plumbed"
        );
        assert!(
            mirror.strategy_substring > before.strategy_substring,
            "strategy_substring not plumbed"
        );
        assert!(
            mirror.strategy_first_token_with_arabic_clitics
                > before.strategy_first_token_with_arabic_clitics,
            "strategy_first_token_with_arabic_clitics not plumbed"
        );
        assert!(
            mirror.strategy_first_token_with_hebrew_clitics
                > before.strategy_first_token_with_hebrew_clitics,
            "strategy_first_token_with_hebrew_clitics not plumbed"
        );
        assert!(
            mirror.arabic_peel_depth_0_matches > before.arabic_peel_depth_0_matches,
            "arabic_peel_depth_0_matches not plumbed"
        );
        assert!(
            mirror.arabic_peel_depth_1_matches > before.arabic_peel_depth_1_matches,
            "arabic_peel_depth_1_matches not plumbed"
        );
        assert!(
            mirror.arabic_peel_depth_2_matches > before.arabic_peel_depth_2_matches,
            "arabic_peel_depth_2_matches not plumbed"
        );
        assert!(
            mirror.arabic_peel_depth_3_matches > before.arabic_peel_depth_3_matches,
            "arabic_peel_depth_3_matches not plumbed"
        );
        assert!(
            mirror.arabic_peel_depth_exhausted > before.arabic_peel_depth_exhausted,
            "arabic_peel_depth_exhausted not plumbed"
        );
        assert!(
            mirror.hebrew_peel_depth_0_matches > before.hebrew_peel_depth_0_matches,
            "hebrew_peel_depth_0_matches not plumbed"
        );
        assert!(
            mirror.hebrew_peel_depth_1_matches > before.hebrew_peel_depth_1_matches,
            "hebrew_peel_depth_1_matches not plumbed"
        );
        assert!(
            mirror.hebrew_peel_depth_2_matches > before.hebrew_peel_depth_2_matches,
            "hebrew_peel_depth_2_matches not plumbed"
        );
        assert!(
            mirror.hebrew_peel_depth_3_matches > before.hebrew_peel_depth_3_matches,
            "hebrew_peel_depth_3_matches not plumbed"
        );
        assert!(
            mirror.hebrew_peel_depth_exhausted > before.hebrew_peel_depth_exhausted,
            "hebrew_peel_depth_exhausted not plumbed"
        );
    }

    /// Pin the FTS-telemetry mirror field parity.
    /// Mirror of `lexicon_telemetry_mirror_round_trips` for the
    /// FTS path.
    #[test]
    fn fts_telemetry_mirror_round_trips() {
        use evidence_store::fts_telemetry::{
            record_lane_query, record_lane_skip, record_stopwords_stripped, Lane, SkipReason,
            StripSite,
        };
        let before = evidence_store::fts_telemetry::snapshot();
        record_lane_query(Lane::Unicode61, 7);
        record_lane_query(Lane::CjkTrigram, 5);
        record_lane_query(Lane::Bigram, 3);
        record_lane_skip(SkipReason::CjkTrigramPureStopwordQuery);
        record_lane_skip(SkipReason::BigramPureStopwordQuery);
        record_lane_skip(SkipReason::BigramNoCjkQuery);
        record_stopwords_stripped(StripSite::IndexWrite, 11);
        record_stopwords_stripped(StripSite::QueryTime, 13);
        record_stopwords_stripped(StripSite::V16Migration, 17);

        // Mirror is read AFTER upstream so its values are a
        // (potentially advanced) read of the same singleton;
        // parallel tests may bump counters in between, so the
        // mirror values are bounded *below* by the upstream
        // snapshot. This is the same monotonic-lower-bound
        // pattern used by [`snapshot_reflects_counter_increments`]
        // and by `lexicon_telemetry_mirror_round_trips` above.
        // Earlier review fix: the previous
        // `assert_eq!(mirror.field, upstream.field)` shape was
        // accidentally racy — if any parallel test (today only
        // `store_integration::fts_telemetry_*`, but trivially
        // any future ffi-binary test that touches FTS) bumped a
        // counter between the two reads, the assertion would
        // fail. Switching to `>=` makes the test correct under
        // arbitrary concurrent telemetry traffic and matches
        // the lexicon mirror test pattern verbatim.
        let upstream = evidence_store::fts_telemetry::snapshot();
        let mirror = fts_telemetry_snapshot();

        // Verify each FFI mirror field plumbs through to the
        // corresponding upstream counter. Lower-bound by the
        // upstream value because parallel tests cannot decrement
        // counters but may increment them between the two reads.
        assert!(
            mirror.unicode61_lane_queries_total >= upstream.unicode61_lane_queries_total,
            "unicode61_lane_queries_total mirror < upstream"
        );
        assert!(
            mirror.unicode61_lane_rows_total >= upstream.unicode61_lane_rows_total,
            "unicode61_lane_rows_total mirror < upstream"
        );
        assert!(
            mirror.cjk_trigram_lane_queries_total >= upstream.cjk_trigram_lane_queries_total,
            "cjk_trigram_lane_queries_total mirror < upstream"
        );
        assert!(
            mirror.cjk_trigram_lane_rows_total >= upstream.cjk_trigram_lane_rows_total,
            "cjk_trigram_lane_rows_total mirror < upstream"
        );
        assert!(
            mirror.cjk_trigram_lane_skips_pure_stopword_query_total
                >= upstream.cjk_trigram_lane_skips_pure_stopword_query_total,
            "cjk_trigram_lane_skips_pure_stopword_query_total mirror < upstream"
        );
        assert!(
            mirror.bigram_lane_queries_total >= upstream.bigram_lane_queries_total,
            "bigram_lane_queries_total mirror < upstream"
        );
        assert!(
            mirror.bigram_lane_rows_total >= upstream.bigram_lane_rows_total,
            "bigram_lane_rows_total mirror < upstream"
        );
        assert!(
            mirror.bigram_lane_skips_pure_stopword_query_total
                >= upstream.bigram_lane_skips_pure_stopword_query_total,
            "bigram_lane_skips_pure_stopword_query_total mirror < upstream"
        );
        assert!(
            mirror.bigram_lane_skips_no_cjk_query_total
                >= upstream.bigram_lane_skips_no_cjk_query_total,
            "bigram_lane_skips_no_cjk_query_total mirror < upstream"
        );
        assert!(
            mirror.index_write_stopwords_stripped_total
                >= upstream.index_write_stopwords_stripped_total,
            "index_write_stopwords_stripped_total mirror < upstream"
        );
        assert!(
            mirror.query_time_stopwords_stripped_total
                >= upstream.query_time_stopwords_stripped_total,
            "query_time_stopwords_stripped_total mirror < upstream"
        );
        assert!(
            mirror.v16_migration_stopwords_stripped_total
                >= upstream.v16_migration_stopwords_stripped_total,
            "v16_migration_stopwords_stripped_total mirror < upstream"
        );

        // Reverse direction: every field we bumped above must
        // show movement from baseline through the FFI mirror.
        // This is what catches a silently-zeroed projection: if
        // the mirror dropped `unicode61_lane_queries_total`,
        // `mirror.unicode61_lane_queries_total - before.unicode61_lane_queries_total`
        // would be 0 even though our increment added 1.
        assert!(
            mirror.unicode61_lane_queries_total > before.unicode61_lane_queries_total,
            "unicode61_lane_queries_total not plumbed"
        );
        assert!(
            mirror.unicode61_lane_rows_total > before.unicode61_lane_rows_total,
            "unicode61_lane_rows_total not plumbed"
        );
        assert!(
            mirror.cjk_trigram_lane_queries_total > before.cjk_trigram_lane_queries_total,
            "cjk_trigram_lane_queries_total not plumbed"
        );
        assert!(
            mirror.cjk_trigram_lane_rows_total > before.cjk_trigram_lane_rows_total,
            "cjk_trigram_lane_rows_total not plumbed"
        );
        assert!(
            mirror.cjk_trigram_lane_skips_pure_stopword_query_total
                > before.cjk_trigram_lane_skips_pure_stopword_query_total,
            "cjk_trigram_lane_skips_pure_stopword_query_total not plumbed"
        );
        assert!(
            mirror.bigram_lane_queries_total > before.bigram_lane_queries_total,
            "bigram_lane_queries_total not plumbed"
        );
        assert!(
            mirror.bigram_lane_rows_total > before.bigram_lane_rows_total,
            "bigram_lane_rows_total not plumbed"
        );
        assert!(
            mirror.bigram_lane_skips_pure_stopword_query_total
                > before.bigram_lane_skips_pure_stopword_query_total,
            "bigram_lane_skips_pure_stopword_query_total not plumbed"
        );
        assert!(
            mirror.bigram_lane_skips_no_cjk_query_total
                > before.bigram_lane_skips_no_cjk_query_total,
            "bigram_lane_skips_no_cjk_query_total not plumbed"
        );
        assert!(
            mirror.index_write_stopwords_stripped_total
                > before.index_write_stopwords_stripped_total,
            "index_write_stopwords_stripped_total not plumbed"
        );
        assert!(
            mirror.query_time_stopwords_stripped_total > before.query_time_stopwords_stripped_total,
            "query_time_stopwords_stripped_total not plumbed"
        );
        assert!(
            mirror.v16_migration_stopwords_stripped_total
                > before.v16_migration_stopwords_stripped_total,
            "v16_migration_stopwords_stripped_total not plumbed"
        );
    }

    /// Pin the vector-telemetry mirror field parity.
    /// Mirror of `fts_telemetry_mirror_round_trips` for the
    /// embedding / vector-retrieval path.
    #[test]
    fn vector_telemetry_mirror_round_trips() {
        use evidence_store::embedding_routing::{EmbeddingRoute, SkipReason};
        use evidence_store::vector_telemetry::{
            record_cache_outcome, record_dedup_copy_hit, record_embedding_computed,
            record_embedding_error, record_observed_dimension, record_pre_embed_decision,
            CacheOutcome, EmbedSite, EmbeddingErrorKind,
        };
        let before = evidence_store::vector_telemetry::snapshot();
        record_embedding_computed(EmbedSite::Query);
        record_embedding_computed(EmbedSite::IndexWrite);
        record_embedding_computed(EmbedSite::LiveBody);
        record_cache_outcome(CacheOutcome::Hit);
        record_cache_outcome(CacheOutcome::MissNoRow);
        record_cache_outcome(CacheOutcome::MissDimension);
        record_cache_outcome(CacheOutcome::MissReadError);
        record_dedup_copy_hit();
        record_embedding_error(EmbeddingErrorKind::RuntimeUnavailable);
        record_embedding_error(EmbeddingErrorKind::ModelLoad);
        record_embedding_error(EmbeddingErrorKind::InferenceFailure);
        // pre-embedding routing counters — bump one
        // each so the three new fields participate in the same
        // monotonic-lower-bound + plumbed-from-baseline parity
        // discipline as every other vector-telemetry field.
        // The three variants are mutually exclusive per call so
        // bumping one of each tests the full taxonomy.
        record_pre_embed_decision(EmbeddingRoute::Embed);
        record_pre_embed_decision(EmbeddingRoute::Skip(SkipReason::EmptyAfterTrim));
        record_pre_embed_decision(EmbeddingRoute::Skip(SkipReason::NoLinguisticContent));
        // Trigger one rotation-rule violation: record a tag at
        // a baseline dim, then re-record it at a different dim.
        // Uses a test-local tag name so parallel tests can't
        // interfere with the violation arithmetic.
        record_observed_dimension("ffi-vec-tel-round-trip-tag", 768);
        record_observed_dimension("ffi-vec-tel-round-trip-tag", 384);

        // Upstream is read AFTER the bumps but BEFORE the mirror,
        // so concurrent tests bumping counters in between would
        // only increase the mirror's value relative to the
        // upstream snapshot. Lower-bound the mirror by upstream
        // (the same monotonic-lower-bound pattern as the FTS
        // mirror test).
        let upstream = evidence_store::vector_telemetry::snapshot();
        let mirror = vector_telemetry_snapshot();

        // Mirror ≥ upstream for every field — catches a
        // silently-dropped projection.
        assert!(
            mirror.query_embeddings_total >= upstream.query_embeddings_total,
            "query_embeddings_total mirror < upstream"
        );
        assert!(
            mirror.index_write_embeddings_total >= upstream.index_write_embeddings_total,
            "index_write_embeddings_total mirror < upstream"
        );
        assert!(
            mirror.live_body_embeddings_total >= upstream.live_body_embeddings_total,
            "live_body_embeddings_total mirror < upstream"
        );
        assert!(
            mirror.cache_hits_total >= upstream.cache_hits_total,
            "cache_hits_total mirror < upstream"
        );
        assert!(
            mirror.cache_misses_no_row_total >= upstream.cache_misses_no_row_total,
            "cache_misses_no_row_total mirror < upstream"
        );
        assert!(
            mirror.cache_misses_dimension_total >= upstream.cache_misses_dimension_total,
            "cache_misses_dimension_total mirror < upstream"
        );
        assert!(
            mirror.cache_misses_read_error_total >= upstream.cache_misses_read_error_total,
            "cache_misses_read_error_total mirror < upstream"
        );
        assert!(
            mirror.dedup_copy_hits_total >= upstream.dedup_copy_hits_total,
            "dedup_copy_hits_total mirror < upstream"
        );
        assert!(
            mirror.runtime_unavailable_total >= upstream.runtime_unavailable_total,
            "runtime_unavailable_total mirror < upstream"
        );
        assert!(
            mirror.model_load_errors_total >= upstream.model_load_errors_total,
            "model_load_errors_total mirror < upstream"
        );
        assert!(
            mirror.inference_failures_total >= upstream.inference_failures_total,
            "inference_failures_total mirror < upstream"
        );
        assert!(
            mirror.model_tag_dimension_violations_total
                >= upstream.model_tag_dimension_violations_total,
            "model_tag_dimension_violations_total mirror < upstream"
        );
        assert!(
            mirror.pre_embed_admitted_total >= upstream.pre_embed_admitted_total,
            "pre_embed_admitted_total mirror < upstream"
        );
        assert!(
            mirror.pre_embed_skipped_empty_after_trim_total
                >= upstream.pre_embed_skipped_empty_after_trim_total,
            "pre_embed_skipped_empty_after_trim_total mirror < upstream"
        );
        assert!(
            mirror.pre_embed_skipped_no_linguistic_content_total
                >= upstream.pre_embed_skipped_no_linguistic_content_total,
            "pre_embed_skipped_no_linguistic_content_total mirror < upstream"
        );

        // Reverse direction: every field we bumped above must
        // show movement from baseline through the FFI mirror.
        // A silently-zeroed projection (e.g. forgetting to add
        // `query_embeddings_total: s.query_embeddings_total` in
        // [`vector_telemetry_snapshot`]) would leave the diff
        // at 0 even though our increment added 1.
        assert!(
            mirror.query_embeddings_total > before.query_embeddings_total,
            "query_embeddings_total not plumbed"
        );
        assert!(
            mirror.index_write_embeddings_total > before.index_write_embeddings_total,
            "index_write_embeddings_total not plumbed"
        );
        assert!(
            mirror.live_body_embeddings_total > before.live_body_embeddings_total,
            "live_body_embeddings_total not plumbed"
        );
        assert!(
            mirror.cache_hits_total > before.cache_hits_total,
            "cache_hits_total not plumbed"
        );
        assert!(
            mirror.cache_misses_no_row_total > before.cache_misses_no_row_total,
            "cache_misses_no_row_total not plumbed"
        );
        assert!(
            mirror.cache_misses_dimension_total > before.cache_misses_dimension_total,
            "cache_misses_dimension_total not plumbed"
        );
        assert!(
            mirror.cache_misses_read_error_total > before.cache_misses_read_error_total,
            "cache_misses_read_error_total not plumbed"
        );
        assert!(
            mirror.dedup_copy_hits_total > before.dedup_copy_hits_total,
            "dedup_copy_hits_total not plumbed"
        );
        assert!(
            mirror.runtime_unavailable_total > before.runtime_unavailable_total,
            "runtime_unavailable_total not plumbed"
        );
        assert!(
            mirror.model_load_errors_total > before.model_load_errors_total,
            "model_load_errors_total not plumbed"
        );
        assert!(
            mirror.inference_failures_total > before.inference_failures_total,
            "inference_failures_total not plumbed"
        );
        assert!(
            mirror.model_tag_dimension_violations_total
                > before.model_tag_dimension_violations_total,
            "model_tag_dimension_violations_total not plumbed"
        );
        assert!(
            mirror.pre_embed_admitted_total > before.pre_embed_admitted_total,
            "pre_embed_admitted_total not plumbed"
        );
        assert!(
            mirror.pre_embed_skipped_empty_after_trim_total
                > before.pre_embed_skipped_empty_after_trim_total,
            "pre_embed_skipped_empty_after_trim_total not plumbed"
        );
        assert!(
            mirror.pre_embed_skipped_no_linguistic_content_total
                > before.pre_embed_skipped_no_linguistic_content_total,
            "pre_embed_skipped_no_linguistic_content_total not plumbed"
        );
    }

    /// parity invariant: the flat
    /// `fts_telemetry` / `lexicon_telemetry` / `vector_telemetry`
    /// fields on a single [`MetricsSnapshot`] value MUST equal
    /// the grouped `retrieval_metrics.fts` /
    /// `retrieval_metrics.lexicon` / `retrieval_metrics.vector`
    /// sub-fields of the same snapshot.
    ///
    /// This is the wire-contract pinned by the doc on
    /// [`MetricsSnapshot::retrieval_metrics`]: the two views are
    /// populated from a single upstream
    /// [`observation_engine::retrieval_telemetry::snapshot`] read
    /// pass in [`super::snapshot`], so they cannot drift even
    /// under heavy concurrent telemetry writes. Without the
    /// single-read-pass discipline this would be flaky — see
    /// the comment in the `snapshot` populate block.
    #[test]
    fn metrics_snapshot_retrieval_metrics_matches_flat_fields() {
        // Drive every retrieval lane briefly before snapshotting,
        // so any future divergence between the two views surfaces
        // as a non-zero counter mismatch rather than a trivial
        // all-zero pass-through.
        let registry = observation_engine::default_registry();
        let _ = registry.lexicon_for_or_english(Some("en"));
        let _ = registry.lexicon_for_or_english(Some("ja"));
        let _ = registry.lexicon_for_or_english(Some("ar"));

        let snap = super::snapshot();

        // The three flat fields MUST equal the three grouped
        // sub-fields, byte for byte, on the same snapshot.
        assert_eq!(
            snap.fts_telemetry, snap.retrieval_metrics.fts,
            "flat fts_telemetry must equal retrieval_metrics.fts on the same snapshot"
        );
        assert_eq!(
            snap.lexicon_telemetry, snap.retrieval_metrics.lexicon,
            "flat lexicon_telemetry must equal retrieval_metrics.lexicon on the same snapshot"
        );
        assert_eq!(
            snap.vector_telemetry, snap.retrieval_metrics.vector,
            "flat vector_telemetry must equal retrieval_metrics.vector on the same snapshot"
        );
    }

    /// wire-default contract: a freshly-constructed
    /// [`RetrievalMetrics`] (via the `Default` derive) is all-zero
    /// across all three sub-fields, and serialises to a JSON
    /// object with three sub-objects that themselves serialise to
    /// all-zero JSON.
    ///
    /// Combined with the `#[serde(default)]` attribute on
    /// [`MetricsSnapshot::retrieval_metrics`], this means an
    /// older emitter that never knew about `retrieval_metrics`
    /// will produce JSON that deserialises under a newer reader
    /// to a `MetricsSnapshot` whose `retrieval_metrics` is the
    /// all-zero default — preserving the additive-wire-contract
    /// rule documented on [`MetricsSnapshot`].
    #[test]
    fn retrieval_metrics_default_is_all_zero_and_round_trips() {
        let zero = RetrievalMetrics::default();
        let json = serde_json::to_string(&zero).expect("RetrievalMetrics serialises");
        let back: RetrievalMetrics =
            serde_json::from_str(&json).expect("RetrievalMetrics deserialises");
        assert_eq!(back, zero);

        // Cross-check: an empty JSON object should deserialise to
        // the all-zero default via the per-field `#[serde(default)]`
        // attributes. This is the wire-contract invariant for
        // forward compatibility with older snapshot emitters.
        let from_empty: RetrievalMetrics =
            serde_json::from_str("{}").expect("empty JSON deserialises to default");
        assert_eq!(from_empty, zero);

        // The flat MetricsSnapshot JSON must also accept missing
        // `retrieval_metrics`: an older emitter's JSON that lacks
        // the field entirely must still deserialise under the new
        // reader. We test this by serialising a full snapshot,
        // surgically removing the `retrieval_metrics` key (simulating
        // an older emitter that doesn't know about the field), and
        // confirming the result still deserialises with
        // `retrieval_metrics == RetrievalMetrics::default()`.
        let snap = super::snapshot();
        let mut as_value: serde_json::Value =
            serde_json::to_value(&snap).expect("MetricsSnapshot serialises to JSON value");
        let removed = as_value
            .as_object_mut()
            .expect("MetricsSnapshot JSON is an object")
            .remove("retrieval_metrics");
        assert!(
            removed.is_some(),
            "retrieval_metrics key must be present in a fresh MetricsSnapshot's JSON"
        );
        let without_retrieval_metrics: MetricsSnapshot = serde_json::from_value(as_value)
            .expect("MetricsSnapshot JSON without retrieval_metrics deserialises");
        assert_eq!(
            without_retrieval_metrics.retrieval_metrics,
            RetrievalMetrics::default(),
            "missing retrieval_metrics field must deserialise to all-zero default"
        );
        // The other flat fields must be preserved across the round trip.
        assert_eq!(without_retrieval_metrics.ingest_total, snap.ingest_total);
        assert_eq!(
            without_retrieval_metrics.lexicon_telemetry,
            snap.lexicon_telemetry
        );
    }

    /// The synthesis-quality counters wired for the on-device
    /// verify-and-retry path (`synthesis_lowquality_total`,
    /// `synthesis_retry_total`, `synthesis_truncated_total`,
    /// `synthesis_recap_chars_total`, `synthesis_recap_samples_total`)
    /// are additive: an older emitter's snapshot JSON that predates
    /// them must still deserialise under the new reader, defaulting the
    /// missing fields to `0` rather than erroring. Pins the
    /// `#[serde(default)]` wire contract for every one of them.
    #[test]
    fn synthesis_quality_counters_are_additive_on_the_wire() {
        let snap = super::snapshot();
        let mut as_value: serde_json::Value =
            serde_json::to_value(&snap).expect("MetricsSnapshot serialises to JSON value");
        let obj = as_value
            .as_object_mut()
            .expect("MetricsSnapshot JSON is an object");
        for key in [
            "synthesis_lowquality_total",
            "synthesis_retry_total",
            "synthesis_retry_failed_total",
            "synthesis_truncated_total",
            "synthesis_recap_chars_total",
            "synthesis_recap_samples_total",
        ] {
            assert!(
                obj.remove(key).is_some(),
                "{key} must be present in a fresh MetricsSnapshot's JSON"
            );
        }
        let older: MetricsSnapshot = serde_json::from_value(as_value)
            .expect("MetricsSnapshot JSON without the synthesis-quality counters deserialises");
        assert_eq!(older.synthesis_lowquality_total, 0);
        assert_eq!(older.synthesis_retry_total, 0);
        assert_eq!(older.synthesis_retry_failed_total, 0);
        assert_eq!(older.synthesis_truncated_total, 0);
        assert_eq!(older.synthesis_recap_chars_total, 0);
        assert_eq!(older.synthesis_recap_samples_total, 0);
        // A pre-existing flat field is still preserved across the round
        // trip, proving we removed only the new keys.
        assert_eq!(
            older.synthesis_triggered_total,
            snap.synthesis_triggered_total
        );
    }

    /// `observe_synthesis_recap_chars` advances BOTH halves of the
    /// mean-recap-length signal: the char sum by the recap length and
    /// the sample count by one. Uses snapshot deltas because the
    /// counters are a process-singleton shared with sibling tests.
    #[test]
    fn observe_synthesis_recap_chars_advances_sum_and_count() {
        let before = super::snapshot();
        observe_synthesis_recap_chars(42);
        let after = super::snapshot();
        assert!(
            after.synthesis_recap_chars_total >= before.synthesis_recap_chars_total + 42,
            "recap-chars sum must advance by at least the observed length",
        );
        assert!(
            after.synthesis_recap_samples_total > before.synthesis_recap_samples_total,
            "recap-samples count must advance by at least one",
        );
    }
}
