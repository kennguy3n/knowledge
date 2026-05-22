//! [`InferenceRouter`] — orchestrates adapter probing, dispatch, and
//! warm-up / idle-unload.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::config::RouterConfig;
use crate::error::RouterError;
use crate::task::InferenceTask;

/// Internal record of the most recent dispatch — drives the
/// idle-unload sweep.
#[derive(Debug, Clone, Copy)]
struct AdapterActivity {
    kind: AdapterKind,
    last_dispatch: Instant,
    loaded: bool,
}

/// Static table of every [`InferenceTask`] variant — kept here so
/// [`InferenceRouter::adapter_states`] can enumerate per-adapter
/// task support without depending on an external introspection
/// crate. Mirrors the variants in `crate::task::InferenceTask`.
/// Adding a new task variant requires a matching entry here; the
/// `adapter_states_lists_every_task_for_each_adapter` unit test
/// pins the cardinality.
const ALL_TASKS: &[InferenceTask] = &[
    InferenceTask::TagImportance,
    InferenceTask::ExtractEntities,
    InferenceTask::PromoteObservation,
    InferenceTask::SynthSummary,
    InferenceTask::SynthConcept,
    InferenceTask::AdjudicateContradiction,
];

/// Wire-flat view of one adapter's current state. Returned by
/// [`InferenceRouter::adapter_states`].
#[derive(Debug, Clone)]
pub struct AdapterState {
    /// Stable string tag for the adapter (the `kind` field).
    pub kind: AdapterKind,
    /// `true` once [`InferenceAdapter::probe`] returned
    /// [`ProbeResult::Available`] and no subsequent failure marked
    /// the adapter offline.
    pub available: bool,
    /// `true` iff the adapter is currently loaded into memory
    /// (i.e. has been dispatched to and not yet swept idle by
    /// [`InferenceRouter::sweep_idle_adapters`]).
    pub loaded: bool,
    /// Tasks this adapter declares it can serve via
    /// [`InferenceAdapter::supports`]. Empty when the adapter is
    /// available but supports none of the substrate's tasks (e.g.
    /// a misconfigured `FallbackAdapter` on a device tier that
    /// disabled classification).
    pub supports: Vec<InferenceTask>,
}

/// The on-device inference router.
///
/// Holds an ordered list of [`InferenceAdapter`]s — typically `MLX →
/// llama.cpp → Fallback`. Probes them at boot, dispatches each
/// [`InferenceTask`] to the highest-priority adapter that is available
/// and supports the task, and unloads adapters whose last dispatch is
/// older than [`RouterConfig::idle_timeout_secs`].
pub struct InferenceRouter {
    config: RouterConfig,
    adapters: Vec<Box<dyn InferenceAdapter>>,
    bootstrapped: AtomicBool,
    activity: Mutex<Vec<AdapterActivity>>,
    warmed: AtomicBool,
    /// Signals completion of [`Self::bootstrap`] / [`Self::spawn_bootstrap`]
    /// to threads blocked in [`Self::wait_for_bootstrap`].
    ///
    /// The bool in the mutex flips to `true` exactly once — either
    /// synchronously from the end of [`Self::bootstrap`] or from
    /// the background thread spawned by [`Self::spawn_bootstrap`].
    /// Once flipped, the condvar is `notify_all`'d so every waiter
    /// can proceed. The atomic [`Self::bootstrapped`] flag is
    /// retained for cheap polling on the dispatch hot path; the
    /// condvar exists specifically for the blocking-wait path that
    /// hosts use when they need a synchronisation point against
    /// the background probe.
    bootstrap_signal: (Mutex<bool>, Condvar),
    /// Join handle for the background bootstrap thread, if one was
    /// spawned via [`Self::spawn_bootstrap`]. Owned here so
    /// [`Self::shutdown`] (and the [`Drop`] impl) can `join()` the
    /// thread instead of leaving it detached — a detached probe
    /// thread that outlives the FFI runtime would briefly keep
    /// network sockets open (e.g. the llama.cpp `/health` probe)
    /// after the host has called `close_store`, which the substrate
    /// avoids by waiting on the handle at shutdown time. Mutexed
    /// rather than atomic-swap because [`JoinHandle`] is not
    /// `Copy` / `Atomic`-friendly; contention is irrelevant in
    /// practice (spawn/shutdown are infrequent).
    bootstrap_handle: Mutex<Option<JoinHandle<()>>>,
}

impl InferenceRouter {
    /// Construct a router from a config and an ordered list of
    /// adapters. Order is priority order — index 0 is highest
    /// priority. Adapters must be probed via [`Self::bootstrap`]
    /// before [`Self::dispatch`] is called.
    pub fn new(config: RouterConfig, adapters: Vec<Box<dyn InferenceAdapter>>) -> Self {
        let activity = adapters
            .iter()
            .map(|a| AdapterActivity {
                kind: a.kind(),
                last_dispatch: Instant::now(),
                loaded: false,
            })
            .collect();
        Self {
            config,
            adapters,
            bootstrapped: AtomicBool::new(false),
            activity: Mutex::new(activity),
            warmed: AtomicBool::new(false),
            bootstrap_signal: (Mutex::new(false), Condvar::new()),
            bootstrap_handle: Mutex::new(None),
        }
    }

    /// Run [`InferenceAdapter::probe`] on every adapter. Must be
    /// called before [`Self::dispatch`].
    ///
    /// Synchronous — the call blocks until every adapter has been
    /// probed. Hosts that want to keep their `open` / startup path
    /// non-blocking should use [`Self::spawn_bootstrap`] instead,
    /// which dispatches this same logic onto a background thread
    /// and lets later [`Self::dispatch`] callers block on
    /// [`Self::wait_for_bootstrap`].
    pub fn bootstrap(&self) -> Vec<(AdapterKind, ProbeResult)> {
        let results = self
            .adapters
            .iter()
            .map(|a| (a.kind(), a.probe()))
            .collect();
        self.bootstrapped.store(true, Ordering::SeqCst);
        // Wake any threads blocked in `wait_for_bootstrap` — the
        // synchronous bootstrap path must satisfy the same
        // contract as the background one so callers can use
        // either entry point interchangeably.
        self.notify_bootstrap_complete();
        results
    }

    /// Run [`Self::bootstrap`] on a dedicated background thread.
    ///
    /// Returns immediately; the thread itself probes every adapter
    /// and, on completion, flips the [`Self::bootstrapped`] flag and
    /// notifies waiters blocked in [`Self::wait_for_bootstrap`].
    /// Use this from the FFI `open_store` path to keep the open
    /// call non-blocking when an adapter probe might hit the
    /// network (e.g. the `http-client`-backed llama.cpp adapter
    /// pings `GET /health` with a multi-second timeout).
    ///
    /// The spawned thread's [`JoinHandle`] is stored on the router so
    /// [`Self::shutdown`] (and the [`Drop`] impl) can `join()` it at
    /// teardown time. Callers do not need to `join` directly; any
    /// thread that needs a synchronisation point against probe
    /// completion calls [`Self::wait_for_bootstrap`]. The handle
    /// exists specifically so the runtime can guarantee no probe
    /// thread outlives the router's logical lifetime — detached
    /// probes would briefly keep network sockets open after the host
    /// has called `close_store`, even though they are memory-safe via
    /// the [`Arc`] the thread holds.
    ///
    /// Calling `spawn_bootstrap` twice on the same router is
    /// supported: the second call joins the prior handle (blocking
    /// briefly if the first probe is still running) before spawning a
    /// fresh thread. This is a defensive guard against host code that
    /// re-bootstraps on configuration reload; the substrate itself
    /// only calls `spawn_bootstrap` once per `open_store`.
    ///
    /// # Panics
    ///
    /// Panics if the underlying OS rejects the thread spawn. This
    /// matches the substrate's policy elsewhere (we treat thread
    /// spawn failures as unrecoverable initialisation faults).
    pub fn spawn_bootstrap(self: Arc<Self>) {
        // Reap any prior handle first so a second call doesn't leak
        // the previous thread. This is a no-op on the common path
        // (one bootstrap per router lifetime).
        self.shutdown();
        // Reset the bootstrap-completion signal so a `wait_for_bootstrap`
        // call that races the second probe parks on the condvar
        // instead of short-circuiting on the prior bootstrap's
        // `done == true` / `bootstrapped == true`. Without this every
        // `dispatch` call between `spawn_bootstrap` (re-spawn) and the
        // new probe's completion would observe stale adapter
        // availability — e.g. an adapter that was previously
        // unreachable but is now back online would still route to
        // `Unavailable` until the new probe lands, which defeats the
        // entire point of re-bootstrapping. This is a no-op on the
        // common path (the very first `spawn_bootstrap` call) because
        // both fields are already `false` from `new`.
        self.reset_bootstrap_state();
        let me = Arc::clone(&self);
        let handle = std::thread::Builder::new()
            .name("inference-router-bootstrap".into())
            .spawn(move || {
                // RAII drop-guard so that even if any adapter's
                // `probe()` panics during [`Self::bootstrap`], the
                // condvar protecting `wait_for_bootstrap` is still
                // signalled — otherwise a single misbehaving adapter
                // would permanently wedge every future
                // `trigger_synthesis` call blocked in
                // [`Self::wait_for_bootstrap`]. The guard is
                // idempotent with the explicit
                // `notify_bootstrap_complete()` inside `bootstrap()`:
                // on the happy path the second notify is a no-op
                // (the flag is already `true` and `notify_all` on an
                // empty waiter list has no observable effect); on a
                // panic, the guard's `Drop` is the only path that
                // notifies. Note that on panic the
                // [`Self::bootstrapped`] atomic stays `false`, so
                // subsequent `dispatch` calls correctly route to
                // their `NotProbed` / `Unavailable` paths rather
                // than racing partial state — the guard only ensures
                // we don't *hang*.
                struct NotifyOnDrop<'r>(&'r InferenceRouter);
                impl Drop for NotifyOnDrop<'_> {
                    fn drop(&mut self) {
                        self.0.notify_bootstrap_complete();
                    }
                }
                let _guard = NotifyOnDrop(&me);
                let results = me.bootstrap();
                for (kind, result) in &results {
                    tracing::info!(
                        adapter = kind.as_str(),
                        probe = ?result,
                        "inference_router adapter probed (background)",
                    );
                }
            })
            .expect("spawn inference-router-bootstrap thread");
        // Stash the handle for `shutdown` / `Drop`. We never call
        // `expect` on the lock here because contention is impossible
        // (the only writers are `spawn_bootstrap` and `shutdown`, and
        // we just returned from the `shutdown` call above) — the
        // `lock()` is conceptually infallible on a non-poisoned mutex
        // and a poisoned one indicates a programmer error worth
        // surfacing as a panic.
        *self.bootstrap_handle.lock().expect("bootstrap_handle lock") = Some(handle);
    }

    /// Join the background bootstrap thread spawned by
    /// [`Self::spawn_bootstrap`], if any. No-op when no background
    /// bootstrap is in flight (either none was spawned, or a prior
    /// `shutdown` already reaped the handle).
    ///
    /// The substrate calls this from `FfiRuntime::Drop` so the
    /// background probe never outlives the runtime that owns it.
    /// External callers may invoke it directly when they want a hard
    /// synchronisation point against probe completion —
    /// [`Self::wait_for_bootstrap`] satisfies the same observable
    /// contract via the condvar, but only `shutdown` guarantees the
    /// OS-level thread has been reaped.
    ///
    /// Safe to call from any thread, multiple times — the second
    /// call sees `None` and returns immediately.
    ///
    /// # Self-join guard
    ///
    /// If `shutdown` is invoked from **the bootstrap thread itself**,
    /// it does NOT join the handle — joining the currently-running
    /// thread would invoke `pthread_join` on `pthread_self()`, which
    /// POSIX leaves explicitly undefined (`EDEADLK` on Linux's glibc,
    /// `EDEADLK` or `EINVAL` on macOS, deadlock on some embedded
    /// libcs). Dropping the [`JoinHandle`] without joining is safe
    /// in that case because the running thread is already past the
    /// closure that owned the work — it can only reach `Drop` /
    /// `shutdown` by having released the last
    /// `Arc<InferenceRouter>` strong reference *after* finishing
    /// the closure. So we are not detaching a still-running probe;
    /// we are detaching a thread that is one stack frame from
    /// returning. Detach in this corner cleans up the join slot
    /// without blocking on a thread that is about to vanish.
    ///
    /// The self-join scenario is only reachable for standalone
    /// embedders that hold no other `Arc<InferenceRouter>` strong
    /// references across the probe's lifetime. The substrate's own
    /// path through [`crate::FfiRuntime`] always pins the router for
    /// the lifetime of the runtime and calls `shutdown` from
    /// `FfiRuntime::Drop` (on the runtime owner's thread, not the
    /// probe thread) before releasing its own `Arc`, so the guard is
    /// a no-op on the production code path.
    pub fn shutdown(&self) {
        let handle = self
            .bootstrap_handle
            .lock()
            .expect("bootstrap_handle lock")
            .take();
        let Some(h) = handle else {
            return;
        };
        if h.thread().id() == std::thread::current().id() {
            // Self-join — see the doc above for the full rationale.
            // We log at `debug` (not `warn`) because this is an
            // expected, documented outcome for standalone-embedder
            // teardown, not a fault. Dropping `h` here detaches the
            // join slot.
            tracing::debug!(
                "inference-router-bootstrap thread is dropping its own router; \
                 skipping self-join (detaching join handle)",
            );
            drop(h);
            return;
        }
        // `join()` on a panicked thread returns `Err`; we don't
        // propagate that here because the bootstrap thread's
        // panic-safety contract is already covered by the
        // `NotifyOnDrop` guard inside `spawn_bootstrap` — the
        // condvar is signalled regardless, so any waiters in
        // `wait_for_bootstrap` have already unblocked. Logging
        // the join failure is the most we can usefully do.
        if let Err(e) = h.join() {
            tracing::warn!(
                error = ?e,
                "inference-router-bootstrap thread panicked during shutdown",
            );
        }
    }

    /// Block the current thread until [`Self::bootstrap`] (or the
    /// background variant spawned by [`Self::spawn_bootstrap`]) has
    /// completed.
    ///
    /// Returns immediately when bootstrap has already finished
    /// (the condvar's `done` flag observed under the lock is the
    /// authoritative signal). Callers racing the probe — typically
    /// an FFI surface invoked right after `open_store` — park on
    /// the condvar until the bootstrap thread notifies completion.
    ///
    /// An earlier revision short-circuited on a lockless atomic read
    /// of [`Self::bootstrapped`] before taking the lock, which under
    /// the [`Self::reset_bootstrap_state`] / re-spawn path was racy:
    /// SeqCst guarantees a total order over the atomic load and the
    /// (reset) atomic store, but does not guarantee the reader's load
    /// is sequenced *after* the writer's store in that order. A
    /// caller could therefore observe `bootstrapped == true` (from
    /// the prior bootstrap) while the reset had already taken the
    /// lock and flipped `done = false`, then return from
    /// `wait_for_bootstrap` without ever parking on the condvar.
    /// Acquiring the mutex first eliminates the race entirely: the
    /// reset and the wait now use the same critical section to read
    /// / write the `done` flag, and the mutex's acquire/release
    /// edges establish a clean happens-before relation. The cost is
    /// one mutex acquire per call, which is negligible —
    /// `wait_for_bootstrap` is called at most once per FFI synthesis
    /// dispatch (rare, host-driven cadence), not from any hot path.
    pub fn wait_for_bootstrap(&self) {
        let (lock, cvar) = &self.bootstrap_signal;
        let mut done = lock.lock().expect("bootstrap_signal lock");
        while !*done {
            done = cvar.wait(done).expect("bootstrap_signal wait");
        }
    }

    /// Internal helper — flip the condvar-protected `done` flag and
    /// wake every blocked [`Self::wait_for_bootstrap`] caller.
    fn notify_bootstrap_complete(&self) {
        let (lock, cvar) = &self.bootstrap_signal;
        let mut done = lock.lock().expect("bootstrap_signal lock");
        *done = true;
        cvar.notify_all();
    }

    /// Internal helper — reset the bootstrap-completion signal so a
    /// subsequent [`Self::spawn_bootstrap`] can re-probe and have its
    /// `wait_for_bootstrap` callers park correctly.
    ///
    /// Called from [`Self::spawn_bootstrap`] between joining the
    /// prior probe thread (via [`Self::shutdown`]) and spawning the
    /// new one. The invariant lives in one place so the
    /// atomic-vs-condvar ordering cannot drift: both fields are
    /// flipped under the condvar's mutex so a `wait_for_bootstrap`
    /// caller cannot interleave between them and observe a
    /// half-reset state (atomic `false` but condvar still `done`).
    ///
    /// No `notify_all` is issued — we are signalling "NOT done",
    /// not "done", and the only consumer of `done == false` is the
    /// `while !*done` loop inside [`Self::wait_for_bootstrap`] which
    /// observes the transition on its next condvar wake.
    fn reset_bootstrap_state(&self) {
        let (lock, _cvar) = &self.bootstrap_signal;
        let mut done = lock.lock().expect("bootstrap_signal lock");
        self.bootstrapped.store(false, Ordering::SeqCst);
        *done = false;
    }

    /// Borrow the underlying config.
    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    /// `true` once [`Self::bootstrap`] has run.
    pub fn is_bootstrapped(&self) -> bool {
        self.bootstrapped.load(Ordering::SeqCst)
    }

    /// `true` after [`Self::warm_up`] succeeds for at least one
    /// adapter.
    pub fn is_warmed(&self) -> bool {
        self.warmed.load(Ordering::SeqCst)
    }

    /// Send a no-op completion to the highest-priority available
    /// adapter so it pages model weights in. Returns the adapter that
    /// served the warm-up, or `None` when no adapter is available.
    ///
    /// The warm-up text comes from [`RouterConfig::warm_up_prompt`].
    pub fn warm_up(&self) -> Option<AdapterKind> {
        for (idx, adapter) in self.adapters.iter().enumerate() {
            if !adapter.is_available() {
                continue;
            }
            let res = adapter.generate("tag_importance", self.config.warm_up_prompt.as_str(), "");
            if res.is_ok() {
                self.mark_active(idx, true);
                self.warmed.store(true, Ordering::SeqCst);
                return Some(adapter.kind());
            }
        }
        None
    }

    /// Dispatch `task` to the first adapter that is available and
    /// supports it.
    pub fn dispatch(&self, task: InferenceTask, prompt: &str) -> Result<String, RouterError> {
        if !self.is_bootstrapped() {
            return Err(RouterError::NotProbed { adapter: "router" });
        }
        for (idx, adapter) in self.adapters.iter().enumerate() {
            if !adapter.is_available() || !adapter.supports(task) {
                continue;
            }
            let result = adapter.generate(task.tag(), prompt, task.grammar());
            self.mark_active(idx, result.is_ok());
            match result {
                Ok(out) => return Ok(out),
                Err(err) if err.is_fallback() => continue,
                Err(err) => return Err(err),
            }
        }
        Err(RouterError::Unavailable { task: task.tag() })
    }

    /// Walk the adapters and "unload" any whose last dispatch is
    /// older than `idle_timeout_secs`. Returns the list of adapters
    /// that were unloaded. The substrate's production runtime calls
    /// this on a wall-clock cadence.
    pub fn sweep_idle_adapters(&self) -> Vec<AdapterKind> {
        self.sweep_idle_adapters_at(Instant::now())
    }

    /// Lower-level variant that takes the wall-clock instant
    /// explicitly so unit tests can simulate elapsed time without
    /// sleeping.
    pub fn sweep_idle_adapters_at(&self, now: Instant) -> Vec<AdapterKind> {
        let mut activity = self.activity.lock().expect("activity lock");
        let timeout = Duration::from_secs(self.config.idle_timeout_secs);
        let mut unloaded = Vec::new();
        for entry in activity.iter_mut() {
            if entry.loaded && now.duration_since(entry.last_dispatch) >= timeout {
                entry.loaded = false;
                unloaded.push(entry.kind);
            }
        }
        unloaded
    }

    /// `true` iff the adapter is currently loaded into memory (i.e.
    /// has been dispatched to and not yet swept idle).
    pub fn is_adapter_loaded(&self, kind: AdapterKind) -> bool {
        let activity = self.activity.lock().expect("activity lock");
        activity
            .iter()
            .any(|entry| entry.kind == kind && entry.loaded)
    }

    /// Wire-flat view of every adapter registered with this router.
    ///
    /// Returned in priority order (index 0 = highest priority) so a
    /// host's diagnostic UI can present the same ladder the router
    /// would walk on the next [`Self::dispatch`] call. Each entry
    /// carries the adapter's stable [`AdapterKind`], its current
    /// availability (post-`probe`), its `loaded` flag (post-idle-
    /// sweep), and the list of [`InferenceTask`]s it supports.
    ///
    /// Used by the Phase 6 `ffi::health::health_check` envelope so
    /// platform hosts can render a per-adapter status panel without
    /// having to thread separate accessors for every property.
    /// The result is a snapshot — concurrent dispatches may flip
    /// `loaded` after the call returns.
    #[must_use]
    pub fn adapter_states(&self) -> Vec<AdapterState> {
        let activity = self.activity.lock().expect("activity lock");
        self.adapters
            .iter()
            .map(|adapter| {
                let kind = adapter.kind();
                let loaded = activity
                    .iter()
                    .any(|entry| entry.kind == kind && entry.loaded);
                AdapterState {
                    kind,
                    available: adapter.is_available(),
                    loaded,
                    supports: ALL_TASKS
                        .iter()
                        .copied()
                        .filter(|task| adapter.supports(*task))
                        .collect(),
                }
            })
            .collect()
    }

    fn mark_active(&self, idx: usize, loaded: bool) {
        let mut activity = self.activity.lock().expect("activity lock");
        if let Some(entry) = activity.get_mut(idx) {
            entry.last_dispatch = Instant::now();
            entry.loaded = loaded || entry.loaded;
        }
    }
}

impl Drop for InferenceRouter {
    /// Reap the background bootstrap thread, if one is still
    /// running. The substrate's normal teardown path calls
    /// [`Self::shutdown`] explicitly from `FfiRuntime::Drop`, but
    /// this `Drop` impl makes the contract robust against routers
    /// constructed in standalone contexts (tests, demos,
    /// embedders that don't go through the FFI surface): no
    /// matter how the router is dropped, the probe thread will
    /// be joined before the heap allocation goes away.
    ///
    /// Note that the bootstrap thread holds an [`Arc`] back to
    /// this router, so `Drop` will only ever fire after that
    /// `Arc` has been released — i.e. only after the probe
    /// itself has returned. The `join()` inside `shutdown()` is
    /// therefore guaranteed to complete promptly (it sees a
    /// thread that has already finished its closure), which
    /// avoids any potential for `Drop` to block on slow I/O.
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
    use crate::adapters::{FallbackAdapter, LlamaCppAdapter};
    use crate::config::DeviceTier;
    use std::sync::atomic::AtomicUsize;

    /// Recording mock adapter that captures every dispatch.
    struct MockAdapter {
        kind: AdapterKind,
        available: AtomicBool,
        supported: Vec<InferenceTask>,
        response: Mutex<Result<String, RouterError>>,
        calls: AtomicUsize,
    }

    impl MockAdapter {
        fn new(
            kind: AdapterKind,
            available: bool,
            supported: Vec<InferenceTask>,
            response: Result<String, RouterError>,
        ) -> Self {
            Self {
                kind,
                available: AtomicBool::new(available),
                supported,
                response: Mutex::new(response),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl InferenceAdapter for MockAdapter {
        fn kind(&self) -> AdapterKind {
            self.kind
        }

        fn probe(&self) -> ProbeResult {
            if self.available.load(Ordering::SeqCst) {
                ProbeResult::Available
            } else {
                ProbeResult::Unavailable
            }
        }

        fn is_available(&self) -> bool {
            self.available.load(Ordering::SeqCst)
        }

        fn supports(&self, task: InferenceTask) -> bool {
            self.supported.contains(&task)
        }

        fn generate(
            &self,
            _task_tag: &str,
            _prompt: &str,
            _grammar: &str,
        ) -> Result<String, RouterError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.response
                .lock()
                .expect("response")
                .clone()
                .map_err(|e| match e {
                    RouterError::Unavailable { task } => RouterError::Unavailable { task },
                    RouterError::TierTooLow { tier, task } => {
                        RouterError::TierTooLow { tier, task }
                    }
                    RouterError::InferenceFailure(msg) => RouterError::InferenceFailure(msg),
                    RouterError::NotProbed { adapter } => RouterError::NotProbed { adapter },
                })
        }
    }

    fn router_with(adapters: Vec<Box<dyn InferenceAdapter>>) -> InferenceRouter {
        let config = RouterConfig::default().with_device_tier(DeviceTier::High);
        InferenceRouter::new(config, adapters)
    }

    #[test]
    fn adapter_states_lists_every_adapter_in_priority_order() {
        let mlx = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![
                InferenceTask::TagImportance,
                InferenceTask::SynthSummary,
                InferenceTask::SynthConcept,
            ],
            Ok("ok".into()),
        );
        let fallback = MockAdapter::new(
            AdapterKind::Fallback,
            true,
            vec![
                InferenceTask::TagImportance,
                InferenceTask::ExtractEntities,
                InferenceTask::PromoteObservation,
            ],
            Ok("ok".into()),
        );
        let router = router_with(vec![Box::new(mlx), Box::new(fallback)]);

        let states = router.adapter_states();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].kind, AdapterKind::Mlx);
        assert_eq!(states[1].kind, AdapterKind::Fallback);
        assert!(states[0].available);
        assert!(states[1].available);
        assert!(!states[0].loaded);
        assert!(!states[1].loaded);
        assert_eq!(states[0].supports.len(), 3);
        assert_eq!(states[1].supports.len(), 3);
        assert!(states[0].supports.contains(&InferenceTask::SynthSummary));
        assert!(states[1]
            .supports
            .contains(&InferenceTask::PromoteObservation));
    }

    #[test]
    fn adapter_states_reflects_unavailability() {
        let offline = MockAdapter::new(
            AdapterKind::LlamaCpp,
            false,
            vec![InferenceTask::SynthSummary],
            Err(RouterError::Unavailable { task: "x" }),
        );
        let router = router_with(vec![Box::new(offline)]);
        let states = router.adapter_states();
        assert_eq!(states.len(), 1);
        assert!(!states[0].available);
        assert!(!states[0].loaded);
        // Even an unavailable adapter must report what it would
        // support if it WERE available — the field describes the
        // adapter's contract, not its current state.
        assert_eq!(states[0].supports, vec![InferenceTask::SynthSummary]);
    }

    #[test]
    fn dispatch_before_bootstrap_errors() {
        let router = router_with(vec![Box::new(FallbackAdapter::new())]);
        let err = router
            .dispatch(InferenceTask::TagImportance, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::NotProbed { .. }));
    }

    #[test]
    fn priority_routes_to_first_available_adapter_supporting_task() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Ok("primary-response".into()),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary-response".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "x")
            .expect("dispatch ok");
        assert_eq!(out, "primary-response");
    }

    #[test]
    fn router_falls_through_when_primary_unsupported() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Ok("never".into()),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::SynthSummary],
            Ok("synth-response".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::SynthSummary, "x")
            .expect("dispatch ok");
        assert_eq!(out, "synth-response");
    }

    #[test]
    fn router_falls_through_when_primary_unavailable() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            false,
            vec![InferenceTask::TagImportance],
            Ok("never".into()),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary-response".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "x")
            .expect("dispatch ok");
        assert_eq!(out, "secondary-response");
    }

    #[test]
    fn router_falls_through_on_unavailable_error() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Err(RouterError::Unavailable {
                task: "tag_importance",
            }),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "x")
            .expect("dispatch ok");
        assert_eq!(out, "secondary");
    }

    #[test]
    fn router_does_not_fall_through_on_inference_failure() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Err(RouterError::InferenceFailure("boom".into())),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let err = router
            .dispatch(InferenceTask::TagImportance, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::InferenceFailure(_)));
    }

    #[test]
    fn router_emits_unavailable_when_no_adapter_serves_task() {
        let only = MockAdapter::new(
            AdapterKind::Fallback,
            true,
            vec![InferenceTask::TagImportance],
            Ok("never".into()),
        );
        let router = router_with(vec![Box::new(only)]);
        router.bootstrap();
        let err = router
            .dispatch(InferenceTask::SynthSummary, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    #[test]
    fn warm_up_marks_router_warmed_and_loads_adapter() {
        let adapter = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("warmup".into()),
        );
        let router = router_with(vec![Box::new(adapter)]);
        router.bootstrap();
        let kind = router.warm_up().expect("warm-up ok");
        assert_eq!(kind, AdapterKind::LlamaCpp);
        assert!(router.is_warmed());
        assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));
    }

    #[test]
    fn idle_sweep_unloads_adapter_after_timeout() {
        let cfg = RouterConfig::default()
            .with_device_tier(DeviceTier::High)
            .with_idle_timeout(60);
        let adapter = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("ok".into()),
        );
        let router = InferenceRouter::new(cfg, vec![Box::new(adapter)]);
        router.bootstrap();
        router.warm_up();
        assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));
        // Simulate 120s passing.
        let later = Instant::now() + Duration::from_secs(120);
        let unloaded = router.sweep_idle_adapters_at(later);
        assert!(unloaded.contains(&AdapterKind::LlamaCpp));
        assert!(!router.is_adapter_loaded(AdapterKind::LlamaCpp));
    }

    #[test]
    fn integration_mlx_llama_fallback_priority() {
        // Boot a router with the production three-adapter ladder
        // using mocks for MLX/LlamaCpp and the real FallbackAdapter.
        // MLX disabled (off Apple Silicon), llama.cpp reachable.
        use crate::adapters::llama_cpp::MockLlamaServerClient;
        use crate::adapters::mlx::MlxAdapter;
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
        let llama = LlamaCppAdapter::new(
            cfg.clone(),
            Box::new(MockLlamaServerClient::ok(
                r#"{"class":"useful","confidence":0.4}"#,
            )),
        );
        let fallback = FallbackAdapter::new();
        let router = InferenceRouter::new(
            cfg,
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        router.bootstrap();
        // TagImportance — llama.cpp should serve it.
        let out = router
            .dispatch(InferenceTask::TagImportance, "msg")
            .unwrap();
        assert!(out.contains("useful"));
        // SynthSummary — only llama.cpp supports synthesis (high tier);
        // returns the mock response.
        let out = router
            .dispatch(InferenceTask::SynthSummary, "session")
            .unwrap();
        assert!(out.contains("useful"));
    }

    #[test]
    fn integration_falls_through_to_fallback_when_llama_unreachable() {
        use crate::adapters::llama_cpp::MockLlamaServerClient;
        use crate::adapters::mlx::MlxAdapter;
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
        let llama =
            LlamaCppAdapter::new(cfg.clone(), Box::new(MockLlamaServerClient::unreachable()));
        let fallback = FallbackAdapter::new();
        let router = InferenceRouter::new(
            cfg,
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        router.bootstrap();
        // The fallback adapter scores against a real lexicon — feed
        // it a prompt body containing a "useful" lexicon term so the
        // classifier picks that class deterministically.
        let prompt = "stuff\n\nMessage:\nplease investigate the question";
        let out = router
            .dispatch(InferenceTask::TagImportance, prompt)
            .unwrap();
        assert!(out.contains("\"class\":\"useful\""), "got {out}");
        // Synthesis routes only to llama.cpp/MLX; with both
        // unavailable the router emits Unavailable.
        let err = router
            .dispatch(InferenceTask::SynthSummary, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    /// `spawn_bootstrap` must run probing on a background thread and
    /// satisfy the same post-condition as the synchronous
    /// `bootstrap`: `is_bootstrapped` flips to true and waiters get
    /// released. Pins the FFI `open_store` contract.
    #[test]
    fn spawn_bootstrap_runs_probing_on_background_thread() {
        let router = Arc::new(router_with(vec![Box::new(FallbackAdapter::new())]));
        assert!(!router.is_bootstrapped());
        Arc::clone(&router).spawn_bootstrap();
        // Block on the same primitive the FFI surface uses.
        router.wait_for_bootstrap();
        assert!(router.is_bootstrapped());
        // Dispatch must succeed after the background probe completes
        // (FallbackAdapter supports TagImportance and is always
        // available once probed).
        router
            .dispatch(InferenceTask::TagImportance, "Message:\nhi")
            .expect("dispatch after spawn_bootstrap should succeed");
    }

    /// `wait_for_bootstrap` blocks until probing completes, then
    /// returns immediately on subsequent calls. Stale callers that
    /// race the spawn-bootstrap path get a synchronisation point.
    #[test]
    fn wait_for_bootstrap_blocks_until_probe_completes() {
        use std::sync::Barrier;
        use std::thread;

        let router = Arc::new(router_with(vec![Box::new(FallbackAdapter::new())]));
        // Two waiters race the bootstrap thread; both must observe
        // bootstrap == complete after their `wait` returns.
        let barrier = Arc::new(Barrier::new(3));
        let r1 = Arc::clone(&router);
        let b1 = Arc::clone(&barrier);
        let t1 = thread::spawn(move || {
            b1.wait();
            r1.wait_for_bootstrap();
            assert!(r1.is_bootstrapped());
        });
        let r2 = Arc::clone(&router);
        let b2 = Arc::clone(&barrier);
        let t2 = thread::spawn(move || {
            b2.wait();
            r2.wait_for_bootstrap();
            assert!(r2.is_bootstrapped());
        });
        barrier.wait();
        Arc::clone(&router).spawn_bootstrap();
        t1.join().unwrap();
        t2.join().unwrap();
        // A subsequent wait is a no-op (no condvar wait).
        router.wait_for_bootstrap();
    }

    /// Adapter whose `probe()` panics — used to exercise the
    /// panic-safety drop-guard inside [`InferenceRouter::spawn_bootstrap`].
    struct PanicProbeAdapter;
    impl InferenceAdapter for PanicProbeAdapter {
        fn kind(&self) -> AdapterKind {
            AdapterKind::LlamaCpp
        }
        fn probe(&self) -> ProbeResult {
            panic!("PanicProbeAdapter::probe is intentionally panicking");
        }
        fn is_available(&self) -> bool {
            false
        }
        fn supports(&self, _: InferenceTask) -> bool {
            false
        }
        fn generate(&self, _: &str, _: &str, _: &str) -> Result<String, RouterError> {
            Err(RouterError::Unavailable {
                task: "synth_summary",
            })
        }
    }

    /// A panic on any adapter's `probe()` must NOT permanently wedge
    /// `wait_for_bootstrap` callers. The detached background thread's
    /// `NotifyOnDrop` guard fires on unwind, flipping the condvar's
    /// `done` flag so blocked waiters are released. `is_bootstrapped`
    /// stays `false` (the atomic is only flipped on the happy path),
    /// so subsequent `dispatch` calls correctly route to their
    /// `NotProbed` / `Unavailable` paths rather than racing partial
    /// state — but no thread hangs.
    #[test]
    fn spawn_bootstrap_panic_does_not_wedge_waiters() {
        let router = Arc::new(router_with(vec![
            Box::new(PanicProbeAdapter),
            Box::new(FallbackAdapter::new()),
        ]));
        assert!(!router.is_bootstrapped());
        Arc::clone(&router).spawn_bootstrap();
        // The wait must return — the drop-guard fires on panic so the
        // condvar is signalled even though `bootstrap()` unwound.
        router.wait_for_bootstrap();
        // The atomic stays false because the panic prevented the
        // happy-path `bootstrapped.store(true)`. Dispatch routes to
        // `NotProbed` rather than hanging.
        assert!(
            !router.is_bootstrapped(),
            "panicking probe must leave bootstrapped == false",
        );
    }

    /// Synchronous `bootstrap` must also notify the condvar so a
    /// caller that mixes the two entry points cannot deadlock.
    #[test]
    fn synchronous_bootstrap_releases_condvar_waiters() {
        use std::thread;

        let router = Arc::new(router_with(vec![Box::new(FallbackAdapter::new())]));
        let r = Arc::clone(&router);
        let t = thread::spawn(move || {
            // Park on the condvar — the synchronous bootstrap below
            // must still wake us.
            r.wait_for_bootstrap();
            assert!(r.is_bootstrapped());
        });
        // Give the waiter a moment to park on the condvar before we
        // notify; the test still passes if `bootstrap` runs first
        // (the wait short-circuits via `is_bootstrapped`), so the
        // sleep just exercises the notify path more often.
        std::thread::sleep(Duration::from_millis(5));
        router.bootstrap();
        t.join().unwrap();
    }

    /// Adapter whose `probe()` parks on a barrier until the test
    /// releases it. Used to deterministically reproduce the
    /// "second `spawn_bootstrap` re-uses stale state" bug — with the
    /// adapter blocked inside `probe()`, we get a clean window
    /// between `spawn_bootstrap` returning and the new probe
    /// completing in which to observe `is_bootstrapped()` /
    /// `wait_for_bootstrap()` behaviour.
    struct GatedProbeAdapter {
        gate: Arc<std::sync::Barrier>,
    }
    impl InferenceAdapter for GatedProbeAdapter {
        fn kind(&self) -> AdapterKind {
            AdapterKind::LlamaCpp
        }
        fn probe(&self) -> ProbeResult {
            self.gate.wait();
            ProbeResult::Available
        }
        fn is_available(&self) -> bool {
            true
        }
        fn supports(&self, _: InferenceTask) -> bool {
            false
        }
        fn generate(&self, _: &str, _: &str, _: &str) -> Result<String, RouterError> {
            Err(RouterError::Unavailable {
                task: "synth_summary",
            })
        }
    }

    /// Regression for the bug Devin Review flagged on commit d5b0a61:
    /// `spawn_bootstrap` (second call) must reset both the
    /// `bootstrapped` atomic *and* the condvar `done` flag, otherwise
    /// `wait_for_bootstrap` short-circuits on the prior bootstrap's
    /// stale state and `dispatch` callers race ahead with outdated
    /// adapter availability.
    ///
    /// Strategy: first bootstrap to set `bootstrapped = true`. Then
    /// re-spawn with a `GatedProbeAdapter` whose `probe()` is blocked
    /// at a barrier. While the new probe is in flight, both
    /// `is_bootstrapped()` and the condvar's `done` flag MUST be
    /// `false` — if either remained `true`, callers would race past
    /// the in-flight probe. Releasing the barrier then completes the
    /// probe; the post-conditions return to `true`.
    #[test]
    fn re_spawn_bootstrap_resets_bootstrapped_and_condvar_done_flags() {
        use std::sync::Barrier;
        use std::thread;
        // Use a barrier with 2 participants: the test thread + the
        // adapter's `probe()`. The test calls `gate.wait()` from the
        // test thread to unblock the probe.
        let gate = Arc::new(Barrier::new(2));
        let router = Arc::new(router_with(vec![
            Box::new(GatedProbeAdapter {
                gate: Arc::clone(&gate),
            }),
            Box::new(FallbackAdapter::new()),
        ]));

        // First, drive a complete bootstrap on a background thread so
        // the test thread can release the gate from outside the
        // probe. After this finishes, `bootstrapped == true` and the
        // condvar's `done == true` — i.e. the exact stale state the
        // bug relies on.
        let r1 = Arc::clone(&router);
        let first = thread::spawn(move || r1.bootstrap());
        gate.wait(); // release the first probe
        first.join().unwrap();
        assert!(router.is_bootstrapped(), "first bootstrap must complete");

        // Now re-spawn with the gate still closed. `spawn_bootstrap`
        // joins the prior (already-finished) thread, resets state,
        // and spawns a new thread which immediately parks inside
        // `probe()` waiting for the gate.
        Arc::clone(&router).spawn_bootstrap();

        // The critical assertion: while the new probe is in flight,
        // the atomic MUST be false. Before the fix, the atomic stayed
        // `true` from the prior bootstrap and a `dispatch` here would
        // race past the in-flight probe.
        assert!(
            !router.is_bootstrapped(),
            "second spawn_bootstrap MUST reset bootstrapped to false until the new probe completes",
        );

        // The condvar's `done` flag must also be reset — verify by
        // spinning up a waiter and confirming it blocks on the
        // condvar (i.e. it does not return until we release the
        // gate). Use a completion-flag pattern to avoid timing-
        // fragile sleeps in the assertion path.
        let r3 = Arc::clone(&router);
        let waiter_done = Arc::new(AtomicBool::new(false));
        let waiter_done_clone = Arc::clone(&waiter_done);
        let waiter = thread::spawn(move || {
            r3.wait_for_bootstrap();
            waiter_done_clone.store(true, Ordering::SeqCst);
        });

        // Give the waiter a chance to park on the condvar. Even with
        // pathological scheduling, a 50ms window is enough to detect
        // the bug — if `done` were still `true` from the prior
        // bootstrap, `wait_for_bootstrap` would short-circuit
        // immediately and the flag would already be set.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !waiter_done.load(Ordering::SeqCst),
            "wait_for_bootstrap MUST block until the new probe completes \
             — it observed stale done==true from the prior bootstrap",
        );

        // Release the second probe. The waiter should now unblock
        // and `is_bootstrapped()` should return to true.
        gate.wait();
        waiter.join().unwrap();
        assert!(waiter_done.load(Ordering::SeqCst));
        assert!(router.is_bootstrapped());
    }

    /// Regression for the latent self-join footgun Devin Review
    /// flagged on commit 8c8ed4f: a standalone embedder that
    /// constructs `Arc<InferenceRouter>`, calls `spawn_bootstrap`,
    /// then drops its only `Arc` without joining or waiting MUST
    /// not invoke `pthread_join` on the bootstrap thread itself.
    ///
    /// Scenario: after the outer `Arc` is released, the only
    /// remaining strong reference is the one captured inside the
    /// bootstrap thread's closure. When that thread finishes its
    /// probe loop, its `me: Arc<InferenceRouter>` is the last
    /// strong reference; dropping it fires
    /// [`InferenceRouter::drop`] on the bootstrap thread, which
    /// in turn calls [`InferenceRouter::shutdown`] — and naively
    /// joining the handle stored in `bootstrap_handle` would be a
    /// self-join (`pthread_join(pthread_self())`), POSIX-undefined.
    /// On glibc this surfaces as `EDEADLK` rather than a hard
    /// deadlock, but on other libcs the behaviour is unspecified.
    ///
    /// Verification: the test downgrades to a [`Weak`] before
    /// dropping the only strong reference, then polls until the
    /// allocation is freed. If the self-join branch in `shutdown`
    /// is not taken, the bootstrap thread either (a) deadlocks
    /// forever (libc-dependent), or (b) logs a `WARN` and proceeds
    /// — but either way the strong count drops to zero and the
    /// `Weak::upgrade` returns `None`. The test thus pins the
    /// substrate-level guarantee: standalone-embedder teardown
    /// completes without leaking the router allocation.
    #[test]
    fn shutdown_skips_self_join_when_dropped_from_bootstrap_thread() {
        use std::time::Instant;

        let router = Arc::new(router_with(vec![Box::new(FallbackAdapter::new())]));
        // Spawn the bootstrap thread — the closure captures a clone
        // of the Arc internally via `Arc::clone(&self)`.
        Arc::clone(&router).spawn_bootstrap();
        // Downgrade to a `Weak` so the test can observe the allocation
        // going away without keeping it alive.
        let weak = Arc::downgrade(&router);
        // Release the outer strong reference. After this, the only
        // remaining strong reference is the closure-captured `me`
        // inside the bootstrap thread.
        drop(router);
        // The bootstrap thread should:
        //   1. Run the probe (FallbackAdapter — instantaneous).
        //   2. Notify the condvar via `NotifyOnDrop`.
        //   3. Drop `me`, taking the strong count from 1 → 0.
        //   4. `Drop for InferenceRouter` fires on the bootstrap
        //      thread → `shutdown` detects self-thread → detaches
        //      the join handle → returns.
        // Once that sequence completes, `Weak::upgrade()` returns
        // `None`. A 5-second deadline is generous — the entire
        // sequence takes microseconds on any reasonable host.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if weak.upgrade().is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("InferenceRouter allocation was not freed within 5s — possible self-join hang");
    }
}
