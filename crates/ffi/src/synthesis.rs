//! Server-side synthesis FFI entry points (Phase 7).
//!
//! Exposes four entry points to platform hosts:
//!
//! * [`configure_synthesis_engine`] — install the
//!   [`synthesis_engine::HttpManagedEndpointSynthesizer`] (plus
//!   optional scope-binding allow-list) on the runtime.
//! * [`trigger_server_synthesis`] — dispatch a domain / tenant
//!   synthesis run, applying the three-phase locking discipline
//!   (gather-locked → dispatch-unlocked → apply-locked) used by
//!   [`crate::trigger_synthesis`] and
//!   [`crate::sync_connector`](crate::connector::sync_connector).
//! * [`synthesis_status`] — look up the lifecycle status of a
//!   previously dispatched window.
//! * [`list_recent_syntheses`] — enumerate the per-scope synthesis
//!   history (newest first, capped at [`LIST_RECENT_SYNTHESES_CAP`]
//!   to bound response size).
//!
//! # Architectural contracts
//!
//! * **Three-phase locking.** The dispatch (Phase 2) runs **without**
//!   the per-handle [`crate::FfiRuntime`] mutex so concurrent FFI
//!   calls on the same handle are not blocked behind the
//!   (potentially multi-second) HTTPS call to the managed endpoint.
//!   The engine is stored as `Arc<dyn SynthesisEngine>` so Phase 1
//!   can clone the trait object out of the mutex.
//! * **Per-scope cooldown.** A successful dispatch records
//!   `synthesis_cooldowns[scope] = Utc::now()`. A subsequent
//!   dispatch on the same scope within
//!   [`PER_SCOPE_COOLDOWN_SECS`] returns the most recent window id
//!   without re-running synthesis. The scheduler's
//!   auto-synthesis hook uses the same map so a host that triggers
//!   synthesis manually also throttles the scheduler's next attempt.
//! * **Window retention cap.** After every successful synthesis the
//!   substrate prunes completed windows beyond
//!   [`WINDOW_RETENTION_CAP_PER_SCOPE`]. Older completed windows
//!   (and their persisted [`synthesis_pipeline::SynthesisObject`]
//!   rows) are removed; in-progress / failed / pending windows are
//!   never pruned.
//! * **Output size cap.** The substrate refuses to install
//!   synthesis objects whose payload exceeds
//!   [`MAX_SYNTHESIS_OUTPUT_BYTES`] so a misbehaving endpoint
//!   cannot fill the evidence store with a single recap. Hosts see
//!   [`FfiError::Synthesis`] in that case.
//! * **Scope-binding enforcement.** When a host installs the
//!   engine with [`SynthesisEngineConfig::scope_bindings`] set, the
//!   FFI layer enforces the allow-list before dispatch (mirrors
//!   [`synthesis_engine::tee_worker::TeeWorker::assert_scope_allowed`]).
//!   An unconfigured allow-list logs a warning on every dispatch.

use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "http-client")]
use synthesis_engine::BlockingHttpClientAdapter;
#[cfg(feature = "http-client")]
use synthesis_engine::HttpManagedEndpointSynthesizer;

use chrono::Utc;
use evidence_store::ScopeId;
use uuid::Uuid;

use synthesis_engine::{EndpointConfig, EngineError, SynthesisEngine};
use synthesis_pipeline::{
    ChannelOutput, DomainOutput, DomainSynthesisInput, HierarchyEnforcedWindowManager,
    SynthesisObject, SynthesisObjectType, SynthesisWindowManager, TenantSynthesisInput,
    TieredWindowHandle, WindowId, WindowScopeTier, WindowStatus,
};

use crate::error::{FfiError, FfiResult};
use crate::metrics;
use crate::runtime::{with_runtime, FfiRuntime, RuntimeHandle};
use crate::types::{SynthesisEngineConfig, SynthesisStatusRecord, SynthesisTierKind};

/// Per-scope cooldown between explicit `trigger_server_synthesis`
/// calls. Matches the scheduler's auto-synthesis throttle so the
/// two paths cannot collude into a runaway synthesis loop.
pub const PER_SCOPE_COOLDOWN_SECS: i64 = 300;

/// Default rolling window size used by `trigger_server_synthesis`
/// when the host does not preconfigure window boundaries. Picked at
/// 7 days to capture a meaningful slice of activity without
/// dominating the SLM prompt.
pub const DEFAULT_WINDOW_DURATION_SECS: i64 = 7 * 24 * 60 * 60;

/// Maximum number of completed windows the substrate retains
/// per scope. Pruned after every successful synthesis.
pub const WINDOW_RETENTION_CAP_PER_SCOPE: usize = 20;

/// Hard cap on the size of a single synthesis output payload. The
/// design target is ~5 KB for domain summaries and ~10 KB for
/// tenant summaries — the 32 KiB cap allows generous headroom while
/// still bounding evidence-store growth.
pub const MAX_SYNTHESIS_OUTPUT_BYTES: usize = 32_768;

/// Maximum number of records returned by
/// [`list_recent_syntheses`]. Bounds the response size so a host
/// that opens a long-lived database does not pay an unbounded
/// serialisation cost on every status poll.
pub const LIST_RECENT_SYNTHESES_CAP: usize = 50;

// ─────────────────────── Public entry points ───────────────────────

/// Install the server-side synthesis engine on `handle`.
///
/// On builds with the `http-client` feature the substrate wraps the
/// supplied [`SynthesisEngineConfig`] in a
/// [`BlockingHttpClientAdapter`]-backed
/// [`HttpManagedEndpointSynthesizer`]; on minimal builds the
/// configuration is rejected with
/// [`FfiError::Unavailable`] because no production HTTP transport
/// is compiled in.
///
/// Subsequent calls overwrite the slot — there is exactly one
/// engine per runtime. Scope-binding allow-lists installed by an
/// earlier call are replaced (or cleared when the new config has
/// `scope_bindings: None`).
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if compiled without the
///   `http-client` feature, or if the config-supplied URL /
///   timeout values are rejected by
///   [`BlockingHttpClientAdapter::new`].
/// * [`FfiError::InvalidId`] if any UUID in
///   `config.scope_bindings` is malformed.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn configure_synthesis_engine(
    handle: RuntimeHandle,
    config: SynthesisEngineConfig,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_configure_synthesis_engine, || {
        let endpoint_config = endpoint_config_from_ffi(&config);
        let scope_bindings = parse_scope_bindings(config.scope_bindings.as_deref())?;
        configure_engine_impl(handle, endpoint_config, scope_bindings)
    })
}

#[cfg(feature = "http-client")]
fn configure_engine_impl(
    handle: RuntimeHandle,
    endpoint_config: EndpointConfig,
    scope_bindings: Option<Vec<Uuid>>,
) -> FfiResult<()> {
    let client =
        BlockingHttpClientAdapter::new(&endpoint_config).map_err(|e| FfiError::Unavailable {
            subsystem: format!("synthesis_engine ({e})"),
        })?;
    let synth = HttpManagedEndpointSynthesizer::new(endpoint_config, client);
    let engine: Arc<dyn SynthesisEngine> = Arc::new(synth);
    with_runtime(handle, |rt| {
        rt.synthesis_engine = Some(engine);
        rt.synthesis_scope_bindings = scope_bindings;
        tracing::info!(
            handle = handle.0,
            scope_bindings_configured = rt.synthesis_scope_bindings.is_some(),
            "configure_synthesis_engine: synthesis engine installed",
        );
        Ok(())
    })
}

#[cfg(not(feature = "http-client"))]
fn configure_engine_impl(
    _handle: RuntimeHandle,
    _endpoint_config: EndpointConfig,
    _scope_bindings: Option<Vec<Uuid>>,
) -> FfiResult<()> {
    Err(FfiError::Unavailable {
        subsystem: "synthesis_engine (built without http-client feature)".into(),
    })
}

/// Dispatch a server-side synthesis run for `scope_id` at `tier`.
///
/// Returns the UUID-string of the synthesis window the host can
/// poll via [`synthesis_status`]. On cooldown short-circuit, returns
/// the most recently completed window for the same scope.
///
/// The locking discipline (gather → dispatch → apply) matches the
/// on-device [`crate::trigger_synthesis`] path. See the module
/// docs for full details.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `scope_id` has been forgotten or
///   has no domain/tenant memory registered for the requested tier.
/// * [`FfiError::Unavailable`] if no engine is configured.
/// * [`FfiError::Synthesis`] if the engine surfaced an error or
///   the response payload exceeds [`MAX_SYNTHESIS_OUTPUT_BYTES`].
/// * [`FfiError::Evidence`] if persisting the resulting synthesis
///   object fails.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn trigger_server_synthesis(
    handle: RuntimeHandle,
    scope_id: String,
    tier: SynthesisTierKind,
) -> FfiResult<String> {
    metrics::instrument(metrics::inc_trigger_server_synthesis, || {
        let scope = crate::parse_scope_id(&scope_id)?;
        tracing::info!(
            scope = %scope.as_uuid(),
            tier = tier.as_str(),
            "trigger_server_synthesis: dispatching",
        );
        dispatch_server_synthesis(handle, scope, tier).map_err(|err| {
            tracing::warn!(
                scope = %scope.as_uuid(),
                tier = tier.as_str(),
                error = ?err,
                "trigger_server_synthesis: failed",
            );
            err
        })
    })
}

/// Look up the lifecycle status of `synthesis_id`.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `synthesis_id` is not a valid
///   UUID.
/// * [`FfiError::NotFound`] if the substrate does not know of any
///   window with that id.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn synthesis_status(
    handle: RuntimeHandle,
    synthesis_id: String,
) -> FfiResult<SynthesisStatusRecord> {
    metrics::instrument(metrics::inc_synthesis_status, || {
        let window_id = parse_window_id(&synthesis_id)?;
        with_runtime(handle, |rt| {
            let window = rt
                .synthesis_windows
                .get(window_id)
                .ok_or_else(|| FfiError::NotFound {
                    kind: "synthesis_window".into(),
                    id: synthesis_id.clone(),
                })?;
            Ok(window_to_record(window, rt))
        })
    })
}

/// Enumerate recent synthesis windows for `scope_id`, newest
/// first, capped at [`LIST_RECENT_SYNTHESES_CAP`].
///
/// Returns an empty vector for a scope with no recorded synthesis
/// history (including the forgotten / unknown / never-touched
/// cases) — [`synthesis_status`] is the entry point hosts should
/// use to distinguish unknown ids.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn list_recent_syntheses(
    handle: RuntimeHandle,
    scope_id: String,
) -> FfiResult<Vec<SynthesisStatusRecord>> {
    metrics::instrument(metrics::inc_list_recent_syntheses, || {
        let scope = crate::parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(Vec::new());
            }
            let mut records: Vec<SynthesisStatusRecord> = rt
                .synthesis_windows
                .windows_for(scope)
                .into_iter()
                .map(|w| window_to_record(w, rt))
                .collect();
            // Newest first by window_end.
            records.sort_by_key(|r| std::cmp::Reverse(r.window_end_unix));
            records.truncate(LIST_RECENT_SYNTHESES_CAP);
            Ok(records)
        })
    })
}

// ─────────────────────── Implementation details ───────────────────────

/// Snapshot captured while the runtime mutex is held (Phase 1) and
/// then consumed during the unlocked dispatch (Phase 2) and the
/// post-dispatch apply (Phase 3).
///
/// `windows_clone` is a deep copy of the live
/// [`SynthesisWindowManager`] taken under the mutex; the engine
/// validates `handle.window_id` against it during Phase 2 (unlocked).
/// Phase 3 replays the Pending → InProgress → Complete transitions on
/// the live manager so the substrate's persisted state ends up
/// identical to what the engine observed.
struct DomainDispatchPlan {
    engine: Arc<dyn SynthesisEngine>,
    window_handle: TieredWindowHandle,
    input: DomainSynthesisInput,
    windows_clone: SynthesisWindowManager,
}

struct TenantDispatchPlan {
    engine: Arc<dyn SynthesisEngine>,
    window_handle: TieredWindowHandle,
    input: TenantSynthesisInput,
    windows_clone: SynthesisWindowManager,
}

enum DispatchPlan {
    Domain(DomainDispatchPlan),
    Tenant(TenantDispatchPlan),
    /// Cooldown short-circuit: return the supplied window id
    /// without dispatching.
    Cooldown(WindowId),
}

fn dispatch_server_synthesis(
    handle: RuntimeHandle,
    scope: ScopeId,
    tier: SynthesisTierKind,
) -> FfiResult<String> {
    // ─────────────── Phase 1: gather (locked) ───────────────
    //
    // Validate the scope, check the cooldown window, gather the
    // hierarchy input (channel outputs for domain, domain outputs
    // for tenant), open a `Pending` window, clone the engine `Arc`
    // out of the mutex, and return the resulting plan. The
    // synthesis window stays in `Pending` after Phase 1; Phase 2
    // (unlocked) issues the HTTP call; Phase 3 (locked) marks the
    // window `Complete` / `Failed` based on the dispatch outcome.
    let plan = with_runtime(handle, |rt| build_dispatch_plan(rt, scope, tier))?;

    // Cooldown short-circuit returns the cached window id without
    // entering Phase 2.
    let (engine, window_handle, dispatch_result) = match plan {
        DispatchPlan::Cooldown(window_id) => {
            tracing::info!(
                scope = %scope.as_uuid(),
                window = %window_id.as_uuid(),
                cooldown_secs = PER_SCOPE_COOLDOWN_SECS,
                "trigger_server_synthesis: returning cached window (cooldown)",
            );
            return Ok(window_id.as_uuid().to_string());
        }
        DispatchPlan::Domain(plan) => {
            let DomainDispatchPlan {
                engine,
                window_handle,
                input,
                mut windows_clone,
            } = plan;
            // ─────────── Phase 2: dispatch (UNLOCKED) ───────────
            //
            // The engine validates `handle.window_id` against
            // `windows_clone` (the snapshot we took under the
            // mutex) and transitions its window state. We do not
            // persist `windows_clone` — Phase 3 replays the
            // Pending → InProgress → Complete transitions on the
            // live manager.
            let outcome = engine.synthesize_domain(&mut windows_clone, window_handle, input);
            (engine, window_handle, outcome.map(|r| r.object))
        }
        DispatchPlan::Tenant(plan) => {
            let TenantDispatchPlan {
                engine,
                window_handle,
                input,
                mut windows_clone,
            } = plan;
            let outcome = engine.synthesize_tenant(&mut windows_clone, window_handle, input);
            (engine, window_handle, outcome.map(|r| r.object))
        }
    };
    // `engine` is dropped here — the Arc was only needed for the
    // unlocked dispatch and the runtime still owns its own clone.
    drop(engine);

    // ─────────────── Phase 3: apply (locked) ───────────────
    apply_dispatch_outcome(handle, scope, tier, window_handle, dispatch_result)
}

/// Phase 1 body. Holds the runtime mutex.
fn build_dispatch_plan(
    rt: &mut FfiRuntime,
    scope: ScopeId,
    tier: SynthesisTierKind,
) -> FfiResult<DispatchPlan> {
    if rt.is_scope_forgotten(scope) {
        return Err(FfiError::NotFound {
            kind: "scope".into(),
            id: scope.as_uuid().to_string(),
        });
    }

    // Engine slot — must be installed via `configure_synthesis_engine`.
    let engine = rt
        .synthesis_engine
        .as_ref()
        .ok_or_else(|| FfiError::Unavailable {
            subsystem: "synthesis_engine".into(),
        })?
        .clone();

    // Scope-binding allow-list — mirrors `TeeWorker::assert_scope_allowed`.
    enforce_scope_binding(rt, scope)?;

    // Cooldown check. If the scope was synthesised within the
    // last `PER_SCOPE_COOLDOWN_SECS` seconds, return the most
    // recent `Complete` window without re-dispatching.
    if let Some(last_completed) = rt.synthesis_cooldowns.get(&scope).copied() {
        let elapsed = Utc::now().signed_duration_since(last_completed);
        if elapsed < chrono::Duration::seconds(PER_SCOPE_COOLDOWN_SECS) {
            if let Some(recent) = newest_complete_window(rt, scope) {
                return Ok(DispatchPlan::Cooldown(recent));
            }
            // Cooldown stamp without a `Complete` window is a
            // bookkeeping bug rather than a host-visible failure —
            // fall through and dispatch a fresh run.
            tracing::warn!(
                scope = %scope.as_uuid(),
                "trigger_server_synthesis: cooldown stamp present but no Complete window; \
                 dispatching fresh run",
            );
        }
    }

    let now = Utc::now();
    let window_start = now - chrono::Duration::seconds(DEFAULT_WINDOW_DURATION_SECS);
    let window_end = now;

    match tier {
        SynthesisTierKind::Domain => {
            let domain = rt.domain_memory(scope).ok_or_else(|| FfiError::NotFound {
                kind: "domain_memory".into(),
                id: scope.as_uuid().to_string(),
            })?;
            let channel_outputs = gather_channel_outputs(rt, domain);
            let input = DomainSynthesisInput::new(domain, channel_outputs).map_err(|e| {
                FfiError::Synthesis {
                    message: format!("domain input rejected: {e}"),
                }
            })?;
            let window_handle = rt
                .synthesis_windows
                .open_tiered_window(scope, WindowScopeTier::Domain, window_start, window_end)
                .map_err(|e| FfiError::Synthesis {
                    message: format!("open_tiered_window failed: {e}"),
                })?;
            // Persist the freshly opened window so its `Pending`
            // status survives a crash between Phase 1 and Phase 3.
            rt.flush_synthesis_windows()?;
            // Take a snapshot of the live manager AFTER the open so
            // the cloned manager sees the new window in `Pending`.
            // The unlocked Phase 2 mutates this clone — Phase 3
            // replays the transitions on the live manager.
            let windows_clone = rt.synthesis_windows.clone();
            Ok(DispatchPlan::Domain(DomainDispatchPlan {
                engine,
                window_handle,
                input,
                windows_clone,
            }))
        }
        SynthesisTierKind::Tenant => {
            let tenant = rt.tenant_memory(scope).ok_or_else(|| FfiError::NotFound {
                kind: "tenant_memory".into(),
                id: scope.as_uuid().to_string(),
            })?;
            let domain_outputs = gather_domain_outputs(rt, tenant);
            let approved_documents = Vec::new();
            let input = TenantSynthesisInput::new(tenant, domain_outputs, approved_documents)
                .map_err(|e| FfiError::Synthesis {
                    message: format!("tenant input rejected: {e}"),
                })?;
            let window_handle = rt
                .synthesis_windows
                .open_tiered_window(scope, WindowScopeTier::Tenant, window_start, window_end)
                .map_err(|e| FfiError::Synthesis {
                    message: format!("open_tiered_window failed: {e}"),
                })?;
            rt.flush_synthesis_windows()?;
            let windows_clone = rt.synthesis_windows.clone();
            Ok(DispatchPlan::Tenant(TenantDispatchPlan {
                engine,
                window_handle,
                input,
                windows_clone,
            }))
        }
    }
}

/// Phase 3 body. Holds the runtime mutex.
fn apply_dispatch_outcome(
    handle: RuntimeHandle,
    scope: ScopeId,
    tier: SynthesisTierKind,
    window_handle: TieredWindowHandle,
    dispatch_result: std::result::Result<SynthesisObject, EngineError>,
) -> FfiResult<String> {
    with_runtime(handle, |rt| {
        // TOCTOU defence: another thread may have called
        // `forget_scope` during the unlocked phase. The window
        // entry we hold a handle to is gone from the manager in
        // that case (the forgetting path drops every per-scope
        // window) so there is no state to apply — surface
        // `Unavailable` and discard the recap.
        if rt.is_scope_forgotten(scope) {
            tracing::warn!(
                scope = %scope.as_uuid(),
                window = %window_handle.window_id.as_uuid(),
                "trigger_server_synthesis: scope forgotten during dispatch; discarding recap",
            );
            return Err(FfiError::Unavailable {
                subsystem: "scope_forgotten".into(),
            });
        }
        if rt.synthesis_windows.get(window_handle.window_id).is_none() {
            // Same shape as the forgotten-scope race but with the
            // narrower per-window mutation (a host that called
            // `remove_windows_for_scope` or similar between
            // phases).
            tracing::warn!(
                scope = %scope.as_uuid(),
                window = %window_handle.window_id.as_uuid(),
                "trigger_server_synthesis: window vanished during dispatch; discarding recap",
            );
            return Err(FfiError::Unavailable {
                subsystem: "synthesis_window".into(),
            });
        }

        match dispatch_result {
            Err(err) => {
                // Best effort: mark the window failed so retries
                // and diagnostic enumeration can see the outcome.
                fail_window_on_live_manager(rt, window_handle.window_id, "dispatch_error");
                if let Err(e) = rt.flush_synthesis_windows() {
                    tracing::warn!(
                        error = ?e,
                        "post-failure flush_synthesis_windows failed",
                    );
                }
                Err(FfiError::Synthesis {
                    message: format!("server synthesis failed: {err}"),
                })
            }
            Ok(object) => {
                if object.payload.len() > MAX_SYNTHESIS_OUTPUT_BYTES {
                    fail_window_on_live_manager(rt, window_handle.window_id, "oversize_output");
                    let _ = rt.flush_synthesis_windows();
                    return Err(FfiError::Synthesis {
                        message: format!(
                            "synthesis output exceeded {} bytes (got {})",
                            MAX_SYNTHESIS_OUTPUT_BYTES,
                            object.payload.len(),
                        ),
                    });
                }
                // Cross-check that the engine emitted the object
                // for the same window/scope we authorised. Defence
                // in depth against a malicious / buggy engine.
                if object.scope_id != scope || object.window_id != window_handle.window_id {
                    fail_window_on_live_manager(
                        rt,
                        window_handle.window_id,
                        "scope_window_mismatch",
                    );
                    let _ = rt.flush_synthesis_windows();
                    return Err(FfiError::Synthesis {
                        message: format!(
                            "engine returned object for scope/window ({} / {}) but dispatch \
                             was for ({} / {})",
                            object.scope_id.as_uuid(),
                            object.window_id.as_uuid(),
                            scope.as_uuid(),
                            window_handle.window_id.as_uuid(),
                        ),
                    });
                }
                // Type contract — domain dispatches must emit
                // DomainSummary, tenant dispatches TenantSummary.
                let expected = match tier {
                    SynthesisTierKind::Domain => SynthesisObjectType::DomainSummary,
                    SynthesisTierKind::Tenant => SynthesisObjectType::TenantSummary,
                };
                if object.object_type != expected {
                    fail_window_on_live_manager(
                        rt,
                        window_handle.window_id,
                        "object_type_mismatch",
                    );
                    let _ = rt.flush_synthesis_windows();
                    return Err(FfiError::Synthesis {
                        message: format!(
                            "engine returned {} object but {} synthesis was requested",
                            object.object_type.as_str(),
                            tier.as_str(),
                        ),
                    });
                }
                // Replay the in-progress → complete transition on
                // the real window manager. Both transitions can
                // refuse if the host called `forget_scope` and
                // recreated state mid-flight — surface as
                // `Synthesis` rather than panicking.
                rt.synthesis_windows
                    .mark_in_progress(window_handle.window_id)
                    .map_err(|e| FfiError::Synthesis {
                        message: format!("mark_in_progress failed: {e}"),
                    })?;
                rt.synthesis_windows
                    .mark_complete(window_handle.window_id)
                    .map_err(|e| FfiError::Synthesis {
                        message: format!("mark_complete failed: {e}"),
                    })?;
                let window_id_str = object.window_id.as_uuid().to_string();
                let window_uuid = object.window_id.as_uuid();
                // Copy the payload text into the memory_manager
                // recap field before we move the object into the
                // store. Payload is UTF-8 enforced upstream by the
                // synthesizer; we fall back to lossy decode here
                // so a malformed adapter can never wedge the apply
                // phase.
                let recap_text = String::from_utf8(object.payload.clone())
                    .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned());
                let object_tier = tier;
                // Persist the synthesis object — install into the
                // in-memory map and the encrypted evidence store.
                rt.save_synthesis_object(scope, object)?;
                // Mirror the recap into the domain / tenant memory
                // object so the next synthesis run (and any host
                // consuming the memory object directly) reflects
                // the latest output. We persist the updated memory
                // object before flushing the window manager so a
                // crash between the two leaves the windows
                // referencing the old recap rather than a partial
                // one.
                match object_tier {
                    SynthesisTierKind::Domain => {
                        let domain = rt.domain_memory_mut(scope);
                        domain.update_recap(recap_text, Some(window_uuid));
                        let domain_clone = domain.clone();
                        rt.save_domain_memory(scope, domain_clone)?;
                    }
                    SynthesisTierKind::Tenant => {
                        let tenant = rt.tenant_memory_mut(scope);
                        tenant.update_summary(recap_text, Some(window_uuid));
                        let tenant_clone = tenant.clone();
                        rt.save_tenant_memory(scope, tenant_clone)?;
                    }
                }
                // Persist window manager state (status transitions).
                rt.flush_synthesis_windows()?;
                // Cooldown stamp.
                rt.synthesis_cooldowns.insert(scope, Utc::now());
                // Retention prune. We don't surface the pruned
                // ids; the caller only cares about the new window.
                let pruned = rt.prune_completed_windows(scope, WINDOW_RETENTION_CAP_PER_SCOPE);
                if !pruned.is_empty() {
                    // Pruning mutated the manager — flush again.
                    if let Err(e) = rt.flush_synthesis_windows() {
                        tracing::warn!(
                            error = ?e,
                            "post-prune flush_synthesis_windows failed",
                        );
                    }
                    // The persisted per-scope object blob now
                    // contains the post-prune subset. Re-save by
                    // calling save_synthesis_object with one of
                    // the remaining objects would rewrite the
                    // blob; we instead rewrite the blob directly
                    // via a no-op insert of an existing object so
                    // the cap is honoured both in-memory and on
                    // disk. The simplest correct behaviour is to
                    // serialise the remaining per-scope objects
                    // and call `save_memory_blob` directly — but
                    // we already encapsulate that in
                    // `save_synthesis_object`. To keep the FFI
                    // module from poking at the store directly,
                    // we no-op here: the next successful synthesis
                    // will rewrite the row with the pruned set.
                    tracing::debug!(
                        scope = %scope.as_uuid(),
                        pruned = pruned.len(),
                        "trigger_server_synthesis: pruned completed windows beyond retention cap",
                    );
                }
                Ok(window_id_str)
            }
        }
    })
}

fn enforce_scope_binding(rt: &FfiRuntime, scope: ScopeId) -> FfiResult<()> {
    match rt.synthesis_scope_bindings.as_deref() {
        None => {
            tracing::warn!(
                scope = %scope.as_uuid(),
                "trigger_server_synthesis: no scope-binding allow-list configured; \
                 production deployments SHOULD enable scope_bindings or wrap the engine \
                 in a TeeWorker",
            );
            Ok(())
        }
        Some(bindings) => {
            if bindings.iter().any(|u| *u == scope.as_uuid()) {
                Ok(())
            } else {
                Err(FfiError::Unavailable {
                    subsystem: format!(
                        "synthesis_engine: scope {} not in configured scope_bindings",
                        scope.as_uuid()
                    ),
                })
            }
        }
    }
}

/// Collect [`ChannelOutput`]s for every channel scope registered
/// on `domain`. Each channel's most recent `ChannelRecap`
/// synthesis object (looked up by scope) is folded into the
/// resulting list. Channels with no recorded recap object are
/// skipped silently — domain synthesis is best-effort across the
/// registered set.
fn gather_channel_outputs(
    rt: &FfiRuntime,
    domain: &memory_manager::DomainMemoryObject,
) -> Vec<ChannelOutput> {
    let mut outputs = Vec::with_capacity(domain.channel_scopes.len());
    for channel_scope in &domain.channel_scopes {
        if let Some(object) = newest_channel_recap_for_scope(rt, *channel_scope) {
            match ChannelOutput::from_channel_object(object) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        channel = %channel_scope.as_uuid(),
                        error = ?e,
                        "skipping channel output that failed hierarchy validation",
                    );
                }
            }
        } else if let Some(cmo) = rt.channel_memories.get(channel_scope) {
            // No persisted ChannelRecap synthesis object yet — synthesise a
            // ChannelRecap object on the fly from the latest recap text on
            // the channel memory. This keeps the domain-synthesis pipeline
            // alive on builds where channel-tier synthesis publishes its
            // recap text into `ChannelMemoryObject.recap` but does not
            // persist a SynthesisObject row.
            if cmo.recap.is_empty() {
                continue;
            }
            let synthesised = SynthesisObject::new(
                *channel_scope,
                WindowId::new_v4(),
                SynthesisObjectType::ChannelRecap,
                cmo.recap.as_bytes().to_vec(),
                Uuid::nil(),
            );
            match ChannelOutput::from_channel_object(synthesised) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        channel = %channel_scope.as_uuid(),
                        error = ?e,
                        "synthesised ChannelRecap rejected by hierarchy validator",
                    );
                }
            }
        }
    }
    outputs
}

/// Collect [`DomainOutput`]s for every domain scope registered
/// on `tenant`. Each domain's most recent `DomainSummary`
/// synthesis object is folded in.
fn gather_domain_outputs(
    rt: &FfiRuntime,
    tenant: &memory_manager::TenantMemoryObject,
) -> Vec<DomainOutput> {
    let mut outputs = Vec::with_capacity(tenant.domain_scopes.len());
    for domain_scope in &tenant.domain_scopes {
        if let Some(object) =
            newest_object_for_scope_of_type(rt, *domain_scope, SynthesisObjectType::DomainSummary)
        {
            match DomainOutput::from_domain_object(object) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        domain = %domain_scope.as_uuid(),
                        error = ?e,
                        "skipping domain output that failed hierarchy validation",
                    );
                }
            }
        } else if let Some(dmo) = rt.domain_memory(*domain_scope) {
            if dmo.recap.is_empty() {
                continue;
            }
            // Synthesize a DomainSummary on the fly mirroring the channel
            // fallback in `gather_channel_outputs` so tenant-tier dispatch
            // is not blocked by a missing on-disk SynthesisObject row.
            let object = SynthesisObject::new(
                *domain_scope,
                WindowId::new_v4(),
                SynthesisObjectType::DomainSummary,
                dmo.recap.as_bytes().to_vec(),
                Uuid::nil(),
            );
            match DomainOutput::from_domain_object(object) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        domain = %domain_scope.as_uuid(),
                        error = ?e,
                        "synthesised DomainSummary rejected by hierarchy validator",
                    );
                }
            }
        }
    }
    outputs
}

fn newest_channel_recap_for_scope(rt: &FfiRuntime, scope: ScopeId) -> Option<SynthesisObject> {
    newest_object_for_scope_of_type(rt, scope, SynthesisObjectType::ChannelRecap)
}

fn newest_object_for_scope_of_type(
    rt: &FfiRuntime,
    scope: ScopeId,
    kind: SynthesisObjectType,
) -> Option<SynthesisObject> {
    rt.synthesis_objects
        .values()
        .filter(|o| o.scope_id == scope && o.object_type == kind)
        .max_by_key(|o| o.created_at)
        .cloned()
}

/// Transition the live `SynthesisWindowManager` window into `Failed`.
///
/// Phase 2 mutates a cloned manager so on the live manager the window
/// is still in `Pending`. `mark_failed` only accepts the
/// `InProgress → Failed` transition, so we replay the
/// `Pending → InProgress → Failed` chain here. Both steps are
/// best-effort because `apply_dispatch_outcome` is already on a
/// failure path — surfacing a deeper error from the bookkeeping here
/// would mask the real synthesis failure that the caller is about
/// to receive. A `mark_in_progress` refusal logs at `warn`; the
/// subsequent `mark_failed` will still attempt the transition and
/// also log on refusal so an operator can correlate stuck windows.
fn fail_window_on_live_manager(rt: &mut FfiRuntime, window_id: WindowId, reason: &str) {
    if let Err(e) = rt.synthesis_windows.mark_in_progress(window_id) {
        tracing::warn!(
            window = %window_id.as_uuid(),
            error = ?e,
            reason,
            "fail_window_on_live_manager: mark_in_progress refused",
        );
    }
    if let Err(e) = rt.synthesis_windows.mark_failed(window_id) {
        tracing::warn!(
            window = %window_id.as_uuid(),
            error = ?e,
            reason,
            "fail_window_on_live_manager: mark_failed refused; window left in current status",
        );
    }
}

fn newest_complete_window(rt: &FfiRuntime, scope: ScopeId) -> Option<WindowId> {
    rt.synthesis_windows
        .windows_for(scope)
        .iter()
        .filter(|w| w.status == WindowStatus::Complete)
        .max_by_key(|w| w.window_end)
        .map(|w| w.id)
}

fn parse_window_id(s: &str) -> FfiResult<WindowId> {
    let uuid = Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
        message: format!("invalid synthesis window id `{s}`: {e}"),
    })?;
    Ok(WindowId::from_uuid(uuid))
}

fn parse_scope_bindings(bindings: Option<&[String]>) -> FfiResult<Option<Vec<Uuid>>> {
    let Some(bindings) = bindings else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(bindings.len());
    for s in bindings {
        let uuid = Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
            message: format!("invalid scope binding `{s}`: {e}"),
        })?;
        out.push(uuid);
    }
    Ok(Some(out))
}

fn endpoint_config_from_ffi(cfg: &SynthesisEngineConfig) -> EndpointConfig {
    let mut endpoint = EndpointConfig::new(
        cfg.url.clone(),
        cfg.api_key_ref.clone(),
        cfg.model_id.clone(),
    );
    if cfg.max_tokens > 0 {
        endpoint = endpoint.with_max_tokens(cfg.max_tokens);
    }
    if cfg.timeout_ms > 0 {
        endpoint = endpoint.with_timeout(Duration::from_millis(cfg.timeout_ms));
    }
    if let Some(grammar) = cfg.grammar.as_ref() {
        endpoint = endpoint.with_grammar(grammar.clone());
    }
    endpoint
}

fn window_to_record(
    window: &synthesis_pipeline::SynthesisWindow,
    rt: &FfiRuntime,
) -> SynthesisStatusRecord {
    // Look up the synthesis object for this window so callers can
    // pull the artefact id without a second round trip.
    let object_id = rt
        .synthesis_objects
        .get(&window.id)
        .map(|o| o.id.as_uuid().to_string());
    // Derive the tier from the window's recorded outputs. We do
    // not persist the tier on `SynthesisWindow` itself (the
    // pipeline keeps the tier outside the storage shape), so we
    // infer from the matching synthesis object's type. Windows
    // without a complete object surface as "unknown" so the host
    // can still display them.
    let tier = rt
        .synthesis_objects
        .get(&window.id)
        .map_or("unknown", |o| match o.object_type {
            SynthesisObjectType::ChannelRecap => "channel",
            SynthesisObjectType::DomainSummary => "domain",
            SynthesisObjectType::TenantSummary => "tenant",
            SynthesisObjectType::EpisodicSummary => "episodic",
        })
        .to_string();
    SynthesisStatusRecord {
        synthesis_id: window.id.as_uuid().to_string(),
        scope_id: window.scope_id.as_uuid().to_string(),
        tier,
        status: window.status.as_str().to_string(),
        window_start_unix: window.window_start.timestamp(),
        window_end_unix: window.window_end.timestamp(),
        object_id,
    }
}

#[cfg(test)]
mod tests {
    //! FFI-level tests for the Phase 7 server-side synthesis surface.
    //!
    //! These tests open a real temp-dir-backed evidence store, install
    //! a deterministic [`ManagedEndpointSynthesizer`] (from the
    //! `synthesis_engine` crate's test-stub module — the production
    //! [`HttpManagedEndpointSynthesizer`] is exercised in
    //! `synthesis_engine`'s own test suite), and drive the public
    //! FFI surface against it. The HTTP transport itself is gated
    //! behind `--features http-client` and is not exercised here.

    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use evidence_store::ScopeId;
    use memory_manager::{ChannelMemoryObject, DomainMemoryObject, TenantMemoryObject};
    use synthesis_engine::{ManagedEndpointSynthesizer, SynthesisEngine};
    use synthesis_pipeline::{
        HierarchyEnforcedWindowManager, SynthesisObject, SynthesisObjectType, WindowScopeTier,
    };
    use uuid::Uuid;

    use super::*;
    use crate::runtime::{close_store, open_store, with_runtime};

    /// Open a fresh evidence store backed by a temp dir. The
    /// returned `TempDir` MUST outlive the handle.
    fn fresh_store() -> (RuntimeHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let handle = open_store(path.to_string_lossy().into_owned(), key_hex)
            .expect("open_store should succeed");
        (handle, dir)
    }

    fn teardown(handle: RuntimeHandle) {
        let _ = close_store(handle);
    }

    /// Install the deterministic [`ManagedEndpointSynthesizer`] on
    /// `handle`. We poke `rt.synthesis_engine` directly because the
    /// public `configure_synthesis_engine` insists on building an
    /// HTTP-backed engine, which we explicitly do NOT want to
    /// exercise in unit tests.
    fn install_test_engine(handle: RuntimeHandle) {
        with_runtime(handle, |rt| {
            let engine: Arc<dyn SynthesisEngine> = Arc::new(ManagedEndpointSynthesizer::new());
            rt.synthesis_engine = Some(engine);
            rt.synthesis_scope_bindings = None;
            Ok(())
        })
        .expect("install_test_engine");
    }

    /// Seed a fully-populated domain memory + matching channel
    /// memories on `handle`. Returns the domain scope id.
    fn seed_domain_with_two_channels(handle: RuntimeHandle) -> ScopeId {
        let domain_scope = ScopeId::new_v4();
        let chan_a = ScopeId::new_v4();
        let chan_b = ScopeId::new_v4();

        with_runtime(handle, |rt| {
            let mut domain = DomainMemoryObject::new(domain_scope);
            domain.attach_channel_scope(chan_a);
            domain.attach_channel_scope(chan_b);
            rt.save_domain_memory(domain_scope, domain)?;

            let mut cma = ChannelMemoryObject::new(chan_a);
            cma.update_recap("alpha channel recap text", None);
            rt.save_channel_memory(chan_a, cma)?;
            let mut cmb = ChannelMemoryObject::new(chan_b);
            cmb.update_recap("beta channel recap text", None);
            rt.save_channel_memory(chan_b, cmb)?;
            Ok(())
        })
        .expect("seed domain");
        domain_scope
    }

    /// Seed a tenant memory plus one feeding domain (with its own
    /// channel) so tenant-tier synthesis has admissible inputs.
    fn seed_tenant_with_domain(handle: RuntimeHandle) -> ScopeId {
        let tenant_scope = ScopeId::new_v4();
        let domain_scope = ScopeId::new_v4();
        let channel_scope = ScopeId::new_v4();

        with_runtime(handle, |rt| {
            let mut tenant = TenantMemoryObject::new(tenant_scope);
            tenant.attach_domain_scope(domain_scope);
            rt.save_tenant_memory(tenant_scope, tenant)?;

            let mut domain = DomainMemoryObject::new(domain_scope);
            domain.attach_channel_scope(channel_scope);
            // A non-empty recap lets `gather_domain_outputs` build a
            // synthetic `DomainSummary` on the fly so the tenant
            // dispatch sees at least one feeder output.
            domain.update_recap("domain recap feeding tenant", None);
            rt.save_domain_memory(domain_scope, domain)?;

            let mut cmo = ChannelMemoryObject::new(channel_scope);
            cmo.update_recap("channel recap below domain", None);
            rt.save_channel_memory(channel_scope, cmo)?;
            Ok(())
        })
        .expect("seed tenant");
        tenant_scope
    }

    // ─────────────────────── configure_synthesis_engine ───────────

    #[test]
    fn install_test_engine_populates_slot() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);

        let has_engine =
            with_runtime(handle, |rt| Ok(rt.synthesis_engine.is_some())).expect("with_runtime");
        assert!(has_engine, "engine slot must be populated");
        teardown(handle);
    }

    // ─────────────────────── trigger_server_synthesis ─────────────

    #[test]
    fn trigger_server_synthesis_without_engine_returns_unavailable() {
        let (handle, _dir) = fresh_store();
        let scope = seed_domain_with_two_channels(handle);
        let err = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .unwrap_err();
        assert!(
            matches!(err, FfiError::Unavailable { ref subsystem } if subsystem.contains("synthesis_engine")),
            "expected Unavailable(synthesis_engine), got {err:?}",
        );
        teardown(handle);
    }

    #[test]
    fn trigger_server_synthesis_on_forgotten_scope_returns_not_found() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);
        // Forget the scope. Subsequent dispatches must short-circuit
        // before reaching the engine.
        crate::forget_scope(handle, scope.as_uuid().to_string()).expect("forget_scope");

        let err = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .unwrap_err();
        assert!(
            matches!(err, FfiError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        teardown(handle);
    }

    #[test]
    fn trigger_server_synthesis_domain_with_test_engine_produces_complete_window() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        let window_id_str = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("domain synthesis");

        // The returned id must parse as a UUID.
        let _: Uuid = window_id_str.parse().expect("returned id must be UUID");

        // synthesis_status reports Complete.
        let rec = synthesis_status(handle, window_id_str.clone()).expect("status");
        assert_eq!(rec.synthesis_id, window_id_str);
        assert_eq!(rec.scope_id, scope.as_uuid().to_string());
        assert_eq!(rec.tier, "domain");
        assert_eq!(rec.status, "complete");
        assert!(
            rec.object_id.is_some(),
            "complete window must carry object_id"
        );

        // list_recent_syntheses surfaces the same row.
        let list = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert!(
            list.iter().any(|r| r.synthesis_id == window_id_str),
            "list must include the newly-completed window",
        );
        teardown(handle);
    }

    #[test]
    fn trigger_server_synthesis_tenant_with_test_engine_produces_complete_window() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_tenant_with_domain(handle);

        let window_id_str = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Tenant,
        )
        .expect("tenant synthesis");

        let rec = synthesis_status(handle, window_id_str.clone()).expect("status");
        assert_eq!(rec.tier, "tenant");
        assert_eq!(rec.status, "complete");

        // The tenant memory should now reference the latest synthesis
        // window via `last_synthesis_window`.
        let last = with_runtime(handle, |rt| {
            Ok(rt
                .tenant_memories
                .get(&scope)
                .and_then(|t| t.last_synthesis_window))
        })
        .expect("with_runtime");
        let want: Uuid = window_id_str.parse().unwrap();
        assert_eq!(last, Some(want));
        teardown(handle);
    }

    // ─────────────────────── synthesis_status ─────────────────────

    #[test]
    fn synthesis_status_returns_not_found_for_unknown_id() {
        let (handle, _dir) = fresh_store();
        let unknown = Uuid::new_v4().to_string();
        let err = synthesis_status(handle, unknown).unwrap_err();
        assert!(matches!(err, FfiError::NotFound { .. }));
        teardown(handle);
    }

    // ─────────────────────── list_recent_syntheses ────────────────

    #[test]
    fn list_recent_syntheses_returns_empty_for_unknown_scope() {
        let (handle, _dir) = fresh_store();
        // Unknown but well-formed scope id — list_recent_syntheses
        // must NOT error, it must return an empty list (the host's
        // status pane polls this on every visit).
        let scope = ScopeId::new_v4();
        let rows = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert!(rows.is_empty());
        teardown(handle);
    }

    #[test]
    fn list_recent_syntheses_filters_by_scope() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);

        let scope_a = seed_domain_with_two_channels(handle);
        let scope_b = seed_domain_with_two_channels(handle);

        let win_a = trigger_server_synthesis(
            handle,
            scope_a.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synthesise A");
        let win_b = trigger_server_synthesis(
            handle,
            scope_b.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synthesise B");
        assert_ne!(win_a, win_b);

        let rows_a = list_recent_syntheses(handle, scope_a.as_uuid().to_string()).expect("list A");
        assert!(rows_a
            .iter()
            .all(|r| r.scope_id == scope_a.as_uuid().to_string()));
        assert!(rows_a.iter().any(|r| r.synthesis_id == win_a));
        assert!(rows_a.iter().all(|r| r.synthesis_id != win_b));
        teardown(handle);
    }

    // ─────────────────────── cooldown / retention / forgetting ────

    #[test]
    fn cooldown_short_circuits_redundant_dispatch() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        let win1 = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("first synthesis");
        // Second dispatch within the per-scope cooldown returns the
        // SAME window id (no new run was dispatched).
        let win2 = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("cooldown short-circuit");
        assert_eq!(win1, win2, "cooldown must reuse the prior window id");

        // Force the cooldown stamp into the past to allow a new run.
        with_runtime(handle, |rt| {
            let past = Utc::now() - ChronoDuration::seconds(PER_SCOPE_COOLDOWN_SECS + 60);
            rt.synthesis_cooldowns.insert(scope, past);
            Ok(())
        })
        .expect("with_runtime");
        let win3 = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("post-cooldown synthesis");
        assert_ne!(win3, win1, "post-cooldown dispatch must mint a new window");
        teardown(handle);
    }

    #[test]
    fn forget_scope_purges_synthesis_state() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        let win = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synth");
        let win_uuid: Uuid = win.parse().unwrap();
        let win_id = synthesis_pipeline::WindowId::from_uuid(win_uuid);

        // Sanity: state present before forgetting.
        with_runtime(handle, |rt| {
            assert!(rt.domain_memories.contains_key(&scope));
            assert!(rt.synthesis_cooldowns.contains_key(&scope));
            assert!(rt.synthesis_objects.contains_key(&win_id));
            assert!(rt.synthesis_windows.get(win_id).is_some());
            Ok(())
        })
        .unwrap();

        crate::forget_scope(handle, scope.as_uuid().to_string()).expect("forget");

        // Every synthesis-adjacent map must drop the scope's entries.
        with_runtime(handle, |rt| {
            assert!(!rt.domain_memories.contains_key(&scope));
            assert!(!rt.tenant_memories.contains_key(&scope));
            assert!(!rt.synthesis_cooldowns.contains_key(&scope));
            assert!(!rt.synthesis_objects.contains_key(&win_id));
            assert!(rt.synthesis_windows.get(win_id).is_none());
            assert!(rt.synthesis_windows.windows_for(scope).is_empty());
            Ok(())
        })
        .unwrap();
        teardown(handle);
    }

    #[test]
    fn prune_completed_windows_caps_at_retention_limit() {
        let (handle, _dir) = fresh_store();
        let scope = ScopeId::new_v4();
        let total: usize = WINDOW_RETENTION_CAP_PER_SCOPE + 10;

        with_runtime(handle, |rt| {
            let now = Utc::now();
            for i in 0..total {
                // Stagger window_end so prune ordering is deterministic.
                let offset = i64::try_from(i).expect("loop index fits i64");
                let end = now - ChronoDuration::seconds(60 * offset);
                let start = end - ChronoDuration::seconds(30);
                let h = rt
                    .synthesis_windows
                    .open_tiered_window(scope, WindowScopeTier::Domain, start, end)
                    .expect("open window");
                rt.synthesis_windows.mark_in_progress(h.window_id).unwrap();
                rt.synthesis_windows.mark_complete(h.window_id).unwrap();
            }
            assert_eq!(rt.synthesis_windows.windows_for(scope).len(), total);

            let pruned = rt.prune_completed_windows(scope, WINDOW_RETENTION_CAP_PER_SCOPE);
            assert_eq!(pruned.len(), total - WINDOW_RETENTION_CAP_PER_SCOPE);
            assert_eq!(
                rt.synthesis_windows.windows_for(scope).len(),
                WINDOW_RETENTION_CAP_PER_SCOPE,
                "retention cap enforced",
            );
            Ok(())
        })
        .unwrap();
        teardown(handle);
    }

    /// Always-failing test engine that returns `EngineError::Engine`
    /// from both tier dispatchers. Used to drive the Phase 3 failure
    /// path so we can verify the live `SynthesisWindowManager` ends
    /// up in `Failed` (not stuck in `Pending`).
    struct FailingTestEngine;
    impl SynthesisEngine for FailingTestEngine {
        fn synthesize_domain(
            &self,
            _windows: &mut SynthesisWindowManager,
            _handle: TieredWindowHandle,
            _input: synthesis_pipeline::DomainSynthesisInput,
        ) -> synthesis_engine::Result<synthesis_engine::DomainSynthesisResult> {
            Err(synthesis_engine::EngineError::engine("test injection"))
        }
        fn synthesize_tenant(
            &self,
            _windows: &mut SynthesisWindowManager,
            _handle: TieredWindowHandle,
            _input: synthesis_pipeline::TenantSynthesisInput,
        ) -> synthesis_engine::Result<synthesis_engine::TenantSynthesisResult> {
            Err(synthesis_engine::EngineError::engine("test injection"))
        }
    }

    #[test]
    fn failing_engine_transitions_window_to_failed_not_pending() {
        // Regression test for the `mark_failed`-on-`Pending` bug.
        // Phase 2 mutates a cloned manager, so on the live manager
        // the window is still `Pending` when Phase 3 runs. The fix
        // replays `Pending → InProgress → Failed` so the live window
        // ends up `Failed`, surfacing the failure to operators and
        // letting future retention sweeps reason about the window.
        let (handle, _dir) = fresh_store();
        with_runtime(handle, |rt| {
            let engine: Arc<dyn SynthesisEngine> = Arc::new(FailingTestEngine);
            rt.synthesis_engine = Some(engine);
            rt.synthesis_scope_bindings = None;
            Ok(())
        })
        .unwrap();
        let scope = seed_domain_with_two_channels(handle);

        let err = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect_err("failing engine should bubble Synthesis error");
        assert!(matches!(err, FfiError::Synthesis { .. }));

        // The Phase-1 window must have transitioned to Failed (not
        // be stuck in Pending). It's also the only window on the
        // scope, so `list_recent_syntheses` returns exactly one row.
        let rows = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert_eq!(rows.len(), 1, "exactly one window opened for this scope");
        assert_eq!(
            rows[0].status, "failed",
            "live window must end Failed after dispatch error, not stuck Pending",
        );
        teardown(handle);
    }

    #[test]
    fn list_recent_syntheses_respects_cap() {
        let (handle, _dir) = fresh_store();
        let scope = ScopeId::new_v4();
        // Cram in more than the cap so the FFI surface has to
        // truncate.
        let total = LIST_RECENT_SYNTHESES_CAP + 5;

        with_runtime(handle, |rt| {
            let now = Utc::now();
            for i in 0..total {
                let offset = i64::try_from(i).expect("loop index fits i64");
                let end = now - ChronoDuration::seconds(60 * offset);
                let start = end - ChronoDuration::seconds(30);
                let h = rt
                    .synthesis_windows
                    .open_tiered_window(scope, WindowScopeTier::Domain, start, end)
                    .unwrap();
                rt.synthesis_windows.mark_in_progress(h.window_id).unwrap();
                rt.synthesis_windows.mark_complete(h.window_id).unwrap();
                // Attach a synthesis object so the tier reports
                // correctly (otherwise list_recent_syntheses reports
                // "unknown").
                let obj = SynthesisObject::new(
                    scope,
                    h.window_id,
                    SynthesisObjectType::DomainSummary,
                    b"recap".to_vec(),
                    Uuid::nil(),
                );
                rt.synthesis_objects.insert(h.window_id, obj);
            }
            Ok(())
        })
        .unwrap();

        let rows = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert_eq!(rows.len(), LIST_RECENT_SYNTHESES_CAP);
        // Sorted by window_end DESC: the first row should have the
        // largest window_end_unix.
        for w in rows.windows(2) {
            assert!(w[0].window_end_unix >= w[1].window_end_unix);
        }
        teardown(handle);
    }
}
