//! Substrate liveness probe — the `health_check` surface.
//!
//! Replaces the original `"ok"` string stub on the napi side. Returns
//! a typed [`HealthStatus`] envelope that platform hosts (Electron
//! desktop status panel, the `health-check` exit-code probe shipped
//! alongside the addon) deserialise on every poll to render:
//!
//! * The substrate's `core_version` (the workspace semver baked
//!   into the build) and `uptime_secs` (wall-clock seconds since
//!   `metrics::prime` / first counter touch).
//! * Whether a `tracing` subscriber has been installed via the
//!   `tracing-subscriber`-feature-gated [`crate::tracing_init`]
//!   helper.
//! * Per-subsystem [`SubsystemHealth`] — one entry per substrate
//!   subsystem, each with a [`SubsystemStatus`] (`Ok` / `Degraded` /
//!   `Unavailable`) and an optional free-form `detail` string.
//! * A [`crate::metrics::MetricsSnapshot`] of every counter / gauge.
//!
//! The envelope is wire-flat (every field `Serialize +
//! Deserialize`) so the same shape crosses the N-API JSON boundary
//! and a future UniFFI binding without re-shaping.
//!
//! # Two modes
//!
//! [`health_check`] takes an `Option<RuntimeHandle>`:
//!
//! * `None` → returns a "bridge-only" envelope: `core_version`,
//!   `uptime_secs`, `tracing_initialized`, the metrics snapshot, and
//!   a single `SubsystemHealth { name: "bridge", status: Ok, … }`
//!   entry. Hosts use this from their early-boot path to confirm
//!   the FFI layer loads and responds before any `open_store` call.
//!
//! * `Some(handle)` → probes the open runtime behind `handle`:
//!   a `SELECT COUNT(*) FROM evidence` against the SQLCipher
//!   connection (via [`evidence_store::EvidenceStore::evidence_count`]),
//!   master-key presence on the crypto layer, user / channel memory counts,
//!   the inference router's adapter ladder. Each probe is real (no
//!   stubs) and surfaces its outcome as a separate `SubsystemHealth`
//!   entry so hosts can render a per-subsystem status panel.
//!
//! `health_check(Some(handle))` returns
//! [`crate::error::FfiError::Unavailable`] if `handle` is the
//! `RuntimeHandle::NONE` sentinel or doesn't refer to an open
//! runtime — mirroring the contract of every other handle-keyed
//! FFI entry point.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use std::fmt::Write as _;

use crate::error::{FfiError, FfiResult};
use crate::metrics::{self, MetricsSnapshot};
use crate::runtime::{self, RuntimeHandle};

/// Top-level health envelope. Wire-flat (every field
/// `Serialize + Deserialize`) so platform hosts can deserialise it
/// from the napi JSON / UniFFI bridge without further reshape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct HealthStatus {
    /// Workspace semver of the Rust core (`CARGO_PKG_VERSION` at
    /// build time). Mirrors the value returned by
    /// `napi_addon::core_version`.
    pub core_version: String,
    /// Wall-clock seconds since the metrics block was first
    /// initialised. Surfaces on host dashboards so operators can
    /// distinguish "just rebooted" from "stuck running". `0` if
    /// the metrics block has never been primed and the host
    /// somehow called `health_check` before any other entry point
    /// (in practice impossible — `init` / `open_store` /
    /// `health_check` all touch the metrics block).
    pub uptime_secs: u64,
    /// `true` once a `tracing` subscriber has been installed via
    /// [`crate::tracing_init::try_init_tracing`]. Read-only —
    /// hosts cannot toggle the flag back to `false`.
    pub tracing_initialized: bool,
    /// One entry per substrate subsystem. When `health_check` is
    /// called without a handle this contains a single
    /// `name = "bridge"` entry; with a handle it contains
    /// `bridge`, `evidence_store`, `crypto`, `memory_manager`, and
    /// `inference_router`.
    pub subsystems: Vec<SubsystemHealth>,
    /// Snapshot of every counter and gauge at the moment of the
    /// `health_check` call.
    pub metrics: MetricsSnapshot,
}

/// Per-subsystem liveness entry. `status` is a coarse three-state
/// indicator; `detail` carries a free-form human-readable string
/// (e.g. "12 user memories rehydrated", "no SLM adapter linked").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct SubsystemHealth {
    /// Stable subsystem tag (`bridge`, `evidence_store`, `crypto`,
    /// `memory_manager`, `inference_router`).
    pub name: String,
    /// Coarse three-state indicator.
    pub status: SubsystemStatus,
    /// Optional free-form diagnostic. Always populated for
    /// `Degraded` / `Unavailable`; usually populated for `Ok` so
    /// hosts can render a per-subsystem detail line.
    pub detail: Option<String>,
    /// Optional per-adapter state for the `inference_router`
    /// subsystem. `None` for every other subsystem.
    pub adapters: Option<Vec<AdapterReport>>,
    /// Optional SLM dispatch latency summary for the
    /// `inference_router` subsystem. `Some` whenever an inference
    /// adapter is available (even with zero recorded dispatches, in
    /// which case the percentiles are `None`); `None` for every other
    /// subsystem.
    pub slm_latency: Option<SlmLatencyReport>,
}

/// Aggregate SLM dispatch-latency summary surfaced on the
/// `inference_router` `SubsystemHealth` payload.
///
/// Percentiles are estimated from the
/// `knowledge_slm_dispatch_duration_seconds` histogram aggregated
/// across every `(task, adapter)` pair (see
/// [`inference_router::InferenceRouter::overall_dispatch_latency`]) and
/// reported in **milliseconds** so the field stays integer-valued and
/// the envelope remains `Eq`. Both percentiles are `None` until the
/// first dispatch is recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct SlmLatencyReport {
    /// Total dispatches recorded across all `(task, adapter)` pairs
    /// since the runtime opened.
    pub sample_count: u64,
    /// Estimated p50 (median) dispatch latency in milliseconds, or
    /// `None` when no dispatch has been recorded yet.
    pub p50_ms: Option<u64>,
    /// Estimated p95 dispatch latency in milliseconds, or `None` when
    /// no dispatch has been recorded yet.
    pub p95_ms: Option<u64>,
}

/// Per-adapter status entry on the `inference_router`
/// `SubsystemHealth` payload. Mirrors the snapshot returned by
/// `inference_router::InferenceRouter::adapter_states` but
/// serialised through the FFI surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct AdapterReport {
    /// Stable adapter tag (`mlx`, `llama_cpp`, `fallback`,
    /// `mock`).
    pub kind: String,
    /// `true` once the adapter's probe returned `Available`.
    pub available: bool,
    /// `true` while the adapter is currently loaded into memory
    /// (set after the first dispatch, cleared by the router's
    /// idle-unload sweep).
    pub loaded: bool,
    /// Tasks this adapter declares it can serve. Stable string
    /// tags from `InferenceTask::tag()`.
    pub supports: Vec<String>,
}

/// Coarse three-state subsystem status. Maps to render decisions
/// in the host UI (green / yellow / red).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemStatus {
    /// Subsystem is fully functional. The detail string (if any)
    /// describes the steady-state (counts, configuration).
    Ok,
    /// Subsystem is partially functional. Hosts SHOULD continue
    /// to use it but surface the degraded status to the user.
    /// E.g. inference router with only the fallback adapter
    /// available (classification works, synthesis does not).
    Degraded,
    /// Subsystem is offline or in an error state. Hosts SHOULD
    /// avoid issuing further calls that depend on it.
    Unavailable,
}

/// Probe the substrate's liveness and return a typed
/// [`HealthStatus`] envelope.
///
/// `handle` controls the probe depth:
///
/// * `None` → bridge-only probe. Returns a `HealthStatus` with a
///   single `name = "bridge"` subsystem entry. Cannot fail.
///
/// * `Some(handle)` → full probe against the runtime behind
///   `handle`. Returns
///   [`FfiError::Unavailable { subsystem: "evidence_store" }`]
///   when `handle` is unknown or has been closed (mirrors every
///   other handle-keyed FFI entry point).
///
/// The metrics snapshot is captured at the **end** of the probe
/// so any counter that fires during the probe itself (e.g. the
/// `evidence_count` SQLCipher read on the `evidence_store`
/// subsystem) is included in the returned snapshot.
///
/// # Errors
///
/// Forwards `FfiError::Unavailable` when `handle` is invalid;
/// otherwise the call is infallible.
#[uniffi::export]
pub fn health_check(handle: Option<RuntimeHandle>) -> FfiResult<HealthStatus> {
    // Wrap the body with `metrics::instrument` per the CONTRIBUTING.md
    // observability rule: every public FFI entry point increments
    // `<name>_total` before its body runs and feeds the `Err` path
    // through `inc_error`. Hosts that spin on `health_check` will
    // now show up in `health_check_total`; an `Err` from an invalid
    // handle increments both `health_check_total` AND
    // `errors_by_kind.unavailable`, matching the contract of every
    // other handle-keyed entry point.
    metrics::instrument(metrics::inc_health_check, || {
        // Force the metrics block to initialise before reading the
        // boot stamp — `health_check` is one of the entry points
        // listed in the metrics docs as anchoring uptime.
        metrics::prime();

        let mut subsystems = vec![bridge_subsystem()];

        match handle {
            None => Ok(finish_envelope(subsystems)),
            Some(RuntimeHandle(0)) => Err(FfiError::Unavailable {
                subsystem: "evidence_store".into(),
            }),
            Some(h) => {
                runtime::with_runtime(h, |rt| {
                    // Refresh the tombstone gauge before snapshotting
                    // so the returned envelope reflects the runtime
                    // we just probed.
                    let tombstones = rt.registry().tombstones().count() as u64;
                    metrics::set_tombstone_count(tombstones);

                    subsystems.push(evidence_store_subsystem(rt));
                    subsystems.push(crypto_subsystem(rt, tombstones));
                    subsystems.push(memory_manager_subsystem(rt));
                    subsystems.push(inference_router_subsystem(rt));
                    subsystems.push(connector_subsystem(rt));
                    subsystems.push(synthesis_subsystem(rt));
                    Ok(())
                })?;
                Ok(finish_envelope(subsystems))
            }
        }
    })
}

// ─── Per-subsystem probes ──────────────────────────────────────────

/// Bridge probe. Always `Ok` — if execution reached this function
/// the FFI layer is by definition reachable. The detail string
/// surfaces the workspace semver baked into the build so the host
/// can confirm it loaded the version it expected.
fn bridge_subsystem() -> SubsystemHealth {
    SubsystemHealth {
        name: "bridge".into(),
        status: SubsystemStatus::Ok,
        detail: Some(format!("core_version={}", core_version())),
        adapters: None,
        slm_latency: None,
    }
}

/// Evidence store probe. Runs `evidence_count()` against the open
/// SQLCipher connection — a real `SELECT COUNT(*) FROM evidence`
/// (table is named `evidence`; see `crates/evidence_store/src/schema.rs`).
/// Exercises the SQLCipher session: the cipher key is unwrapped, the
/// SQLite VFS reads + decrypts the encrypted page(s) backing the
/// `evidence` table at the page-cipher layer (default PRAGMA cipher,
/// AES-256-CBC + HMAC-SHA512), and the count is returned. The probe
/// does NOT exercise the per-body XChaCha20-Poly1305 AEAD layer in
/// `crates/crypto` (a row count never touches the `body` blob) —
/// for that, hosts can issue a real query/get_evidence call after
/// the probe, which does decrypt one or more bodies. The `detail`
/// string uses the label `evidence_rows=…` which is the display
/// label on the host UI, not the table name.
fn evidence_store_subsystem(rt: &crate::runtime::FfiRuntime) -> SubsystemHealth {
    match rt.store().evidence_count() {
        Ok(count) => SubsystemHealth {
            name: "evidence_store".into(),
            status: SubsystemStatus::Ok,
            detail: Some(format!("evidence_rows={count}")),
            adapters: None,
            slm_latency: None,
        },
        Err(e) => SubsystemHealth {
            name: "evidence_store".into(),
            status: SubsystemStatus::Unavailable,
            detail: Some(format!("evidence_count failed: {e}")),
            adapters: None,
            slm_latency: None,
        },
    }
}

/// Crypto probe. Surfaces:
///
/// * That the in-memory master key is non-zero (a runtime that
///   somehow held an all-zero [`crypto::MasterKey`] would be
///   either an uninitialised runtime or a security regression —
///   `open_store` never returns one because the master key is
///   derived from the host-supplied passphrase via the
///   [`crypto::derive_key_and_zeroize`] ladder).
/// * The DEK tombstone count (real `forget` calls land here).
/// * The cached-DEK count (per-scope keys currently resident in
///   the evidence store's in-memory cache).
///
/// Tombstones never downgrade the crypto status to `Unavailable`
/// — they record completed forgetting, not a fault. An all-zero
/// master key DOES downgrade to `Unavailable` since it indicates
/// the runtime cannot derive a usable sub-key.
fn crypto_subsystem(rt: &crate::runtime::FfiRuntime, tombstones: u64) -> SubsystemHealth {
    let mk = rt.master_key();
    // Use the count-only accessor — `cached_scope_keys()` would
    // clone the full `HashMap<ScopeId, AeadKey>` just to call
    // `.len()`, which is O(N · key-bytes) per probe. The probe runs
    // on every health-check, so the saving is meaningful.
    let cached_deks = rt.store().cached_scope_key_count();
    if mk.iter().all(|b| *b == 0) {
        SubsystemHealth {
            name: "crypto".into(),
            status: SubsystemStatus::Unavailable,
            detail: Some("master key is all-zero; runtime is uninitialised or corrupt".into()),
            adapters: None,
            slm_latency: None,
        }
    } else {
        SubsystemHealth {
            name: "crypto".into(),
            status: SubsystemStatus::Ok,
            detail: Some(format!(
                "master_key=present, tombstones={tombstones}, cached_deks={cached_deks}"
            )),
            adapters: None,
            slm_latency: None,
        }
    }
}

/// Memory manager probe. Reports the count of user / channel
/// memories rehydrated for this runtime. `Ok` even when both
/// counts are zero — an empty store is a legitimate steady state.
fn memory_manager_subsystem(rt: &crate::runtime::FfiRuntime) -> SubsystemHealth {
    let users = rt.user_memory_count();
    let channels = rt.channel_memory_count();
    SubsystemHealth {
        name: "memory_manager".into(),
        status: SubsystemStatus::Ok,
        detail: Some(format!(
            "user_memories={users}, channel_memories={channels}"
        )),
        adapters: None,
        slm_latency: None,
    }
}

/// Inference router probe. Reports per-adapter state via the
/// `adapter_states()` accessor on the router. Status downgrades:
///
/// * `Unavailable` when no adapter is available at all.
/// * `Degraded` when no available adapter supports any synthesis
///   task (classification-only ladder — `trigger_synthesis` will
///   return `Unavailable`).
/// * `Ok` otherwise.
fn inference_router_subsystem(rt: &crate::runtime::FfiRuntime) -> SubsystemHealth {
    use inference_router::InferenceTask;

    let states = rt.inference_router.adapter_states();
    let adapters: Vec<AdapterReport> = states
        .iter()
        .map(|s| AdapterReport {
            kind: s.kind.as_str().to_string(),
            available: s.available,
            loaded: s.loaded,
            supports: s.supports.iter().map(|t| t.tag().to_string()).collect(),
        })
        .collect();

    let any_available = states.iter().any(|s| s.available);
    let any_synthesis_capable = states
        .iter()
        .filter(|s| s.available)
        .any(|s| s.supports.iter().copied().any(InferenceTask::is_synthesis));

    // SLM dispatch latency, aggregated across every (task, adapter)
    // pair. Surfaced whenever an adapter is available so hosts can
    // render p50/p95 next to the adapter ladder; the percentiles stay
    // `None` until the first dispatch is recorded. Reported in
    // milliseconds (rounded) to keep the envelope integer-valued.
    let slm_latency = if any_available {
        let hist = rt.inference_router.overall_dispatch_latency();
        // Quantiles come from `LATENCY_BUCKETS_SECONDS` (largest finite
        // bound 10 s), so `secs` is non-negative and far below the u64
        // millisecond ceiling. Saturate via `as u64` after rounding —
        // the casts are provably lossless for this domain.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let to_ms = |secs: f64| (secs * 1_000.0).round() as u64;
        Some(SlmLatencyReport {
            sample_count: hist.count(),
            p50_ms: hist.quantile(0.50).map(to_ms),
            p95_ms: hist.quantile(0.95).map(to_ms),
        })
    } else {
        None
    };

    let (status, detail) = if !any_available {
        (
            SubsystemStatus::Unavailable,
            "no adapter is available; inference will return Unavailable".to_string(),
        )
    } else if !any_synthesis_capable {
        (
            SubsystemStatus::Degraded,
            "no available adapter supports synthesis; trigger_synthesis will return Unavailable"
                .to_string(),
        )
    } else {
        let names: Vec<&str> = states
            .iter()
            .filter(|s| s.available)
            .map(|s| s.kind.as_str())
            .collect();
        (
            SubsystemStatus::Ok,
            format!("available adapters: {}", names.join(", ")),
        )
    };

    SubsystemHealth {
        name: "inference_router".into(),
        status,
        detail: Some(detail),
        adapters: Some(adapters),
        slm_latency,
    }
}

/// Connector framework probe. Reports the per-runtime connector
/// registry size and the current distribution of sync states
/// across the live instances. Mirrors the wiring contract from
/// `CONTRIBUTING.md` §4: every new substrate subsystem ships with
/// a corresponding `health_check` probe so platform hosts can render
/// a per-subsystem status panel without reaching into substrate-
/// internal counters.
///
/// Status downgrades:
///
/// * `Degraded` when one or more connectors are in
///   [`SyncStatus::Failed`] — the rest of the substrate is fine,
///   but the host should know that at least one source is not
///   currently synchronising. The detail string carries the
///   per-status counts so the host UI can render `(3 ok, 1 failed)`
///   or similar.
/// * `Ok` in every other case, including the steady-state of zero
///   connectors (a runtime that hasn't called [`crate::create_connector`]
///   yet is a legitimate state, not a fault).
///
/// Authenticated count comes from the per-runtime [`OAuth2TokenVault`]
/// — every successful [`crate::authenticate_connector`] call stashes
/// a bearer token there, so the count is a proxy for "how many
/// connectors can run a sync right now". Comparing it against
/// `total` surfaces dangling registrations (created but never
/// authenticated) without an extra round-trip through the host.
fn connector_subsystem(rt: &crate::runtime::FfiRuntime) -> SubsystemHealth {
    use connector_framework::SyncStatus;

    let total = rt.connector_instances.len();
    let authenticated = rt.token_vault.len();
    let mut never_run = 0u64;
    let mut in_progress = 0u64;
    let mut succeeded = 0u64;
    let mut failed = 0u64;
    for inst in rt.connector_instances.values() {
        match inst.sync_state.status {
            SyncStatus::NeverRun => never_run += 1,
            SyncStatus::InProgress => in_progress += 1,
            SyncStatus::Succeeded => succeeded += 1,
            SyncStatus::Failed => failed += 1,
        }
    }

    // Surface the HTTP-transport availability so hosts can detect
    // the soft-fail-on-open path (see `crate::open_store`) without
    // calling `create_connector` first and parsing the
    // `FfiError::Unavailable { subsystem: "connector-http-client" }`
    // envelope. The transport is the load-bearing dependency for
    // every connector lifecycle call — when it's missing, the
    // subsystem is `Degraded` even if zero connectors are
    // registered, because the host can no longer recover by
    // registering new ones.
    #[cfg(feature = "http-client")]
    let http_transport_available = rt.http_transport.is_some() && rt.oauth_client.is_some();
    // When the feature is off the transport is *intentionally* absent
    // and the subsystem still reports `Ok` (it's the
    // `connector-http-client` `Unavailable` path described on
    // `crate::FfiError::Unavailable` — degrading the whole probe
    // would force every offline / ingest-only host to render a red
    // tile for a behaviour they explicitly opted into via Cargo
    // features).
    #[cfg(not(feature = "http-client"))]
    let http_transport_available = true;

    // Surface the OAuth2 `ClientSecretResolver` registration state
    // alongside the transport availability so an operator
    // diagnosing an `invalid_client` grant rejection can tell at a
    // glance which `client_secret` resolution layer is wired up.
    // Pure diagnostic signal — the probe stays `Ok` when no
    // resolver is registered, because public-client providers
    // (Slack PKCE-only, Notion test mode) work fine without one;
    // the host might also be relying on the
    // `auth_config_json["client_secret"]` fallback layer. Only the
    // `failed > 0 || !http_transport` conditions remain
    // load-bearing for the subsystem status.
    //
    // Under `not(http-client)` the resolver slot is
    // architecturally inert (no `OAuth2Client` is ever
    // constructed) so we don't compute or surface this signal.
    #[cfg(feature = "http-client")]
    let oauth_resolver_registered = rt.oauth_client.as_ref().is_some_and(|c| c.has_resolver());

    let status = if failed > 0 || !http_transport_available {
        SubsystemStatus::Degraded
    } else {
        SubsystemStatus::Ok
    };
    let mut detail = format!(
        "total={total}, authenticated={authenticated}, \
         never_run={never_run}, in_progress={in_progress}, \
         succeeded={succeeded}, failed={failed}"
    );
    if !http_transport_available {
        // Append rather than replace so the per-status counts stay
        // machine-parseable for host UIs that already key off them.
        detail.push_str(", http_transport=unavailable");
    }
    // Always append the resolver state when the http-client
    // feature is on — both `registered` and `unset` are
    // diagnostically useful (host that wired up a resolver wants
    // to confirm it stuck across `open_store`; host that hasn't
    // wants to know why confidential-client grants might be
    // failing). Under `not(http-client)` the resolver slot is
    // architecturally inert, so skip the field to keep the detail
    // string focused on what's actually actionable.
    #[cfg(feature = "http-client")]
    {
        detail.push_str(if oauth_resolver_registered {
            ", oauth_resolver=registered"
        } else {
            ", oauth_resolver=unset"
        });
    }
    // Surface the webhook receiver state: how many servers
    // are currently bound + how many `(provider_id, instance_id)`
    // dispatch rows are registered across them. Tells the operator
    // at a glance whether the substrate is configured to receive
    // provider webhooks natively (server count > 0) and whether the
    // host's boot-wiring of dispatch routes completed (registration
    // count > 0 when at least one server is up). The probe stays
    // `Ok` regardless — most ingest-only hosts never start a
    // webhook server (Electron status panels, offline CLI batch
    // tools), and treating "no webhook server" as a degraded state
    // would force them to render a yellow tile for a deliberate
    // configuration choice.
    let webhook_server_count = rt.webhook_servers.len();
    let webhook_registration_count: usize = rt
        .webhook_servers
        .values()
        .map(super::webhook::RunningWebhookServer::router_registration_count)
        .sum();
    let _ = write!(
        &mut detail,
        ", webhook_servers={webhook_server_count}, \
         webhook_registrations={webhook_registration_count}"
    );
    // Surface the background sync scheduler's running
    // state. Pure diagnostic: stays `Ok` regardless because most
    // ingest-only hosts (offline CLI batch tools, Electron status
    // panels) never start a scheduler, and treating "no scheduler"
    // as Degraded would force a yellow tile for a deliberate
    // configuration choice — same rationale as the webhook server
    // count above.
    detail.push_str(", ");
    detail.push_str(crate::sync_scheduler::scheduler_health_detail(rt));
    SubsystemHealth {
        name: "connector".into(),
        status,
        detail: Some(detail),
        adapters: None,
        slm_latency: None,
    }
}

/// Server-side synthesis subsystem probe.
///
/// Reports:
///
/// * Whether the host has installed an engine via
///   [`crate::synthesis::configure_synthesis_engine`]
///   (`Unavailable` when not).
/// * Window manager population (count of tracked synthesis
///   windows, regardless of status).
/// * Number of rehydrated domain / tenant memory objects, which
///   are the inputs to domain / tenant synthesis respectively.
/// * Whether a scope-binding allow-list is configured. When the
///   engine is configured but bindings are absent the probe stays
///   `Degraded` so an operator can see at a glance that the
///   substrate is dispatching without scope enforcement (the
///   recommended production posture is to either configure
///   bindings or wrap the engine in a TEE worker).
fn synthesis_subsystem(rt: &crate::runtime::FfiRuntime) -> SubsystemHealth {
    let engine_configured = rt.synthesis_engine.is_some();
    let total_windows = rt.synthesis_windows.len();
    let domain_count = rt.domain_memory_count();
    let tenant_count = rt.tenant_memory_count();
    // The in-memory shape is nested
    // (`HashMap<ScopeId, HashMap<WindowId, SynthesisObject>>`), so a
    // bare `.len()` on the outer map would report the number of
    // *scopes* with at least one object rather than the total object
    // count surfaced in the probe detail. Sum over the per-scope
    // sub-maps via the helper instead.
    let synthesis_objects = rt.synthesis_object_count();
    let scope_bindings_configured = rt.synthesis_scope_bindings.is_some();
    let scope_binding_count = rt
        .synthesis_scope_bindings
        .as_ref()
        .map_or(0, std::vec::Vec::len);
    let cooldown_count = rt.synthesis_cooldowns.len();

    let status = if !engine_configured {
        SubsystemStatus::Unavailable
    } else if !scope_bindings_configured && !rt.synthesis_single_tenant {
        SubsystemStatus::Degraded
    } else {
        SubsystemStatus::Ok
    };

    // Surface the rate-limiter's configured
    // posture on the detail string so operators can confirm
    // `configure_synthesis_engine` actually landed the host's
    // rate-shaping values. Same diagnostic-gap rationale as the
    // `single_tenant=` token below: without these fields the only
    // way to verify the host's configuration is in-process state
    // inspection, which the health probe is meant to obviate.
    let rate_capacity = rt.synthesis_rate_limiter.capacity();
    let rate_refill_per_sec = rt.synthesis_rate_limiter.refill_per_sec();

    SubsystemHealth {
        name: "synthesis_engine".into(),
        status,
        detail: Some(format!(
            "engine={}, windows={total_windows}, objects={synthesis_objects}, \
             domain_memories={domain_count}, tenant_memories={tenant_count}, \
             scope_bindings={}, single_tenant={}, cooldowns={cooldown_count}, \
             rate_capacity={rate_capacity}, rate_refill_per_sec={rate_refill_per_sec}",
            if engine_configured {
                "configured"
            } else {
                "unconfigured"
            },
            if scope_bindings_configured {
                format!("{scope_binding_count} configured")
            } else {
                "unconfigured".to_string()
            },
            rt.synthesis_single_tenant,
        )),
        adapters: None,
        slm_latency: None,
    }
}

// ─── Helpers ───────────────────────────────────────────────────────

/// Build the final envelope from a populated subsystems vec. Reads
/// the metrics snapshot AFTER the probes have run so any counter
/// that incremented during the probe is included.
fn finish_envelope(subsystems: Vec<SubsystemHealth>) -> HealthStatus {
    let snapshot = metrics::snapshot();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let uptime_secs = now.saturating_sub(snapshot.boot_unix_secs);
    HealthStatus {
        core_version: core_version(),
        uptime_secs,
        tracing_initialized: crate::metrics::tracing_initialized(),
        subsystems,
        metrics: snapshot,
    }
}

/// Workspace semver baked into the build. Same source as
/// `napi_addon::core_version` (which calls this through the FFI
/// surface).
///
/// **Not exported via UniFFI by design.** Mobile (Swift / Kotlin)
/// hosts read the build version from
/// [`HealthStatus::core_version`] on the value returned by
/// [`health_check`], so a standalone `core_version()` entry point
/// would be a redundant second source of truth in the binding
/// surface. The N-API side re-exports this as a standalone
/// `coreVersion()` JS function purely so the Electron bootstrap
/// path can log the version before any handle is opened (no
/// `health_check` call would have a real subsystem fan-out to
/// report anyway).
#[must_use]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_check_without_handle_returns_bridge_only_envelope() {
        let env = health_check(None).expect("bridge probe is infallible");
        assert_eq!(env.core_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(env.subsystems.len(), 1);
        assert_eq!(env.subsystems[0].name, "bridge");
        assert_eq!(env.subsystems[0].status, SubsystemStatus::Ok);
        assert!(env.subsystems[0].adapters.is_none());
        // Uptime is unchecked-low — we just primed the metrics
        // block earlier in the same test process, so the value is
        // small but it must be a sane u64 (no overflow / negative
        // wrap via the saturating sub).
        // Boot stamp must be set after the call. We deliberately do
        // NOT assert exact equality against a freshly-captured
        // `metrics::snapshot()` here — the metrics block is a
        // process-wide singleton and another test thread can
        // increment any counter between the snapshot embedded in
        // `env.metrics` (captured inside `health_check`) and a
        // second snapshot captured here. Instead we assert the
        // envelope's snapshot is **internally consistent** with the
        // probe (boot stamp set, snapshot fields are well-formed)
        // and that the boot stamp is non-zero, which is the only
        // semantic guarantee the singleton provides under parallel
        // test execution.
        assert!(env.metrics.boot_unix_secs > 0);
        // Snapshot fields are monotonic counters / gauges — verify
        // they are at least the values we last observed for the
        // boot stamp (which is set-once and never decreases).
        let snap = metrics::snapshot();
        assert!(snap.boot_unix_secs > 0);
        assert!(snap.boot_unix_secs >= env.metrics.boot_unix_secs);
    }

    #[test]
    fn health_check_with_none_handle_returns_unavailable() {
        let err = health_check(Some(RuntimeHandle::NONE)).unwrap_err();
        match err {
            FfiError::Unavailable { subsystem } => {
                assert_eq!(subsystem, "evidence_store");
            }
            _ => panic!("expected Unavailable"),
        }
    }

    #[test]
    fn health_check_with_unknown_handle_returns_unavailable() {
        let err = health_check(Some(RuntimeHandle(u64::MAX))).unwrap_err();
        match err {
            FfiError::Unavailable { subsystem } => {
                assert_eq!(subsystem, "evidence_store");
            }
            _ => panic!("expected Unavailable"),
        }
    }

    #[test]
    fn subsystem_status_serialises_snake_case() {
        let ok = serde_json::to_string(&SubsystemStatus::Ok).unwrap();
        let degraded = serde_json::to_string(&SubsystemStatus::Degraded).unwrap();
        let unavail = serde_json::to_string(&SubsystemStatus::Unavailable).unwrap();
        assert_eq!(ok, "\"ok\"");
        assert_eq!(degraded, "\"degraded\"");
        assert_eq!(unavail, "\"unavailable\"");
    }

    #[test]
    fn envelope_round_trips_through_serde() {
        let env = health_check(None).expect("infallible");
        let json = serde_json::to_string(&env).expect("serialise");
        let parsed: HealthStatus = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(parsed, env);
    }

    #[test]
    fn core_version_matches_cargo_pkg_version() {
        assert_eq!(core_version(), env!("CARGO_PKG_VERSION"));
    }
}
