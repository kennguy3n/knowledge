//! Connector management FFI surface.
//!
//! Per `docs/DESIGN.md` §10.2 and `ARCHITECTURE.md` §4.1, the
//! substrate ingests evidence from external systems through the
//! [`connector_framework::Connector`] trait. Each connector is
//! a real HTTP client (Google Drive REST v3, Notion API,
//! Slack Web API, …) wired through
//! [`connector_framework::BlockingHttpTransport`] (reqwest blocking
//! client with retry-with-backoff + Retry-After honouring) and
//! [`connector_framework::OAuth2Client`] for `authorization_code`
//! exchange.
//!
//! This module exposes five FFI functions that mirror the connector
//! lifecycle:
//!
//! 1. [`create_connector`] — instantiate a connector for one source
//!    kind, binding it to a scope.
//! 2. [`authenticate_connector`] — run the OAuth2
//!    `authorization_code` exchange and stash the bearer token in
//!    the per-runtime [`OAuth2TokenVault`].
//! 3. [`sync_connector`] — run `initial_sync` or `incremental_sync`
//!    (chosen by [`SyncState::can_run_incremental`]), forward every
//!    emitted [`ConnectorEvent`] into the evidence store via
//!    [`EvidenceStore::ingest`], and advance the per-connector
//!    [`SyncState`].
//! 4. [`list_connectors`] — read the in-memory connector registry
//!    and surface a wire-flat [`ConnectorStatus`] row per instance.
//! 5. [`remove_connector`] — tear down a connector (drop the
//!    `Box<dyn Connector>`, drop the cached token, drop the
//!    `ConnectorInstance` row).
//!
//! Status:
//!
//! * **`http-client` feature ON (production builds):** the factory
//!   wires every connector to a real
//!   [`BlockingHttpTransport`] + [`OAuth2Client`] pair, so every
//!   call crosses the wire.
//! * **`http-client` feature OFF (offline / cross-compile lints):**
//!   the factory returns
//!   [`FfiError::Unavailable { subsystem: "connector-http-client" }`]
//!   because no HTTP transport is linked in. The rest of the
//!   substrate (evidence store, memory manager, classification) still
//!   compiles, so a host can ship a build that exposes the connector
//!   FFI surface without enabling the feature — calls will simply
//!   surface `Unavailable` to the host, which is the same recovery
//!   path as "subsystem not initialised".
//!
//! All five functions require a prior successful call to
//! [`crate::open_store`] (enforced by [`with_runtime`]) and operate
//! synchronously against the per-handle `Arc<Mutex<FfiRuntime>>`
//! mutex — connector calls against the same handle serialise, while
//! calls against different handles run in parallel.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorEvent, ConnectorInstance, ConnectorInstanceId,
    ConnectorKind, SyncMode, SyncState, SyncStatus,
};
use evidence_store::{ImportanceClass, ScopeId};
use uuid::Uuid;

use crate::error::{FfiError, FfiResult};
use crate::metrics;
use crate::parse_scope_id;
use crate::runtime::{with_runtime, FfiRuntime, RuntimeHandle};
use crate::types::{
    ConnectorKindTag, ConnectorStatus, ScopeIdString, SyncModeKind, SyncReport, SyncStatusKind,
};

// ──────────────────────────── FFI surface ────────────────────────────

/// Instantiate a connector for `kind`, bound to `scope_id`, with
/// `config_json` as the connector's
/// [`ConnectorConfig::auth_config_json`] payload (provider-specific:
/// `client_id`, `redirect_uri`, `token_url`, OAuth scopes, etc.).
///
/// Returns the freshly-allocated UUID of the new
/// [`ConnectorInstance`] — the host should hand this back to
/// [`authenticate_connector`], [`sync_connector`], or
/// [`remove_connector`] in subsequent calls.
///
/// The connector is registered with [`AuthKind::OAuth2`] — the only
/// auth strategy currently surfaced through the FFI. API-key and
/// webhook-only connectors are constructed in tests via the
/// underlying [`ConnectorConfig`] but not yet exposed here; the host
/// can extend the surface (e.g. a `create_connector_with_auth` variant)
/// once a real call site materialises.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `scope_id` has been cryptographically
///   forgotten via [`crate::forget_scope`].
/// * [`FfiError::Connector`] if `config_json` is not valid JSON.
/// * [`FfiError::Unavailable { subsystem: "connector-http-client" }`]
///   if the build was compiled without the `http-client` feature
///   (no real reqwest transport is linked in).
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn create_connector(
    handle: RuntimeHandle,
    kind: ConnectorKindTag,
    scope_id: ScopeIdString,
    config_json: String,
) -> FfiResult<String> {
    metrics::instrument(metrics::inc_create_connector, || {
        let scope = parse_scope_id(&scope_id)?;
        let auth_config_json: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|e| FfiError::Connector {
                message: format!("invalid auth config JSON: {e}"),
            })?;
        let kind_framework = connector_kind_to_framework(kind);
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope_id.clone(),
                });
            }
            let instance_id = ConnectorInstanceId::new_v4();
            let connector = build_connector(rt, kind_framework, instance_id)?;
            let mut config = ConnectorConfig::new(kind_framework, AuthKind::OAuth2, scope);
            config.auth_config_json = auth_config_json;
            let instance = ConnectorInstance {
                id: instance_id,
                config,
                sync_state: SyncState::new(instance_id),
            };
            rt.connector_instances.insert(instance_id, instance);
            rt.connectors.insert(instance_id, connector);
            Ok(instance_id.0.to_string())
        })
    })
}

/// Run the OAuth2 `authorization_code` exchange for `instance_id`
/// against the provider's token endpoint, storing the resulting
/// access / refresh token bundle in the per-runtime
/// [`OAuth2TokenVault`]. The connector's
/// [`ConnectorConfig::auth_config_json`] must carry `client_id`,
/// `redirect_uri`, and `token_url`; missing fields surface as
/// [`FfiError::Connector`].
///
/// `auth_code` is the opaque value the host received from the
/// provider's authorization redirect (e.g. the `code=` query
/// parameter on `https://app/oauth/callback`). The substrate never
/// persists it — it is consumed once during the exchange and dropped.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::NotFound`] if `instance_id` does not match any
///   connector instance registered with this runtime.
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid UUID.
/// * [`FfiError::Connector`] if the OAuth2 exchange fails (provider
///   rejected the code, malformed `auth_config_json`, transport
///   failure, malformed token response).
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn authenticate_connector(
    handle: RuntimeHandle,
    instance_id: String,
    auth_code: String,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_authenticate_connector, || {
        let instance = parse_instance_id(&instance_id)?;
        // ─────────────── Phase 1: snapshot (locked) ───────────────
        //
        // Clone the `Arc<dyn Connector>` handle + a fresh
        // `ConnectorConfig` out of the runtime, then drop the
        // mutex. Mirrors the locking discipline documented on
        // `synthesize_scope` in `crates/ffi/src/lib.rs`:
        // long-latency I/O (here the OAuth2 token endpoint
        // round-trip) MUST NOT hold the per-handle mutex, otherwise
        // every other FFI call on the same handle (`query`,
        // `ingest_message`, `health_check`, …) blocks for the
        // duration of the provider's network round-trip.
        let (connector, mut config_with_code) = with_runtime(handle, |rt| {
            let connector = rt
                .connectors
                .get(&instance)
                .ok_or_else(|| FfiError::NotFound {
                    kind: "connector".into(),
                    id: instance_id.clone(),
                })?
                .clone(); // `Arc::clone` — one atomic increment.
            let config_clone = rt
                .connector_instances
                .get(&instance)
                .ok_or_else(|| FfiError::NotFound {
                    kind: "connector".into(),
                    id: instance_id.clone(),
                })?
                .config
                .clone();
            Ok((connector, config_clone))
        })?;
        // ─────────── Phase 2: OAuth2 exchange (UNLOCKED) ────────────
        //
        // The connector's own `authenticate` impl drives the
        // OAuth2 exchange through its bundled
        // `Arc<dyn OAuth2CodeExchange>` — by the time this returns
        // the bearer token is fully validated against the
        // provider's token endpoint. The runtime mutex is NOT held
        // here so concurrent FFI calls against the same handle
        // run in parallel with the network round-trip.
        //
        // Every concrete connector's `authenticate` impl reads the
        // OAuth2 authorisation code from
        // `config.auth_config_json.authorization_code` (search
        // `crates/connectors/src/*.rs` for `"authorization_code"` —
        // Slack, Notion, HubSpot, Email, OneDrive, Confluence,
        // Jira, Figma, and Google Drive all hard-code this key in
        // their `ConnectorError::Auth` diagnostic strings). The FFI
        // splice MUST use the same key — otherwise every host
        // `authenticate_connector` call would surface
        // `auth_config_json.authorization_code is required` even
        // when the host correctly passed an `auth_code` argument.
        if !config_with_code.auth_config_json.is_object() {
            config_with_code.auth_config_json = serde_json::Map::new().into();
        }
        if let Some(obj) = config_with_code.auth_config_json.as_object_mut() {
            // `auth_code` is owned and never read after this point — move
            // it into the JSON value instead of cloning. One fewer heap
            // copy per authenticate call.
            obj.insert(
                "authorization_code".to_string(),
                serde_json::Value::String(auth_code),
            );
        }
        let token = connector.authenticate(&config_with_code)?;
        // ──────────────── Phase 3: persist (locked) ────────────────
        //
        // Re-acquire the mutex. We re-check the instance is still
        // registered because another thread could have called
        // `remove_connector` while the mutex was released — racing
        // the token put against a removed instance would resurrect
        // dropped state. If the instance is gone we drop the token
        // on the floor (the provider's OAuth2 token will expire on
        // its own schedule) and surface `NotFound` so the host
        // sees a clean diagnostic.
        with_runtime(handle, |rt| {
            if !rt.connector_instances.contains_key(&instance) {
                return Err(FfiError::NotFound {
                    kind: "connector".into(),
                    id: instance_id.clone(),
                });
            }
            rt.token_vault.put(instance, token);
            Ok(())
        })
    })
}

/// Run a sync against the source system. If the connector's
/// [`SyncState`] reports `can_run_incremental() == true` (a previous
/// successful sync produced a cursor), the runtime dispatches
/// [`Connector::incremental_sync`]; otherwise it falls back to
/// [`Connector::initial_sync`].
///
/// Each emitted [`ConnectorEvent`] is folded into the encrypted
/// evidence store via [`EvidenceStore::ingest`] with the connector's
/// [`ConnectorConfig::scope_id`] and a source tag derived from
/// the [`ConnectorKind`]. The returned [`SyncReport`] carries:
///
/// * `events_total` — every event emitted by the run (including
///   non-ingested deletes / permission changes).
/// * `events_ingested` — the subset that produced fresh evidence
///   rows.
/// * `ingested_evidence_ids` — UUIDs of the freshly-created evidence
///   rows, in emission order.
/// * `next_cursor` — the cursor the connector returned for the next
///   incremental run.
/// * `started_at` / `completed_at` — wall-clock seconds (Unix epoch).
///
/// On success the runtime advances the connector's [`SyncState`] to
/// [`SyncMode::Incremental`] with the new cursor and stamps
/// `last_synced_at`. On failure the state is marked
/// [`SyncStatus::Failed`] with the diagnostic message.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::NotFound`] if `instance_id` is unknown or the
///   connector has not been [`authenticate_connector`]-ed (no token
///   in the vault).
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid UUID.
/// * [`FfiError::Connector`] if the connector's sync method fails
///   (transport, provider rate limit, malformed payload, …). The
///   per-connector `SyncState` is also marked `Failed` so subsequent
///   `list_connectors` calls surface the diagnostic.
/// * [`FfiError::Evidence`] if an event ingest fails mid-sync. In
///   that case the partial-progress events_ingested count is still
///   accurate for the events that did succeed.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn sync_connector(handle: RuntimeHandle, instance_id: String) -> FfiResult<SyncReport> {
    metrics::instrument(metrics::inc_sync_connector, || {
        let instance = parse_instance_id(&instance_id)?;
        let started_at = Utc::now();
        // ─────────────── Phase 1: snapshot (locked) ───────────────
        //
        // Validate the instance + scope, ensure scope registration,
        // flip `SyncState` to `InProgress`, and snapshot the data
        // the unlocked dispatch needs (`Arc<dyn Connector>` clone,
        // `ConnectorConfig`, `OAuth2Token`, `SyncMode`, source kind
        // + tag). After this closure returns we drop the runtime
        // mutex so the (potentially multi-second) HTTP round-trip
        // does NOT block concurrent `query` / `ingest_message` /
        // `health_check` calls on the same handle. Mirrors the
        // locking discipline documented on `synthesize_scope` in
        // `crates/ffi/src/lib.rs`.
        let snapshot = with_runtime(handle, |rt| -> FfiResult<SyncSnapshot> {
            // Read every immutable field out of the connector
            // instance up front, then drop the immutable borrow
            // before any `&mut rt.…` call. Avoids the disjoint
            // borrow collision against `rt.ensure_scope_registered`
            // and `rt.connector_instances.get_mut` further below.
            let (scope, config, source_kind, mode, sync_state_snapshot) = {
                let inst =
                    rt.connector_instances
                        .get(&instance)
                        .ok_or_else(|| FfiError::NotFound {
                            kind: "connector".into(),
                            id: instance_id.clone(),
                        })?;
                // Reject a second concurrent `sync_connector` against
                // the same instance — the existing in-flight call
                // owns the dispatch (and will Phase-3-ingest its
                // events). Letting both calls proceed in parallel
                // would double-ingest the same provider events into
                // the evidence store (the connector framework's
                // `incremental_sync` is not idempotent against
                // overlapping cursors), so the substrate refuses the
                // race at the call site rather than relying on the
                // host to serialise.
                //
                // Hosts that *want* to abandon a stuck sync can call
                // `remove_connector(instance_id)` followed by
                // `create_connector` to get a fresh registration — at
                // which point the per-instance state machine restarts
                // from `NeverRun`. This is the intentional escape
                // hatch from the conflict path.
                if matches!(inst.sync_state.status, SyncStatus::InProgress) {
                    return Err(FfiError::Connector {
                        message: format!(
                            "sync_connector: another sync is already in progress for connector instance {instance_id} \
                             (last_synced_at={:?}); call remove_connector + create_connector to abandon a stuck sync",
                            inst.sync_state.last_synced_at,
                        ),
                    });
                }
                let mode = if inst.sync_state.can_run_incremental() {
                    SyncMode::Incremental
                } else {
                    SyncMode::Full
                };
                (
                    inst.config.scope_id,
                    inst.config.clone(),
                    inst.config.kind,
                    mode,
                    inst.sync_state.clone(),
                )
            };
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope.as_uuid().to_string(),
                });
            }
            rt.ensure_scope_registered(scope)?;
            let token = rt
                .token_vault
                .get(instance)
                .map_err(FfiError::from)?
                .clone();
            let connector = rt
                .connectors
                .get(&instance)
                .ok_or_else(|| FfiError::NotFound {
                    kind: "connector".into(),
                    id: instance_id.clone(),
                })?
                .clone(); // `Arc::clone` — one atomic increment.
                          // Flip the per-instance state to `InProgress` *inside*
                          // the mutex so concurrent `list_connectors` calls see
                          // the in-flight marker for the duration of the unlocked
                          // dispatch below.
            if let Some(inst) = rt.connector_instances.get_mut(&instance) {
                inst.sync_state.mark_in_progress();
            }
            Ok(SyncSnapshot {
                scope,
                config,
                source_kind,
                mode,
                sync_state_snapshot,
                token,
                connector,
            })
        })?;
        // ──────────────── Phase 2: dispatch (UNLOCKED) ────────────────
        //
        // Drive the connector's HTTP round-trip with the runtime
        // mutex released. Concurrent FFI calls against the same
        // handle (queries, memory reads, sync against a *different*
        // connector instance) run in parallel with this network
        // call. A second `sync_connector` against the **same**
        // instance is rejected in Phase 1 with
        // `FfiError::Connector` after the `SyncStatus::InProgress`
        // check above — the substrate refuses the race at the call
        // site rather than relying on the host to serialise, which
        // means we never double-ingest the same provider events
        // into the evidence store.
        let dispatch_result = if snapshot.mode == SyncMode::Incremental {
            snapshot.connector.incremental_sync(
                &snapshot.config,
                &snapshot.token,
                &snapshot.sync_state_snapshot,
            )
        } else {
            snapshot
                .connector
                .initial_sync(&snapshot.config, &snapshot.token)
        };
        let run_result = match dispatch_result {
            Ok(r) => r,
            Err(err) => {
                // Network / transport failure: roll the state
                // forward to `Failed` (under the mutex) and bubble.
                let msg = err.to_string();
                let _ = with_runtime(handle, |rt| {
                    if let Some(inst) = rt.connector_instances.get_mut(&instance) {
                        inst.sync_state.mark_failed(&msg);
                    }
                    Ok(())
                });
                return Err(FfiError::from(err));
            }
        };
        // ─────────────── Phase 3: persist (locked) ────────────────
        //
        // Re-acquire the mutex. TOCTOU defence: another thread may
        // have called `forget_scope(scope)` or `remove_connector(id)`
        // during the unlocked dispatch. Re-check both before
        // ingesting — racing fresh evidence into a forgotten scope
        // would resurrect deleted state and break the
        // cryptographic-forgetting guarantee documented on
        // `crate::forget_scope`. Every error path here is also
        // responsible for rolling the per-instance state to
        // `Failed` so a subsequent `sync_connector` call goes
        // through `can_run_incremental() == false` and falls back
        // to a fresh `initial_sync` rather than racing an
        // incremental against a half-ingested cursor.
        with_runtime(handle, |rt| {
            let post_sync_result: FfiResult<(Vec<String>, DateTime<Utc>)> = (|| {
                if !rt.connector_instances.contains_key(&instance) {
                    return Err(FfiError::NotFound {
                        kind: "connector".into(),
                        id: instance_id.clone(),
                    });
                }
                if rt.is_scope_forgotten(snapshot.scope) {
                    return Err(FfiError::NotFound {
                        kind: "scope".into(),
                        id: snapshot.scope.as_uuid().to_string(),
                    });
                }
                // Persist events into the evidence store. Holding
                // the mutex across the ingest loop is fine — the
                // ingest is purely local SQLCipher I/O, not a
                // multi-second network round-trip, so concurrent
                // calls serialise on the local-only critical
                // section, not on the network.
                let mut ingested_ids: Vec<String> = Vec::new();
                for ev in &run_result.events {
                    if let Some(body) = event_to_evidence_body(ev) {
                        let source_tag = connector_source_tag(snapshot.source_kind);
                        let result = rt
                            .store_mut()
                            .ingest(
                                snapshot.scope,
                                body.as_bytes(),
                                Some(source_tag),
                                ImportanceClass::Important,
                            )
                            .map_err(|e| FfiError::Evidence {
                                message: e.to_string(),
                            })?;
                        ingested_ids.push(result.evidence_id.to_string());
                    }
                }
                // Advance sync state to reflect the successful
                // run. Single-write success path; the failure path
                // below performs its own `mark_failed`.
                let completed_at = Utc::now();
                if let Some(inst) = rt.connector_instances.get_mut(&instance) {
                    inst.sync_state
                        .mark_succeeded(run_result.next_cursor.clone(), completed_at);
                }
                Ok((ingested_ids, completed_at))
            })();
            let (ingested_ids, completed_at) = match post_sync_result {
                Ok(v) => v,
                Err(err) => {
                    let msg = err.to_string();
                    if let Some(inst) = rt.connector_instances.get_mut(&instance) {
                        inst.sync_state.mark_failed(&msg);
                    }
                    return Err(err);
                }
            };
            Ok(SyncReport {
                instance_id: instance.0.to_string(),
                mode: framework_sync_mode_to_kind(snapshot.mode),
                #[allow(clippy::cast_possible_truncation)] // events_total fits in u32 for any realistic sync window
                events_total: run_result.events.len() as u32,
                #[allow(clippy::cast_possible_truncation)] // ingested subset is ≤ events_total
                events_ingested: ingested_ids.len() as u32,
                ingested_evidence_ids: ingested_ids,
                next_cursor: run_result.next_cursor.clone(),
                started_at: started_at.timestamp(),
                completed_at: completed_at.timestamp(),
            })
        })
    })
}

/// Per-call snapshot captured under the runtime mutex in
/// [`sync_connector`]'s Phase 1 and consumed by the unlocked
/// dispatch in Phase 2 + the locked persist in Phase 3.
///
/// Owning every field (no borrows back into `FfiRuntime`) is what
/// lets the mutex drop between Phase 1 and Phase 2 — see the
/// function-level comments for the locking discipline.
struct SyncSnapshot {
    scope: ScopeId,
    config: ConnectorConfig,
    source_kind: ConnectorKind,
    mode: SyncMode,
    sync_state_snapshot: SyncState,
    token: connector_framework::OAuth2Token,
    connector: Arc<dyn Connector>,
}

/// Return the list of configured connector instances on this
/// runtime, with their current [`SyncState`] flattened into
/// [`ConnectorStatus`] rows for host-side UI rendering.
///
/// The order is unspecified (the runtime's `HashMap` iteration
/// order) — hosts that need a stable ordering should sort the
/// returned vector themselves (e.g. by `instance_id` for stable
/// UI ordering or by `last_synced_at` for "most recently synced
/// first" UX).
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
#[uniffi::export]
pub fn list_connectors(handle: RuntimeHandle) -> FfiResult<Vec<ConnectorStatus>> {
    metrics::instrument(metrics::inc_list_connectors, || {
        with_runtime(handle, |rt| {
            let mut out: Vec<ConnectorStatus> = Vec::with_capacity(rt.connector_instances.len());
            for inst in rt.connector_instances.values() {
                out.push(ConnectorStatus {
                    instance_id: inst.id.0.to_string(),
                    kind: framework_kind_to_ffi(inst.config.kind),
                    scope_id: inst.config.scope_id.as_uuid().to_string(),
                    sync_mode: framework_sync_mode_to_kind(inst.sync_state.mode),
                    sync_status: framework_sync_status_to_kind(inst.sync_state.status),
                    last_synced_at: inst.sync_state.last_synced_at.map(|t| t.timestamp()),
                    last_error: inst.sync_state.last_error.clone(),
                });
            }
            Ok(out)
        })
    })
}

/// Tear down the connector with `instance_id` — drop the live
/// `Box<dyn Connector>`, remove the [`ConnectorInstance`] row, and
/// purge the cached OAuth2 token from the vault. Idempotent within
/// the lifetime of a single runtime: removing an unknown id is a
/// no-op and returns `Ok(())`.
///
/// **Phase 2 scope:** no on-disk state is dropped (there is none
/// yet). Phase 3 will also delete the persisted
/// `connector_instances` / `connector_tokens` SQLCipher rows here.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn remove_connector(handle: RuntimeHandle, instance_id: String) -> FfiResult<()> {
    metrics::instrument(metrics::inc_remove_connector, || {
        let instance = parse_instance_id(&instance_id)?;
        with_runtime(handle, |rt| {
            rt.connector_instances.remove(&instance);
            rt.connectors.remove(&instance);
            // `OAuth2TokenVault::remove` returns
            // `ConnectorError::TokenNotFound` if no token exists;
            // we treat that as a benign no-op so `remove_connector`
            // is idempotent.
            let _ = rt.token_vault.remove(instance);
            Ok(())
        })
    })
}

// ──────────────────────────── Internals ──────────────────────────────

/// Build a fresh `Arc<dyn Connector>` for `kind`, wiring it to the
/// runtime-shared [`BlockingHttpTransport`] + [`OAuth2Client`] pair
/// when the `http-client` feature is enabled. When it is not, or
/// when the per-runtime transport failed to build at
/// [`crate::open_store`] time (see
/// `FfiRuntime::http_transport`'s soft-fail rationale), every
/// kind returns
/// [`FfiError::Unavailable { subsystem: "connector-http-client" }`]
/// so the host can detect the configuration gap explicitly instead
/// of seeing every call mysteriously fail.
///
/// The transport / OAuth2 client are built **once per runtime** at
/// [`crate::open_store`] time (see `FfiRuntime::http_transport` and
/// `FfiRuntime::oauth_client`) and cloned here as `Arc` handles, so
/// every connector on the same runtime shares one reqwest
/// connection pool / TLS session cache / thread pool. The `Arc<T>`
/// → `Arc<dyn Trait>` coercion lets the connector constructors
/// keep their trait-object-typed wiring contract without forcing
/// every concrete connector to monomorphise on
/// `BlockingHttpTransport`.
///
/// The returned handle is `Arc<dyn Connector>` rather than
/// `Box<dyn Connector>` so [`sync_connector`] and
/// [`authenticate_connector`] can clone the handle out of the
/// runtime mutex, drop the lock, and run the connector's HTTP
/// round-trip with the lock released. See those functions for the
/// three-phase locking pattern.
#[cfg(feature = "http-client")]
fn build_connector(
    rt: &FfiRuntime,
    kind: ConnectorKind,
    instance: ConnectorInstanceId,
) -> FfiResult<Arc<dyn Connector>> {
    use connector_framework::{HttpTransport, OAuth2CodeExchange};
    use connectors::{
        ConfluenceConnector, EmailConnector, FigmaConnector, GoogleDriveConnector,
        HubSpotConnector, JiraConnector, NotionConnector, OneDriveConnector, SlackConnector,
    };
    // If the per-runtime transport failed to build at
    // `open_store` time the connector subsystem is disabled —
    // surface the same `Unavailable` envelope that the
    // `not(http-client)` build returns so hosts have one uniform
    // recovery contract regardless of why the transport is absent.
    let (transport_arc, oauth_arc) = match (rt.http_transport.as_ref(), rt.oauth_client.as_ref()) {
        (Some(t), Some(o)) => (Arc::clone(t), Arc::clone(o)),
        _ => {
            return Err(FfiError::Unavailable {
                subsystem: "connector-http-client".into(),
            })
        }
    };
    // `.clone()` on `Arc<ConcreteT>` returns `Arc<ConcreteT>`; the
    // let-binding type ascription triggers the standard
    // `Arc<T>` → `Arc<dyn Trait>` unsize coercion. Using
    // `Arc::clone(&…)` instead would force the type inference
    // through `Arc::<dyn Trait>::clone`, which can't see through
    // the `&Arc<ConcreteT>` argument.
    let transport: Arc<dyn HttpTransport> = transport_arc;
    let oauth_client: Arc<dyn OAuth2CodeExchange> = oauth_arc;
    Ok(match kind {
        ConnectorKind::GoogleDrive => {
            Arc::new(GoogleDriveConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::OneDrive => {
            Arc::new(OneDriveConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Notion => Arc::new(NotionConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Jira => Arc::new(JiraConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Confluence => {
            Arc::new(ConfluenceConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Figma => Arc::new(FigmaConnector::new(instance, transport, oauth_client)),
        ConnectorKind::HubSpot => {
            Arc::new(HubSpotConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Slack => Arc::new(SlackConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Email => Arc::new(EmailConnector::new(instance, transport, oauth_client)),
        ConnectorKind::GitHub | ConnectorKind::GenericWebhook => {
            // Phase 2 ships the nine listed connector implementations
            // in `crates/connectors/`. GitHub and the generic webhook
            // connector are described in `docs/DESIGN.md` §10.2 but
            // do not have concrete implementors yet.
            return Err(FfiError::Unimplemented {
                method: format!("create_connector(kind={})", kind.as_str()),
            });
        }
    })
}

/// `http-client`-off fallback. The runtime exposes the connector FFI
/// functions unconditionally so the bindings layout stays stable
/// across builds, but calls surface `Unavailable` until the host
/// links a real HTTP transport.
#[cfg(not(feature = "http-client"))]
#[allow(clippy::unnecessary_wraps)] // signature matches the http-client-enabled variant for branch-free callers
fn build_connector(
    _rt: &FfiRuntime,
    _kind: ConnectorKind,
    _instance: ConnectorInstanceId,
) -> FfiResult<Arc<dyn Connector>> {
    Err(FfiError::Unavailable {
        subsystem: "connector-http-client".into(),
    })
}

/// Translate a [`ConnectorEvent`] into the evidence-store body
/// payload that will be ingested as a fresh evidence row.
///
/// Returns `None` for `DocumentDeleted` / `PermissionChanged` — those
/// events express the *absence* of, or a change-of-state on, an
/// existing document; the substrate's evidence store is append-only
/// and doesn't materialise "the document was deleted" as a new
/// evidence row. They still count in `events_total` so callers can
/// reconcile against the source system's change-feed cursor.
fn event_to_evidence_body(event: &ConnectorEvent) -> Option<String> {
    // Use `serde_json::Value` so the body round-trips through the
    // evidence store's UTF-8 view and the host can re-parse it.
    // Embedding `kind` + `document_id` + `occurred_at` gives the FTS
    // index something queryable without forcing the host to re-fetch
    // the source document before showing the evidence row in a
    // listing view.
    match event {
        ConnectorEvent::DocumentCreated {
            document_id,
            occurred_at,
        }
        | ConnectorEvent::DocumentUpdated {
            document_id,
            occurred_at,
        } => Some(
            serde_json::to_string(&serde_json::json!({
                "kind": event.kind(),
                "document_id": document_id.as_str(),
                "occurred_at": occurred_at,
            }))
            .expect("serialising a borrowed Value never fails"),
        ),
        ConnectorEvent::DocumentDeleted { .. } | ConnectorEvent::PermissionChanged { .. } => None,
    }
}

/// Pick the `SourceKind`-equivalent string tag stored in the
/// evidence store's `source_ref` column. The evidence store keeps
/// `source_ref` as opaque UTF-8 — there's no enum constraint — but
/// stable tags let downstream filters (`WHERE source_ref = 'Slack'`)
/// keep working without parsing.
fn connector_source_tag(kind: ConnectorKind) -> &'static str {
    match kind {
        ConnectorKind::GoogleDrive => "GoogleWorkspace",
        ConnectorKind::OneDrive => "MicrosoftGraph",
        ConnectorKind::Notion => "Notion",
        ConnectorKind::Jira | ConnectorKind::Confluence => "Atlassian",
        ConnectorKind::GitHub => "GitHub",
        ConnectorKind::Slack => "Slack",
        ConnectorKind::Figma => "Figma",
        ConnectorKind::HubSpot => "HubSpot",
        ConnectorKind::Email => "Email",
        ConnectorKind::GenericWebhook => "GenericWebhook",
    }
}

// ───────────────────────── Identifier helpers ────────────────────────

/// Per-call connector-instance UUID parser.
///
/// Kept here (not promoted to `crate::`) because the connector
/// lifecycle is the only caller — every other crate-internal callsite
/// owns `ConnectorInstanceId` values directly, never raw strings. If
/// a future entry point starts accepting instance ids by string from
/// outside this module, promote to `pub(crate)` in `lib.rs` next to
/// `parse_scope_id`.
fn parse_instance_id(s: &str) -> FfiResult<ConnectorInstanceId> {
    Uuid::parse_str(s)
        .map(ConnectorInstanceId)
        .map_err(|e| FfiError::InvalidId {
            message: format!("invalid connector instance id `{s}`: {e}"),
        })
}

// ───────────────────────── Enum translation ──────────────────────────

fn connector_kind_to_framework(tag: ConnectorKindTag) -> ConnectorKind {
    match tag {
        ConnectorKindTag::GoogleDrive => ConnectorKind::GoogleDrive,
        ConnectorKindTag::OneDrive => ConnectorKind::OneDrive,
        ConnectorKindTag::Notion => ConnectorKind::Notion,
        ConnectorKindTag::Jira => ConnectorKind::Jira,
        ConnectorKindTag::Confluence => ConnectorKind::Confluence,
        ConnectorKindTag::GitHub => ConnectorKind::GitHub,
        ConnectorKindTag::Slack => ConnectorKind::Slack,
        ConnectorKindTag::Figma => ConnectorKind::Figma,
        ConnectorKindTag::HubSpot => ConnectorKind::HubSpot,
        ConnectorKindTag::Email => ConnectorKind::Email,
        ConnectorKindTag::GenericWebhook => ConnectorKind::GenericWebhook,
    }
}

fn framework_kind_to_ffi(kind: ConnectorKind) -> ConnectorKindTag {
    match kind {
        ConnectorKind::GoogleDrive => ConnectorKindTag::GoogleDrive,
        ConnectorKind::OneDrive => ConnectorKindTag::OneDrive,
        ConnectorKind::Notion => ConnectorKindTag::Notion,
        ConnectorKind::Jira => ConnectorKindTag::Jira,
        ConnectorKind::Confluence => ConnectorKindTag::Confluence,
        ConnectorKind::GitHub => ConnectorKindTag::GitHub,
        ConnectorKind::Slack => ConnectorKindTag::Slack,
        ConnectorKind::Figma => ConnectorKindTag::Figma,
        ConnectorKind::HubSpot => ConnectorKindTag::HubSpot,
        ConnectorKind::Email => ConnectorKindTag::Email,
        ConnectorKind::GenericWebhook => ConnectorKindTag::GenericWebhook,
    }
}

fn framework_sync_mode_to_kind(mode: SyncMode) -> SyncModeKind {
    match mode {
        SyncMode::Full => SyncModeKind::Full,
        SyncMode::Incremental => SyncModeKind::Incremental,
    }
}

fn framework_sync_status_to_kind(status: SyncStatus) -> SyncStatusKind {
    match status {
        SyncStatus::NeverRun => SyncStatusKind::NeverRun,
        SyncStatus::InProgress => SyncStatusKind::InProgress,
        SyncStatus::Succeeded => SyncStatusKind::Succeeded,
        SyncStatus::Failed => SyncStatusKind::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use connector_framework::SourceDocumentId;

    #[test]
    fn document_created_serialises_to_evidence_body() {
        let now = Utc.with_ymd_and_hms(2026, 5, 28, 3, 35, 0).unwrap();
        let ev = ConnectorEvent::DocumentCreated {
            document_id: SourceDocumentId::new("doc-1"),
            occurred_at: now,
        };
        let body = event_to_evidence_body(&ev).expect("created events ingest");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["kind"], "document_created");
        assert_eq!(parsed["document_id"], "doc-1");
    }

    #[test]
    fn document_deleted_skips_ingestion() {
        let ev = ConnectorEvent::DocumentDeleted {
            document_id: SourceDocumentId::new("doc-2"),
            occurred_at: Utc::now(),
        };
        assert!(event_to_evidence_body(&ev).is_none());
    }

    #[test]
    fn permission_changed_skips_ingestion() {
        let ev = ConnectorEvent::PermissionChanged {
            document_id: SourceDocumentId::new("doc-3"),
            user_id: connector_framework::SourceUserId::new("user-1"),
            new_level: None,
            occurred_at: Utc::now(),
        };
        assert!(event_to_evidence_body(&ev).is_none());
    }

    #[test]
    fn kind_translation_round_trips() {
        let all = [
            ConnectorKindTag::GoogleDrive,
            ConnectorKindTag::OneDrive,
            ConnectorKindTag::Notion,
            ConnectorKindTag::Jira,
            ConnectorKindTag::Confluence,
            ConnectorKindTag::GitHub,
            ConnectorKindTag::Slack,
            ConnectorKindTag::Figma,
            ConnectorKindTag::HubSpot,
            ConnectorKindTag::Email,
            ConnectorKindTag::GenericWebhook,
        ];
        for tag in all {
            assert_eq!(framework_kind_to_ffi(connector_kind_to_framework(tag)), tag);
        }
    }

    #[test]
    fn sync_mode_translation_round_trips() {
        for mode in [SyncMode::Full, SyncMode::Incremental] {
            let kind = framework_sync_mode_to_kind(mode);
            let back = match kind {
                SyncModeKind::Full => SyncMode::Full,
                SyncModeKind::Incremental => SyncMode::Incremental,
            };
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn sync_status_translation_round_trips() {
        for status in [
            SyncStatus::NeverRun,
            SyncStatus::InProgress,
            SyncStatus::Succeeded,
            SyncStatus::Failed,
        ] {
            let kind = framework_sync_status_to_kind(status);
            let back = match kind {
                SyncStatusKind::NeverRun => SyncStatus::NeverRun,
                SyncStatusKind::InProgress => SyncStatus::InProgress,
                SyncStatusKind::Succeeded => SyncStatus::Succeeded,
                SyncStatusKind::Failed => SyncStatus::Failed,
            };
            assert_eq!(back, status);
        }
    }

    #[test]
    fn source_tag_is_stable_for_every_kind() {
        for kind in [
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
            ConnectorKind::GenericWebhook,
        ] {
            // Stability assertion: the tag must not be empty and
            // must round-trip as ASCII so the evidence store's
            // FTS5 column doesn't have to deal with unicode.
            let tag = connector_source_tag(kind);
            assert!(!tag.is_empty());
            assert!(tag.is_ascii());
        }
    }

    #[test]
    fn parse_scope_id_rejects_non_uuid() {
        let err = parse_scope_id("not-a-uuid").unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn parse_instance_id_rejects_non_uuid() {
        let err = parse_instance_id("xyzzy").unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn parse_scope_id_accepts_valid_uuid() {
        let id = Uuid::new_v4().to_string();
        let scope = parse_scope_id(&id).unwrap();
        assert_eq!(scope.as_uuid().to_string(), id);
    }
}
