//! Background sync scheduler FFI surface.
//!
//! Per `ARCHITECTURE.md` §4.4 and the open backlog from , the
//! substrate ships its own in-process scheduler so connectors poll
//! their upstream providers on a configurable cadence without
//! requiring the host to drive every [`crate::sync_connector`] call
//! itself. The scheduler walks the per-runtime
//! [`crate::runtime::FfiRuntime::connector_instances`] map, picks
//! the rows whose `last_synced_at + sync_interval` has elapsed, and
//! dispatches a sync through the existing [`crate::sync_connector`]
//! entry point — i.e. the scheduler thread is just another FFI
//! client, observing the substrate's three-phase locking discipline.
//!
//! # Why a dedicated module (mirroring `crate::webhook`)
//!
//! [`crate::webhook`] ships the framework's axum receiver behind a
//! per-server tokio runtime hosted on a dedicated OS thread. The
//! scheduler is the dual: a single synchronous OS thread per runtime
//! (no tokio — [`crate::sync_connector`] is itself synchronous) that
//! ticks on a configurable interval, dispatching due syncs through
//! the public FFI entry point. We share the structural
//! invariants:
//!
//! **1. Singleton lifecycle.** Exactly one [`RunningSyncScheduler`]
//! per [`crate::runtime::FfiRuntime`]. [`start_sync_scheduler`] is
//! idempotent in the sense that a second call while a scheduler is
//! already running fails with [`crate::error::FfiError::Connector`]
//! rather than silently replacing it (matches the explicit lifecycle
//! contract on `webhook_servers` — hosts must call
//! [`stop_sync_scheduler`] to swap configuration).
//!
//! **2. `std::thread` driven.** The dispatch loop is sync top to
//! bottom: [`std::sync::mpsc::SyncSender`] for the shutdown oneshot,
//! [`std::sync::mpsc::Receiver::recv_timeout`] doubling as the tick
//! gate. No tokio dependency on the scheduler path keeps the
//! substrate's "minimum-required async island" rule (see
//! [`crate::webhook`] module doc) intact.
//!
//! **3. Three-phase locking around `sync_connector`.** The scheduler
//! thread is a substrate client, not an internal helper: it acquires
//! the per-handle mutex through [`crate::runtime::with_runtime`]
//! only for the *snapshot* (pick due instances) and *result-record*
//! (update consecutive_failures + next_attempt_at) phases. The
//! actual `sync_connector` call runs UNLOCKED — the entry point
//! itself walks the substrate's own three-phase discipline (Step 1
//! snapshot,  HTTP,  result) so any deadlock here
//! would also have caused one for host-driven syncs.
//!
//! **4. `close_store` pre-drain.** `close_store` consumes the
//! [`RunningSyncScheduler`] slot BEFORE the `Arc::try_unwrap` spin
//! loop. The scheduler thread re-enters
//! [`crate::runtime::with_runtime`] on every tick, briefly cloning
//! the entry `Arc` and blocking the spin loop; without an explicit
//! pre-drain step the loop would race the scheduler.
//!
//! # FFI surface (5 entry points)
//!
//! * [`start_sync_scheduler`] — spawn the dispatch thread with the
//!   supplied default interval / max backoff / tick cadence. Fails
//!   if a scheduler is already running.
//! * [`stop_sync_scheduler`] — signal shutdown via the oneshot,
//!   synchronously [`std::thread::JoinHandle::join`] the worker.
//!   Idempotent: stopping an already-stopped scheduler returns
//!   `Ok(())`.
//! * [`configure_sync_schedule`] — set per-instance overrides
//!   (custom `sync_interval` + `max_backoff`). Idempotent: a second
//!   call replaces the prior policy.
//! * [`clear_sync_schedule`] — remove a per-instance override so
//!   the row falls back to the scheduler's defaults.
//! * [`sync_scheduler_status`] — diagnostic enumeration: running
//!   state, policy-override count, total instance count, last-tick
//!   timestamp, totals.
//!
//! # Backoff policy
//!
//! Successful sync resets the per-instance `consecutive_failures`
//! counter to zero. Failed sync increments it; the next attempt is
//! scheduled at `now + min(sync_interval * 2^consecutive_failures, max_backoff)`.
//! `2^n` is implemented as [`u32::checked_shl`] with saturating
//! fall-through to `max_backoff` so a host that holds the policy in
//! a Failed state across a process lifetime never overflows
//! [`std::time::Duration`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, Utc};
use connector_framework::{ConnectorInstanceId, SyncStatus};
use evidence_store::ScopeId;
use tracing::{debug, info, warn};

use crate::error::{FfiError, FfiResult};
use crate::metrics;
use crate::runtime::{with_runtime, RuntimeHandle};
use crate::types::SyncSchedulerStatus;

// ──────────────────────── Constants ─────────────────────────────

/// Default per-instance sync interval — 15 minutes. Mirrors the
/// "polite polling cadence" most SaaS providers will tolerate from
/// a single client without rate-limiting (Slack, Notion, Atlassian
/// each document either explicit per-minute caps or an
/// expected-best-practice in the 5–30 minute window).
const DEFAULT_INTERVAL_SECS: u64 = 15 * 60;

/// Default per-instance max backoff — 8 hours. Caps the
/// exponential-backoff curve so an instance that has been Failing
/// for an extended period still gets retried roughly thrice per day
/// (substrate clients may run for weeks at a time — a 24-hour cap
/// would mean a stuck connector is silently dormant for a full day
/// before any further attempt).
const DEFAULT_MAX_BACKOFF_SECS: u64 = 8 * 60 * 60;

/// Default tick cadence — 30 seconds. Tradeoff: a tick that runs
/// "too often" pays the cost of one runtime mutex acquisition +
/// map-walk per tick (cheap — ~microseconds for any realistic
/// connector count), while a tick that runs "too rarely" caps the
/// minimum useful `sync_interval` at the tick cadence (a host
/// configuring `sync_interval=10s` against a `tick=30s` scheduler
/// would never see sub-30s sync cadence). 30 s is fine-grained
/// enough that the operator-visible cadence and the configured
/// cadence agree at the resolution of any realistic dashboard.
const DEFAULT_TICK_SECS: u64 = 30;

/// Lower bound on `sync_interval_secs`. Prevents a host from
/// configuring `sync_interval=0` (which would dispatch every tick
/// regardless of upstream pressure — every provider would
/// rate-limit immediately).
const MIN_INTERVAL_SECS: u64 = 1;

/// Lower bound on `tick_interval_secs`. The scheduler's spin cost
/// is dominated by the per-tick runtime mutex acquisition; a tick
/// faster than 1 s would burn substantial CPU for no useful resolution
/// improvement.
const MIN_TICK_SECS: u64 = 1;

// ──────────────────────── Wire types are in `types.rs` ──────────

// The host-facing [`SyncSchedulerStatus`] (a `uniffi::Record`) lives
// in `crate::types` for the same reason every other UniFFI-exported
// record sits there: keeps the FFI ABI surface in a single file
// for foreign-binding generators.

// ──────────────────────── Per-instance policy ───────────────────

/// Per-connector-instance scheduling policy.
///
/// Stored under [`SchedulerState::policies`] keyed by
/// [`ConnectorInstanceId`]. The scheduler thread reads every entry
/// on every tick (under the policies mutex briefly) and the FFI
/// surface [`configure_sync_schedule`] / [`clear_sync_schedule`]
/// mutates it under the same mutex. The map starts empty: an
/// instance with no entry uses the scheduler's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SchedulePolicy {
    /// How often this instance should be synced when healthy
    /// (i.e. `consecutive_failures == 0`). Always `>= MIN_INTERVAL_SECS`
    /// — [`configure_sync_schedule`] validates this on entry.
    sync_interval: Duration,
    /// Cap on the exponential-backoff curve when sync fails. The
    /// scheduler never schedules a retry further out than
    /// `now + max_backoff`.
    max_backoff: Duration,
    /// If `true`, the scheduler fires a domain-tier
    /// `trigger_server_synthesis` against the connector's scope
    /// after a successful sync (subject to the per-scope cooldown
    /// in [`crate::synthesis::PER_SCOPE_COOLDOWN_SECS`]). Defaults
    /// to `false` so existing hosts opt in explicitly. A host that
    /// has not called [`crate::synthesis::configure_synthesis_engine`]
    /// observes the field as a no-op — the post-sync hook short
    /// circuits when no engine is configured.
    auto_synthesize: bool,
}

impl SchedulePolicy {
    /// Build the default policy from the scheduler config.
    fn from_defaults(cfg: &SchedulerConfig) -> Self {
        Self {
            sync_interval: cfg.default_interval,
            max_backoff: cfg.default_max_backoff,
            auto_synthesize: false,
        }
    }

    /// Compute the next-attempt delay for an instance currently
    /// reporting `consecutive_failures`. `0` failures yields
    /// `sync_interval`; subsequent failures double the delay up to
    /// `max_backoff`. Saturates on shift overflow so a long-Failing
    /// instance never overflows [`Duration`].
    fn next_attempt_delay(&self, consecutive_failures: u32) -> Duration {
        if consecutive_failures == 0 {
            return self.sync_interval;
        }
        // Saturate the doubling exponent at 30 to keep the math
        // inside `u64` without overflow concerns. 2^30 * 15min is
        // already centuries — long beyond `max_backoff`.
        let exp = consecutive_failures.min(30);
        let multiplier: u64 = 1u64 << exp;
        let interval_secs = self.sync_interval.as_secs();
        let candidate_secs = interval_secs.saturating_mul(multiplier);
        let candidate = Duration::from_secs(candidate_secs);
        if candidate > self.max_backoff {
            self.max_backoff
        } else {
            candidate
        }
    }
}

/// Per-instance runtime accounting maintained by the scheduler
/// thread. Updated under the policies mutex AFTER every dispatch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct InstanceAccounting {
    /// Consecutive failures since the last successful sync. Reset
    /// to `0` on success. Used as the exponent in the next-attempt
    /// delay calculation.
    consecutive_failures: u32,
    /// Wall-clock time the next dispatch attempt is allowed. `None`
    /// means "fire on the next tick" (used for new entries that
    /// have never been dispatched by the scheduler).
    next_attempt_at: Option<DateTime<Utc>>,
}

// ──────────────────────── Scheduler config ──────────────────────

/// Snapshotted-at-`start_sync_scheduler`-time configuration. Cloned
/// into the scheduler thread; subsequent FFI calls cannot mutate
/// the defaults without [`stop_sync_scheduler`] +
/// [`start_sync_scheduler`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct SchedulerConfig {
    /// Default sync interval used for any instance without a
    /// per-instance policy.
    default_interval: Duration,
    /// Default max backoff cap used for any instance without a
    /// per-instance policy.
    default_max_backoff: Duration,
    /// Tick cadence — how often the scheduler thread wakes and
    /// scans the connector map.
    tick_interval: Duration,
}

// ──────────────────────── Scheduler state (shared) ──────────────

/// Mutable state shared between the scheduler thread and the FFI
/// callers. Stored behind a single [`Mutex`] so policy mutations
/// and dispatch-result updates serialise cleanly without a more
/// complicated read-write lock surface.
struct SchedulerState {
    /// Per-instance policy overrides. Absent entries use defaults.
    policies: HashMap<ConnectorInstanceId, SchedulePolicy>,
    /// Per-instance accounting state. The scheduler thread is the
    /// only writer; FFI callers read it for diagnostics via
    /// [`sync_scheduler_status`].
    accounting: HashMap<ConnectorInstanceId, InstanceAccounting>,
    /// Wall-clock time of the most recent tick the scheduler
    /// performed. `None` until the first tick fires.
    last_tick_at: Option<DateTime<Utc>>,
}

impl SchedulerState {
    fn new() -> Self {
        Self {
            policies: HashMap::new(),
            accounting: HashMap::new(),
            last_tick_at: None,
        }
    }
}

// ──────────────────────── Counters ──────────────────────────────

/// Process-singleton counters surfaced through
/// [`sync_scheduler_status`]. Kept as `AtomicU64` so the scheduler
/// thread and the FFI status reader never contend on a mutex for
/// counter reads.
#[derive(Default)]
struct SchedulerCounters {
    /// Total ticks the scheduler thread has completed since
    /// `start_sync_scheduler`. Includes ticks that found no due
    /// instances.
    ticks_completed: AtomicU64,
    /// Total dispatch attempts (i.e. `sync_connector` calls
    /// initiated by the scheduler thread). Counts the call, NOT
    /// the result.
    dispatches_attempted: AtomicU64,
    /// Total dispatches that completed with `Ok(SyncReport)`.
    dispatches_succeeded: AtomicU64,
    /// Total dispatches that completed with any `Err(_)`.
    dispatches_failed: AtomicU64,
    /// Total dispatches the scheduler skipped because the instance
    /// was already in [`SyncStatus::InProgress`] (a host-driven
    /// sync was running concurrently). Distinct from
    /// `dispatches_failed` because the scheduler did not call
    /// `sync_connector` — the InProgress check happens in the
    /// scheduler's snapshot phase.
    dispatches_skipped_in_progress: AtomicU64,
}

// ──────────────────────── Running scheduler ─────────────────────

/// Owned state held on [`crate::runtime::FfiRuntime::sync_scheduler`].
///
/// The drop impl signals shutdown + joins the worker so any forced
/// teardown (e.g. `forget_scope` failure unwinding `open_store` with
/// the scheduler half-initialised) cannot leak the thread.
pub(crate) struct RunningSyncScheduler {
    /// Wall-clock time `start_sync_scheduler` returned. Surfaced
    /// via [`SyncSchedulerStatus::started_at_unix`] for diagnostics.
    started_at: DateTime<Utc>,
    /// Configuration snapshot the worker is running against.
    config: SchedulerConfig,
    /// Mutex-protected mutable state shared with the worker.
    state: Arc<Mutex<SchedulerState>>,
    /// Counters shared with the worker.
    counters: Arc<SchedulerCounters>,
    /// Shutdown signal. `Some` until [`Self::shutdown_and_join`]
    /// consumes it.
    shutdown_tx: Option<mpsc::SyncSender<()>>,
    /// Worker thread handle. `None` once joined.
    worker_thread: Option<JoinHandle<()>>,
}

impl RunningSyncScheduler {
    /// Consume the scheduler, signal shutdown, and synchronously
    /// join the worker. Called from [`stop_sync_scheduler`] and
    /// from [`drain_scheduler`] on `close_store`.
    ///
    /// Idempotent in the sense that the drop impl tolerates an
    /// already-joined scheduler — but this method panics if called
    /// twice because consuming `Self` makes a second call
    /// type-impossible.
    fn shutdown_and_join(mut self) {
        // Send the shutdown signal. `sync_channel(1)` is bounded
        // so a `send_timeout` would let us bail on a wedged worker,
        // but in practice the worker drains the queue every
        // `tick_interval` and a `send` cannot block for longer
        // than that — the worker's `recv_timeout` will pick up the
        // signal on its next iteration.
        //
        // If `send` returns `Err`, the worker is already gone
        // (panicked on the previous tick — joined below either
        // way). Silently swallow because the join handle is the
        // canonical source of truth for "worker finished".
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.worker_thread.take() {
            // Worker thread is bounded by one final
            // `recv_timeout` returning `Disconnected` (the
            // sender was the `tx` we just dropped) or the
            // explicit `Ok(())` from `send` above — either way
            // it exits its loop in at most one `tick_interval`.
            // Join with no timeout: the wait is bounded.
            if let Err(panic) = handle.join() {
                // Worker panicked. Log and continue — substrate
                // hosts can't recover from a panic anyway and a
                // panicking join propagation would re-panic the
                // caller (often itself inside an FFI call where
                // a panic unwinds across the C ABI boundary).
                warn!("sync scheduler worker thread panicked; join result: {:?}",
                    panic
                );
            }
        }
    }
}

impl Drop for RunningSyncScheduler {
    fn drop(&mut self) {
        // If the scheduler was dropped without an explicit
        // `shutdown_and_join` (e.g. an `open_store` failed mid-way
        // through scheduler initialisation), still try to signal
        // + join so the worker thread does not leak.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.worker_thread.take() {
            // Same join discipline as `shutdown_and_join`, except
            // we cannot propagate panics out of `Drop` so the
            // join is always swallowed.
            let _ = handle.join();
        }
    }
}

// ──────────────────────── Close-store drain hook ────────────────

/// Consume the per-runtime scheduler slot during `close_store`. The
/// scheduler thread re-enters [`with_runtime`] on every tick, so
/// the slot MUST be drained BEFORE the [`crate::close_store`]
/// `Arc::try_unwrap` spin loop, the same way
/// [`crate::webhook::drain_all_servers`] is.
///
/// Takes ownership of the [`RunningSyncScheduler`] so the call
/// site cannot accidentally leave a stale entry in
/// [`crate::runtime::FfiRuntime::sync_scheduler`] — passing the
/// `Option<…>` by value pins the post-condition at the type level.
///
/// NOT part of the FFI surface.
pub(crate) fn drain_scheduler(scheduler: Option<RunningSyncScheduler>) {
    if let Some(s) = scheduler {
        debug!("draining sync scheduler on close_store");
        s.shutdown_and_join();
    }
}

// ──────────────────────── FFI surface ──────────────────────────

/// Start the background sync scheduler.
///
/// Spawns a dedicated OS thread that wakes every `tick_interval_secs`
/// and dispatches [`crate::sync_connector`] for every connector
/// instance whose `last_synced_at + sync_interval` has elapsed.
/// Per-instance sync intervals (overriding the
/// `default_interval_secs` argument) can be set with
/// [`configure_sync_schedule`].
///
/// # Arguments
///
/// * `default_interval_secs` — default per-instance sync cadence.
///   Used for any instance without a [`configure_sync_schedule`]
///   override. Must be `>= 1`.
/// * `default_max_backoff_secs` — cap on the exponential-backoff
///   curve applied to instances reporting consecutive failures.
///   Must be `>= default_interval_secs` (a max_backoff smaller
///   than the base interval would never engage).
/// * `tick_interval_secs` — cadence at which the scheduler wakes
///   to scan the connector map. Must be `>= 1`. Defines the
///   minimum useful `sync_interval` — a per-instance
///   `sync_interval` smaller than `tick_interval` will still be
///   dispatched at the tick cadence (not faster).
///
/// # Errors
///
/// * [`FfiError::Connector`] if a scheduler is already running on
///   this handle (call [`stop_sync_scheduler`] first).
/// * [`FfiError::InvalidId`] if any argument is `0` or
///   `default_max_backoff_secs < default_interval_secs`.
/// * [`FfiError::Unavailable`] if the OS rejects the
///   [`std::thread::Builder::spawn`] (resource exhaustion).
#[uniffi::export]
pub fn start_sync_scheduler(handle: RuntimeHandle,
    default_interval_secs: u64,
    default_max_backoff_secs: u64,
    tick_interval_secs: u64,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_start_sync_scheduler, || {
        validate_interval("default_interval_secs", default_interval_secs)?;
        validate_tick("tick_interval_secs", tick_interval_secs)?;
        if default_max_backoff_secs < default_interval_secs {
            return Err(FfiError::InvalidId {
                message: format!("start_sync_scheduler: default_max_backoff_secs ({default_max_backoff_secs}) \
                     must be >= default_interval_secs ({default_interval_secs}) so the \
                     backoff cap actually engages above the base interval"
                ),
            });
        }
        let config = SchedulerConfig {
            default_interval: Duration::from_secs(default_interval_secs),
            default_max_backoff: Duration::from_secs(default_max_backoff_secs),
            tick_interval: Duration::from_secs(tick_interval_secs),
        };
        let state = Arc::new(Mutex::new(SchedulerState::new()));
        let counters = Arc::new(SchedulerCounters::default());
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel::<()>(1);

        // Capture the Arc clones for the worker BEFORE the locked
        // section so the with_runtime closure stays tight.
        let worker_state = Arc::clone(&state);
        let worker_counters = Arc::clone(&counters);
        let worker_config = config.clone();

        with_runtime(handle, |rt| {
            if rt.sync_scheduler.is_some() {
                return Err(FfiError::Connector {
                    message: "start_sync_scheduler: a scheduler is already running on this \
                              runtime; call stop_sync_scheduler first"
                        .into(),
                });
            }
            let worker_thread = std::thread::Builder::new()
                .name(format!("knowledge-sync-scheduler-{}", handle.0))
                .spawn(move || {
                    // The closure owns the moved values; pass them
                    // into the loop by reference so clippy's
                    // `needless_pass_by_value` lint stays clean
                    // (the worker function does not consume them).
                    run_scheduler_loop(handle,
                        &worker_config,
                        &worker_state,
                        &worker_counters,
                        &shutdown_rx,
                    );
                })
                .map_err(|e| FfiError::Unavailable {
                    subsystem: format!("sync-scheduler-thread (spawn rejected: {e})"),
                })?;
            rt.sync_scheduler = Some(RunningSyncScheduler {
                started_at: Utc::now(),
                config,
                state,
                counters,
                shutdown_tx: Some(shutdown_tx),
                worker_thread: Some(worker_thread),
            });
            info!(handle = handle.0,
                default_interval_secs,
                default_max_backoff_secs,
                tick_interval_secs,
                "sync scheduler started",
            );
            Ok(())
        })
    })
}

/// Stop the background sync scheduler.
///
/// Signals shutdown via the per-runtime oneshot and synchronously
/// joins the worker thread. The worker drains its current tick
/// before exiting; in-flight `sync_connector` calls run to
/// completion under the substrate's three-phase locking discipline.
///
/// Idempotent: calling this on a runtime with no scheduler running
/// returns `Ok(())`.
///
/// # Errors
///
/// * [`FfiError::NotFound`] if `handle` does not correspond to a
///   currently-open runtime. (No-scheduler-running is NOT an error.)
#[uniffi::export]
pub fn stop_sync_scheduler(handle: RuntimeHandle) -> FfiResult<()> {
    metrics::instrument(metrics::inc_stop_sync_scheduler, || {
        //  (locked): take the scheduler out of the runtime
        // slot. Releases the runtime mutex before the join so any
        // in-flight tick — which is itself blocked on the runtime
        // mutex inside `with_runtime` — can complete and drop the
        // guard.
        let scheduler = with_runtime(handle, |rt| -> FfiResult<Option<RunningSyncScheduler>> {
            Ok(rt.sync_scheduler.take())
        })?;
        //  (unlocked): synchronously join the worker. May
        // take up to `tick_interval` (the worker's recv_timeout)
        // to surface the shutdown signal.
        if let Some(s) = scheduler {
            s.shutdown_and_join();
            info!(handle = handle.0, "sync scheduler stopped");
        }
        Ok(())
    })
}

/// Override the scheduling policy for a specific connector
/// instance.
///
/// Per-instance policies take precedence over the scheduler-wide
/// defaults set at [`start_sync_scheduler`] time. A subsequent
/// [`clear_sync_schedule`] call restores the defaults.
///
/// # Arguments
///
/// * `instance_id` — UUID-string identifier of the connector
///   instance to configure. The instance does NOT need to exist in
///   [`crate::runtime::FfiRuntime::connector_instances`] at
///   configuration time (a host that wires its policy table at boot
///   may not have called [`crate::create_connector`] yet); policies
///   for absent instances simply never fire.
/// * `sync_interval_secs` — per-instance sync cadence. Must be
///   `>= 1`.
/// * `max_backoff_secs` — per-instance backoff cap. Must be
///   `>= sync_interval_secs`.
///
/// # Errors
///
/// * [`FfiError::Connector`] if no scheduler is running on this
///   handle.
/// * [`FfiError::InvalidId`] if either time argument is `0` or
///   `max_backoff_secs < sync_interval_secs`, or if `instance_id`
///   does not parse as a UUID.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
pub fn configure_sync_schedule(handle: RuntimeHandle,
    instance_id: String,
    sync_interval_secs: u64,
    max_backoff_secs: u64,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_configure_sync_schedule, || {
        let instance = parse_instance_id(&instance_id)?;
        validate_interval("sync_interval_secs", sync_interval_secs)?;
        if max_backoff_secs < sync_interval_secs {
            return Err(FfiError::InvalidId {
                message: format!("configure_sync_schedule: max_backoff_secs ({max_backoff_secs}) \
                     must be >= sync_interval_secs ({sync_interval_secs})"
                ),
            });
        }
        with_runtime(handle, |rt| {
            let scheduler = rt
                .sync_scheduler
                .as_ref()
                .ok_or_else(|| FfiError::Connector {
                    message: "configure_sync_schedule: no scheduler is running on this runtime; \
                          call start_sync_scheduler first"
                        .into(),
                })?;
            // Acquire the policies mutex under the runtime mutex.
            // The lock ordering is intentional: every other path
            // that touches the policies mutex also goes through
            // `with_runtime` first, so there is one canonical
            // acquisition order across the codebase. A poisoned
            // mutex is taken via `into_inner` so a panic on a
            // previous tick does not propagate poisoning to a
            // legitimate FFI call.
            let mut state = match scheduler.state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Preserve `auto_synthesize` across a `configure_sync_schedule`
            // call so a host that previously enabled the
            // post-sync synthesis hook is not silently downgraded
            // by an interval-only update. Hosts that want to flip
            // the bit go through `configure_sync_auto_synthesize`.
            let prior_auto_synth = state
                .policies
                .get(&instance)
                .is_some_and(|p| p.auto_synthesize);
            let policy = SchedulePolicy {
                sync_interval: Duration::from_secs(sync_interval_secs),
                max_backoff: Duration::from_secs(max_backoff_secs),
                auto_synthesize: prior_auto_synth,
            };
            state.policies.insert(instance, policy);
            // Reset accounting for the freshly-configured instance.
            // Any prior consecutive_failures counter is for the
            // old policy and the new policy should start from a
            // clean slate (the host explicitly opted into a fresh
            // schedule for this instance).
            state
                .accounting
                .insert(instance, InstanceAccounting::default());
            Ok(())
        })
    })
}

/// Toggle the post-sync auto-synthesis hook for `instance_id`.
///
/// When `enabled` is `true` the scheduler dispatches a domain-tier
/// `trigger_server_synthesis` after every successful sync of this
/// instance (subject to the per-scope cooldown in
/// [`crate::synthesis::PER_SCOPE_COOLDOWN_SECS`]). The hook is a
/// no-op if [`crate::synthesis::configure_synthesis_engine`] has
/// not been called or if the scope has no domain memory registered.
///
/// Defaults to `false` for every instance. The setting persists
/// across `configure_sync_schedule` calls.
///
/// # Side effects
///
/// If `instance_id` has no per-instance policy override yet (i.e.
/// the host has never called [`configure_sync_schedule`] for it),
/// this function creates a fresh policy entry seeded from the
/// scheduler defaults (`scheduler.config`) with `auto_synthesize`
/// set to `enabled`. As a consequence, the instance shows up in the
/// `policy_override_count` reported by `sync_scheduler_status` even
/// when only the auto-synthesis bit was customised. This is
/// intentional: the alternative would be to require a prior
/// `configure_sync_schedule` call before toggling auto-synth, which
/// is needlessly ceremonious for hosts that want to opt in to the
/// default cadence + auto-synth.
///
/// To return an instance to the pure-defaults state (no policy
/// override **and** no auto-synth), call **in this order**:
///
/// 1. `configure_sync_auto_synthesize(handle, instance_id, false)`
/// 2. [`clear_sync_schedule(handle, instance_id)`](clear_sync_schedule)
///
/// The order matters. [`clear_sync_schedule`] preserves the
/// `auto_synthesize` flag by re-inserting a defaults-seeded policy
/// when it was `true`, so calling `clear` *first* leaves a
/// defaults-seeded entry behind and a subsequent
/// `configure_sync_auto_synthesize(false)` only mutates that
/// entry's flag — `policy_override_count` stays at 1. Setting
/// `auto_synthesize = false` *before* `clear` lets `clear` see
/// `prior_auto_synth = false` and remove the entry entirely,
/// dropping `policy_override_count` to 0. See
/// [`clear_sync_schedule`]'s "Auto-synthesis interaction" section
/// for the underlying mechanic and the integration test
/// `clear_sync_schedule_preserves_auto_synthesize` for a
/// runnable example of both orderings.
///
/// # Errors
///
/// * [`FfiError::Connector`] if no scheduler is running on this
///   handle.
/// * [`FfiError::InvalidId`] if `instance_id` does not parse as a
///   UUID.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
pub fn configure_sync_auto_synthesize(handle: RuntimeHandle,
    instance_id: String,
    enabled: bool,
) -> FfiResult<()> {
    crate::metrics::instrument(crate::metrics::inc_configure_sync_auto_synthesize, || {
        let instance = parse_instance_id(&instance_id)?;
        with_runtime(handle, |rt| {
            let scheduler = rt
                .sync_scheduler
                .as_ref()
                .ok_or_else(|| FfiError::Connector {
                    message: "configure_sync_auto_synthesize: no scheduler is running on this \
                              runtime; call start_sync_scheduler first"
                        .into(),
                })?;
            let mut state = match scheduler.state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let policy = state
                .policies
                .entry(instance)
                .or_insert_with(|| SchedulePolicy::from_defaults(&scheduler.config));
            policy.auto_synthesize = enabled;
            Ok(())
        })
    })
}

/// Remove the per-instance policy override for `instance_id`.
///
/// After this call the instance falls back to the scheduler's
/// defaults (set at [`start_sync_scheduler`] time). Also clears the
/// instance's accounting state so a long-Failing instance gets a
/// fresh chance under the defaults.
///
/// Idempotent: clearing an instance with no override returns
/// `Ok(())`.
///
/// # Auto-synthesis interaction
///
/// `clear_sync_schedule` resets the instance’s interval / backoff
/// to the scheduler’s defaults while **preserving** the
/// `auto_synthesize` flag set via
/// [`configure_sync_auto_synthesize`]. This is symmetric with
/// [`configure_sync_schedule`], which also carries the
/// `auto_synthesize` bit forward on interval-only updates.
///
/// If the instance never had `auto_synthesize: true`, the policy
/// entry is removed entirely (bringing `policy_override_count`
/// down by one); if it *did*, the entry is replaced by a
/// defaults-seeded policy that keeps `auto_synthesize: true`.
///
/// # Errors
///
/// * [`FfiError::Connector`] if no scheduler is running on this
///   handle.
/// * [`FfiError::InvalidId`] if `instance_id` does not parse as a
///   UUID.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
pub fn clear_sync_schedule(handle: RuntimeHandle, instance_id: String) -> FfiResult<()> {
    metrics::instrument(metrics::inc_clear_sync_schedule, || {
        let instance = parse_instance_id(&instance_id)?;
        with_runtime(handle, |rt| {
            let scheduler = rt
                .sync_scheduler
                .as_ref()
                .ok_or_else(|| FfiError::Connector {
                    message: "clear_sync_schedule: no scheduler is running on this runtime; \
                          call start_sync_scheduler first"
                        .into(),
                })?;
            let mut state = match scheduler.state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let prior_auto_synth = state
                .policies
                .get(&instance)
                .is_some_and(|p| p.auto_synthesize);
            state.policies.remove(&instance);
            state.accounting.remove(&instance);
            // Preserve the `auto_synthesize` flag so a clear /
            // re-cadence cycle does not silently disable
            // post-sync synthesis.  If the flag was false (or
            // never set), the remove above already restored
            // pure-defaults semantics.
            if prior_auto_synth {
                state
                    .policies
                    .entry(instance)
                    .or_insert_with(|| SchedulePolicy::from_defaults(&scheduler.config))
                    .auto_synthesize = true;
            }
            Ok(())
        })
    })
}

/// Snapshot the scheduler's diagnostic state.
///
/// Returns a [`SyncSchedulerStatus`] with running-or-stopped state
/// plus per-counter totals + the most recent tick timestamp. The
/// counters reset on [`start_sync_scheduler`] (a fresh scheduler
/// starts at zero); they are monotonic across the scheduler's
/// lifetime.
///
/// # Errors
///
/// * [`FfiError::NotFound`] if `handle` does not correspond to a
///   currently-open runtime. (Stopped scheduler is NOT an error —
///   the status reports `is_running=false` and zero counters.)
#[uniffi::export]
pub fn sync_scheduler_status(handle: RuntimeHandle) -> FfiResult<SyncSchedulerStatus> {
    metrics::instrument(metrics::inc_sync_scheduler_status, || {
        with_runtime(handle, |rt| {
            let Some(scheduler) = rt.sync_scheduler.as_ref() else {
                return Ok(SyncSchedulerStatus {
                    is_running: false,
                    started_at_unix: None,
                    default_interval_secs: 0,
                    default_max_backoff_secs: 0,
                    tick_interval_secs: 0,
                    policy_override_count: 0,
                    total_instance_count: 0,
                    last_tick_at_unix: None,
                    ticks_completed: 0,
                    dispatches_attempted: 0,
                    dispatches_succeeded: 0,
                    dispatches_failed: 0,
                    dispatches_skipped_in_progress: 0,
                });
            };
            // `total_instance_count` is captured under the same
            // `with_runtime` mutex acquisition that lets us reach
            // `scheduler` at all — the connector_instances map and
            // the scheduler slot are siblings on the same runtime,
            // so reading both atomically across the same mutex hold
            // means the two counts cannot disagree because one was
            // sampled before a mutation and the other after.
            let total_instance_count =
                u32::try_from(rt.connector_instances.len()).unwrap_or(u32::MAX);
            let state = match scheduler.state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            Ok(SyncSchedulerStatus {
                is_running: true,
                started_at_unix: Some(scheduler.started_at.timestamp()),
                default_interval_secs: scheduler.config.default_interval.as_secs(),
                default_max_backoff_secs: scheduler.config.default_max_backoff.as_secs(),
                tick_interval_secs: scheduler.config.tick_interval.as_secs(),
                policy_override_count: u32::try_from(state.policies.len()).unwrap_or(u32::MAX),
                total_instance_count,
                last_tick_at_unix: state.last_tick_at.map(|t| t.timestamp()),
                ticks_completed: scheduler.counters.ticks_completed.load(Ordering::Relaxed),
                dispatches_attempted: scheduler
                    .counters
                    .dispatches_attempted
                    .load(Ordering::Relaxed),
                dispatches_succeeded: scheduler
                    .counters
                    .dispatches_succeeded
                    .load(Ordering::Relaxed),
                dispatches_failed: scheduler.counters.dispatches_failed.load(Ordering::Relaxed),
                dispatches_skipped_in_progress: scheduler
                    .counters
                    .dispatches_skipped_in_progress
                    .load(Ordering::Relaxed),
            })
        })
    })
}

// ──────────────────────── Health probe helper ───────────────────

/// Surface scheduler running state for the connector subsystem
/// health probe. Used by [`crate::health::connector_subsystem`] to
/// extend its `detail` string with `sync_scheduler=running` /
/// `sync_scheduler=stopped`.
///
/// Pure diagnostic — never causes the probe to flip to `Degraded`.
/// Most ingest-only hosts (offline CLI tools, Electron status
/// panels) never start a scheduler.
pub(crate) fn scheduler_health_detail(rt: &crate::runtime::FfiRuntime) -> &'static str {
    if rt.sync_scheduler.is_some() {
        "sync_scheduler=running"
    } else {
        "sync_scheduler=stopped"
    }
}

// ──────────── Per-instance scheduler-state probe ───────────────

/// Snapshot of one connector instance's scheduler-side state for
/// the  [`crate::connector::connector_status`]
/// surface. Bundled into a single record so the caller can build
/// the wire-flat `ConnectorHealthRecord` without holding the
/// scheduler-state mutex across the rest of the assembly logic.
///
/// `None` for either field means "scheduler is not running on
/// this runtime": when the dispatch worker is stopped the
/// `default_interval` / `default_max_backoff` configured at
/// [`start_sync_scheduler`] time are not available either, so a
/// `0` interval is reported by the caller (the
/// `ConnectorHealthRecord` documents the `0`-iff-stopped
/// convention explicitly).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct InstanceSchedulerSnapshot {
    /// `true` iff a [`RunningSyncScheduler`] is currently
    /// installed on this runtime.
    pub(crate) is_scheduler_running: bool,
    /// Effective per-instance sync interval in seconds. Reflects
    /// the host-supplied [`configure_sync_schedule`] override if
    /// present, or the scheduler-default
    /// [`SchedulerConfig::default_interval`] otherwise. `0` iff
    /// the scheduler is not running.
    pub(crate) sync_interval_secs: u64,
    /// Effective per-instance max-backoff cap in seconds. Same
    /// override-then-default precedence as
    /// [`Self::sync_interval_secs`]; `0` iff the scheduler is not
    /// running.
    pub(crate) max_backoff_secs: u64,
    /// `true` iff the per-instance policy is configured to fire a
    /// domain-tier
    /// [`crate::synthesis::trigger_server_synthesis`] after each
    /// successful sync. Mirrors
    /// [`SchedulePolicy::auto_synthesize`].
    pub(crate) auto_synthesize: bool,
    /// Consecutive failures since the last successful sync.
    /// Reset to `0` on success. `0` if the scheduler has never
    /// dispatched this instance OR if the scheduler is stopped.
    pub(crate) consecutive_failures: u32,
    /// Unix epoch seconds for the next scheduled dispatch
    /// attempt, or `None` if the scheduler is stopped / has never
    /// dispatched this instance (in which case the next tick
    /// fires it immediately).
    pub(crate) next_attempt_unix: Option<i64>,
}

/// Probe the scheduler's per-instance state for one
/// [`ConnectorInstanceId`]. Used by
/// [`crate::connector::connector_status`] to fold the
/// scheduler-side fields into the [`ConnectorHealthRecord`] without
/// duplicating the policy + accounting lookup logic.
///
/// Acquires the scheduler-state mutex briefly to read both the
/// policy override (if any) and the accounting entry (if any).
/// The mutex is dropped before this function returns so the
/// caller does NOT hold scheduler-state across the rest of the
/// `connector_status` assembly.
///
/// Returns an all-zero / `is_scheduler_running=false` snapshot
/// when the scheduler is stopped — `connector_status` is meant
/// to remain useful even on hosts that never call
/// [`start_sync_scheduler`].
pub(crate) fn instance_scheduler_snapshot(rt: &crate::runtime::FfiRuntime,
    instance: ConnectorInstanceId,
) -> InstanceSchedulerSnapshot {
    let Some(scheduler) = rt.sync_scheduler.as_ref() else {
        return InstanceSchedulerSnapshot::default();
    };
    let state = match scheduler.state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let policy = state
        .policies
        .get(&instance)
        .cloned()
        .unwrap_or_else(|| SchedulePolicy::from_defaults(&scheduler.config));
    let accounting = state.accounting.get(&instance).cloned().unwrap_or_default();
    InstanceSchedulerSnapshot {
        is_scheduler_running: true,
        sync_interval_secs: policy.sync_interval.as_secs(),
        max_backoff_secs: policy.max_backoff.as_secs(),
        auto_synthesize: policy.auto_synthesize,
        consecutive_failures: accounting.consecutive_failures,
        next_attempt_unix: accounting.next_attempt_at.map(|t| t.timestamp()),
    }
}

// ──────────────────────── remove_connector hook ─────────────────

/// Drop the scheduler's per-instance policy + accounting entries
/// for `instance`. Called from [`crate::remove_connector`] under
/// the runtime mutex.
///
/// Without this hook, an instance removed via `remove_connector`
/// would leave a stale [`SchedulePolicy`] in `state.policies` and
/// a stale [`InstanceAccounting`] in `state.accounting`. The leak
/// is bounded by the number of distinct connector instances the
/// process has ever created (each entry is ~80 bytes), but on a
/// long-running substrate where hosts churn instances this is a
/// latent resource concern and the policy-count gauge surfaced
/// through [`SyncSchedulerStatus::policy_override_count`] would
/// drift away from the live instance count surfaced by
/// [`SyncSchedulerStatus::total_instance_count`].
///
/// Pruning here is `pub(crate)` rather than part of the public
/// FFI: it's a coupling between two substrate-internal modules
/// (the connector lifecycle and the scheduler state), not a host
/// observable.
///
/// Idempotent — pruning an instance that has no policy or
/// accounting entry is a no-op.
pub(crate) fn prune_instance(rt: &crate::runtime::FfiRuntime, instance: ConnectorInstanceId) {
    let Some(scheduler) = rt.sync_scheduler.as_ref() else {
        // No scheduler running — nothing to prune.
        return;
    };
    // Same poisoned-mutex discipline as every other policies
    // mutex acquisition in this module: take the inner state
    // through `into_inner` so a panic on a previous tick does
    // not propagate poisoning to a legitimate FFI call.
    let mut state = match scheduler.state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.policies.remove(&instance);
    state.accounting.remove(&instance);
}

// ──────────────────────── Worker thread ─────────────────────────

/// Main loop of the scheduler worker thread.
///
/// Wakes every `tick_interval`, walks the connector map, dispatches
/// `sync_connector` for every due instance, records the result.
/// Exits cleanly when:
///
/// * The shutdown oneshot fires (`recv_timeout` returns `Ok(())` —
///   [`stop_sync_scheduler`] sent us a signal).
/// * The shutdown sender is dropped (`recv_timeout` returns
///   `Disconnected` — the [`RunningSyncScheduler`] was dropped
///   without an explicit stop, e.g. a runtime teardown crashed
///   mid-way through).
fn run_scheduler_loop(handle: RuntimeHandle,
    config: &SchedulerConfig,
    state: &Arc<Mutex<SchedulerState>>,
    counters: &Arc<SchedulerCounters>,
    shutdown_rx: &mpsc::Receiver<()>,
) {
    debug!(handle = handle.0, "sync scheduler worker thread entered");
    loop {
        match shutdown_rx.recv_timeout(config.tick_interval) {
            Ok(()) => {
                debug!(handle = handle.0, "sync scheduler received shutdown signal");
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                // Tick boundary — run one pass.
                run_one_tick(handle, config, state, counters);
                counters.ticks_completed.fetch_add(1, Ordering::Relaxed);
                metrics::inc_sync_scheduler_tick();
            }
            Err(RecvTimeoutError::Disconnected) => {
                debug!(handle = handle.0,
                    "sync scheduler sender disconnected (likely runtime drop)"
                );
                break;
            }
        }
    }
    debug!(handle = handle.0, "sync scheduler worker thread exited");
}

/// One pass of the scheduler. Snapshot due instances, dispatch
/// each, record the result. Three-phase locking applies: the
/// snapshot acquires the runtime mutex briefly to enumerate
/// instances + read their `last_synced_at`, the dispatch runs
/// UNLOCKED through `sync_connector`, the result-record phase
/// re-acquires the policies mutex to update `consecutive_failures`
/// + `next_attempt_at`.
///
/// # Timestamp discipline (load-bearing — read before refactoring)
///
/// `now = Utc::now()` is captured ONCE at tick start and used
/// only for the  due-instance check.  captures a
/// FRESH `dispatch_completed_at = Utc::now()` after each
/// `sync_connector` call returns and uses that for the
/// `next_attempt_at` arithmetic. Reusing the tick-start `now`
/// for  would (a) schedule retries in the past whenever
/// the backoff delay is shorter than the cumulative dispatch
/// time of preceding instances in the same tick — defeating
/// exponential backoff entirely — and (b) synchronise every
/// cohort that first became due on the same tick to a single
/// future timestamp, producing a persistent thundering herd on
/// every subsequent tick. The per-dispatch capture fixes both.
///
/// # Lock ordering (load-bearing — read before refactoring)
///
/// The worker thread NEVER holds the runtime mutex and the
/// scheduler state mutex simultaneously. The acquisition pattern
/// in this function is:
///
/// 1. Acquire runtime mutex via [`with_runtime`] (Step 1
///    snapshot). Drop it on closure return.
/// 2. Acquire scheduler state mutex (read policies + accounting).
///    Drop it before Step 2.
/// 3.  dispatch: NO locks held — `sync_connector`
///    re-acquires the runtime mutex on its own, observing the
///    substrate's published three-phase discipline.
/// 4.  result-record: re-acquire the scheduler state
///    mutex briefly to update accounting. Drop it before exit.
///
/// The FFI surface (`configure_sync_schedule`, `clear_sync_schedule`,
/// `prune_instance`) takes the opposite order: it holds the runtime
/// mutex (via `with_runtime`) and acquires the scheduler state
/// mutex INSIDE that closure. This is the canonical ordering
/// documented at the FFI sites (see [`configure_sync_schedule`]).
///
/// The worker's reversed order (state mutex outside the runtime
/// mutex) is deadlock-free ONLY because the worker drops the
/// runtime mutex before acquiring the state mutex — both locks
/// are never held simultaneously. A future refactor that pulled
/// the state-mutex acquisition INSIDE the `with_runtime` closure
/// here would NOT introduce a deadlock (it would match the FFI
/// ordering), but a refactor that pulled a `with_runtime` call
/// INSIDE a `state.lock()` guard WOULD deadlock against the FFI
/// path. Maintain this invariant when modifying the function.
fn run_one_tick(handle: RuntimeHandle,
    config: &SchedulerConfig,
    state: &Arc<Mutex<SchedulerState>>,
    counters: &Arc<SchedulerCounters>,
) {
    let now = Utc::now();

    // ─── Step 1: snapshot due instances (locked) ─────────────
    let due_instances: Vec<ConnectorInstanceId> = {
        // Re-entering `with_runtime` from the scheduler thread is
        // exactly the contract the FFI surface requires of every
        // host-driven call. If the runtime is being torn down
        // (handle no longer in the registry), this returns
        // `NotFound` and we silently stop dispatching — the
        // close_store pre-drain will join us shortly.
        let snapshot_result = with_runtime(handle,
            |rt| -> FfiResult<Vec<(ConnectorInstanceId, SyncStatus, Option<DateTime<Utc>>)>> {
                Ok(rt
                    .connector_instances
                    .values()
                    .map(|inst| {
                        (inst.id,
                            inst.sync_state.status,
                            inst.sync_state.last_synced_at,
                        )
                    })
                    .collect())
            },
        );
        let Ok(rows) = snapshot_result else {
            // Runtime gone — bail without dispatching. The
            // worker will exit on the next `recv_timeout` cycle
            // when the sender drops (close_store consumed the
            // RunningSyncScheduler).
            return;
        };

        // Single state-mutex acquisition for the whole snapshot
        // phase: update `last_tick_at` (keeps the diagnostic
        // timestamp accurate even on a tick that finds no due
        // instances) and read out the policies / accounting maps
        // under one critical section. A prior implementation split
        // these into two consecutive lock-then-drop / lock-then-drop
        // pairs; the merge is correctness-neutral (no observer can
        // ever see a state where last_tick_at is updated but the
        // policy/accounting reads aren't yet — they're racing
        // counterparts in the same tick) but spares one
        // lock/unlock cycle per tick.
        let mut s = match state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        s.last_tick_at = Some(now);
        let mut due = Vec::with_capacity(rows.len());
        for (id, status, last_synced_at) in rows {
            // Skip instances already syncing — a host-driven
            // sync collided with our tick. The scheduler is a
            // background driver, not a parallel executor.
            if matches!(status, SyncStatus::InProgress) {
                counters
                    .dispatches_skipped_in_progress
                    .fetch_add(1, Ordering::Relaxed);
                metrics::inc_sync_scheduler_dispatch_skipped_in_progress();
                continue;
            }
            // Resolve policy: per-instance override wins over
            // defaults.
            let policy = s
                .policies
                .get(&id)
                .cloned()
                .unwrap_or_else(|| SchedulePolicy::from_defaults(config));
            let accounting = s.accounting.get(&id).cloned().unwrap_or_default();
            let next_attempt_at = accounting.next_attempt_at.unwrap_or_else(|| {
                // Brand-new entry, scheduler has not dispatched it
                // yet. Derive the first attempt time from the
                // connector's own `last_synced_at` (if any) so a
                // host that wires the scheduler at boot doesn't
                // immediately re-sync a connector that just
                // finished a successful sync 30s before
                // start_sync_scheduler.
                last_synced_at.map_or(now, |t| {
                    t + chrono::Duration::from_std(policy.sync_interval)
                        .unwrap_or_else(|_| chrono::Duration::seconds(0))
                })
            });
            if now >= next_attempt_at {
                due.push(id);
            }
        }
        due
    };

    // ─── Step 2: dispatch each due instance (unlocked) ───────
    //
    // Each `sync_connector` call walks the substrate's three-phase
    // discipline itself; the scheduler is just another client.
    //
    //  below uses a FRESH `Utc::now()` captured AFTER each
    // dispatch returns — NOT the tick-start `now`. With small
    // intervals and slow upstream providers (e.g. 1 s `sync_interval`
    // against a 10 s dispatch) reusing the tick-start `now` would
    // schedule `next_attempt_at` in the past for every instance
    // beyond the first in the tick, defeating exponential backoff
    // entirely. Capturing the timestamp per-dispatch also breaks
    // the thundering-herd pattern: instances that first became
    // due on the same tick will diverge by the dispatch latency of
    // the previous instances, naturally staggering future ticks
    // instead of synchronising every cohort on a single future
    // timestamp.
    for instance_id in due_instances {
        counters
            .dispatches_attempted
            .fetch_add(1, Ordering::Relaxed);
        metrics::inc_sync_scheduler_dispatch_attempted();
        let result = crate::sync_connector(handle, instance_id.0.to_string());
        let dispatch_completed_at = Utc::now();
        // ─── Step 3: record result (locked) ─────────────────
        let mut s = match state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let policy = s
            .policies
            .get(&instance_id)
            .cloned()
            .unwrap_or_else(|| SchedulePolicy::from_defaults(config));
        let entry = s.accounting.entry(instance_id).or_default();
        match result {
            Ok(_) => {
                counters
                    .dispatches_succeeded
                    .fetch_add(1, Ordering::Relaxed);
                metrics::inc_sync_scheduler_dispatch_succeeded();
                entry.consecutive_failures = 0;
                let delay = policy.next_attempt_delay(0);
                entry.next_attempt_at = Some(dispatch_completed_at
                        + chrono::Duration::from_std(delay)
                            .unwrap_or_else(|_| chrono::Duration::seconds(0)),
                );
                let auto_synthesize = policy.auto_synthesize;
                // Drop the scheduler state lock before we re-enter
                // the runtime mutex to resolve the scope and
                // dispatch synthesis. Reusing the same lock-order
                // discipline as the rest of the scheduler: runtime
                // → scheduler-state, never the reverse.
                drop(s);
                if auto_synthesize {
                    maybe_dispatch_auto_synthesis(handle, instance_id);
                }
            }
            Err(err) => {
                counters.dispatches_failed.fetch_add(1, Ordering::Relaxed);
                metrics::inc_sync_scheduler_dispatch_failed();
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                let delay = policy.next_attempt_delay(entry.consecutive_failures);
                entry.next_attempt_at = Some(dispatch_completed_at
                        + chrono::Duration::from_std(delay)
                            .unwrap_or_else(|_| chrono::Duration::seconds(0)),
                );
                debug!(handle = handle.0,
                    instance = %instance_id.0,
                    consecutive_failures = entry.consecutive_failures,
                    delay_secs = delay.as_secs(),
                    error = %err,
                    "scheduler dispatch failed; backing off",
                );
            }
        }
    }
}

// ──────────────────────── Helpers ───────────────────────────────

/// Best-effort post-sync synthesis dispatch. Called from the
/// scheduler's `Ok(_)` arm when the per-instance policy has
/// `auto_synthesize: true`.
///
/// The dispatch is **fire-and-forget** from the scheduler's
/// perspective — synthesis failure does NOT roll back the sync
/// success or feed the failure counter; the scheduler is a
/// best-effort driver, not a transactional coordinator. The
/// substrate's per-scope cooldown
/// ([`crate::synthesis::PER_SCOPE_COOLDOWN_SECS`]) prevents a
/// runaway loop when the scheduler ticks faster than synthesis can
/// complete.
fn maybe_dispatch_auto_synthesis(handle: RuntimeHandle, instance_id: ConnectorInstanceId) {
    // ─── Resolve the scope / engine state under the runtime mutex.
    let resolved = with_runtime(handle, |rt| -> FfiResult<Option<(ScopeId, bool)>> {
        if rt.synthesis_engine.is_none() {
            return Ok(None);
        }
        let Some(inst) = rt.connector_instances.get(&instance_id) else {
            return Ok(None);
        };
        let scope = inst.config.scope_id;
        let domain_registered = rt.domain_memory(scope).is_some();
        Ok(Some((scope, domain_registered)))
    });
    let Ok(Some((scope, domain_registered))) = resolved else {
        return;
    };
    if !domain_registered {
        // Channel-tier scope — fall back to the on-device
        // `trigger_synthesis` path. We do not auto-trigger that
        // here because the host owns the channel-recap policy
        // (it knows whether the channel has accumulated enough
        // evidence to warrant a recap).
        return;
    }
    // The actual dispatch runs with the runtime mutex released —
    // `trigger_server_synthesis` re-acquires it inside its own
    // three-phase locking discipline.
    match crate::synthesis::trigger_server_synthesis(handle,
        scope.as_uuid().to_string(),
        crate::types::SynthesisTierKind::Domain,
    ) {
        Ok(window_id) => {
            debug!(instance = %instance_id.0,
                scope = %scope.as_uuid(),
                window = %window_id,
                "scheduler: post-sync auto-synthesis dispatched",
            );
        }
        Err(err) => {
            debug!(instance = %instance_id.0,
                scope = %scope.as_uuid(),
                error = %err,
                "scheduler: post-sync auto-synthesis skipped (best-effort)",
            );
        }
    }
}

fn parse_instance_id(s: &str) -> FfiResult<ConnectorInstanceId> {
    s.parse::<uuid::Uuid>()
        .map(ConnectorInstanceId)
        .map_err(|e| FfiError::InvalidId {
            message: format!("instance_id is not a UUID: {e}"),
        })
}

fn validate_interval(name: &str, v: u64) -> FfiResult<()> {
    if v < MIN_INTERVAL_SECS {
        Err(FfiError::InvalidId {
            message: format!("{name} must be >= {MIN_INTERVAL_SECS} (a zero interval would dispatch every \
                 tick regardless of upstream pressure)"
            ),
        })
    } else {
        Ok(())
    }
}

fn validate_tick(name: &str, v: u64) -> FfiResult<()> {
    if v < MIN_TICK_SECS {
        Err(FfiError::InvalidId {
            message: format!("{name} must be >= {MIN_TICK_SECS} (a faster tick burns CPU without useful \
                 resolution improvement)"
            ),
        })
    } else {
        Ok(())
    }
}

// ──────────────────────── Public sentinel constants ─────────────

/// Default `default_interval_secs` (15 minutes). Hosts that don't
/// have a strong opinion can pass this directly to
/// [`start_sync_scheduler`].
pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = DEFAULT_INTERVAL_SECS;

/// Default `default_max_backoff_secs` (8 hours).
pub const DEFAULT_SYNC_MAX_BACKOFF_SECS: u64 = DEFAULT_MAX_BACKOFF_SECS;

/// Default `tick_interval_secs` (30 seconds).
pub const DEFAULT_SYNC_TICK_SECS: u64 = DEFAULT_TICK_SECS;

// ──────────────────────── Internal unit tests ───────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_attempt_delay_baseline_is_interval() {
        let p = SchedulePolicy {
            sync_interval: Duration::from_secs(60),
            max_backoff: Duration::from_secs(3600),
            auto_synthesize: false,
        };
        assert_eq!(p.next_attempt_delay(0), Duration::from_secs(60));
    }

    #[test]
    fn next_attempt_delay_doubles_with_failures() {
        let p = SchedulePolicy {
            sync_interval: Duration::from_secs(60),
            max_backoff: Duration::from_secs(3600),
            auto_synthesize: false,
        };
        assert_eq!(p.next_attempt_delay(1), Duration::from_secs(120));
        assert_eq!(p.next_attempt_delay(2), Duration::from_secs(240));
        assert_eq!(p.next_attempt_delay(3), Duration::from_secs(480));
    }

    #[test]
    fn next_attempt_delay_caps_at_max_backoff() {
        let p = SchedulePolicy {
            sync_interval: Duration::from_secs(60),
            max_backoff: Duration::from_secs(300),
            auto_synthesize: false,
        };
        // 60 * 2^10 = 61440s, well past the 300s cap.
        assert_eq!(p.next_attempt_delay(10), Duration::from_secs(300));
    }

    #[test]
    fn next_attempt_delay_saturates_on_huge_consecutive_failures() {
        // A long-Failing instance held in a Failed state across
        // weeks must never overflow Duration.
        let p = SchedulePolicy {
            sync_interval: Duration::from_secs(60),
            max_backoff: Duration::from_secs(3600),
            auto_synthesize: false,
        };
        // u32::MAX failures is the worst case. Should saturate
        // cleanly at max_backoff, not panic on shift overflow.
        assert_eq!(p.next_attempt_delay(u32::MAX), Duration::from_secs(3600));
    }

    #[test]
    fn parse_instance_id_round_trips() {
        let id = ConnectorInstanceId::new_v4();
        let parsed = parse_instance_id(&id.0.to_string()).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn parse_instance_id_rejects_garbage() {
        let err = parse_instance_id("not-a-uuid").unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn validate_interval_rejects_zero() {
        assert!(validate_interval("test", 0).is_err());
    }

    #[test]
    fn validate_interval_accepts_one() {
        assert!(validate_interval("test", 1).is_ok());
    }

    #[test]
    fn validate_tick_rejects_zero() {
        assert!(validate_tick("test", 0).is_err());
    }
}
