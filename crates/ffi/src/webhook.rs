//! Webhook-receiver FFI surface.
//!
//! Per `docs/technical/architecture.md` §4.3 and the `connector_framework::webhook_server`
//! module docs, the substrate ships its own in-process HTTP receiver
//! so providers (Slack `events_api`, Notion webhooks, Atlassian
//! Connect, …) can POST directly to the substrate without an
//! intermediary edge service. The framework crate ships the axum 0.8
//! router, the [`WebhookDispatcher`] trait, and the graceful-shutdown
//! plumbing; this module is the **FFI wiring** that:
//!
//! 1. Owns one [`tokio::runtime::Runtime`] per running server and
//!    drives it on a dedicated OS thread so the substrate's
//!    synchronous FFI mutex never deadlocks against an in-flight axum
//!    `await`.
//! 2. Builds an [`FfiWebhookRouter`] (the single
//!    [`WebhookDispatcher`] the framework's static-table router
//!    forwards every `POST /webhooks/{provider_id}` to) that resolves
//!    `provider_id` → [`ConnectorInstanceId`] through an interior
//!    [`RwLock`]-protected map, then fans the payload to the
//!    matching connector's `handle_webhook_event` impl under the
//!    three-phase FFI locking discipline.
//! 3. Persists every successfully-emitted [`ConnectorEvent`] back
//!    into the encrypted evidence store under the same source-tag
//!    contract that [`super::sync_connector`] uses — webhook-driven
//!    evidence is indistinguishable from polled evidence once it
//!    lands.
//! 4. Exposes five FFI entry points:
//!
//!    * [`start_webhook_server`] — bind an axum listener, return an
//!      opaque [`WebhookServerHandle`].
//!    * [`stop_webhook_server`] — drive a graceful shutdown
//!      (in-flight requests drain) and synchronously join the
//!      runtime thread.
//!    * [`register_webhook_dispatch`] — bind a `provider_id` to a
//!      live `ConnectorInstanceId` on a specific server.
//!    * [`unregister_webhook_dispatch`] — drop a `(server, provider_id)`
//!      binding without stopping the server.
//!    * [`list_webhook_servers`] — diagnostic enumeration of the
//!      runtime's live servers + per-server counters.
//!
//! # Why a per-server tokio runtime?
//!
//! The rest of the substrate is synchronous. Mixing async into the
//! existing handle-mutex discipline would force every FFI call to
//! run inside a tokio context, which (a) breaks the `napi-rs` worker
//! thread contract (workers are sync), (b) inflates the FFI surface
//! for hosts that never touch webhooks (Electron status panels,
//! offline CLI batch tools), and (c) couples the substrate's
//! lifecycle to tokio version churn. The framework's
//! `WebhookServer::serve_on` is async, so we spin up exactly the
//! minimum-required async island: one OS thread per server, running
//! a `current_thread` tokio runtime that drives the axum server until
//! the shutdown channel fires. The runtime is dropped on the same
//! thread before the join completes, so SQLCipher and the master key
//! teardown that follow in [`crate::close_store`] cannot race against
//! still-live tokio tasks.
//!
//! # Three-phase locking in the dispatch closure
//!
//! [`FfiWebhookRouter::dispatch`] is the load-bearing piece. Every
//! webhook lands here, and the substrate's FFI handle mutex CANNOT
//! be held across the `connector.handle_webhook_event(...)` HTTP
//! processing call — that call can re-enter the runtime if a future
//! connector implements webhook → on-disk-fetch (e.g. Notion's
//! resolved-page fetch), and even when it doesn't, holding the mutex
//! across an unbounded user-supplied callback would deadlock every
//! other FFI call on the same handle. So the dispatcher is split
//! into three phases:
//!
//! 1. **Snapshot phase** (locked): looks up the
//!    `(Arc<dyn Connector>, ScopeId, ConnectorKind)` triple from the
//!    runtime's connector registry. Releases the mutex before
//!    returning.
//! 2. **Dispatch phase** (unlocked): calls
//!    `connector.handle_webhook_event(&body)` on a
//!    `spawn_blocking` worker so the synchronous Connector trait
//!    method does not block the tokio runtime.
//! 3. **Persist phase** (locked): re-acquires the mutex,
//!    re-validates that the scope is still alive (the host may have
//!    called `forget_scope` between phases), and ingests each
//!    [`ConnectorEvent`] through [`evidence_store::EvidenceStore::ingest`].
//!
//! # Lifecycle ordering at `close_store`
//!
//! [`crate::close_store`] explicitly drains the per-runtime webhook
//! server map BEFORE its `Arc::try_unwrap` spin loop runs. This is
//! load-bearing: the dispatcher closures running on the tokio runtime
//! thread call back into [`crate::runtime::with_runtime`], which
//! briefly clones the runtime's `Arc<Mutex<FfiRuntime>>` to access
//! `connector_instances`. Without an explicit pre-drain step the
//! spin loop would race the in-flight tokio task — a worst-case
//! pathology where webhooks keep arriving during a shutdown would
//! turn `close_store` into an unbounded busy-wait. Draining
//! synchronously (sending the shutdown oneshot, joining the OS
//! thread) before the drain loop guarantees no future dispatch
//! closure ever clones the runtime `Arc`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;

use async_trait::async_trait;
use axum::body::Bytes;
use chrono::Utc;
use connector_framework::{
    ConnectorError, ConnectorInstanceId, ConnectorKind, WebhookDispatch, WebhookDispatcher,
    WebhookServer, WebhookServerConfig,
};
use tokio::sync::oneshot;

use crate::connector::{connector_source_tag, event_to_evidence_body, parse_instance_id};
use crate::error::{FfiError, FfiResult};
use crate::metrics;
use crate::runtime::{with_runtime, RuntimeHandle};
use crate::types::{WebhookServerHandle, WebhookServerSummary};

// ───────────────────────── Handle allocation ──────────────────────

/// Monotone allocator for [`WebhookServerHandle`] values.
///
/// Identical pattern to [`crate::runtime::next_handle`] — every
/// successful [`start_webhook_server`] call consumes one value, and
/// after exactly `u64::MAX` allocations the counter wraps to the
/// reserved [`WebhookServerHandle::NONE`] sentinel. The wrap-check
/// in [`start_webhook_server`] rejects that case so the sentinel
/// remains forbidden in the running-server map.
///
/// Process-global (not per-runtime) so cross-runtime handle aliasing
/// is impossible — a host that opens two SQLCipher stores in the
/// same process can hand a [`WebhookServerHandle`] from one runtime
/// to a webhook fn on the other and the latter's `webhook_servers`
/// map lookup will fail-closed with [`FfiError::NotFound`].
fn next_server_handle() -> u64 {
    // Matches the bare-static convention used by `runtime::next_handle`
    // for [`RuntimeHandle`] allocation — no lazy init required because
    // `AtomicU64::new` is `const`. `Relaxed` suffices for the same
    // reason as there: `fetch_add` is an atomic RMW so each caller
    // receives a distinct value, and the runtime mutex taken in
    // `start_webhook_server` immediately after carries the actual
    // happens-before edge for inserting the new entry into
    // `webhook_servers`.
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ───────────────────────── Router (the WebhookDispatcher) ──────────

/// The single [`WebhookDispatcher`] every framework-side route entry
/// points at. Holds the dynamic `provider_id → instance_id` routing
/// table plus the per-server outcome counters that
/// [`list_webhook_servers`] exposes.
///
/// One [`FfiWebhookRouter`] exists per running server. Cloned
/// `Arc` references live in two places:
///
/// * Inside the framework's [`WebhookServer`] state (the axum
///   `with_state` mount that powers `POST /webhooks/{provider_id}`).
/// * Inside the [`RunningWebhookServer`] slot on
///   [`crate::runtime::FfiRuntime::webhook_servers`] so
///   [`register_webhook_dispatch`] / [`unregister_webhook_dispatch`]
///   / [`list_webhook_servers`] can find it.
///
/// Routes are stored behind an [`RwLock`] so FFI mutations
/// (register / unregister, taking the write lock for ~1µs) coexist
/// with concurrent dispatcher reads (taking the read lock for as
/// long as the [`FfiWebhookRouter::dispatch`] call holds it — the
/// read guard is dropped BEFORE the snapshot phase calls
/// [`with_runtime`] so the lock is never held across an unbounded
/// substrate-side call).
pub(crate) struct FfiWebhookRouter {
    /// Runtime this router belongs to. Stored as a [`RuntimeHandle`]
    /// (not an `Arc<Mutex<FfiRuntime>>`) so the dispatcher never
    /// pins the runtime past [`crate::close_store`]'s drain loop —
    /// the dispatcher re-acquires the runtime via [`with_runtime`]
    /// per call, and once the close-loop removes the entry the
    /// dispatcher's lookups fail with
    /// [`FfiError::Unavailable { subsystem: "evidence_store" }`]
    /// which maps to a `502 Bad Gateway` (the framework's "any other
    /// `ConnectorError`" branch).
    handle: RuntimeHandle,
    /// `provider_id → instance_id` routing table. The set of valid
    /// `provider_id`s comes from the static `dispatchers` HashMap
    /// the framework holds (one entry per [`ConnectorKind`] slug,
    /// all pointing at this same router), and the value is set /
    /// cleared by [`register_webhook_dispatch`] /
    /// [`unregister_webhook_dispatch`] at runtime.
    routes: RwLock<HashMap<String, ConnectorInstanceId>>,
    /// Total dispatches that returned `Ok(())` from
    /// `connector.handle_webhook_event(...)` — the framework maps
    /// this to an HTTP `200 OK`. Monotone for the lifetime of the
    /// server.
    dispatch_ok_total: AtomicU64,
    /// Total dispatches that returned
    /// [`ConnectorError::Webhook(_)`] — the framework maps this to
    /// `400 Bad Request` (so upstream stops re-delivering a
    /// malformed payload).
    dispatch_bad_request_total: AtomicU64,
    /// Total dispatches that returned any other [`ConnectorError`]
    /// — the framework maps this to `502 Bad Gateway` (so upstream
    /// retries with backoff). Includes
    /// [`FfiError::Unavailable`] / closed-store / unknown-instance
    /// failures translated into a synthetic
    /// [`ConnectorError::Transport`] on the substrate side.
    dispatch_bad_gateway_total: AtomicU64,
}

impl FfiWebhookRouter {
    fn new(handle: RuntimeHandle) -> Self {
        Self {
            handle,
            routes: RwLock::new(HashMap::new()),
            dispatch_ok_total: AtomicU64::new(0),
            dispatch_bad_request_total: AtomicU64::new(0),
            dispatch_bad_gateway_total: AtomicU64::new(0),
        }
    }

    /// Bind `provider_id` to `instance_id`. Replaces any previous
    /// binding for the same `provider_id` (idempotent re-registration
    /// is the expected path — a host that re-runs its boot wiring
    /// without an explicit unregister should not see a "duplicate"
    /// error).
    fn register(&self, provider_id: String, instance_id: ConnectorInstanceId) {
        let mut routes = self
            .routes
            .write()
            .expect("FfiWebhookRouter routes RwLock poisoned");
        routes.insert(provider_id, instance_id);
    }

    /// Drop the binding for `provider_id`. Returns `true` if a
    /// binding existed, `false` otherwise — the FFI surface treats
    /// "unregistering an unknown id" as a no-op success because the
    /// post-condition (no binding) is satisfied either way.
    fn unregister(&self, provider_id: &str) -> bool {
        let mut routes = self
            .routes
            .write()
            .expect("FfiWebhookRouter routes RwLock poisoned");
        routes.remove(provider_id).is_some()
    }

    /// Number of currently-registered `(provider_id, instance_id)`
    /// rows.
    fn registration_count(&self) -> usize {
        self.routes
            .read()
            .expect("FfiWebhookRouter routes RwLock poisoned")
            .len()
    }
}

#[async_trait]
impl WebhookDispatcher for FfiWebhookRouter {
    async fn dispatch(&self, provider_id: &str, body: Bytes) -> Result<(), ConnectorError> {
        // Snapshot the routing lookup outside any tokio task so the
        // read lock is dropped before we cross the spawn_blocking
        // boundary. The lookup result (an `Option<ConnectorInstanceId>`)
        // is `Copy` so the snapshot is a single u128 + 1 bit.
        let instance_id = {
            let routes = self
                .routes
                .read()
                .expect("FfiWebhookRouter routes RwLock poisoned");
            let Some(id) = routes.get(provider_id).copied() else {
                // 400: surface as a `Webhook` error so the framework
                // returns 400 Bad Request. We deliberately do NOT
                // return a 404 — the framework's static-table router
                // returns 404 only for paths the table doesn't pre-
                // register, and "registered path, no dynamic
                // binding" is a semantic mismatch (the substrate IS
                // configured for this provider, it just has no
                // instance bound). 400 also signals "stop re-
                // delivering" to providers that follow the standard
                // re-delivery contract.
                self.dispatch_bad_request_total
                    .fetch_add(1, Ordering::Relaxed);
                metrics::inc_webhook_dispatch_bad_request();
                return Err(ConnectorError::Webhook(format!(
                    "no instance registered for provider_id={provider_id}; \
                     call register_webhook_dispatch first",
                )));
            };
            id
        };

        let handle = self.handle;
        let provider_id_owned = provider_id.to_string();

        // Move the synchronous three-phase work onto a
        // spawn_blocking worker so the tokio runtime's executor
        // threads (which the framework's axum server is driving) are
        // not blocked by the `with_runtime` mutex acquisition / the
        // synchronous `handle_webhook_event` call / the synchronous
        // SQLCipher ingest. Without this, a slow `handle_webhook_event`
        // impl would head-of-line block every other in-flight
        // dispatch on the same server.
        let join = tokio::task::spawn_blocking(move || -> Result<usize, ConnectorError> {
            dispatch_blocking(handle, instance_id, &provider_id_owned, body.as_ref())
        })
        .await;

        let outcome: DispatchOutcome = match join {
            Ok(result) => result,
            Err(join_err) => {
                // Tokio JoinError — either the worker panicked or the
                // runtime is shutting down. Both map to 502 Bad
                // Gateway: substrate-side failure, upstream should
                // retry. Surface the framework error string redacted
                // so we don't leak internal pool state to the
                // provider — the framework's 502 response uses a
                // fixed "internal error" body anyway, so the
                // message we put here only shows up in tracing.
                self.dispatch_bad_gateway_total
                    .fetch_add(1, Ordering::Relaxed);
                metrics::inc_webhook_dispatch_bad_gateway();
                return Err(ConnectorError::Transport(format!(
                    "webhook dispatch worker join failed: {join_err}",
                )));
            }
        };

        match outcome {
            Ok(_) => {
                self.dispatch_ok_total.fetch_add(1, Ordering::Relaxed);
                metrics::inc_webhook_dispatch_ok();
                Ok(())
            }
            Err(e) => {
                if matches!(e, ConnectorError::Webhook(_)) {
                    self.dispatch_bad_request_total
                        .fetch_add(1, Ordering::Relaxed);
                    metrics::inc_webhook_dispatch_bad_request();
                } else {
                    self.dispatch_bad_gateway_total
                        .fetch_add(1, Ordering::Relaxed);
                    metrics::inc_webhook_dispatch_bad_gateway();
                }
                Err(e)
            }
        }
    }
}

/// Successful-path outcome carried out of [`dispatch_blocking`]. The
/// `usize` records how many events were ingested into the evidence
/// store — currently observable only via metrics + tracing, but kept
/// in the return shape so a future host can surface
/// "webhook → events" provenance through an additional FFI fn
/// without changing the dispatch path's signature.
type DispatchOutcome = Result<usize, ConnectorError>;

/// Run the synchronous three-phase dispatch on a [`spawn_blocking`]
/// worker. Split out as a free function so unit tests can exercise
/// the phase ordering without standing up a real axum server.
fn dispatch_blocking(
    handle: RuntimeHandle,
    instance_id: ConnectorInstanceId,
    provider_id: &str,
    body: &[u8],
) -> Result<usize, ConnectorError> {
    // ── Step 1: snapshot — locked ────────────────────────────────
    //
    // Pull the connector, scope, and kind out of the runtime map and
    // drop the mutex before crossing the unbounded-cost
    // `handle_webhook_event` call. The lookup is `O(1)` (HashMap on
    // the instance UUID) so the mutex is held for microseconds.
    let snapshot = with_runtime(handle, |rt| {
        // The host could have removed the connector instance between
        // the route registration and this dispatch — surface that as
        // a `Webhook` 400 so the upstream stops re-delivering rather
        // than as a generic `Transport` 502 (the provider can't fix
        // its own retry behaviour without knowing the substrate
        // de-provisioned the binding).
        let Some(inst) = rt.connector_instances.get(&instance_id) else {
            return Err(FfiError::NotFound {
                kind: "connector_instance".into(),
                id: instance_id.to_string(),
            });
        };
        // Refuse to dispatch for a scope the host already forgot.
        // Same defense-in-depth pattern as
        // `crate::sync_connector` Step 1.
        if rt.is_scope_forgotten(inst.config.scope_id) {
            return Err(FfiError::NotFound {
                kind: "scope".into(),
                id: inst.config.scope_id.to_string(),
            });
        }
        let Some(connector_arc) = rt.connectors.get(&instance_id) else {
            // `connector_instances` and `connectors` should be in
            // lock-step (every `create_connector` inserts into
            // both, every `remove_connector` deletes from both).
            // If they ever drift the dispatcher would be pointing
            // at a half-removed instance — surface as a `502`-mapped
            // `Transport` error so the upstream retries, and emit
            // a tracing warn so the drift is observable.
            tracing::warn!(instance = %instance_id,
                "webhook dispatch found connector_instance without matching connector Arc"
            );
            return Err(FfiError::Connector {
                message: format!(
                    "internal: connector instance {instance_id} \
                     missing from connectors map"
                ),
            });
        };
        let connector = Arc::clone(connector_arc);
        Ok((connector, inst.config.scope_id, inst.config.kind))
    });

    let (connector, scope, kind) = match snapshot {
        Ok(triple) => triple,
        Err(FfiError::NotFound { kind: nf_kind, id }) => {
            return Err(ConnectorError::Webhook(format!(
                "no live binding for provider_id={provider_id}: {nf_kind}={id} not found"
            )));
        }
        Err(other) => {
            // Unavailable (closed store) / Connector / Evidence / …
            // → 502 Bad Gateway. The framework wraps the
            // `ConnectorError` discriminant; we use `Transport`
            // because the "substrate-side fault" semantics match.
            return Err(ConnectorError::Transport(format!(
                "webhook dispatch Step 1 snapshot failed: {other}"
            )));
        }
    };

    // ── Step 2: dispatch — UNLOCKED ──────────────────────────────
    //
    // Run the connector's webhook handler. Any error here surfaces
    // unchanged into the framework's error mapper (`Webhook` → 400,
    // anything else → 502). The mutex is NOT held; concurrent FFI
    // calls on the same runtime keep running.
    let events = connector.handle_webhook_event(body)?;

    // ── Step 3: persist — locked ─────────────────────────────────
    //
    // Re-acquire the runtime mutex and ingest each event into the
    // encrypted evidence store. Re-validate the scope first (the
    // host may have called `forget_scope` while step 2 was in
    // flight); ingest the events under the source-tag contract the
    // sync path uses. Failures here are substrate-side faults → 502.
    let persisted = with_runtime(handle, |rt| {
        if rt.is_scope_forgotten(scope) {
            // The host raced a `forget_scope` against this webhook.
            // Drop the payload silently from the substrate's POV
            // (the scope is gone; we can't keep its data) but
            // surface the count as 0 so the dispatcher's `OK`
            // counter still increments. Aliasing this to a 400
            // would re-trigger upstream redelivery for data that's
            // cryptographically unrecoverable anyway.
            tracing::info!(instance = %instance_id,
                scope = %scope,
                "webhook payload dropped: scope was forgotten between dispatch phases"
            );
            return Ok(0usize);
        }
        rt.ensure_scope_registered(scope)?;
        let source_tag = connector_source_tag(kind);
        let mut ingested = 0usize;
        for ev in &events {
            if let Some(body) = event_to_evidence_body(ev) {
                // Stamp the BCP-47 primary subtag on each
                // webhook-dispatched event. Same fail-closed
                // contract as the connector sync path: a NULL
                // outcome means "language unknown" (the body
                // failed detection or is too short / pure
                // structural JSON). Detection runs at the
                // persistent write boundary so the schema-v13
                // column populates for every production payload,
                // not just the in-memory observation pipeline.
                let detection = observation_engine::detect_language(&body);
                let language_tag = detection.as_ref().map(|d| d.tag.as_str());
                rt.store_mut()
                    .ingest_with_language(
                        scope,
                        body.as_bytes(),
                        Some(source_tag),
                        evidence_store::ImportanceClass::Important,
                        language_tag,
                    )
                    .map_err(|e| FfiError::Evidence {
                        message: e.to_string(),
                    })?;
                ingested += 1;
            }
        }
        Ok(ingested)
    });

    match persisted {
        Ok(n) => Ok(n),
        Err(FfiError::Unavailable { subsystem }) => Err(ConnectorError::Transport(format!(
            "webhook dispatch persist phase failed: subsystem={subsystem} unavailable"
        ))),
        Err(e) => Err(ConnectorError::Transport(format!(
            "webhook dispatch persist phase failed: {e}"
        ))),
    }
}

// ───────────────────────── Server lifecycle ────────────────────────

/// One running webhook server. Owned by
/// [`crate::runtime::FfiRuntime::webhook_servers`] keyed by
/// [`WebhookServerHandle`].
pub(crate) struct RunningWebhookServer {
    /// Resolved bind address. Captured AFTER the OS picked the
    /// ephemeral port (so `0.0.0.0:0` resolves to a real port the
    /// host can use to point its ingress at).
    bind_addr: SocketAddr,
    /// Unix epoch seconds when the server started.
    started_at: i64,
    /// Router (also held inside the framework's axum state). Cloned
    /// out of here on every `register_webhook_dispatch` /
    /// `unregister_webhook_dispatch` / `list_webhook_servers` call.
    router: Arc<FfiWebhookRouter>,
    /// `None` once [`Self::shutdown_and_join`] has consumed it.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// OS thread hosting the per-server tokio runtime. `None` once
    /// joined.
    runtime_thread: Option<JoinHandle<()>>,
}

impl RunningWebhookServer {
    /// Number of currently-registered `(provider_id, instance_id)`
    /// rows on this server's [`FfiWebhookRouter`]. Exposed for the
    /// connector health probe (`crates/ffi/src/health.rs`)
    /// so the operator can see at a glance how much of the
    /// configured webhook surface is bound.
    pub(crate) fn router_registration_count(&self) -> usize {
        self.router.registration_count()
    }

    /// Drive the graceful shutdown and synchronously join the
    /// runtime thread. Idempotent: a server that has already been
    /// stopped completes the call as a no-op.
    pub(crate) fn shutdown_and_join(&mut self) {
        // Drop the shutdown sender first — the framework's
        // `serve_on(listener, async move { let _ = rx.await; })`
        // futures completes the moment the channel is closed, which
        // tells axum to stop accepting new connections and drain
        // in-flight ones.
        if let Some(tx) = self.shutdown_tx.take() {
            // `Result::Err` here just means the receiver already
            // dropped (e.g. the runtime panicked); the desired
            // post-condition (channel closed) is satisfied either
            // way so we ignore the result.
            let _ = tx.send(());
        }
        if let Some(t) = self.runtime_thread.take() {
            // Joining the OS thread blocks until the tokio runtime's
            // `block_on` returns (i.e. axum's graceful shutdown
            // drained every in-flight request). The thread also
            // owns the tokio Runtime, which is dropped on the way
            // out of `block_on` — this drop joins every spawned
            // task synchronously so by the time `join` returns the
            // server has zero outstanding work.
            //
            // A panic on the runtime thread shows up as
            // `Err(_)` here; we log and continue so a single
            // server's panic doesn't block `close_store` from
            // tearing down the rest of the runtime.
            if let Err(e) = t.join() {
                tracing::warn!(?e, "webhook server runtime thread panicked during shutdown");
            }
        }
    }
}

/// Synchronously drain every entry in a webhook-server map by
/// driving each server's graceful shutdown and joining its runtime
/// thread.
///
/// `close_store` calls this with the runtime-mutex-protected
/// `webhook_servers` map taken out via `std::mem::take`, then drops
/// the runtime mutex BEFORE invoking this function. That ordering is
/// load-bearing: each server's runtime thread is blocked inside a
/// dispatcher closure that re-enters the runtime via [`with_runtime`]
/// (which acquires the same mutex). Holding the runtime mutex across
/// the join would deadlock.
///
/// The map is consumed by value to make the
/// "every-server-shut-down" post-condition observable in the type
/// system — once this function returns, the entries are guaranteed
/// gone. Individual server panics are logged but do not abort the
/// drain (one bad server should not block tearing down the rest of
/// the runtime).
///
/// Exposed at `pub(crate)` so [`crate::close_store`] can call it and
/// so intra-doc links from
/// [`crate::runtime::FfiRuntime::webhook_servers`] resolve. NOT part
/// of the FFI surface.
pub(crate) fn drain_all_servers(servers: HashMap<WebhookServerHandle, RunningWebhookServer>) {
    // `into_iter` over the owned `HashMap` is the canonical Rust
    // idiom for "consume and drop"; it avoids the otherwise-needed
    // `mut servers` binding that `servers.drain()` requires.
    for (sh, mut server) in servers {
        tracing::debug!(
            server_handle = sh.0,
            "draining webhook server on close_store",
        );
        server.shutdown_and_join();
    }
}

impl Drop for RunningWebhookServer {
    fn drop(&mut self) {
        // Belt-and-braces shutdown for the case where a server slot
        // is dropped without an explicit `stop_webhook_server` (e.g.
        // a future code path that swaps server slots, or
        // `close_store`'s drain consuming the HashMap by value). The
        // explicit `pre_close_drain` step in `crate::close_store` is
        // the load-bearing one — this Drop is just defence in depth
        // so a panicked runtime can't leak a still-running server.
        self.shutdown_and_join();
    }
}

// ───────────────────────── FFI entry points ────────────────────────

/// Start a webhook receiver server bound to `bind_addr`.
///
/// `bind_addr` is parsed via [`SocketAddr::from_str`] — pass IPv4
/// `127.0.0.1:9001`, IPv6 `[::1]:9001`, or `0.0.0.0:0` for an
/// ephemeral port (the resolved port is surfaced via
/// [`list_webhook_servers`]). Returns the opaque
/// [`WebhookServerHandle`] the host re-presents to every other
/// webhook FFI fn.
///
/// The framework's axum router pre-registers one entry per known
/// [`ConnectorKind`] slug (`"slack"`, `"notion"`, `"jira"`, …) all
/// pointing at a single per-server [`FfiWebhookRouter`]. The host
/// then calls [`register_webhook_dispatch`] to bind a
/// `(provider_id, instance_id)` pair before the upstream provider
/// starts POSTing — webhooks that land before a binding gets a
/// `400 Bad Request` with a diagnostic body.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called for `handle`.
/// * [`FfiError::InvalidId`] if `bind_addr` is not a valid
///   `host:port` string.
/// * [`FfiError::Connector`] if the OS rejects the bind (port in
///   use, permission denied) or the tokio runtime fails to spin up.
///
/// # Concurrency
///
/// The handle map mutation is performed under the runtime mutex; the
/// tokio runtime is spawned AFTER the mutex is released (the
/// three-phase locking pattern). Two concurrent
/// `start_webhook_server` calls on the same handle serialise on the
/// handle mutex; on different handles they run fully in parallel.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn start_webhook_server(
    handle: RuntimeHandle,
    bind_addr: String,
) -> FfiResult<WebhookServerHandle> {
    metrics::instrument(metrics::inc_start_webhook_server, || {
        let parsed: SocketAddr = bind_addr.parse().map_err(|e| FfiError::InvalidId {
            message: format!("invalid bind_addr `{bind_addr}`: {e}"),
        })?;

        let server_handle = WebhookServerHandle(next_server_handle());
        if server_handle.0 == WebhookServerHandle::NONE.0 {
            return Err(FfiError::Connector {
                message: "webhook server handle allocator wrapped to NONE sentinel".into(),
            });
        }

        // Build the per-server router OUTSIDE the runtime mutex so
        // the `Arc::new` allocation and the pre-registration of
        // every connector-kind slug into the framework's static
        // dispatcher map can run without blocking other FFI calls
        // against the same handle.
        let router = Arc::new(FfiWebhookRouter::new(handle));
        let router_for_framework: Arc<dyn WebhookDispatcher> = router.clone();
        let dispatches = enumerate_provider_dispatches(&router_for_framework);

        let config = WebhookServerConfig { bind_addr: parsed };
        // Spawn the tokio runtime + axum server on a dedicated OS
        // thread. The thread captures (config, dispatches,
        // shutdown_rx, listener_tx) and runs until the shutdown
        // oneshot fires. The current thread waits on the
        // listener_rx so it can surface the resolved bind address
        // (and any bind error) synchronously to the FFI caller.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (listener_tx, listener_rx) =
            std::sync::mpsc::sync_channel::<Result<SocketAddr, String>>(1);

        let runtime_thread = std::thread::Builder::new()
            .name(format!("knowledge-webhook-{}", server_handle.0))
            .spawn(move || {
                run_server_thread(config, dispatches, shutdown_rx, listener_tx);
            })
            .map_err(|e| FfiError::Connector {
                message: format!("failed to spawn webhook runtime thread: {e}"),
            })?;

        // Block until the runtime thread reports its bind result.
        // The thread always sends exactly one message; a closed
        // channel indicates the thread crashed before binding.
        let resolved_addr = match listener_rx.recv() {
            Ok(Ok(addr)) => addr,
            Ok(Err(msg)) => {
                // Bind failed — let the thread exit cleanly (the
                // shutdown_tx going out of scope at end of this
                // call would close the oneshot anyway, but joining
                // here makes the error path deterministic).
                let _ = runtime_thread.join();
                return Err(FfiError::Connector {
                    message: format!("webhook server bind failed: {msg}"),
                });
            }
            Err(_) => {
                let _ = runtime_thread.join();
                return Err(FfiError::Connector {
                    message: "webhook server runtime thread exited before binding".into(),
                });
            }
        };

        let server = RunningWebhookServer {
            bind_addr: resolved_addr,
            started_at: Utc::now().timestamp(),
            router,
            shutdown_tx: Some(shutdown_tx),
            runtime_thread: Some(runtime_thread),
        };

        // Insert into the runtime's webhook_servers map under the
        // runtime mutex via a `Vacant`-entry probe. `next_server_handle`
        // is a monotonic process-global counter, so a collision against
        // an existing handle is unreachable in practice; the
        // `Entry::Occupied` arm is defense-in-depth that fails closed
        // if a future change ever weakens that invariant. On the
        // `Occupied` early-return the closure-local `server` is
        // dropped, firing [`RunningWebhookServer::Drop`] which calls
        // `shutdown_and_join` — so the spawned tokio runtime is torn
        // down cleanly even when we never inserted it.
        with_runtime(handle, |rt| {
            use std::collections::hash_map::Entry;
            match rt.webhook_servers.entry(server_handle) {
                Entry::Vacant(slot) => {
                    slot.insert(server);
                    Ok(server_handle)
                }
                Entry::Occupied(_) => Err(FfiError::Connector {
                    message: format!(
                        "webhook server handle {} collided during allocation",
                        server_handle.0,
                    ),
                }),
            }
        })
    })
}

/// Stop a previously-started webhook server and synchronously join
/// its runtime thread.
///
/// Idempotent: stopping an unknown / already-stopped server returns
/// `Ok(())` with no work. Hosts can safely call this inside a
/// `try/finally` shutdown block without first probing whether the
/// server is still alive.
///
/// # Concurrency
///
/// The runtime mutex is released BEFORE the runtime thread is
/// joined. This is load-bearing: the joined thread runs
/// dispatcher closures that themselves re-acquire the mutex; holding
/// the mutex across the join would deadlock the moment a webhook
/// arrives during shutdown. The framework's graceful-shutdown
/// guarantee ensures every in-flight dispatch completes before the
/// join returns.
#[uniffi::export]
pub fn stop_webhook_server(
    handle: RuntimeHandle,
    server_handle: WebhookServerHandle,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_stop_webhook_server, || {
        // Take ownership of the server slot under the runtime mutex,
        // then drop the mutex before driving the join. The join can
        // be O(seconds) under heavy in-flight load — holding the
        // mutex would block every other FFI call against the same
        // handle for the entire drain.
        let server_opt = with_runtime(handle, |rt| Ok(rt.webhook_servers.remove(&server_handle)))?;

        if let Some(mut server) = server_opt {
            server.shutdown_and_join();
        }
        // Unknown / already-stopped server is a successful no-op.
        Ok(())
    })
}

/// Bind `provider_id` to `instance_id` on `server_handle`.
///
/// `provider_id` is the URL path segment that providers POST to —
/// the framework's static router pre-registers one entry per known
/// [`ConnectorKind`] slug ([`provider_id_for_kind`] is the source
/// of truth). The host calls this fn AFTER
/// [`crate::create_connector`] / [`crate::authenticate_connector`]
/// have built the instance.
///
/// Re-registering an already-bound `provider_id` REPLACES the prior
/// binding (idempotent — boot wiring that re-runs without an
/// explicit unregister sees a clean state).
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called for `handle`.
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `server_handle` does not name a
///   running server on this runtime.
/// * [`FfiError::NotFound`] if `instance_id` does not name a
///   connector instance on this runtime.
/// * [`FfiError::Connector`] if `provider_id` is not one of the
///   recognised connector-kind slugs the framework's static router
///   pre-registers (recognised set:
///   `slack`, `notion`, `jira`, `confluence`, `googledrive`,
///   `onedrive`, `hubspot`, `figma`, `email`, `github`,
///   `genericwebhook`).
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn register_webhook_dispatch(
    handle: RuntimeHandle,
    server_handle: WebhookServerHandle,
    provider_id: String,
    instance_id: String,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_register_webhook_dispatch, || {
        let instance = parse_instance_id(&instance_id)?;

        // Validate `provider_id` against the set the framework's
        // static router actually pre-registers. The check is a
        // pure-Rust lookup — no allocation, no system calls.
        if !is_known_provider_id(&provider_id) {
            return Err(FfiError::Connector {
                message: format!(
                    "unknown provider_id `{provider_id}`: must be one of \
                     slack, notion, jira, confluence, googledrive, onedrive, \
                     hubspot, figma, email, github, genericwebhook",
                ),
            });
        }

        // Snapshot the router Arc + validate the connector instance
        // under the runtime mutex; mutate the routing map AFTER
        // releasing the mutex so the (briefly-held) RwLock write
        // never overlaps with a runtime-mutex acquisition.
        let router = with_runtime(handle, |rt| {
            let Some(server) = rt.webhook_servers.get(&server_handle) else {
                return Err(FfiError::NotFound {
                    kind: "webhook_server".into(),
                    id: server_handle.0.to_string(),
                });
            };
            if !rt.connector_instances.contains_key(&instance) {
                return Err(FfiError::NotFound {
                    kind: "connector_instance".into(),
                    id: instance_id.clone(),
                });
            }
            Ok(Arc::clone(&server.router))
        })?;

        router.register(provider_id, instance);
        Ok(())
    })
}

/// Drop the binding for `(server_handle, provider_id)`.
///
/// Idempotent — unregistering an unknown `provider_id` returns
/// `Ok(())` because the post-condition (no binding) is satisfied
/// either way.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called for `handle`.
/// * [`FfiError::NotFound`] if `server_handle` does not name a
///   running server on this runtime.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn unregister_webhook_dispatch(
    handle: RuntimeHandle,
    server_handle: WebhookServerHandle,
    provider_id: String,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_unregister_webhook_dispatch, || {
        let router = with_runtime(handle, |rt| match rt.webhook_servers.get(&server_handle) {
            Some(s) => Ok(Arc::clone(&s.router)),
            None => Err(FfiError::NotFound {
                kind: "webhook_server".into(),
                id: server_handle.0.to_string(),
            }),
        })?;

        router.unregister(&provider_id);
        Ok(())
    })
}

/// List every running webhook server on `handle` with its
/// per-server counters.
///
/// Useful for diagnostics ("which servers are bound?", "which one
/// is dispatching?") and for hosts that requested an ephemeral
/// port at start time and need to discover the resolved port.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called for `handle`.
#[uniffi::export]
pub fn list_webhook_servers(handle: RuntimeHandle) -> FfiResult<Vec<WebhookServerSummary>> {
    metrics::instrument(metrics::inc_list_webhook_servers, || {
        with_runtime(handle, |rt| {
            let mut out = Vec::with_capacity(rt.webhook_servers.len());
            // Sort by handle so the output is deterministic across
            // calls — `HashMap` iteration order is randomised, and
            // host UIs that diff successive snapshots would
            // otherwise see spurious churn.
            let mut handles: Vec<WebhookServerHandle> =
                rt.webhook_servers.keys().copied().collect();
            handles.sort_by_key(|h| h.0);
            for sh in handles {
                let server = &rt.webhook_servers[&sh];
                out.push(WebhookServerSummary {
                    server_handle: sh,
                    bind_addr: server.bind_addr.to_string(),
                    started_at: server.started_at,
                    // Saturating cast: registration_count is the
                    // number of registered (provider_id,
                    // instance_id) rows. Bounded by the static
                    // `KNOWN_PROVIDER_IDS` enumeration in this
                    // module (the framework's connector kind set
                    // — currently ≤16 entries). A u32 ceiling at
                    // ~4 billion is comfortably larger than any
                    // realistic future expansion; the saturating
                    // cast here is purely defensive against the
                    // unlikely case of a host registering more
                    // entries than KNOWN_PROVIDER_IDS allows.
                    registration_count: u32::try_from(server.router.registration_count())
                        .unwrap_or(u32::MAX),
                    dispatch_ok_total: server.router.dispatch_ok_total.load(Ordering::Relaxed),
                    dispatch_bad_request_total: server
                        .router
                        .dispatch_bad_request_total
                        .load(Ordering::Relaxed),
                    dispatch_bad_gateway_total: server
                        .router
                        .dispatch_bad_gateway_total
                        .load(Ordering::Relaxed),
                });
            }
            Ok(out)
        })
    })
}

// ───────────────────────── Server-thread entry point ───────────────

/// Entry point of the per-server OS thread. Builds a `current_thread`
/// tokio runtime, binds the listener, and drives the axum server
/// until the shutdown oneshot fires.
fn run_server_thread(
    config: WebhookServerConfig,
    dispatches: Vec<WebhookDispatch>,
    shutdown_rx: oneshot::Receiver<()>,
    listener_tx: std::sync::mpsc::SyncSender<Result<SocketAddr, String>>,
) {
    // `current_thread` (single-threaded) is sufficient — the
    // dispatcher's blocking work runs on a separate spawn_blocking
    // worker pool that tokio manages internally, so the main reactor
    // thread is only responsible for the axum I/O loop. Using
    // multi_thread here would double the OS-thread footprint per
    // running server with no measurable throughput improvement
    // (incoming webhook POSTs are bound by spawn_blocking dispatch
    // latency, not by the reactor's request-dispatch loop).
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = listener_tx.send(Err(format!("tokio runtime build failed: {e}")));
            return;
        }
    };

    runtime.block_on(async move {
        // Pre-bind the listener so the FFI caller learns the
        // resolved address (`0.0.0.0:0` → real port) before this
        // task starts accepting connections.
        let listener = match tokio::net::TcpListener::bind(config.bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                let _ = listener_tx.send(Err(format!(
                    "bind to {addr} failed: {e}",
                    addr = config.bind_addr,
                )));
                return;
            }
        };
        let resolved = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => {
                let _ = listener_tx.send(Err(format!("local_addr() failed: {e}")));
                return;
            }
        };
        if listener_tx.send(Ok(resolved)).is_err() {
            // The caller dropped the receiver before we could
            // report the bind — they're tearing down. Exit cleanly.
            return;
        }

        let server = WebhookServer::new(config, dispatches);
        // `serve_on` is the arbitrary-future-driven shutdown
        // variant; we pass the `shutdown_rx` await directly so the
        // moment the FFI side drops `shutdown_tx`, axum begins
        // graceful shutdown.
        let shutdown_fut = async move {
            let _ = shutdown_rx.await;
        };
        if let Err(e) = server.serve_on(listener, shutdown_fut).await {
            tracing::warn!(?e, "webhook server serve_on returned an error");
        }
    });
    // Dropping `runtime` here joins every spawn_blocking worker
    // before this fn returns, which is exactly what
    // `RunningWebhookServer::shutdown_and_join` is waiting on. No
    // explicit shutdown_timeout call needed — the runtime's Drop
    // impl already does it synchronously.
}

// ───────────────────────── Provider-id helpers ─────────────────────

/// Canonical ASCII slug the framework's static router uses for
/// `kind`'s `/webhooks/{provider_id}` path. Mirrors the
/// `ConnectorKind` enum exhaustively so adding a new kind in the
/// framework forces a compile-time update here.
///
/// Currently consumed by the in-module test that pins exhaustive
/// coverage of [`KNOWN_PROVIDER_IDS`]; kept `pub(crate)` so
/// auto-registration on `create_connector` can derive the slug from
/// the `ConnectorKind` without duplicating the match arm.
#[allow(dead_code)]
pub(crate) fn provider_id_for_kind(kind: ConnectorKind) -> &'static str {
    match kind {
        ConnectorKind::GoogleDrive => "googledrive",
        ConnectorKind::OneDrive => "onedrive",
        ConnectorKind::Notion => "notion",
        ConnectorKind::Jira => "jira",
        ConnectorKind::Confluence => "confluence",
        ConnectorKind::GitHub => "github",
        ConnectorKind::Slack => "slack",
        ConnectorKind::Figma => "figma",
        ConnectorKind::HubSpot => "hubspot",
        ConnectorKind::Email => "email",
        ConnectorKind::QuickBooks => "quickbooks",
        ConnectorKind::Xero => "xero",
        ConnectorKind::Stripe => "stripe",
        ConnectorKind::Shopify => "shopify",
        ConnectorKind::Airtable => "airtable",
        ConnectorKind::GitLab => "gitlab",
        ConnectorKind::Bitbucket => "bitbucket",
        ConnectorKind::Trello => "trello",
        ConnectorKind::Miro => "miro",
        ConnectorKind::DocuSign => "docusign",
        ConnectorKind::Dropbox => "dropbox",
        ConnectorKind::Box => "box",
        ConnectorKind::SharePoint => "sharepoint",
        ConnectorKind::Teams => "teams",
        ConnectorKind::Discord => "discord",
        ConnectorKind::Zoom => "zoom",
        ConnectorKind::GoogleCalendar => "googlecalendar",
        ConnectorKind::GoogleDocs => "googledocs",
        ConnectorKind::GoogleSheets => "googlesheets",
        ConnectorKind::GoogleMeet => "googlemeet",
        ConnectorKind::Salesforce => "salesforce",
        ConnectorKind::ServiceNow => "servicenow",
        ConnectorKind::Zendesk => "zendesk",
        ConnectorKind::Linear => "linear",
        ConnectorKind::Asana => "asana",
        ConnectorKind::Monday => "monday",
        ConnectorKind::ClickUp => "clickup",
        ConnectorKind::Freshdesk => "freshdesk",
        ConnectorKind::Intercom => "intercom",
        ConnectorKind::Pipedrive => "pipedrive",
        // Singapore/Thailand/SEA connectors
        ConnectorKind::Line => "line",
        ConnectorKind::Grab => "grab",
        ConnectorKind::Gojek => "gojek",
        ConnectorKind::Talenox => "talenox",
        ConnectorKind::OdooSea => "odoosea",
        ConnectorKind::Fastwork => "fastwork",
        ConnectorKind::TrueMoney => "truemoney",
        ConnectorKind::ScbEasy => "scbeasy",
        ConnectorKind::PromptPay => "promptpay",
        ConnectorKind::Tokopedia => "tokopedia",
        // Vietnam connectors (WS5).
        ConnectorKind::Zalo => "zalo",
        ConnectorKind::VNPay => "vnpay",
        ConnectorKind::MoMo => "momo",
        ConnectorKind::Tiki => "tiki",
        ConnectorKind::ShopeeVN => "shopeevn",
        ConnectorKind::LazadaVN => "lazadavn",
        ConnectorKind::ViettelPost => "viettelpost",
        ConnectorKind::KiotViet => "kiotviet",
        ConnectorKind::Sapo => "sapo",
        ConnectorKind::BaseVN => "basevn",
        // GCC / Middle East connectors
        ConnectorKind::Careem => "careem",
        ConnectorKind::Talabat => "talabat",
        ConnectorKind::Noon => "noon",
        ConnectorKind::AmazonAE => "amazonae",
        ConnectorKind::Tabby => "tabby",
        ConnectorKind::Foodics => "foodics",
        ConnectorKind::Zoho => "zoho",
        ConnectorKind::Bayt => "bayt",
        ConnectorKind::Fetchr => "fetchr",
        ConnectorKind::Payfort => "payfort",
        ConnectorKind::MonzoBusiness => "monzobusiness",
        ConnectorKind::RevolutBusiness => "revolutbusiness",
        ConnectorKind::FreeAgent => "freeagent",
        ConnectorKind::GoCardless => "gocardless",
        ConnectorKind::RoyalMail => "royalmail",
        ConnectorKind::Deliveroo => "deliveroo",
        ConnectorKind::JustEat => "justeat",
        ConnectorKind::CompaniesHouse => "companieshouse",
        ConnectorKind::HmrcMtd => "hmrcmtd",
        ConnectorKind::Starling => "starling",
        ConnectorKind::N26Business => "n26business",
        ConnectorKind::Datev => "datev",
        ConnectorKind::Lexoffice => "lexoffice",
        ConnectorKind::DhlBusiness => "dhlbusiness",
        ConnectorKind::Otto => "otto",
        ConnectorKind::Zalando => "zalando",
        ConnectorKind::DeutschePost => "deutschepost",
        ConnectorKind::Personio => "personio",
        ConnectorKind::SevDesk => "sevdesk",
        ConnectorKind::Billomat => "billomat",
        ConnectorKind::Qonto => "qonto",
        ConnectorKind::Pennylane => "pennylane",
        ConnectorKind::PayFit => "payfit",
        ConnectorKind::Colissimo => "colissimo",
        ConnectorKind::Cdiscount => "cdiscount",
        ConnectorKind::MangoPay => "mangopay",
        ConnectorKind::Sendinblue => "sendinblue",
        ConnectorKind::OvhCloud => "ovhcloud",
        ConnectorKind::Alan => "alan",
        ConnectorKind::Swile => "swile",
        ConnectorKind::PostFinance => "postfinance",
        ConnectorKind::Twint => "twint",
        ConnectorKind::SwissPost => "swisspost",
        ConnectorKind::Bexio => "bexio",
        ConnectorKind::Abacus => "abacus",
        ConnectorKind::Ricardo => "ricardo",
        ConnectorKind::DigitecGalaxus => "digitecgalaxus",
        ConnectorKind::SixPayment => "sixpayment",
        ConnectorKind::Klara => "klara",
        ConnectorKind::Beem => "beem",
        ConnectorKind::GenericWebhook => "genericwebhook",
    }
}

/// All recognised `provider_id` slugs, in the same order
/// [`provider_id_for_kind`] enumerates them.
const KNOWN_PROVIDER_IDS: &[&str] = &[
    "googledrive",
    "onedrive",
    "notion",
    "jira",
    "confluence",
    "github",
    "slack",
    "figma",
    "hubspot",
    "email",
    "quickbooks",
    "xero",
    "stripe",
    "shopify",
    "airtable",
    "gitlab",
    "bitbucket",
    "trello",
    "miro",
    "docusign",
    "dropbox",
    "box",
    "sharepoint",
    "teams",
    "discord",
    "zoom",
    "googlecalendar",
    "googledocs",
    "googlesheets",
    "googlemeet",
    "salesforce",
    "servicenow",
    "zendesk",
    "linear",
    "asana",
    "monday",
    "clickup",
    "freshdesk",
    "intercom",
    "pipedrive",
    // Singapore/Thailand/SEA connectors
    "line",
    "grab",
    "gojek",
    "talenox",
    "odoosea",
    "fastwork",
    "truemoney",
    "scbeasy",
    "promptpay",
    "tokopedia",
    // Vietnam connectors (WS5).
    "zalo",
    "vnpay",
    "momo",
    "tiki",
    "shopeevn",
    "lazadavn",
    "viettelpost",
    "kiotviet",
    "sapo",
    "basevn",
    // GCC / Middle East connectors
    "careem",
    "talabat",
    "noon",
    "amazonae",
    "tabby",
    "foodics",
    "zoho",
    "bayt",
    "fetchr",
    "payfort",
    "monzobusiness",
    "revolutbusiness",
    "freeagent",
    "gocardless",
    "royalmail",
    "deliveroo",
    "justeat",
    "companieshouse",
    "hmrcmtd",
    "starling",
    "n26business",
    "datev",
    "lexoffice",
    "dhlbusiness",
    "otto",
    "zalando",
    "deutschepost",
    "personio",
    "sevdesk",
    "billomat",
    "qonto",
    "pennylane",
    "payfit",
    "colissimo",
    "cdiscount",
    "mangopay",
    "sendinblue",
    "ovhcloud",
    "alan",
    "swile",
    "postfinance",
    "twint",
    "swisspost",
    "bexio",
    "abacus",
    "ricardo",
    "digitecgalaxus",
    "sixpayment",
    "klara",
    "beem",
    "genericwebhook",
];

fn is_known_provider_id(s: &str) -> bool {
    KNOWN_PROVIDER_IDS.contains(&s)
}

/// Build the `Vec<WebhookDispatch>` the framework's `WebhookServer::new`
/// consumes — one entry per known [`ConnectorKind`] slug, every
/// entry pointing at the same per-server [`FfiWebhookRouter`]. The
/// framework's static-table lookup then routes
/// `POST /webhooks/<slug>` through this single dispatcher.
fn enumerate_provider_dispatches(router: &Arc<dyn WebhookDispatcher>) -> Vec<WebhookDispatch> {
    KNOWN_PROVIDER_IDS
        .iter()
        .map(|slug| WebhookDispatch {
            provider_id: (*slug).to_string(),
            dispatcher: Arc::clone(router),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_for_kind_is_exhaustive_and_known() {
        // Every slug returned by `provider_id_for_kind` MUST be in
        // `KNOWN_PROVIDER_IDS` — otherwise `register_webhook_dispatch`
        // would reject a kind the framework's router DOES accept.
        let all_kinds = [
            ConnectorKind::GoogleDrive,
            ConnectorKind::OneDrive,
            ConnectorKind::Notion,
            ConnectorKind::Jira,
            ConnectorKind::Confluence,
            ConnectorKind::GitHub,
            ConnectorKind::Slack,
            ConnectorKind::Figma,
            ConnectorKind::HubSpot,
            ConnectorKind::Email,
            ConnectorKind::QuickBooks,
            ConnectorKind::Xero,
            ConnectorKind::Stripe,
            ConnectorKind::Shopify,
            ConnectorKind::Airtable,
            ConnectorKind::GitLab,
            ConnectorKind::Bitbucket,
            ConnectorKind::Trello,
            ConnectorKind::Miro,
            ConnectorKind::DocuSign,
            ConnectorKind::Dropbox,
            ConnectorKind::Box,
            ConnectorKind::SharePoint,
            ConnectorKind::Teams,
            ConnectorKind::Discord,
            ConnectorKind::Zoom,
            ConnectorKind::GoogleCalendar,
            ConnectorKind::GoogleDocs,
            ConnectorKind::GoogleSheets,
            ConnectorKind::GoogleMeet,
            ConnectorKind::Salesforce,
            ConnectorKind::ServiceNow,
            ConnectorKind::Zendesk,
            ConnectorKind::Linear,
            ConnectorKind::Asana,
            ConnectorKind::Monday,
            ConnectorKind::ClickUp,
            ConnectorKind::Freshdesk,
            ConnectorKind::Intercom,
            ConnectorKind::Pipedrive,
            // Singapore/Thailand/SEA connectors
            ConnectorKind::Line,
            ConnectorKind::Grab,
            ConnectorKind::Gojek,
            ConnectorKind::Talenox,
            ConnectorKind::OdooSea,
            ConnectorKind::Fastwork,
            ConnectorKind::TrueMoney,
            ConnectorKind::ScbEasy,
            ConnectorKind::PromptPay,
            ConnectorKind::Tokopedia,
            // Vietnam connectors (WS5).
            ConnectorKind::Zalo,
            ConnectorKind::VNPay,
            ConnectorKind::MoMo,
            ConnectorKind::Tiki,
            ConnectorKind::ShopeeVN,
            ConnectorKind::LazadaVN,
            ConnectorKind::ViettelPost,
            ConnectorKind::KiotViet,
            ConnectorKind::Sapo,
            ConnectorKind::BaseVN,
            // GCC / Middle East connectors
            ConnectorKind::Careem,
            ConnectorKind::Talabat,
            ConnectorKind::Noon,
            ConnectorKind::AmazonAE,
            ConnectorKind::Tabby,
            ConnectorKind::Foodics,
            ConnectorKind::Zoho,
            ConnectorKind::Bayt,
            ConnectorKind::Fetchr,
            ConnectorKind::Payfort,
            ConnectorKind::MonzoBusiness,
            ConnectorKind::RevolutBusiness,
            ConnectorKind::FreeAgent,
            ConnectorKind::GoCardless,
            ConnectorKind::RoyalMail,
            ConnectorKind::Deliveroo,
            ConnectorKind::JustEat,
            ConnectorKind::CompaniesHouse,
            ConnectorKind::HmrcMtd,
            ConnectorKind::Starling,
            ConnectorKind::N26Business,
            ConnectorKind::Datev,
            ConnectorKind::Lexoffice,
            ConnectorKind::DhlBusiness,
            ConnectorKind::Otto,
            ConnectorKind::Zalando,
            ConnectorKind::DeutschePost,
            ConnectorKind::Personio,
            ConnectorKind::SevDesk,
            ConnectorKind::Billomat,
            ConnectorKind::Qonto,
            ConnectorKind::Pennylane,
            ConnectorKind::PayFit,
            ConnectorKind::Colissimo,
            ConnectorKind::Cdiscount,
            ConnectorKind::MangoPay,
            ConnectorKind::Sendinblue,
            ConnectorKind::OvhCloud,
            ConnectorKind::Alan,
            ConnectorKind::Swile,
            ConnectorKind::PostFinance,
            ConnectorKind::Twint,
            ConnectorKind::SwissPost,
            ConnectorKind::Bexio,
            ConnectorKind::Abacus,
            ConnectorKind::Ricardo,
            ConnectorKind::DigitecGalaxus,
            ConnectorKind::SixPayment,
            ConnectorKind::Klara,
            ConnectorKind::Beem,
            ConnectorKind::GenericWebhook,
        ];
        for k in all_kinds {
            let slug = provider_id_for_kind(k);
            assert!(
                is_known_provider_id(slug),
                "provider_id_for_kind({k:?}) = {slug:?} not in KNOWN_PROVIDER_IDS",
            );
        }
        assert_eq!(KNOWN_PROVIDER_IDS.len(), all_kinds.len());
    }

    #[test]
    fn next_server_handle_is_monotone_and_nonzero() {
        let a = next_server_handle();
        let b = next_server_handle();
        let c = next_server_handle();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(c, 0);
        assert!(b > a);
        assert!(c > b);
    }

    #[test]
    fn ffi_webhook_router_register_unregister_round_trip() {
        let router = FfiWebhookRouter::new(RuntimeHandle::NONE);
        assert_eq!(router.registration_count(), 0);

        let id = ConnectorInstanceId::new_v4();
        router.register("slack".into(), id);
        assert_eq!(router.registration_count(), 1);

        // Re-register replaces, not duplicates.
        let id2 = ConnectorInstanceId::new_v4();
        router.register("slack".into(), id2);
        assert_eq!(router.registration_count(), 1);

        // Unregister of a bound provider returns true.
        assert!(router.unregister("slack"));
        assert_eq!(router.registration_count(), 0);

        // Unregister of an unbound provider returns false.
        assert!(!router.unregister("slack"));
    }
}
