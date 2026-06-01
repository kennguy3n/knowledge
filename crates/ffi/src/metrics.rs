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
    /// Total `create_connector` calls initiated.
    pub(crate) create_connector_total: AtomicU64,
    /// Total `authenticate_connector` calls initiated.
    pub(crate) authenticate_connector_total: AtomicU64,
    /// Total `sync_connector` calls initiated.
    pub(crate) sync_connector_total: AtomicU64,
    /// Total `list_connectors` calls initiated.
    pub(crate) list_connectors_total: AtomicU64,
    /// Total `connector_status` calls initiated
    /// (Phase 10 Item 3 — single-instance health probe symmetric
    /// with [`Self::synthesis_status_total`]).
    pub(crate) connector_status_total: AtomicU64,
    /// Total `remove_connector` calls initiated.
    pub(crate) remove_connector_total: AtomicU64,
    /// Total `refresh_connector_token` calls initiated.
    pub(crate) refresh_connector_token_total: AtomicU64,
    /// Total `set_oauth_client_secret_resolver` calls initiated
    /// (Phase 4.1 — host-supplied resolver registration).
    pub(crate) set_oauth_client_secret_resolver_total: AtomicU64,
    /// Total `clear_oauth_client_secret_resolver` calls initiated
    /// (Phase 4.1 — host-supplied resolver de-registration).
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
    /// (Phase 5 — webhook receiver server startup).
    pub(crate) start_webhook_server_total: AtomicU64,
    /// Total `stop_webhook_server` calls initiated
    /// (Phase 5 — webhook receiver server shutdown).
    pub(crate) stop_webhook_server_total: AtomicU64,
    /// Total `register_webhook_dispatch` calls initiated
    /// (Phase 5 — bind a `provider_id` to a connector instance).
    pub(crate) register_webhook_dispatch_total: AtomicU64,
    /// Total `unregister_webhook_dispatch` calls initiated
    /// (Phase 5 — drop a `(server, provider_id)` binding).
    pub(crate) unregister_webhook_dispatch_total: AtomicU64,
    /// Total `list_webhook_servers` calls initiated
    /// (Phase 5 — diagnostic enumeration of running servers).
    pub(crate) list_webhook_servers_total: AtomicU64,
    /// Total `start_sync_scheduler` calls initiated
    /// (Phase 6 — background sync scheduler startup).
    pub(crate) start_sync_scheduler_total: AtomicU64,
    /// Total `stop_sync_scheduler` calls initiated
    /// (Phase 6 — background sync scheduler shutdown).
    pub(crate) stop_sync_scheduler_total: AtomicU64,
    /// Total `configure_sync_schedule` calls initiated
    /// (Phase 6 — per-instance policy override).
    pub(crate) configure_sync_schedule_total: AtomicU64,
    /// Total `clear_sync_schedule` calls initiated
    /// (Phase 6 — per-instance policy clear).
    pub(crate) clear_sync_schedule_total: AtomicU64,
    /// Total `sync_scheduler_status` calls initiated
    /// (Phase 6 — diagnostic snapshot read).
    pub(crate) sync_scheduler_status_total: AtomicU64,
    /// Total ticks the scheduler worker thread has completed
    /// across every scheduler instance the process has ever run
    /// (Phase 6). Process-singleton sum because per-runtime
    /// counters live inside the per-runtime
    /// [`crate::sync_scheduler::RunningSyncScheduler`] and would
    /// be invisible to a host that polls only `metrics_snapshot`.
    pub(crate) sync_scheduler_ticks_total: AtomicU64,
    /// Total scheduler-initiated dispatches attempted across every
    /// runtime (Phase 6). Counts `sync_connector` calls made by
    /// scheduler worker threads, not their success/failure.
    pub(crate) sync_scheduler_dispatches_attempted_total: AtomicU64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Ok(SyncReport)` (Phase 6).
    pub(crate) sync_scheduler_dispatches_succeeded_total: AtomicU64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Err(_)` (Phase 6). Drives the per-instance
    /// exponential-backoff curve.
    pub(crate) sync_scheduler_dispatches_failed_total: AtomicU64,
    /// Total candidate instances the scheduler skipped because
    /// they were already in
    /// [`connector_framework::SyncStatus::InProgress`] when the
    /// tick fired (a host-driven sync was running concurrently).
    /// Distinct from `*_dispatches_failed_total` because the
    /// scheduler never invoked `sync_connector` for these
    /// (Phase 6).
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
    /// (Phase 7 — host installs the server-side synthesis
    /// endpoint configuration on a runtime).
    pub(crate) configure_synthesis_engine_total: AtomicU64,
    /// Total `trigger_server_synthesis` calls initiated
    /// (Phase 7 — host explicitly dispatches a domain / tenant
    /// synthesis on a configured engine).
    pub(crate) trigger_server_synthesis_total: AtomicU64,
    /// Total `synthesis_status` calls initiated
    /// (Phase 7 — host polls the status of a previously
    /// dispatched window).
    pub(crate) synthesis_status_total: AtomicU64,
    /// Total `list_recent_syntheses` calls initiated
    /// (Phase 7 — host enumerates per-scope synthesis history).
    pub(crate) list_recent_syntheses_total: AtomicU64,
    /// Total `configure_sync_auto_synthesize` calls initiated
    /// (Phase 7 — host toggles the per-instance auto-synthesise
    /// flag on the sync scheduler).
    pub(crate) configure_sync_auto_synthesize_total: AtomicU64,
    /// Total `admit_approved_document` calls initiated
    /// (Phase 8 — host attaches an approved-document payload to
    /// the tenant memory).
    pub(crate) admit_approved_document_total: AtomicU64,
    /// Total `revoke_approved_document` calls initiated
    /// (Phase 8 — host removes a previously admitted approved
    /// document from tenant memory).
    pub(crate) revoke_approved_document_total: AtomicU64,
    /// Total `replace_approved_document` calls initiated
    /// (Phase 9 — host replaces payload on an existing
    /// approved document without revoking and re-admitting).
    pub(crate) replace_approved_document_total: AtomicU64,
    /// Total `list_approved_documents` calls initiated
    /// (Phase 8 — host enumerates approved-document refs for a
    /// tenant scope).
    pub(crate) list_approved_documents_total: AtomicU64,
    /// Total synthesis windows transitioned from `Pending` → `Failed`
    /// by the `open_store` stuck-Pending recovery sweep (Phase 10
    /// Item 1). Incremented once per swept window. A non-zero value
    /// here indicates a prior host run crashed mid-dispatch (between
    /// the Phase-1 `flush_synthesis_windows` and the Phase-3
    /// `apply_dispatch_outcome` commit) OR a Phase-3 commit failed
    /// and the in-process recovery flush also failed to land. Either
    /// way the next `open_store` reclaimed the stranded window so
    /// the host can retry it.
    pub(crate) stuck_pending_window_recovered_total: AtomicU64,
    /// Total `trigger_server_synthesis` calls rejected by the
    /// global token-bucket rate limiter (Phase 10 Item 5).
    /// Incremented once per `Throttled` return. Distinct from
    /// the per-kind [`Self::errors_throttled`] counter — that
    /// one ticks on every `FfiError::Throttled` regardless of
    /// surface, while this one isolates the synthesis-trigger
    /// surface so operators can spot rate-shaping-driven
    /// throttles separately from any future throttled
    /// surfaces.
    pub(crate) trigger_server_synthesis_throttled_total: AtomicU64,
    /// Total `replay_synthesis` calls initiated (Phase 10 Item 4).
    /// Counts entries to the surface — both successful replays
    /// AND failure paths (engine error, transaction commit
    /// failure, invalid-state refusal). Per-kind error counters
    /// can be cross-referenced to disambiguate.
    pub(crate) replay_synthesis_total: AtomicU64,
    /// Total `list_synthesis_versions` calls (Phase 10 Item 4).
    pub(crate) list_synthesis_versions_total: AtomicU64,

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
    pub(crate) errors_connector: AtomicU64,
    pub(crate) errors_throttled: AtomicU64,
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
counter_inc!(pub(crate) fn inc_open_store_with_resolver => open_store_with_resolver_total);
counter_inc!(pub(crate) fn inc_close_store => close_store_total);
counter_inc!(pub(crate) fn inc_encrypt => encrypt_total);
counter_inc!(pub(crate) fn inc_decrypt => decrypt_total);
counter_inc!(pub(crate) fn inc_generate_keypair => generate_keypair_total);
counter_inc!(pub(crate) fn inc_escape_fts_query => escape_fts_query_total);
counter_inc!(pub(crate) fn inc_metrics_snapshot => metrics_snapshot_total);
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
        FfiError::NotFound { .. } => &m.errors_not_found,
        FfiError::Evidence { .. } => &m.errors_evidence,
        FfiError::Memory { .. } => &m.errors_memory,
        FfiError::Synthesis { .. } => &m.errors_synthesis,
        FfiError::Crypto { .. } => &m.errors_crypto,
        FfiError::Unavailable { .. } => &m.errors_unavailable,
        FfiError::InferenceFailure { .. } => &m.errors_inference_failure,
        FfiError::Connector { .. } => &m.errors_connector,
        FfiError::Throttled { .. } => &m.errors_throttled,
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
    /// (Phase 10 Item 3 — single-instance health probe symmetric
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
    /// Total `set_oauth_client_secret_resolver` calls initiated
    /// (Phase 4.1). Increments every time a host (re-)registers a
    /// resolver; high frequency indicates the host is treating the
    /// resolver registration as a per-request operation rather
    /// than a once-per-`open_store` lifecycle event — worth
    /// investigating.
    #[serde(default)]
    pub set_oauth_client_secret_resolver_total: u64,
    /// Total `clear_oauth_client_secret_resolver` calls initiated
    /// (Phase 4.1).
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
    /// Total `start_webhook_server` calls initiated (Phase 5).
    #[serde(default)]
    pub start_webhook_server_total: u64,
    /// Total `stop_webhook_server` calls initiated (Phase 5).
    #[serde(default)]
    pub stop_webhook_server_total: u64,
    /// Total `register_webhook_dispatch` calls initiated (Phase 5).
    #[serde(default)]
    pub register_webhook_dispatch_total: u64,
    /// Total `unregister_webhook_dispatch` calls initiated (Phase 5).
    #[serde(default)]
    pub unregister_webhook_dispatch_total: u64,
    /// Total `list_webhook_servers` calls initiated (Phase 5).
    #[serde(default)]
    pub list_webhook_servers_total: u64,
    /// Total `start_sync_scheduler` calls initiated (Phase 6).
    #[serde(default)]
    pub start_sync_scheduler_total: u64,
    /// Total `stop_sync_scheduler` calls initiated (Phase 6).
    #[serde(default)]
    pub stop_sync_scheduler_total: u64,
    /// Total `configure_sync_schedule` calls initiated (Phase 6).
    #[serde(default)]
    pub configure_sync_schedule_total: u64,
    /// Total `clear_sync_schedule` calls initiated (Phase 6).
    #[serde(default)]
    pub clear_sync_schedule_total: u64,
    /// Total `sync_scheduler_status` calls initiated (Phase 6).
    #[serde(default)]
    pub sync_scheduler_status_total: u64,
    /// Total ticks the scheduler worker thread has completed
    /// across every scheduler instance in this process (Phase 6).
    /// Tracked as a process-singleton sum because the per-runtime
    /// counter lives inside the per-runtime
    /// [`crate::sync_scheduler::RunningSyncScheduler`] and would
    /// be invisible to a host that polls only `metrics_snapshot`.
    #[serde(default)]
    pub sync_scheduler_ticks_total: u64,
    /// Total scheduler-initiated dispatches attempted across every
    /// runtime (Phase 6).
    #[serde(default)]
    pub sync_scheduler_dispatches_attempted_total: u64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Ok(SyncReport)` (Phase 6).
    #[serde(default)]
    pub sync_scheduler_dispatches_succeeded_total: u64,
    /// Total scheduler-initiated dispatches that completed with
    /// `Err(_)` (Phase 6).
    #[serde(default)]
    pub sync_scheduler_dispatches_failed_total: u64,
    /// Total candidate instances the scheduler skipped because
    /// they were already in
    /// [`connector_framework::SyncStatus::InProgress`] when the
    /// tick fired (Phase 6).
    #[serde(default)]
    pub sync_scheduler_dispatches_skipped_in_progress_total: u64,
    /// Total webhook dispatches that returned `200 OK` across every
    /// running server in this process (Phase 5). The per-server
    /// counters live in
    /// [`crate::types::WebhookServerSummary::dispatch_ok_total`];
    /// this counter is the process-wide sum, surfaced through
    /// `metrics_snapshot` so a host that polls only the metrics
    /// surface sees webhook activity without enumerating servers.
    #[serde(default)]
    pub webhook_dispatch_ok_total: u64,
    /// Total webhook dispatches that returned `400 Bad Request`
    /// across every running server in this process (Phase 5).
    /// Companion to [`Self::webhook_dispatch_ok_total`].
    #[serde(default)]
    pub webhook_dispatch_bad_request_total: u64,
    /// Total webhook dispatches that returned `502 Bad Gateway`
    /// across every running server in this process (Phase 5).
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
    /// Total `configure_synthesis_engine` calls initiated (Phase 7).
    #[serde(default)]
    pub configure_synthesis_engine_total: u64,
    /// Total `trigger_server_synthesis` calls initiated (Phase 7).
    #[serde(default)]
    pub trigger_server_synthesis_total: u64,
    /// Total `synthesis_status` calls initiated (Phase 7).
    #[serde(default)]
    pub synthesis_status_total: u64,
    /// Total `list_recent_syntheses` calls initiated (Phase 7).
    #[serde(default)]
    pub list_recent_syntheses_total: u64,
    /// Total `configure_sync_auto_synthesize` calls initiated
    /// (Phase 7).
    #[serde(default)]
    pub configure_sync_auto_synthesize_total: u64,
    /// Total `admit_approved_document` calls initiated (Phase 8).
    #[serde(default)]
    pub admit_approved_document_total: u64,
    /// Total `revoke_approved_document` calls initiated (Phase 8).
    #[serde(default)]
    pub revoke_approved_document_total: u64,
    /// Total `replace_approved_document` calls initiated (Phase 9).
    #[serde(default)]
    pub replace_approved_document_total: u64,
    /// Total `list_approved_documents` calls initiated (Phase 8).
    #[serde(default)]
    pub list_approved_documents_total: u64,
    /// Total synthesis windows transitioned from `Pending` → `Failed`
    /// by the `open_store` stuck-Pending recovery sweep (Phase 10
    /// Item 1). A non-zero value indicates a prior run left at least
    /// one window stranded mid-dispatch and the next `open_store`
    /// reclaimed it; the host can retry the recovered window via the
    /// normal trigger path.
    #[serde(default)]
    pub stuck_pending_window_recovered_total: u64,
    /// Total `trigger_server_synthesis` calls rejected by the
    /// FFI-wide rate-shaping token bucket (Phase 10 Item 5).
    /// Distinct from `errors_by_kind.throttled` because that
    /// total covers every surface returning
    /// `FfiError::Throttled` — currently only this one, but
    /// future surfaces should reuse the variant rather than
    /// minting a new one.
    #[serde(default)]
    pub trigger_server_synthesis_throttled_total: u64,
    /// Total `replay_synthesis` calls (Phase 10 Item 4). Counts
    /// every entry to the surface, regardless of outcome — pair
    /// with `errors_by_kind.synthesis` / `.evidence` for failure
    /// rates.
    #[serde(default)]
    pub replay_synthesis_total: u64,
    /// Total `list_synthesis_versions` calls (Phase 10 Item 4).
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
    /// Multilingual lexicon-path telemetry (Phase 1.10).  Counts
    /// per-BCP-47 lexicon hits, [`observation_engine::MatchStrategy`]
    /// fires, and Arabic / Hebrew clitic-peel depth distribution.
    /// `#[serde(default)]` per the additive-wire-contract rule —
    /// older emitters' JSON lacks this field and deserialises to
    /// [`LexiconTelemetry::default()`] (all zeroes).
    #[serde(default)]
    pub lexicon_telemetry: LexiconTelemetry,
    /// Multilingual FTS5-path telemetry (Phase 1.10).  Counts
    /// per-lane query / row totals, recall-lane skip causes, and
    /// stopword strip volumes per call site.
    /// `#[serde(default)]` per the additive-wire-contract rule.
    #[serde(default)]
    pub fts_telemetry: FtsTelemetry,
}

/// Multilingual lexicon-path observability counters mirrored from
/// [`observation_engine::lexicon_telemetry`] (Phase 1.10).
///
/// The mirror lives here rather than upstream because the FFI
/// crate is where the `uniffi::Record` / serde derive lives, and
/// the upstream `observation_engine` crate intentionally does not
/// depend on either FFI runtime.  The field list mirrors
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
/// sentences or documents.  A typical sentence triggers several
/// resolution calls — up to three classifier-loop resolutions
/// plus one per inspected capitalised word for the stop-word
/// filter — so e.g. a 5-capitalised-word English sentence with
/// no class match will bump `hits_en` ~8 times.  Operators
/// inferring "documents classified" from these counters should
/// divide by their measured calls-per-document ratio rather
/// than reading the counter directly.  See the upstream
/// `observation_engine::lexicon_telemetry` module doc for the
/// full rationale (Phase 1.10 sweep 2 ANALYSIS-0003).
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
    /// Resolved-lexicon hits for `en`.  Includes the
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
    /// Resolved-lexicon hits for `vi`.
    #[serde(default)]
    pub hits_vi: u64,
    /// Resolved-lexicon hits for `zh`.
    #[serde(default)]
    pub hits_zh: u64,
    /// Times an input primary_tag was `Some(t)` but no lexicon
    /// was configured for `t`, so the registry fell back to
    /// English.  Always satisfies
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
/// [`evidence_store::fts_telemetry`] (Phase 1.10).
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
    /// no-CJK counter.  See the upstream
    /// `evidence_store::fts_telemetry::SkipReason` doc for the
    /// taxonomic rationale (added Phase 1.10 sweep 2).
    #[serde(default)]
    pub bigram_lane_skips_pure_stopword_query_total: u64,
    /// Times the CJK bigram lane was skipped because the
    /// stripped query was non-empty but contained no adjacent-
    /// CJK codepoint pair (e.g. a Latin-only query).  Mutually
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
    /// `FfiError::Throttled` (Phase 10 Item 5). `#[serde(default)]`
    /// per the additive-wire-contract rule — older emitters'
    /// `ErrorCounters` JSON lacks the `throttled` key and must
    /// still deserialise without surfacing a missing-field
    /// error.
    #[serde(default)]
    pub throttled: u64,
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
        decay_sweeps_total: m.decay_sweeps_total.load(Ordering::Relaxed),
        forgets_total: m.forgets_total.load(Ordering::Relaxed),
        forget_scopes_total: m.forget_scopes_total.load(Ordering::Relaxed),
        encrypt_total: m.encrypt_total.load(Ordering::Relaxed),
        decrypt_total: m.decrypt_total.load(Ordering::Relaxed),
        generate_keypair_total: m.generate_keypair_total.load(Ordering::Relaxed),
        escape_fts_query_total: m.escape_fts_query_total.load(Ordering::Relaxed),
        metrics_snapshot_total: m.metrics_snapshot_total.load(Ordering::Relaxed),
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
            not_found: m.errors_not_found.load(Ordering::Relaxed),
            evidence: m.errors_evidence.load(Ordering::Relaxed),
            memory: m.errors_memory.load(Ordering::Relaxed),
            synthesis: m.errors_synthesis.load(Ordering::Relaxed),
            crypto: m.errors_crypto.load(Ordering::Relaxed),
            unavailable: m.errors_unavailable.load(Ordering::Relaxed),
            inference_failure: m.errors_inference_failure.load(Ordering::Relaxed),
            connector: m.errors_connector.load(Ordering::Relaxed),
            throttled: m.errors_throttled.load(Ordering::Relaxed),
        },
        errors_total: m.errors_total.load(Ordering::Relaxed),
        open_handles: m.open_handles.load(Ordering::Relaxed),
        tombstone_count: m.tombstone_count.load(Ordering::Relaxed),
        boot_unix_secs: m.boot_unix_secs.load(Ordering::Relaxed),
        lexicon_telemetry: lexicon_telemetry_snapshot(),
        fts_telemetry: fts_telemetry_snapshot(),
    }
}

/// Read the upstream
/// [`observation_engine::lexicon_telemetry::snapshot`] and
/// project it into the FFI mirror struct.  One-to-one field
/// mapping by name — the field lists are kept symmetric by the
/// `lexicon_telemetry_mirror_field_parity` test below.
fn lexicon_telemetry_snapshot() -> LexiconTelemetry {
    let s = observation_engine::lexicon_telemetry::snapshot();
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

/// Read the upstream
/// [`evidence_store::fts_telemetry::snapshot`] and project it
/// into the FFI mirror struct.
fn fts_telemetry_snapshot() -> FtsTelemetry {
    let s = evidence_store::fts_telemetry::snapshot();
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

    /// Pin the lexicon-telemetry mirror field parity (Phase 1.10).
    ///
    /// Every field on
    /// [`observation_engine::LexiconTelemetrySnapshot`] must have
    /// a one-to-one counterpart on the FFI [`LexiconTelemetry`]
    /// struct.  The check is byte-by-byte: we read the upstream
    /// snapshot, project into the FFI mirror via
    /// [`lexicon_telemetry_snapshot`], and then re-project field
    /// values by name to make sure no field was dropped or
    /// silently zeroed.  When this test fails after an
    /// upstream-counter addition, the FFI mirror is missing the
    /// new field — extend [`LexiconTelemetry`] and the
    /// projection helper symmetrically.
    ///
    /// The test bumps the upstream counters first so all fields
    /// have non-zero distinct values, then asserts the projection
    /// preserves them.  We use distinct prime-ish increments per
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
            "ms", "my", "pt", "ru", "th", "vi", "zh",
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
        // snapshot.  This is the same monotonic-lower-bound
        // pattern used by [`snapshot_reflects_counter_increments`]
        // above.  We additionally lower-bound the mirror by
        // `before + N` so the test catches a mirror that
        // silently zeroes a field (mirror==0 would be < before+1
        // even on a clean process).
        let upstream = observation_engine::lexicon_telemetry::snapshot();
        let mirror = lexicon_telemetry_snapshot();

        // Verify each FFI mirror field plumbs through to the
        // corresponding upstream counter.  Lower-bound by the
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

    /// Pin the FTS-telemetry mirror field parity (Phase 1.10).
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
        // snapshot.  This is the same monotonic-lower-bound
        // pattern used by [`snapshot_reflects_counter_increments`]
        // and by `lexicon_telemetry_mirror_round_trips` above.
        // Phase 1.10 sweep 1 (INFO-0002 fix): the previous
        // `assert_eq!(mirror.field, upstream.field)` shape was
        // accidentally racy — if any parallel test (today only
        // `store_integration::fts_telemetry_*`, but trivially
        // any future ffi-binary test that touches FTS) bumped a
        // counter between the two reads, the assertion would
        // fail.  Switching to `>=` makes the test correct under
        // arbitrary concurrent telemetry traffic and matches
        // the lexicon mirror test pattern verbatim.
        let upstream = evidence_store::fts_telemetry::snapshot();
        let mirror = fts_telemetry_snapshot();

        // Verify each FFI mirror field plumbs through to the
        // corresponding upstream counter.  Lower-bound by the
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
}
