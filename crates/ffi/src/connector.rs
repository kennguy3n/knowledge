//! Connector management FFI surface.
//!
//! Per `docs/technical/design.md` §10.2 and `docs/technical/architecture.md` §4.1, the
//! substrate ingests evidence from external systems through the
//! [`connector_framework::Connector`] trait. Each connector is
//! a real HTTP client (Google Drive REST v3, Notion API,
//! Slack Web API, …) wired through
//! [`connector_framework::BlockingHttpTransport`] (reqwest blocking
//! client with retry-with-backoff + Retry-After honouring) and
//! [`connector_framework::OAuth2Client`] for `authorization_code`
//! exchange.
//!
//! This module exposes eight FFI functions that mirror the
//! connector lifecycle:
//!
//! 1. [`create_connector`] — instantiate a connector for one source
//!    kind, binding it to a scope.
//! 2. [`authenticate_connector`] — run the OAuth2
//!    `authorization_code` exchange and stash the bearer token in
//!    the per-runtime [`OAuth2TokenVault`].
//! 3. [`refresh_connector_token`] — drive the OAuth2
//!    `grant_type=refresh_token` flow against the provider's token
//!    endpoint, persist the refreshed token to SQLCipher, and
//!    update the in-memory vault. Hosts call this on-demand (e.g.
//!    a scheduled job before a long-running batch sync); the
//!    [`sync_connector`] entry point also auto-invokes this path
//!    transparently when its snapshot of the cached token is close
//!    to expiry.
//! 4. [`sync_connector`] — run `initial_sync` or `incremental_sync`
//!    (chosen by [`SyncState::can_run_incremental`]), forward every
//!    emitted [`ConnectorEvent`] into the evidence store via
//!    [`EvidenceStore::ingest`], and advance the per-connector
//!    [`SyncState`].
//! 5. [`list_connectors`] — read the in-memory connector registry
//!    and surface a wire-flat [`ConnectorStatus`] row per instance.
//! 6. [`remove_connector`] — tear down a connector (drop the
//!    `Box<dyn Connector>`, drop the cached token, drop the
//!    `ConnectorInstance` row).
//! 7. [`set_oauth_client_secret_resolver`] — register a host-supplied
//!    [`OAuthClientSecretResolver`] callback that the substrate
//!    consults at every OAuth2 grant (both `authorization_code` and
//!    `refresh_token`) to fetch the `client_secret` from the host's
//!    keychain — production hosts use this to keep confidential
//!    credentials off the substrate's persisted state.
//! 8. [`clear_oauth_client_secret_resolver`] — unregister the
//!    previously-registered resolver. After this call the framework
//!    falls back to `auth_config_json["client_secret"]` (and then
//!    omits the form field) on subsequent grants.
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
//! All eight functions require a prior successful call to
//! [`crate::open_store`] (enforced by [`with_runtime`]) and operate
//! synchronously against the per-handle `Arc<Mutex<FfiRuntime>>`
//! mutex — connector calls against the same handle serialise, while
//! calls against different handles run in parallel. The two
//! resolver-management entry points
//! ([`set_oauth_client_secret_resolver`] /
//! [`clear_oauth_client_secret_resolver`]) hold the runtime mutex
//! only long enough to update the per-runtime
//! [`connector_framework::OAuth2Client`]'s resolver slot; they do
//! NOT call the resolver themselves.

use std::collections::HashSet;
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
    ConnectorHealthRecord, ConnectorKindTag, ConnectorStatus, RefreshReport, ScopeIdString,
    SyncModeKind, SyncReport, SyncStatusKind,
};

/// Default expiry skew used by [`sync_connector`]'s auto-refresh
/// path. If the snapshot's `OAuth2Token` is expiring within this
/// many seconds of `Utc::now()`, the runtime transparently runs
/// a refresh round-trip before dispatching the sync HTTP call.
///
/// Matches [`connector_framework::OAuth2TokenVault`]'s
/// `default_skew`. Hosts that need a different value for the
/// explicit refresh entry point can call
/// [`refresh_connector_token`] which forces a refresh regardless
/// of skew.
const AUTO_REFRESH_SKEW_SECS: i64 = 60;

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
/// At most one connector instance may exist per `(scope_id, kind)`
/// pair on a given runtime. A second call against the same scope
/// and kind is rejected with [`FfiError::Connector`] carrying the
/// `connector_framework::ConnectorError::DuplicateConnector`
/// message. Hosts that need to re-create (e.g. to reset the cached
/// config) must call [`remove_connector`] first. The product
/// decision behind this constraint: a single source — say one
/// Slack workspace — is bound to a single scope at a time, and
/// permitting multiple instances would silently double-ingest the
/// same upstream events on every sync.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `scope_id` has been cryptographically
///   forgotten via [`crate::forget_scope`].
/// * [`FfiError::Connector`] if `config_json` is not valid JSON.
/// * [`FfiError::Connector`] (carrying the `DuplicateConnector`
///   message) if another connector with the same `(scope_id, kind)`
///   pair is already registered on this runtime.
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
            // Uniqueness: reject if a connector of the same
            // (scope, kind) pair is already registered. Flattened
            // through the standard `ConnectorError::DuplicateConnector`
            // → `FfiError::Connector` mapping so hosts see the same
            // shape they get from any other framework-side error.
            if rt
                .connector_instances
                .values()
                .any(|inst| inst.config.scope_id == scope && inst.config.kind == kind_framework)
            {
                return Err(FfiError::from(
                    connector_framework::ConnectorError::DuplicateConnector,
                ));
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
            // Register the scope's DEK *before* writing the encrypted
            // connector row so the new scope gets a random per-scope
            // key from the OS RNG (via `ensure_scope_dek`, which uses
            // `rand::rngs::SysRng` per SECURITY.md §"Random number
            // generation") rather than the legacy HKDF fallback that
            // `EvidenceStore::scope_key`
            // would synthesise for an unregistered scope. Without
            // this, `create_connector` would be the first crypto
            // touch on a fresh scope and would silently bind every
            // future connector row to the HKDF-derived key — which
            // `forget_scope_state` can't truly destroy because the
            // derivation is deterministic from the master key.
            rt.ensure_scope_registered(scope)?;
            // Persist BEFORE the in-memory insert so a SQLCipher
            // write failure leaves the runtime exactly as it was
            // (no orphan in-memory row, no orphan persisted row).
            // The unique index on `(scope_id, kind)` pins the
            // single-instance-per-(scope, kind) contract at the DB
            // layer; the in-memory check above already rejected the
            // duplicate, so this insert cannot collide on the
            // unique constraint under normal operation. A collision
            // here would indicate a regression of the runtime check
            // (e.g. someone removing it without dropping the index)
            // and is surfaced as `FfiError::Evidence` so the host
            // sees a structured error rather than a panic.
            persist_connector_instance(rt, &instance)?;
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
        // ─────────────── Step 1: snapshot (locked) ───────────────
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
            // `lookup_connector_handle` returns `Unavailable` (not
            // `NotFound`) when the instance row rehydrated from
            // SQLCipher but the `Arc<dyn Connector>` could not be
            // rebuilt — matching the create-time error shape.
            let connector = lookup_connector_handle(rt, instance, &instance_id)?;
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
        // ─────────── Step 2: OAuth2 exchange (UNLOCKED) ────────────
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
        // ──────────────── Step 3: persist (locked) ────────────────
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
            let scope = match rt.connector_instances.get(&instance) {
                Some(inst) => inst.config.scope_id,
                None => {
                    return Err(FfiError::NotFound {
                        kind: "connector".into(),
                        id: instance_id.clone(),
                    })
                }
            };
            // Race with `forget_scope_state` on the same handle: if
            // the scope was forgotten during the unlocked dispatch,
            // the in-memory instance map is already empty (step 6 of
            // the helper drops every instance bound to the forgotten
            // scope), so the `get` above returns `None` and we bail.
            // Still re-check `is_scope_forgotten` here as defense in
            // depth
            // — a future refactor could decouple the in-memory map
            // from `forget_scope_state`'s sweep, and we want the
            // token to land on a removed-but-not-forgotten instance
            // (allowed) versus a forgotten-scope leftover (banned)
            // to keep behaving correctly.
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope.as_uuid().to_string(),
                });
            }
            // Persist BEFORE the in-memory `put` so a SQLCipher
            // write failure surfaces to the host before the token
            // becomes the substrate-side source of truth. The
            // already-completed OAuth2 token endpoint exchange in
            // cannot be undone — the access + refresh tokens
            // are valid against the provider — but at-rest persistence
            // is what carries the token across `close_store`/`open_store`,
            // so a host that observes `Ok(())` should be able to rely
            // on the persisted state surviving the restart.
            persist_connector_token(rt, instance, scope, &token)?;
            rt.token_vault.put(instance, token);
            Ok(())
        })
    })
}

/// Drive an OAuth2 `grant_type=refresh_token` round-trip against
/// the provider's token endpoint, persist the refreshed token to
/// SQLCipher, and update the per-runtime [`OAuth2TokenVault`].
///
/// This is the explicit, host-driven counterpart to the auto-refresh
/// path inside [`sync_connector`]. Hosts call this when they want
/// to refresh a token proactively — e.g. a scheduled job that runs
/// 30 minutes before a long batch sync, or a UI action that asks
/// "warm up the connector before I start using it". The auto-refresh
/// inside [`sync_connector`] handles the common case of "the token
/// is about to expire and we're about to need it", but is not a
/// substitute for the explicit refresh entry point which always
/// performs the refresh regardless of how fresh the cached token
/// appears to be.
///
/// The function follows the same three-phase locking discipline as
/// [`authenticate_connector`] (Step 1: snapshot under lock; Step 2:
/// unlocked HTTP refresh round-trip; Step 3: re-acquire lock,
/// re-validate the instance + scope are still alive, persist
/// BEFORE updating the in-memory vault). The runtime mutex is never
/// held for the duration of the provider's network call, so
/// concurrent FFI calls on the same handle (`query`,
/// `ingest_message`, …) continue to run.
///
/// On success returns a [`RefreshReport`] with `refreshed: true`
/// (the explicit entry point always refreshes — the `refreshed`
/// flag exists for the [`sync_connector`] auto-refresh callback
/// path which can short-circuit when the token is still fresh) and
/// `expires_at` reflecting the new token's expiry so hosts can
/// schedule the next refresh.
///
/// # Concurrency
///
/// Hosts MUST NOT issue concurrent
/// `refresh_connector_token` / `sync_connector` calls against the
/// same `instance_id`. The three-phase optimistic-locking pattern
/// (snapshot under lock → unlocked refresh → re-lock + persist)
/// intentionally releases the runtime mutex for the duration of
/// the provider's HTTP round-trip so concurrent FFI calls on the
/// same handle (`query`, `ingest_message`, …) continue to run.
/// For providers that rotate the `refresh_token` on every
/// `grant_type=refresh_token` response (e.g. Notion, Google's
/// rotated-RT mode, Slack's rotating refresh tokens), the
/// optimistic snapshot means two concurrent refreshes on the same
/// instance both capture the SAME `refresh_token`. Whichever call
/// completes its provider round-trip first wins; the second
/// receives `invalid_grant` because the snapshotted refresh token
/// was consumed and invalidated. The losing call surfaces
/// [`FfiError::Connector`] carrying the framework's `TokenRefresh`
/// diagnostic — the recovery is clean, the host retries (or
/// re-authorises if the provider returned `invalid_grant` for a
/// different reason), and the now-rotated token from the winning
/// call is already persisted to SQLCipher. Serialise
/// per-`instance_id` on the host side (e.g. a `Map<instance_id,
/// Promise>` of in-flight refreshes that subsequent calls await,
/// or an instance-scoped mutex) to avoid the race in the first
/// place.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::Unavailable { subsystem: "connector-http-client" }`]
///   if no real HTTP transport is linked in (the `http-client`
///   feature is off, or the per-runtime transport failed to build
///   at `open_store` time).
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid UUID.
/// * [`FfiError::NotFound`] (`kind: "connector"`) if `instance_id`
///   is unknown.
/// * [`FfiError::NotFound`] (`kind: "scope"`) if the connector's
///   scope was cryptographically forgotten between and
///   Step 3.
/// * [`FfiError::Connector`] (carrying the framework's
///   `TokenRefresh` diagnostic) if the provider rejects the
///   refresh grant — typically because the refresh token was
///   revoked or expired. The host should treat this as
///   "re-authorisation required" and prompt the user through
///   [`authenticate_connector`] rather than retrying the refresh.
/// * [`FfiError::Connector`] (`"no refresh_token stored …"`) if the
///   cached token was issued without a refresh token (Slack
///   legacy, PKCE-only public clients, etc.). Same recovery as
///   above — the host must drive a fresh `authorization_code`
///   exchange.
/// * [`FfiError::Evidence`] if the persist call to SQLCipher fails
///   in Step 3.
///
/// # Confidential-client support
///
/// The substrate resolves the `client_secret` form field through
/// a three-layer fallback ladder at grant-time (see the framework's
/// [`ClientSecretResolver`](connector_framework::ClientSecretResolver)
/// rustdoc):
///
/// 1. Host-supplied resolver registered via
///    [`set_oauth_client_secret_resolver`] (production path —
///    secret stays in the OS keychain).
/// 2. `auth_config_json["client_secret"]` (fallback for tests /
///    dev hosts; the secret persists encrypted under the per-scope
///    DEK in SQLCipher).
/// 3. Field omitted entirely (public-client / PKCE-only flows).
///
/// Both `authenticate_connector`'s `exchange_code` grant AND this
/// entry point's `refresh_token` grant share the per-runtime
/// [`OAuth2Client`](connector_framework::OAuth2Client) so the
/// resolver registration applies to both. Confidential-client
/// providers (Notion production, Google, Atlassian, Microsoft
/// Graph, HubSpot, …) work when EITHER the resolver returns a
/// secret OR `auth_config_json` carries one; public-client flows
/// (Slack PKCE-only) work against this entry point as-is with no
/// resolver registered.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn refresh_connector_token(
    handle: RuntimeHandle,
    instance_id: String,
) -> FfiResult<RefreshReport> {
    metrics::instrument(metrics::inc_refresh_connector_token, || {
        let instance = parse_instance_id(&instance_id)?;
        // ─────────────── Step 1: snapshot (locked) ───────────────
        //
        // Mirror `authenticate_connector` / `sync_connector` —
        // clone every owned value out of the runtime mutex so the
        // unlocked round-trip in `refresh_token_three_phase`
        // does NOT block concurrent FFI calls on the same handle.
        // `lookup_connector_handle` is still consulted so the host
        // sees the same `Unavailable` vs `NotFound` disambiguation
        // as the create / authenticate / sync paths when the
        // instance row rehydrated from SQLCipher but the
        // `Arc<dyn Connector>` could not be rebuilt.
        let (config, token) = with_runtime(handle, |rt| -> FfiResult<_> {
            let _ = lookup_connector_handle(rt, instance, &instance_id)?;
            let config = rt
                .connector_instances
                .get(&instance)
                .ok_or_else(|| FfiError::NotFound {
                    kind: "connector".into(),
                    id: instance_id.clone(),
                })?
                .config
                .clone();
            if rt.is_scope_forgotten(config.scope_id) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: config.scope_id.as_uuid().to_string(),
                });
            }
            let token = rt
                .token_vault
                .get(instance)
                .map_err(FfiError::from)?
                .clone();
            Ok((config, token))
        })?;
        // ── Step 2+3: refresh + persist (delegated helper) ──
        //
        // Force the refresh regardless of expiry by passing
        // `skew: None` (force-refresh mode). The helper skips the
        // `is_expiring_within` check entirely so the explicit
        // entry point's "always refreshes" contract is unbreakable
        // regardless of `current_token.expires_at` — no risk that
        // a far-future expiry (e.g. a misconfigured provider
        // returning a 100+ year TTL) silently short-circuits a
        // host-driven refresh.
        //
        // The "did we actually refresh?" question is only
        // meaningful for the `sync_connector` auto-refresh path
        // where the skew is `Some(AUTO_REFRESH_SKEW_SECS)`; here
        // a `false` from the helper would indicate a contract
        // mismatch with the docs above, so we surface it through
        // the `refreshed` flag for diagnostic clarity rather than
        // asserting (in practice, force-refresh mode always
        // returns `true` because the helper never short-circuits).
        //
        // `now` is unused inside the helper under force-refresh
        // mode (the only consumer was `is_expiring_within`, which
        // we skip), but we still pass `Utc::now()` so the helper's
        // signature is identical across both callers. The
        // post-round-trip `RefreshReport::refreshed_at` timestamp
        // (line below) is captured separately AFTER the helper
        // returns so it honestly reflects when the round-trip
        // completed — per `RefreshReport`'s doc contract — rather
        // than when the FFI call entered the substrate. For a
        // multi-second network call the two timestamps can diverge
        // noticeably and hosts use `refreshed_at` for correlation
        // / scheduling, so the post-round-trip stamp is the honest
        // one to surface.
        let (new_token, refreshed) = refresh_token_three_phase(
            handle,
            instance,
            &instance_id,
            token,
            &config,
            None,
            Utc::now(),
        )?;
        Ok(RefreshReport {
            instance_id: instance.0.to_string(),
            refreshed,
            expires_at: new_token.expires_at.timestamp(),
            refreshed_at: Utc::now().timestamp(),
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
/// Before the HTTP dispatch (the unlocked phase in the locking
/// sequence — see the module-level docs), `sync_connector`
/// transparently runs the
/// same three-phase refresh path as [`refresh_connector_token`]
/// when the cached OAuth2 token is within `AUTO_REFRESH_SKEW_SECS`
/// of expiry. This recovers from the
/// long-`close_store`/`open_store` case where the rehydrated token
/// has lapsed while the substrate was closed — Notion / Slack /
/// Atlassian / Google Drive access tokens have ~1h TTLs by default,
/// so any substrate that is closed overnight rehydrates with a
/// stale token on the next morning's first sync. Auto-refresh
/// failures (`TokenRefresh` from the provider, no `refresh_token`
/// stored, transport failure, …) surface as
/// [`FfiError::Connector`] with the per-instance state rolled to
/// [`SyncStatus::Failed`] exactly like an HTTP dispatch failure.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::NotFound`] if `instance_id` is unknown or the
///   connector has not been [`authenticate_connector`]-ed (no token
///   in the vault).
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid UUID.
/// * [`FfiError::Connector`] if the auto-refresh path tripped
///   (provider rejected the refresh grant — host should drive a
///   fresh [`authenticate_connector`] — or the cached token has
///   no `refresh_token`). The per-connector `SyncState` is also
///   marked `Failed` and persisted so subsequent `list_connectors`
///   calls surface the diagnostic across `close_store`/`open_store`.
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
        // ─────────────── Step 1: snapshot (locked) ───────────────
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
                // owns the dispatch (and will earlier-ingest its
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
                        message: format!("sync_connector: another sync is already in progress for connector instance {instance_id} \
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
            // `lookup_connector_handle` returns `Unavailable` (not
            // `NotFound`) when the instance rehydrated from SQLCipher
            // but the `Arc<dyn Connector>` could not be rebuilt.
            let connector = lookup_connector_handle(rt, instance, &instance_id)?;
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
        // ──────── Step 2a: auto-refresh (delegated helper) ────────
        //
        // Before the HTTP dispatch, transparently refresh the
        // cached OAuth2 access token if it is expiring within
        // `AUTO_REFRESH_SKEW_SECS` of `Utc::now()`. The helper
        // short-circuits when the token is still fresh, so the
        // typical "sync against a fresh token" path pays zero
        // overhead (no network round-trip, no SQLCipher write).
        //
        // This recovers from the long-`close_store`/`open_store`
        // case where the rehydrated token has lapsed while the
        // substrate was closed (Notion/Slack/Atlassian/GDrive
        // access tokens have ~1h TTLs by default). Without this
        // hook, the first `sync_connector` after a long close
        // would fail at the connector's `incremental_sync` with
        // a generic `invalid_token` 401 from the provider, and
        // the host would have to know to translate that into a
        // refresh request. Wiring auto-refresh into the substrate
        // means the host sees `SyncReport` not
        // `FfiError::Connector` in that flow.
        //
        // Failure path: a TokenRefresh error (provider rejected
        // the refresh grant, no refresh_token in the cached
        // bundle, transport failure, …) rolls the per-instance
        // state to `Failed` exactly like the HTTP dispatch failure
        // path below — the host's recovery contract is uniform
        // regardless of which phase tripped. The persisted Failed
        // status survives `close_store`/`open_store` so the
        // diagnostic is observable through `list_connectors`
        // post-restart.
        let mut snapshot = snapshot;
        // `skew: Some(AUTO_REFRESH_SKEW_SECS)` = auto-refresh mode.
        // The helper short-circuits BEFORE any clone of
        // `snapshot.config` is taken when the cached token is
        // still outside the skew window — zero allocation cost on
        // the typical "sync against a fresh token" hot path.
        match refresh_token_three_phase(
            handle,
            instance,
            &instance_id,
            snapshot.token.clone(),
            &snapshot.config,
            Some(chrono::Duration::seconds(AUTO_REFRESH_SKEW_SECS)),
            Utc::now(),
        ) {
            Ok((new_token, _was_refreshed)) => {
                snapshot.token = new_token;
            }
            Err(err) => {
                let msg = err.to_string();
                let _ = with_runtime(handle, |rt| {
                    if let Some(inst) = rt.connector_instances.get_mut(&instance) {
                        inst.sync_state.mark_failed(&msg);
                    }
                    let instance_snapshot =
                        rt.connector_instances.get(&instance).map(|i| (*i).clone());
                    if let Some(inst_clone) = instance_snapshot {
                        if let Err(persist_err) = persist_connector_instance(rt, &inst_clone) {
                            tracing::warn!(instance = %instance,
                                error = %persist_err,
                                "failed to persist sync_state Failed status after auto-refresh failure; in-memory state still updated",
                            );
                        }
                    }
                    Ok(())
                });
                return Err(err);
            }
        }
        // ──────────────── Step 2: dispatch (UNLOCKED) ────────────────
        //
        // Drive the connector's HTTP round-trip with the runtime
        // mutex released. Concurrent FFI calls against the same
        // handle (queries, memory reads, sync against a *different*
        // connector instance) run in parallel with this network
        // call. A second `sync_connector` against the **same**
        // instance is rejected in with
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
                    // Best-effort flush of the Failed row so the
                    // post-restart `list_connectors` report reflects
                    // the failure. Persist errors are logged but do
                    // not mask the original transport error returned
                    // to the host — the in-memory `mark_failed` has
                    // already run so `sync_connector` retries observe
                    // the same Failed state.
                    let instance_snapshot =
                        rt.connector_instances.get(&instance).map(|i| (*i).clone());
                    if let Some(inst_clone) = instance_snapshot {
                        if let Err(persist_err) = persist_connector_instance(rt, &inst_clone) {
                            tracing::warn!(instance = %instance,
                                error = %persist_err,
                                "failed to persist sync_state Failed status after dispatch failure; in-memory state still updated",
                            );
                        }
                    }
                    Ok(())
                });
                return Err(FfiError::from(err));
            }
        };
        // ─────────────── Step 3: persist (locked) ────────────────
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
                        // Stamp the BCP-47 primary subtag on
                        // each ingested connector
                        // event. Connector events serialise as
                        // a small JSON shell (`kind` +
                        // `document_id` + `occurred_at`) today,
                        // so `detect_language` will typically
                        // return `None` and the column stays
                        // NULL — that's the correct "language
                        // unknown" outcome for the current
                        // event shape. When provider events
                        // later carry richer natural-language
                        // payloads (e.g. Slack message bodies,
                        // Notion page excerpts), the same
                        // call site picks up the detected tag
                        // without further plumbing.
                        let detection = observation_engine::detect_language(&body);
                        let language_tag = detection.as_ref().map(|d| d.tag.as_str());
                        let result = rt
                            .store_mut()
                            .ingest_with_language(
                                snapshot.scope,
                                body.as_bytes(),
                                Some(source_tag),
                                ImportanceClass::Important,
                                language_tag,
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
                // Persist the advanced sync state so the cursor
                // survives `close_store` / `open_store`. We clone the
                // instance for the persist call to avoid holding an
                // immutable borrow on `rt.connector_instances` across
                // the `&mut rt.store` access inside
                // `persist_connector_instance`. A failure here rolls
                // the in-memory state to `Failed` (handled by the
                // outer match arm below) so the host sees the
                // persistence gap rather than a "succeeded" run whose
                // cursor was lost on restart.
                let instance_snapshot = rt.connector_instances.get(&instance).map(|i| (*i).clone());
                if let Some(inst_clone) = instance_snapshot {
                    persist_connector_instance(rt, &inst_clone)?;
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
                    // Best-effort flush of the failed-status row so
                    // `list_connectors` post-restart reports the
                    // failure rather than silently rewinding to the
                    // last persisted Succeeded state. A persistence
                    // failure here is logged but does not mask the
                    // original error returned to the host (which is
                    // what they need to retry against).
                    let instance_snapshot =
                        rt.connector_instances.get(&instance).map(|i| (*i).clone());
                    if let Some(inst_clone) = instance_snapshot {
                        if let Err(persist_err) = persist_connector_instance(rt, &inst_clone) {
                            tracing::warn!(instance = %instance,
                                error = %persist_err,
                                "failed to persist sync_state Failed status; in-memory state still updated",
                            );
                        }
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
/// [`sync_connector`]'s and consumed by the unlocked
/// dispatch in + the locked persist in Step 3.
///
/// Owning every field (no borrows back into `FfiRuntime`) is what
/// lets the mutex drop between and — see the
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

/// Single-instance health probe — symmetric with
/// [`crate::synthesis::synthesis_status`]. Returns a wire-flat
/// [`ConnectorHealthRecord`] that bundles the per-connector
/// `ConnectorStatus` view (kind, scope, sync mode / status,
/// last-synced timestamp, last error) WITH the scheduler-side
/// posture (effective interval / max backoff, auto-synthesize
/// flag, consecutive failures, next-attempt-at, cooldown flag).
///
/// This closes the symmetric gap the connector framework had
/// against the synthesis subsystem: synthesis already had a
/// single-window probe ([`crate::synthesis::synthesis_status`]),
/// but the only per-connector probe was [`list_connectors`] which
/// requires a linear scan + manual scheduler join when the host
/// just wants the state of one instance.
///
/// The scheduler-side fields gracefully degrade when no scheduler
/// is running: `is_scheduled=false`, `sync_interval_secs=0`,
/// `max_backoff_secs=0`, `auto_synthesize=false`,
/// `consecutive_failures=0`, `next_attempt_unix=None`,
/// `in_cooldown=false`. The `auto_synthesize` flag does NOT
/// survive a `stop_sync_scheduler` / `start_sync_scheduler`
/// cycle — the per-instance `SchedulePolicy` table lives inside
/// the `RunningSyncScheduler` value and is dropped together with
/// it on stop. Hosts that need the flag to persist across
/// restarts must re-apply
/// [`crate::sync_scheduler::configure_sync_auto_synthesize`]
/// after each `start_sync_scheduler`. (The same applies to
/// per-instance interval / max-backoff overrides set via
/// [`crate::sync_scheduler::configure_sync_schedule`].)
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid
///   UUID. Mirrors [`parse_instance_id`]'s contract.
/// * [`FfiError::NotFound`] with `kind = "connector_instance"`
///   if the parsed UUID is not present in
///   [`FfiRuntime::connector_instances`] (the host called
///   [`remove_connector`] or never created one with this id).
/// * [`FfiError::NotFound`] with `kind = "scope"` if the instance
///   exists but its bound scope has been tombstoned by
///   [`crate::forget_scope`] — matching the same
///   tombstoned-scope-shielding behavior the other connector
///   surfaces ([`sync_connector`],
///   [`authenticate_connector`]) apply.
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn connector_status(
    handle: RuntimeHandle,
    instance_id: String,
) -> FfiResult<ConnectorHealthRecord> {
    metrics::instrument(metrics::inc_connector_status, || {
        let instance = parse_instance_id(&instance_id)?;
        with_runtime(handle, |rt| {
            let inst = rt
                .connector_instances
                .get(&instance)
                .ok_or_else(|| FfiError::NotFound {
                    kind: "connector_instance".into(),
                    id: instance_id.clone(),
                })?;
            // Shield tombstoned-scope leftovers — the connector
            // row may still be hanging around after a partial
            // `forget_scope` (the SQLCipher delete cascade is
            // best-effort, see the matching guard in
            // `sync_connector` ). Surfacing a stale
            // connector row past tombstoning would let the host
            // act on a logically-removed scope.
            if rt.is_scope_forgotten(inst.config.scope_id) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: inst.config.scope_id.as_uuid().to_string(),
                });
            }
            let scheduler_snapshot =
                crate::sync_scheduler::instance_scheduler_snapshot(rt, instance);
            let in_cooldown = scheduler_snapshot.is_scheduler_running
                && scheduler_snapshot.consecutive_failures > 0;
            Ok(ConnectorHealthRecord {
                instance_id: inst.id.0.to_string(),
                kind: framework_kind_to_ffi(inst.config.kind),
                scope_id: inst.config.scope_id.as_uuid().to_string(),
                sync_mode: framework_sync_mode_to_kind(inst.sync_state.mode),
                sync_status: framework_sync_status_to_kind(inst.sync_state.status),
                last_synced_at: inst.sync_state.last_synced_at.map(|t| t.timestamp()),
                last_error: inst.sync_state.last_error.clone(),
                is_scheduled: scheduler_snapshot.is_scheduler_running,
                sync_interval_secs: scheduler_snapshot.sync_interval_secs,
                max_backoff_secs: scheduler_snapshot.max_backoff_secs,
                auto_synthesize: scheduler_snapshot.auto_synthesize,
                consecutive_failures: scheduler_snapshot.consecutive_failures,
                next_attempt_unix: scheduler_snapshot.next_attempt_unix,
                in_cooldown,
            })
        })
    })
}

/// Tear down the connector with `instance_id` — drop the live
/// `Arc<dyn Connector>`, remove the [`ConnectorInstance`] row, drop
/// the cached OAuth2 token from the vault, and delete the persisted
/// `connector_instances` / `connector_tokens` SQLCipher rows.
/// Idempotent within the lifetime of a single runtime: removing an
/// unknown id is a no-op and returns `Ok(())`.
///
/// The persisted-row deletes are best-effort — if the SQLCipher
/// delete fails, the in-memory state is still cleared (callers
/// observing `Ok(())` will see the connector gone from the next
/// `list_connectors` call) and the dangling row will be picked up
/// either on the next `remove_connector` retry or, in the
/// scope-forgetting case, by the cryptographic-forgetting contract
/// (the row's AEAD payload becomes unrecoverable when the scope DEK
/// is destroyed). A persistence failure here returns
/// [`FfiError::Evidence`] so the host can decide whether to retry.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not been
///   called.
/// * [`FfiError::InvalidId`] if `instance_id` is not a valid UUID.
/// * [`FfiError::Evidence`] if the persisted-row delete fails.
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
            // Drop any sync-scheduler policy + accounting state for
            // this instance so the scheduler's `state.policies` /
            // `state.accounting` maps don't accumulate stale entries
            // across a long-running substrate process. The hook is
            // a no-op when no scheduler is running. Inside the
            // `with_runtime` closure so the canonical
            // runtime-mutex → scheduler-state-mutex acquisition
            // order documented at `sync_scheduler::run_one_tick`
            // is preserved.
            crate::sync_scheduler::prune_instance(rt, instance);
            // Persisted-row deletes. Both are `DELETE … WHERE
            // instance_id = ?` so a missing row is a no-op — exactly
            // matching the idempotency contract.
            rt.store()
                .delete_connector_instance(instance.0)
                .map_err(|e| FfiError::Evidence {
                    message: format!("delete_connector_instance failed: {e}"),
                })?;
            rt.store()
                .delete_connector_token(instance.0)
                .map_err(|e| FfiError::Evidence {
                    message: format!("delete_connector_token failed: {e}"),
                })?;
            Ok(())
        })
    })
}

// ──────────────── OAuth2 client-secret resolver ────────────────

/// Host-implemented callback that the substrate consults at every
/// OAuth2 grant to fetch the matching `client_secret` from the
/// host's keychain.
///
/// Hosts implement this on their side of the FFI boundary (Swift /
/// Kotlin via UniFFI's callback-trait infrastructure; JS via the
/// N-API binding's [`JsFunction`]-backed adapter), then register
/// the implementation against an open runtime by calling
/// [`set_oauth_client_secret_resolver`]. The substrate stores the
/// callback on the per-runtime [`OAuth2Client`] (interior-mutable
/// resolver slot) so every connector on the same runtime shares one
/// resolver registration — the host only needs to wire this up
/// once per `open_store` lifecycle.
///
/// # Calling discipline
///
/// `resolve` is invoked on the thread that drives the grant — the
/// host's thread driving `authenticate_connector` /
/// `refresh_connector_token`, or the `sync_connector` worker
/// thread. The runtime mutex is NOT held during the call (the FFI
/// substrate's three-phase locking pattern guarantees this), so
/// implementations are free to do their own synchronisation /
/// async work, but they MUST be cheap — the recommended pattern is
/// to populate an in-memory cache from the host's keychain at
/// startup and answer `resolve` from the cache.
///
/// # Return value semantics
///
/// * `Some(secret)` — the resolver has produced a `client_secret`
///   for the `(kind, scope_id, client_id)` tuple. The framework
///   uses it verbatim and skips the lower fallback layers.
/// * `Some("")` — explicit "no-secret" choice. The framework omits
///   the `client_secret` form field entirely AND short-circuits
///   the lower fallback layers (this lets a host pin a specific
///   `(scope_id, client_id)` to public-client mode even when
///   `auth_config_json` happens to carry a secret).
/// * `None` — defer to the next layer (`auth_config_json
///   ["client_secret"]`, then the static `with_client_secret`
///   value, then field-omitted).
///
/// # Threading
///
/// The trait requires `Send + Sync` because the framework's
/// `OAuth2Client` is shared across connector workers. UniFFI
/// enforces this on the foreign side (Swift `@unchecked Sendable`
/// / Kotlin `@Synchronized` patterns); the N-API adapter wraps the
/// host's `JsFunction` in a `Mutex`-guarded slot so the JS engine
/// only sees one in-flight call at a time even when multiple
/// connector workers race.
#[uniffi::export(with_foreign)]
pub trait OAuthClientSecretResolver: Send + Sync {
    /// Resolve the `client_secret` for an upcoming OAuth2 grant
    /// against the substrate's per-runtime `OAuth2Client`. See
    /// the trait-level docs for the return-value semantics.
    fn resolve(&self, kind: String, scope_id: String, client_id: String) -> Option<String>;
}

/// Adapter wrapping the foreign callback trait so it satisfies the
/// `connector_framework::ClientSecretResolver` trait contract. The
/// framework layer is bound-incompatible with UniFFI's generated
/// trait (the framework trait takes `&str` arguments for zero
/// allocation on the hot path; the FFI trait takes `String` for
/// UniFFI marshalling compatibility), so we bridge once at the FFI
/// boundary.
///
/// Only used when the `http-client` feature is on — without it,
/// the per-runtime `OAuth2Client` is absent and
/// `set_oauth_client_secret_resolver` short-circuits with
/// `Unavailable` before constructing the adapter.
#[cfg(feature = "http-client")]
struct FfiClientSecretResolverAdapter {
    inner: Arc<dyn OAuthClientSecretResolver>,
}

#[cfg(feature = "http-client")]
impl connector_framework::ClientSecretResolver for FfiClientSecretResolverAdapter {
    fn resolve(&self, kind: &str, scope_id: &str, client_id: &str) -> Option<String> {
        // Allocate fresh `String` copies because the foreign trait
        // takes owned strings — UniFFI's marshalling layer takes
        // ownership of the value as it crosses the language
        // boundary, so we can't loan `&str` here even though the
        // framework gave us borrowed input.
        self.inner.resolve(
            kind.to_string(),
            scope_id.to_string(),
            client_id.to_string(),
        )
    }
}

/// Register a host-supplied [`OAuthClientSecretResolver`] against
/// `handle`'s per-runtime [`OAuth2Client`].
///
/// After this call, every OAuth2 grant (both
/// `authentication_code` via [`authenticate_connector`] and
/// `refresh_token` via [`refresh_connector_token`] or
/// [`sync_connector`]'s auto-refresh path) consults the resolver
/// before falling through to `auth_config_json["client_secret"]`
/// and the static `OAuth2Client::with_client_secret` value (see
/// the framework's `ClientSecretResolver` rustdoc for the full
/// resolution ladder).
///
/// Calling this multiple times REPLACES the previously-registered
/// resolver; the substrate holds at most one resolver per
/// runtime. Production hosts typically call this exactly once
/// per `open_store` lifecycle. If the host wants different
/// resolver behaviour per `(kind, scope_id, client_id)`, the
/// resolver implementation itself should branch — registering a
/// new resolver per call is a sign the host is treating the
/// resolver as request-scoped, which is the wrong abstraction
/// (the metrics expose `set_oauth_client_secret_resolver_total`
/// so operators can spot this).
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called, OR the build was compiled without the
///   `http-client` feature (no `OAuth2Client` exists to receive
///   the registration).
#[uniffi::export]
pub fn set_oauth_client_secret_resolver(
    handle: RuntimeHandle,
    resolver: Arc<dyn OAuthClientSecretResolver>,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_set_oauth_client_secret_resolver, || {
        with_runtime(handle, |rt| {
            #[cfg(feature = "http-client")]
            {
                let client = rt
                    .oauth_client
                    .as_ref()
                    .ok_or_else(|| FfiError::Unavailable {
                        subsystem: "connector-http-client".into(),
                    })?;
                client.set_resolver(Arc::new(FfiClientSecretResolverAdapter { inner: resolver }));
                Ok(())
            }
            #[cfg(not(feature = "http-client"))]
            {
                // Reference the inputs so they're not flagged as
                // unused on `--no-default-features` builds; the
                // `Arc<dyn OAuthClientSecretResolver>` drop here is
                // the only observable side effect.
                let _ = (rt, resolver);
                Err(FfiError::Unavailable {
                    subsystem: "connector-http-client".into(),
                })
            }
        })
    })
}

/// Unregister the previously-registered
/// [`OAuthClientSecretResolver`].
///
/// After this call, subsequent OAuth2 grants fall through to
/// `auth_config_json["client_secret"]` and the static
/// `OAuth2Client::with_client_secret` value (see the framework's
/// `ClientSecretResolver` rustdoc). Hosts call this on keychain-
/// locked events, on sign-out, or before tearing down their
/// resolver implementation.
///
/// Calling this when no resolver is registered is a no-op (no
/// error); the substrate holds the resolver slot in an `Option`
/// that simply becomes `None` again.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called, OR the build was compiled without the
///   `http-client` feature (no `OAuth2Client` exists to clear).
#[uniffi::export]
pub fn clear_oauth_client_secret_resolver(handle: RuntimeHandle) -> FfiResult<()> {
    metrics::instrument(metrics::inc_clear_oauth_client_secret_resolver, || {
        with_runtime(handle, |rt| {
            #[cfg(feature = "http-client")]
            {
                let client = rt
                    .oauth_client
                    .as_ref()
                    .ok_or_else(|| FfiError::Unavailable {
                        subsystem: "connector-http-client".into(),
                    })?;
                client.clear_resolver();
                Ok(())
            }
            #[cfg(not(feature = "http-client"))]
            {
                let _ = rt;
                Err(FfiError::Unavailable {
                    subsystem: "connector-http-client".into(),
                })
            }
        })
    })
}

// ──────────────────────────── Internals ──────────────────────────────

/// Resolve the live `Arc<dyn Connector>` for `instance`, distinguishing
/// the three failure modes the host needs to disambiguate:
///
/// * **Live handle present** → returns `Ok(Arc::clone)` (one atomic
///   refcount bump).
/// * **Instance row exists but no live handle** → returns
///   `Err(Unavailable { subsystem: "connector" })`. This happens
///   when [`rehydrate_connectors`] loaded the persisted
///   `(config, sync_state)` blob but [`build_connector`] returned
///   `Unavailable` (e.g. the `http-client` feature is off, or the
///   runtime's shared `BlockingHttpTransport` failed to construct at
///   `open_store` time on an exotic platform). The persisted state
///   is still observable through [`list_connectors`] and the host
///   can re-`create_connector` to rebuild the handle once the
///   transport is restored, so surfacing this as a structured
///   *unavailability* — not *missing* — matches the create-time
///   error path documented at lines 114–116 and 783–786 of this
///   file.
/// * **Instance row absent** → returns `Err(NotFound)`. This is the
///   "host referenced an instance that was never created or has
///   been removed" case.
///
/// Both call sites (`authenticate_connector` and
/// `sync_connector` ) route through this helper so the
/// asymmetry between the create path (Unavailable on missing
/// transport) and the post-rehydrate path (was NotFound, now also
/// Unavailable) is eliminated.
fn lookup_connector_handle(
    rt: &FfiRuntime,
    instance: ConnectorInstanceId,
    instance_id_display: &str,
) -> FfiResult<Arc<dyn Connector>> {
    if let Some(handle) = rt.connectors.get(&instance) {
        return Ok(Arc::clone(handle));
    }
    if rt.connector_instances.contains_key(&instance) {
        return Err(FfiError::Unavailable {
            subsystem: "connector".into(),
        });
    }
    Err(FfiError::NotFound {
        kind: "connector".into(),
        id: instance_id_display.to_string(),
    })
}

/// Drive the three-phase refresh-and-persist sequence for the
/// OAuth2 token bound to `instance`.
///
/// The caller is responsible for (capturing
/// `current_token` + `config` under the runtime mutex and then
/// dropping the lock). This helper owns (unlocked refresh
/// round-trip against the provider) and (re-acquire the
/// lock, re-validate the instance + scope, persist the refreshed
/// token to SQLCipher, update the in-memory vault).
///
/// Two refresh modes:
///
/// * `skew: Some(s)` — **auto-refresh mode.** Skips the refresh
///   entirely (returns the caller's `current_token` unchanged with
///   `refreshed == false`) when
///   `!current_token.is_expiring_within(now, s)`. Used by the
///   [`sync_connector`] auto-refresh hook so the typical hot path
///   (token still fresh) pays zero overhead.
/// * `skew: None` — **force-refresh mode.** Always performs the
///   refresh round-trip regardless of `current_token.expires_at`.
///   Used by the explicit [`refresh_connector_token`] entry point
///   so the contract ("always refreshes") is literally unbreakable
///   — no risk of a far-future `expires_at` (e.g. a misconfigured
///   provider returning 100+ year TTL) silently short-circuiting
///   the host-driven refresh.
///
/// Three-phase locking discipline (matches
/// `authenticate_connector`):
///
/// * **Step 2 (here, UNLOCKED):** call
///   [`ConfiguredRefresher::refresh`] (via the
///   [`TokenRefresher`](connector_framework::TokenRefresher) trait
///   object) — the provider's network round-trip happens with the
///   runtime mutex released so concurrent FFI calls on the same
///   handle (`query`, `ingest_message`, …) keep running.
/// * **Step 3 (here, LOCKED):** re-acquire the mutex, re-check
///   the instance + scope are still alive (TOCTOU: a concurrent
///   [`forget_scope`](crate::forget_scope) or
///   [`remove_connector`] may have removed the row during the
///   unlocked ). Persist the refreshed token to SQLCipher
///   BEFORE updating the in-memory vault — mirrors
///   `authenticate_connector`'s discipline so a SQLCipher write
///   failure surfaces to the host before the vault becomes the
///   substrate-side source of truth.
///
/// Hot tokens (still expiring outside `skew`, in auto-refresh
/// mode) short-circuit before the OAuth2 client is built AND
/// before any clone of `config` is taken — so the typical "sync
/// against a fresh token" path in [`sync_connector`] pays zero
/// network overhead AND zero allocation overhead when no refresh
/// is needed.
///
/// # Errors
///
/// * [`FfiError::Connector`] (`"no refresh_token stored …"`) if the
///   cached token has `refresh_token = None` — re-authorisation is
///   required, the substrate refuses to POST `refresh_token=` to
///   the provider's token endpoint (every compliant provider
///   rejects that with `invalid_grant`, so the substrate-side
///   short-circuit returns a more actionable diagnostic instead).
/// * [`FfiError::Connector`] (carrying the framework's
///   [`ConnectorError::TokenRefresh`](connector_framework::ConnectorError)
///   diagnostic) if the provider rejects the refresh grant.
/// * [`FfiError::NotFound`] (`kind: "connector" | "scope"`) if
///   the instance was removed or the scope was forgotten during
///   the unlocked round-trip.
/// * [`FfiError::Unavailable`] (`subsystem: "connector-http-client"`)
///   if the per-runtime [`OAuth2Client`] was not built (no
///   `http-client` feature, or transport construction failed at
///   `open_store` time — the `not(http-client)` variant of this
///   function unconditionally returns this error).
/// * [`FfiError::Evidence`] if the SQLCipher persist call fails.
#[cfg(feature = "http-client")]
fn refresh_token_three_phase(
    handle: RuntimeHandle,
    instance: ConnectorInstanceId,
    instance_id_display: &str,
    current_token: connector_framework::OAuth2Token,
    config: &ConnectorConfig,
    skew: Option<chrono::Duration>,
    now: DateTime<Utc>,
) -> FfiResult<(connector_framework::OAuth2Token, bool)> {
    // Auto-refresh mode: short-circuit when the token is still
    // fresh, BEFORE any clone of `config` is taken. Force-refresh
    // mode (`skew == None`) always falls through to the refresh
    // round-trip below — the contract on the explicit
    // `refresh_connector_token` entry point is unbreakable.
    if let Some(s) = skew {
        if !current_token.is_expiring_within(now, s) {
            return Ok((current_token, false));
        }
    }
    // No refresh token in the cached bundle → re-auth required.
    // Mirrors `OAuth2TokenVault::refresh_if_expiring`'s rationale:
    // refusing to POST `refresh_token=` to the provider is strictly
    // better than letting it come back as a generic `invalid_grant`,
    // since the substrate-side message names the actionable recovery
    // path ("re-authorisation required") while the provider's
    // response would not.
    let refresh_secret = match current_token.refresh_token.as_ref() {
        Some(s) => s.expose().to_string(),
        None => {
            return Err(FfiError::Connector {
                message: format!("cannot refresh connector token: no refresh_token stored for instance {instance_id_display} \
                     — re-authorisation required",
                ),
            });
        }
    };
    // Build the refresher under the runtime lock. This is cheap —
    // an `Arc<OAuth2Client>::clone` + an `OAuth2Client::clone`
    // whose Clone impl is itself an `Arc` refcount bump on the
    // shared transport plus a fresh allocation for the
    // `SecretToken`-wrapped client secret. We deliberately do this
    // here (not inside ) so the unlocked refresh below
    // doesn't have to revisit the mutex just to grab the client
    // handle.
    //
    // `config` is cloned INSIDE this `with_runtime` closure (not
    // at the call site) so the auto-refresh hot path in
    // `sync_connector` — which short-circuits above when the
    // token is still fresh — does NOT pay the clone cost. We only
    // allocate when an actual refresh is happening.
    //
    // `scope` (Copy) is read off `config` for Step 3's
    // `is_scope_forgotten(scope)` re-check, which guards against a
    // concurrent `forget_scope` running while the unlocked refresh
    // is in flight.
    let scope = config.scope_id;
    let refresher: connector_framework::ConfiguredRefresher<
        connector_framework::BlockingHttpTransport,
    > = with_runtime(handle, |rt| {
        let client_arc = rt
            .oauth_client
            .as_ref()
            .ok_or_else(|| FfiError::Unavailable {
                subsystem: "connector-http-client".into(),
            })?;
        Ok(connector_framework::ConfiguredRefresher::new(
            (**client_arc).clone(),
            config.clone(),
        ))
    })?;
    // ─────────── Step 2: refresh (UNLOCKED) ────────────
    let refreshed = <connector_framework::ConfiguredRefresher<_> as connector_framework::TokenRefresher>::refresh(&refresher,
        &refresh_secret,
    )
    .map_err(FfiError::from)?;
    // ─── Step 3: persist + vault.put (LOCKED) ───
    let mut new_token = current_token;
    new_token.access_token = refreshed.access_token;
    if let Some(rt_tok) = refreshed.refresh_token {
        new_token.refresh_token = Some(rt_tok);
    }
    new_token.expires_at = refreshed.expires_at;
    if let Some(s) = refreshed.scope {
        new_token.scope = s;
    }
    with_runtime(handle, |rt| {
        if !rt.connector_instances.contains_key(&instance) {
            return Err(FfiError::NotFound {
                kind: "connector".into(),
                id: instance_id_display.to_string(),
            });
        }
        if rt.is_scope_forgotten(scope) {
            return Err(FfiError::NotFound {
                kind: "scope".into(),
                id: scope.as_uuid().to_string(),
            });
        }
        // Persist BEFORE the in-memory `put` (mirrors
        // `authenticate_connector` line 354) so a SQLCipher write
        // failure surfaces to the host before the vault becomes
        // the substrate-side source of truth. The just-completed
        // refresh round-trip in cannot be undone — the
        // provider has already minted the new access + refresh
        // tokens — but at-rest persistence is what carries the
        // token across `close_store`/`open_store`, so a host that
        // observes `Ok(_)` should be able to rely on the persisted
        // state surviving the restart.
        persist_connector_token(rt, instance, scope, &new_token)?;
        rt.token_vault.put(instance, new_token.clone());
        Ok(())
    })?;
    Ok((new_token, true))
}

/// `http-client`-off fallback for [`refresh_token_three_phase`].
/// Mirrors [`build_connector`]'s discipline: the signature is
/// identical across cfg arms so callers don't have to gate every
/// call site, and the unconditional `Unavailable` return surfaces
/// the same recovery contract a host gets from any other connector
/// call on a `not(http-client)` build.
#[cfg(not(feature = "http-client"))]
#[allow(clippy::needless_pass_by_value)] // signature matches the http-client-enabled variant for branch-free callers
fn refresh_token_three_phase(
    _handle: RuntimeHandle,
    _instance: ConnectorInstanceId,
    _instance_id_display: &str,
    _current_token: connector_framework::OAuth2Token,
    _config: &ConnectorConfig,
    _skew: Option<chrono::Duration>,
    _now: DateTime<Utc>,
) -> FfiResult<(connector_framework::OAuth2Token, bool)> {
    Err(FfiError::Unavailable {
        subsystem: "connector-http-client".into(),
    })
}

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
        AirtableConnector, AmazonAeConnector, AsanaConnector, BaseVNConnector, BaytConnector,
        BitbucketConnector, BoxConnector, CareemConnector, ClickUpConnector, ConfluenceConnector,
        DiscordConnector, DocuSignConnector, DropboxConnector, EmailConnector, FastworkConnector,
        FetchrConnector, FigmaConnector, FoodicsConnector, FreshdeskConnector, GitHubConnector,
        GitLabConnector, GojekConnector, GoogleCalendarConnector, GoogleDocsConnector,
        GoogleDriveConnector, GoogleMeetConnector, GoogleSheetsConnector, GrabConnector,
        HubSpotConnector, IntercomConnector, JiraConnector, KiotVietConnector, LazadaVNConnector,
        LineConnector, LinearConnector, MiroConnector, MoMoConnector, MondayConnector,
        NoonConnector, NotionConnector, OdooSeaConnector, OneDriveConnector, PayfortConnector,
        PipedriveConnector, PromptPayConnector, QuickBooksConnector, SalesforceConnector,
        SapoConnector, ScbEasyConnector, ServiceNowConnector, SharePointConnector,
        ShopeeVNConnector, ShopifyConnector, SlackConnector, StripeConnector, TabbyConnector,
        TalabatConnector, TalenoxConnector, TeamsConnector, TikiConnector, TokopediaConnector,
        TrelloConnector, TrueMoneyConnector, VNPayConnector, ViettelPostConnector, XeroConnector,
        ZaloConnector, ZendeskConnector, ZohoConnector, ZoomConnector,
    };
    // BEGIN Workstream B regional connector imports
    use connectors::{
        AlanConnector, BillomatConnector, CdiscountConnector, ColissimoConnector,
        CompaniesHouseConnector, DatevConnector, DeliverooConnector, DeutschePostConnector,
        DhlBusinessConnector, FreeAgentConnector, GoCardlessConnector, HmrcMtdConnector,
        JustEatConnector, LexofficeConnector, MangoPayConnector, MonzoBusinessConnector,
        N26BusinessConnector, OttoConnector, OvhCloudConnector, PayFitConnector,
        PennylaneConnector, PersonioConnector, QontoConnector, RevolutBusinessConnector,
        RoyalMailConnector, SendinblueConnector, SevDeskConnector, StarlingConnector,
        SwileConnector, ZalandoConnector,
    };
    // END Workstream B regional connector imports
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
        ConnectorKind::QuickBooks => {
            Arc::new(QuickBooksConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Xero => Arc::new(XeroConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Stripe => Arc::new(StripeConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Shopify => {
            Arc::new(ShopifyConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Airtable => {
            Arc::new(AirtableConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::GitLab => Arc::new(GitLabConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Bitbucket => {
            Arc::new(BitbucketConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Trello => Arc::new(TrelloConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Miro => Arc::new(MiroConnector::new(instance, transport, oauth_client)),
        ConnectorKind::DocuSign => {
            Arc::new(DocuSignConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Dropbox => {
            Arc::new(DropboxConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Box => Arc::new(BoxConnector::new(instance, transport, oauth_client)),
        ConnectorKind::SharePoint => {
            Arc::new(SharePointConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Teams => Arc::new(TeamsConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Discord => {
            Arc::new(DiscordConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Zoom => Arc::new(ZoomConnector::new(instance, transport, oauth_client)),
        ConnectorKind::GoogleCalendar => Arc::new(GoogleCalendarConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::GoogleDocs => {
            Arc::new(GoogleDocsConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::GoogleSheets => Arc::new(GoogleSheetsConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::GoogleMeet => {
            Arc::new(GoogleMeetConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Salesforce => {
            Arc::new(SalesforceConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::ServiceNow => {
            Arc::new(ServiceNowConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Zendesk => {
            Arc::new(ZendeskConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Linear => Arc::new(LinearConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Asana => Arc::new(AsanaConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Monday => Arc::new(MondayConnector::new(instance, transport, oauth_client)),
        ConnectorKind::ClickUp => {
            Arc::new(ClickUpConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Freshdesk => {
            Arc::new(FreshdeskConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Intercom => {
            Arc::new(IntercomConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Pipedrive => {
            Arc::new(PipedriveConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::GitHub => Arc::new(GitHubConnector::new(instance, transport, oauth_client)),
        // Singapore/Thailand/SEA connectors
        ConnectorKind::Line => Arc::new(LineConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Grab => Arc::new(GrabConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Gojek => Arc::new(GojekConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Talenox => {
            Arc::new(TalenoxConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::OdooSea => {
            Arc::new(OdooSeaConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Fastwork => {
            Arc::new(FastworkConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::TrueMoney => {
            Arc::new(TrueMoneyConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::ScbEasy => {
            Arc::new(ScbEasyConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::PromptPay => {
            Arc::new(PromptPayConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Tokopedia => {
            Arc::new(TokopediaConnector::new(instance, transport, oauth_client))
        }
        // Vietnam connectors (WS5).
        ConnectorKind::Zalo => Arc::new(ZaloConnector::new(instance, transport, oauth_client)),
        ConnectorKind::VNPay => Arc::new(VNPayConnector::new(instance, transport, oauth_client)),
        ConnectorKind::MoMo => Arc::new(MoMoConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Tiki => Arc::new(TikiConnector::new(instance, transport, oauth_client)),
        ConnectorKind::ShopeeVN => {
            Arc::new(ShopeeVNConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::LazadaVN => {
            Arc::new(LazadaVNConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::ViettelPost => {
            Arc::new(ViettelPostConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::KiotViet => {
            Arc::new(KiotVietConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Sapo => Arc::new(SapoConnector::new(instance, transport, oauth_client)),
        ConnectorKind::BaseVN => Arc::new(BaseVNConnector::new(instance, transport, oauth_client)),
        // GCC / Middle East connectors
        ConnectorKind::Careem => Arc::new(CareemConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Talabat => {
            Arc::new(TalabatConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Noon => Arc::new(NoonConnector::new(instance, transport, oauth_client)),
        ConnectorKind::AmazonAE => {
            Arc::new(AmazonAeConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Tabby => Arc::new(TabbyConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Foodics => {
            Arc::new(FoodicsConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Zoho => Arc::new(ZohoConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Bayt => Arc::new(BaytConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Fetchr => Arc::new(FetchrConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Payfort => {
            Arc::new(PayfortConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::MonzoBusiness => Arc::new(MonzoBusinessConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::RevolutBusiness => Arc::new(RevolutBusinessConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::FreeAgent => {
            Arc::new(FreeAgentConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::GoCardless => {
            Arc::new(GoCardlessConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::RoyalMail => {
            Arc::new(RoyalMailConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Deliveroo => {
            Arc::new(DeliverooConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::JustEat => {
            Arc::new(JustEatConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::CompaniesHouse => Arc::new(CompaniesHouseConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::HmrcMtd => {
            Arc::new(HmrcMtdConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Starling => {
            Arc::new(StarlingConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::MonzoBusiness => Arc::new(MonzoBusinessConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::RevolutBusiness => Arc::new(RevolutBusinessConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::CompaniesHouse => Arc::new(CompaniesHouseConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::N26Business => {
            Arc::new(N26BusinessConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Datev => Arc::new(DatevConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Lexoffice => {
            Arc::new(LexofficeConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::DhlBusiness => {
            Arc::new(DhlBusinessConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Otto => Arc::new(OttoConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Zalando => {
            Arc::new(ZalandoConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::DeutschePost => Arc::new(DeutschePostConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::Personio => {
            Arc::new(PersonioConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::SevDesk => {
            Arc::new(SevDeskConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Billomat => {
            Arc::new(BillomatConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::MonzoBusiness => Arc::new(MonzoBusinessConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::RevolutBusiness => Arc::new(RevolutBusinessConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::CompaniesHouse => Arc::new(CompaniesHouseConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::Datev => Arc::new(DatevConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Otto => Arc::new(OttoConnector::new(instance, transport, oauth_client)),
        ConnectorKind::DeutschePost => Arc::new(DeutschePostConnector::new(
            instance,
            transport,
            oauth_client,
        )),
        ConnectorKind::Qonto => Arc::new(QontoConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Pennylane => {
            Arc::new(PennylaneConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::PayFit => Arc::new(PayFitConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Colissimo => {
            Arc::new(ColissimoConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Cdiscount => {
            Arc::new(CdiscountConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::MangoPay => {
            Arc::new(MangoPayConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Sendinblue => {
            Arc::new(SendinblueConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::OvhCloud => {
            Arc::new(OvhCloudConnector::new(instance, transport, oauth_client))
        }
        ConnectorKind::Alan => Arc::new(AlanConnector::new(instance, transport, oauth_client)),
        ConnectorKind::Swile => Arc::new(SwileConnector::new(instance, transport, oauth_client)),
        ConnectorKind::GenericWebhook => {
            // The generic webhook connector is described in
            // `docs/technical/design.md` §10.2 but does not have a
            // concrete implementor yet.
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
pub(crate) fn event_to_evidence_body(event: &ConnectorEvent) -> Option<String> {
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
pub(crate) fn connector_source_tag(kind: ConnectorKind) -> &'static str {
    match kind {
        ConnectorKind::GoogleDrive
        | ConnectorKind::GoogleCalendar
        | ConnectorKind::GoogleDocs
        | ConnectorKind::GoogleSheets
        | ConnectorKind::GoogleMeet => "GoogleWorkspace",
        ConnectorKind::OneDrive | ConnectorKind::SharePoint | ConnectorKind::Teams => {
            "MicrosoftGraph"
        }
        ConnectorKind::Notion => "Notion",
        ConnectorKind::Jira | ConnectorKind::Confluence => "Atlassian",
        ConnectorKind::GitHub => "GitHub",
        ConnectorKind::Slack => "Slack",
        ConnectorKind::Figma => "Figma",
        ConnectorKind::HubSpot => "HubSpot",
        ConnectorKind::Email => "Email",
        ConnectorKind::QuickBooks => "QuickBooks",
        ConnectorKind::Xero => "Xero",
        ConnectorKind::Stripe => "Stripe",
        ConnectorKind::Shopify => "Shopify",
        ConnectorKind::Airtable => "Airtable",
        ConnectorKind::GitLab => "GitLab",
        ConnectorKind::Bitbucket => "Bitbucket",
        ConnectorKind::Trello => "Trello",
        ConnectorKind::Miro => "Miro",
        ConnectorKind::DocuSign => "DocuSign",
        ConnectorKind::Dropbox => "Dropbox",
        ConnectorKind::Box => "Box",
        ConnectorKind::Discord => "Discord",
        ConnectorKind::Zoom => "Zoom",
        ConnectorKind::Salesforce => "Salesforce",
        ConnectorKind::ServiceNow => "ServiceNow",
        ConnectorKind::Zendesk => "Zendesk",
        ConnectorKind::Linear => "Linear",
        ConnectorKind::Asana => "Asana",
        ConnectorKind::Monday => "Monday",
        ConnectorKind::ClickUp => "ClickUp",
        ConnectorKind::Freshdesk => "Freshdesk",
        ConnectorKind::Intercom => "Intercom",
        ConnectorKind::Pipedrive => "Pipedrive",
        // Singapore/Thailand/SEA connectors
        ConnectorKind::Line => "Line",
        ConnectorKind::Grab => "Grab",
        ConnectorKind::Gojek => "Gojek",
        ConnectorKind::Talenox => "Talenox",
        ConnectorKind::OdooSea => "Odoo",
        ConnectorKind::Fastwork => "Fastwork",
        ConnectorKind::TrueMoney => "TrueMoney",
        ConnectorKind::ScbEasy => "ScbEasy",
        ConnectorKind::PromptPay => "PromptPay",
        ConnectorKind::Tokopedia => "Tokopedia",
        // Vietnam connectors (WS5).
        ConnectorKind::Zalo => "Zalo",
        ConnectorKind::VNPay => "VNPay",
        ConnectorKind::MoMo => "MoMo",
        ConnectorKind::Tiki => "Tiki",
        ConnectorKind::ShopeeVN => "ShopeeVN",
        ConnectorKind::LazadaVN => "LazadaVN",
        ConnectorKind::ViettelPost => "ViettelPost",
        ConnectorKind::KiotViet => "KiotViet",
        ConnectorKind::Sapo => "Sapo",
        ConnectorKind::BaseVN => "BaseVN",
        // GCC / Middle East connectors
        ConnectorKind::Careem => "Careem",
        ConnectorKind::Talabat => "Talabat",
        ConnectorKind::Noon => "Noon",
        ConnectorKind::AmazonAE => "AmazonAE",
        ConnectorKind::Tabby => "Tabby",
        ConnectorKind::Foodics => "Foodics",
        ConnectorKind::Zoho => "Zoho",
        ConnectorKind::Bayt => "Bayt",
        ConnectorKind::Fetchr => "Fetchr",
        ConnectorKind::Payfort => "Payfort",
        ConnectorKind::MonzoBusiness => "MonzoBusiness",
        ConnectorKind::RevolutBusiness => "RevolutBusiness",
        ConnectorKind::FreeAgent => "FreeAgent",
        ConnectorKind::GoCardless => "GoCardless",
        ConnectorKind::RoyalMail => "RoyalMail",
        ConnectorKind::Deliveroo => "Deliveroo",
        ConnectorKind::JustEat => "JustEat",
        ConnectorKind::CompaniesHouse => "CompaniesHouse",
        ConnectorKind::HmrcMtd => "HmrcMtd",
        ConnectorKind::Starling => "Starling",
        ConnectorKind::N26Business => "N26Business",
        ConnectorKind::Datev => "Datev",
        ConnectorKind::Lexoffice => "Lexoffice",
        ConnectorKind::DhlBusiness => "DhlBusiness",
        ConnectorKind::Otto => "Otto",
        ConnectorKind::Zalando => "Zalando",
        ConnectorKind::DeutschePost => "DeutschePost",
        ConnectorKind::Personio => "Personio",
        ConnectorKind::SevDesk => "SevDesk",
        ConnectorKind::Billomat => "Billomat",
        ConnectorKind::Qonto => "Qonto",
        ConnectorKind::Pennylane => "Pennylane",
        ConnectorKind::PayFit => "PayFit",
        ConnectorKind::Colissimo => "Colissimo",
        ConnectorKind::Cdiscount => "Cdiscount",
        ConnectorKind::MangoPay => "MangoPay",
        ConnectorKind::Sendinblue => "Sendinblue",
        ConnectorKind::OvhCloud => "OvhCloud",
        ConnectorKind::Alan => "Alan",
        ConnectorKind::Swile => "Swile",
        ConnectorKind::GenericWebhook => "GenericWebhook",
    }
}

// ──────────────────────── Persistence helpers ────────────────────────
//
// JSON-serialise the in-memory `(config, sync_state)` tuple and hand
// the byte payload to `EvidenceStore::save_connector_instance`. The
// store owns the AEAD round-trip under the per-scope DEK and the
// SQLCipher upsert; this helper just funnels the borrowed instance
// into the right shape and translates the evidence-store error type.

/// JSON shape persisted in the encrypted `connector_instances.payload`
/// column. Distinct from the runtime [`ConnectorInstance`] so we can
/// add fields (e.g. webhook state) without forcing a schema migration —
/// the store reads the row, decrypts the blob, and the FFI runtime
/// owns the `serde_json` round-trip.
///
/// Write path takes borrowed references via the lifetime-parameterised
/// [`PersistedConnectorInstanceRef`] (no allocations beyond the JSON
/// buffer itself); read path uses the owned [`PersistedConnectorInstance`]
/// to materialise the deserialised values.
#[derive(serde::Serialize)]
struct PersistedConnectorInstanceRef<'a> {
    schema: u32,
    config: &'a ConnectorConfig,
    sync_state: &'a SyncState,
}

#[derive(serde::Deserialize)]
struct PersistedConnectorInstance {
    schema: u32,
    config: ConnectorConfig,
    sync_state: SyncState,
}

/// Current persisted-blob schema version. Bump together with the SQL
/// schema in `evidence_store::schema` only on **incompatible** shape
/// changes; additive optional fields can ship without a bump because
/// `serde` will accept old payloads that lack the new field as long
/// as the field has a `#[serde(default)]` default.
const PERSISTED_INSTANCE_SCHEMA: u32 = 1;

pub(crate) fn persist_connector_instance(
    rt: &FfiRuntime,
    instance: &ConnectorInstance,
) -> FfiResult<()> {
    let payload = PersistedConnectorInstanceRef {
        schema: PERSISTED_INSTANCE_SCHEMA,
        config: &instance.config,
        sync_state: &instance.sync_state,
    };
    let json = serde_json::to_vec(&payload).map_err(|e| FfiError::Connector {
        message: format!("connector instance JSON encode failed: {e}"),
    })?;
    rt.store()
        .save_connector_instance(
            instance.id.0,
            instance.config.scope_id,
            instance.config.kind.as_str(),
            &json,
        )
        .map_err(|e| FfiError::Evidence {
            message: format!("save_connector_instance failed: {e}"),
        })
}

pub(crate) fn persist_connector_token(
    rt: &FfiRuntime,
    instance: ConnectorInstanceId,
    scope: ScopeId,
    token: &connector_framework::OAuth2Token,
) -> FfiResult<()> {
    let json = serde_json::to_vec(token).map_err(|e| FfiError::Connector {
        message: format!("OAuth2 token JSON encode failed: {e}"),
    })?;
    rt.store()
        .save_connector_token(instance.0, scope, &json)
        .map_err(|e| FfiError::Evidence {
            message: format!("save_connector_token failed: {e}"),
        })
}

/// Rehydrate persisted connector state from SQLCipher on `open_store`.
///
/// Loads every `connector_instances` row, skips ones whose scope is
/// tombstoned (the row's AEAD payload is unrecoverable anyway, but
/// we never want to mint an `Arc<dyn Connector>` for a forgotten
/// scope — `sync_connector` would refuse on the `is_scope_forgotten`
/// check, just wastes the build), deserialises the
/// `(config, sync_state)` pair, rebuilds the `Arc<dyn Connector>`
/// via [`build_connector`], and inserts into the runtime's
/// `connector_instances` + `connectors` maps. Then loads every
/// `connector_tokens` row, deserialises the `OAuth2Token`, and
/// inserts into the runtime's `token_vault`.
///
/// Tolerant of partial corruption: a row that fails to decrypt or
/// deserialise is skipped with a `tracing::warn!` rather than
/// blocking the open — matching the user_memory / channel_memory
/// rehydration discipline at `runtime.rs::open_store_inner`. The
/// host's `health_check` reports the rehydrated count so the
/// operator can detect rows that fail to come back.
///
/// Under `not(http-client)` builds, the data structs are still
/// loaded (so `list_connectors` returns the persisted state) but the
/// `Arc<dyn Connector>` rebuild is skipped — `sync_connector` /
/// `authenticate_connector` will return `Unavailable` for those
/// instances exactly like the create-time path. This keeps the
/// no-feature-flag binary observable + queryable without requiring a
/// real HTTP transport.
pub(crate) fn rehydrate_connectors(rt: &mut FfiRuntime, tombstones: &HashSet<ScopeId>) {
    // 1. Instances + (Arc<dyn Connector>) handles.
    let rows = match rt.store().load_connector_instances() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e,
                "load_connector_instances failed; connector subsystem starts empty for this open",
            );
            return;
        }
    };
    for (instance_uuid, scope_id, kind_tag, payload) in rows {
        if tombstones.contains(&scope_id) {
            // Best-effort: also purge the dangling row from disk so
            // the next open does not re-walk it. Tracing-only on
            // failure — the AEAD payload is unrecoverable anyway.
            if let Err(e) = rt.store().delete_connector_instance(instance_uuid) {
                tracing::warn!(instance = %instance_uuid,
                    scope = %scope_id.as_uuid(),
                    error = %e,
                    "failed to clean up dangling connector_instances row for forgotten scope",
                );
            }
            continue;
        }
        let parsed = match serde_json::from_slice::<PersistedConnectorInstance>(&payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(instance = %instance_uuid,
                    scope = %scope_id.as_uuid(),
                    kind = %kind_tag,
                    error = %e,
                    "connector_instances payload failed to deserialise; row skipped",
                );
                continue;
            }
        };
        if parsed.schema != PERSISTED_INSTANCE_SCHEMA {
            tracing::warn!(instance = %instance_uuid,
                expected = PERSISTED_INSTANCE_SCHEMA,
                actual = parsed.schema,
                "connector_instances payload has unexpected schema version; row skipped",
            );
            continue;
        }
        // Sanity-check the row matches the deserialised payload's
        // identity. If the kind tag drifts between the row column
        // and the encrypted blob the AAD check should have caught
        // it already; this is belt-and-braces.
        if parsed.config.kind.as_str() != kind_tag {
            tracing::warn!(instance = %instance_uuid,
                row_kind = %kind_tag,
                payload_kind = %parsed.config.kind.as_str(),
                "connector_instances row kind tag does not match payload; row skipped",
            );
            continue;
        }
        let instance_id = ConnectorInstanceId::from_uuid(instance_uuid);
        let connector = match build_connector(rt, parsed.config.kind, instance_id) {
            Ok(c) => Some(c),
            Err(FfiError::Unavailable { subsystem }) => {
                tracing::debug!(instance = %instance_uuid,
                    subsystem = %subsystem,
                    "connector subsystem unavailable at rehydration time; instance loaded without live Arc<dyn Connector>",
                );
                None
            }
            Err(e) => {
                tracing::warn!(instance = %instance_uuid,
                    error = %e,
                    "build_connector failed during rehydration; instance loaded without live Arc<dyn Connector>",
                );
                None
            }
        };
        let instance = ConnectorInstance {
            id: instance_id,
            config: parsed.config,
            sync_state: parsed.sync_state,
        };
        rt.connector_instances.insert(instance_id, instance);
        if let Some(c) = connector {
            rt.connectors.insert(instance_id, c);
        }
    }

    // 2. OAuth2 tokens.
    let token_rows = match rt.store().load_connector_tokens() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e,
                "load_connector_tokens failed; token vault starts empty for this open",
            );
            return;
        }
    };
    for (instance_uuid, scope_id, payload) in token_rows {
        if tombstones.contains(&scope_id) {
            if let Err(e) = rt.store().delete_connector_token(instance_uuid) {
                tracing::warn!(instance = %instance_uuid,
                    scope = %scope_id.as_uuid(),
                    error = %e,
                    "failed to clean up dangling connector_tokens row for forgotten scope",
                );
            }
            continue;
        }
        let instance_id = ConnectorInstanceId::from_uuid(instance_uuid);
        // Skip tokens whose owning instance failed to rehydrate above
        // (deserialise error, schema mismatch, build_connector miss,
        // tombstone race). Inserting such a token into the vault is
        // harmless — no consumer can ask for it without first
        // resolving the instance — but the orphan would never be
        // retired and would survive in memory until `close_store`.
        // Best-effort purge from disk too so the next open doesn't
        // re-walk it; tracing-only on failure.
        if !rt.connector_instances.contains_key(&instance_id) {
            tracing::warn!(instance = %instance_uuid,
                scope = %scope_id.as_uuid(),
                "connector_tokens row references an instance that did not rehydrate; skipping",
            );
            if let Err(e) = rt.store().delete_connector_token(instance_uuid) {
                tracing::warn!(instance = %instance_uuid,
                    scope = %scope_id.as_uuid(),
                    error = %e,
                    "failed to clean up orphaned connector_tokens row",
                );
            }
            continue;
        }
        match serde_json::from_slice::<connector_framework::OAuth2Token>(&payload) {
            Ok(token) => {
                rt.token_vault.put(instance_id, token);
            }
            Err(e) => {
                tracing::warn!(instance = %instance_uuid,
                    scope = %scope_id.as_uuid(),
                    error = %e,
                    "connector_tokens payload failed to deserialise; row skipped",
                );
            }
        }
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
pub(crate) fn parse_instance_id(s: &str) -> FfiResult<ConnectorInstanceId> {
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
        ConnectorKindTag::QuickBooks => ConnectorKind::QuickBooks,
        ConnectorKindTag::Xero => ConnectorKind::Xero,
        ConnectorKindTag::Stripe => ConnectorKind::Stripe,
        ConnectorKindTag::Shopify => ConnectorKind::Shopify,
        ConnectorKindTag::Airtable => ConnectorKind::Airtable,
        ConnectorKindTag::GitLab => ConnectorKind::GitLab,
        ConnectorKindTag::Bitbucket => ConnectorKind::Bitbucket,
        ConnectorKindTag::Trello => ConnectorKind::Trello,
        ConnectorKindTag::Miro => ConnectorKind::Miro,
        ConnectorKindTag::DocuSign => ConnectorKind::DocuSign,
        ConnectorKindTag::Dropbox => ConnectorKind::Dropbox,
        ConnectorKindTag::Box => ConnectorKind::Box,
        ConnectorKindTag::SharePoint => ConnectorKind::SharePoint,
        ConnectorKindTag::Teams => ConnectorKind::Teams,
        ConnectorKindTag::Discord => ConnectorKind::Discord,
        ConnectorKindTag::Zoom => ConnectorKind::Zoom,
        ConnectorKindTag::GoogleCalendar => ConnectorKind::GoogleCalendar,
        ConnectorKindTag::GoogleDocs => ConnectorKind::GoogleDocs,
        ConnectorKindTag::GoogleSheets => ConnectorKind::GoogleSheets,
        ConnectorKindTag::GoogleMeet => ConnectorKind::GoogleMeet,
        ConnectorKindTag::Salesforce => ConnectorKind::Salesforce,
        ConnectorKindTag::ServiceNow => ConnectorKind::ServiceNow,
        ConnectorKindTag::Zendesk => ConnectorKind::Zendesk,
        ConnectorKindTag::Linear => ConnectorKind::Linear,
        ConnectorKindTag::Asana => ConnectorKind::Asana,
        ConnectorKindTag::Monday => ConnectorKind::Monday,
        ConnectorKindTag::ClickUp => ConnectorKind::ClickUp,
        ConnectorKindTag::Freshdesk => ConnectorKind::Freshdesk,
        ConnectorKindTag::Intercom => ConnectorKind::Intercom,
        ConnectorKindTag::Pipedrive => ConnectorKind::Pipedrive,
        // Singapore/Thailand/SEA connectors
        ConnectorKindTag::Line => ConnectorKind::Line,
        ConnectorKindTag::Grab => ConnectorKind::Grab,
        ConnectorKindTag::Gojek => ConnectorKind::Gojek,
        ConnectorKindTag::Talenox => ConnectorKind::Talenox,
        ConnectorKindTag::OdooSea => ConnectorKind::OdooSea,
        ConnectorKindTag::Fastwork => ConnectorKind::Fastwork,
        ConnectorKindTag::TrueMoney => ConnectorKind::TrueMoney,
        ConnectorKindTag::ScbEasy => ConnectorKind::ScbEasy,
        ConnectorKindTag::PromptPay => ConnectorKind::PromptPay,
        ConnectorKindTag::Tokopedia => ConnectorKind::Tokopedia,
        // Vietnam connectors (WS5).
        ConnectorKindTag::Zalo => ConnectorKind::Zalo,
        ConnectorKindTag::VNPay => ConnectorKind::VNPay,
        ConnectorKindTag::MoMo => ConnectorKind::MoMo,
        ConnectorKindTag::Tiki => ConnectorKind::Tiki,
        ConnectorKindTag::ShopeeVN => ConnectorKind::ShopeeVN,
        ConnectorKindTag::LazadaVN => ConnectorKind::LazadaVN,
        ConnectorKindTag::ViettelPost => ConnectorKind::ViettelPost,
        ConnectorKindTag::KiotViet => ConnectorKind::KiotViet,
        ConnectorKindTag::Sapo => ConnectorKind::Sapo,
        ConnectorKindTag::BaseVN => ConnectorKind::BaseVN,
        // GCC / Middle East connectors
        ConnectorKindTag::Careem => ConnectorKind::Careem,
        ConnectorKindTag::Talabat => ConnectorKind::Talabat,
        ConnectorKindTag::Noon => ConnectorKind::Noon,
        ConnectorKindTag::AmazonAE => ConnectorKind::AmazonAE,
        ConnectorKindTag::Tabby => ConnectorKind::Tabby,
        ConnectorKindTag::Foodics => ConnectorKind::Foodics,
        ConnectorKindTag::Zoho => ConnectorKind::Zoho,
        ConnectorKindTag::Bayt => ConnectorKind::Bayt,
        ConnectorKindTag::Fetchr => ConnectorKind::Fetchr,
        ConnectorKindTag::Payfort => ConnectorKind::Payfort,
        ConnectorKindTag::MonzoBusiness => ConnectorKind::MonzoBusiness,
        ConnectorKindTag::RevolutBusiness => ConnectorKind::RevolutBusiness,
        ConnectorKindTag::FreeAgent => ConnectorKind::FreeAgent,
        ConnectorKindTag::GoCardless => ConnectorKind::GoCardless,
        ConnectorKindTag::RoyalMail => ConnectorKind::RoyalMail,
        ConnectorKindTag::Deliveroo => ConnectorKind::Deliveroo,
        ConnectorKindTag::JustEat => ConnectorKind::JustEat,
        ConnectorKindTag::CompaniesHouse => ConnectorKind::CompaniesHouse,
        ConnectorKindTag::HmrcMtd => ConnectorKind::HmrcMtd,
        ConnectorKindTag::Starling => ConnectorKind::Starling,
        ConnectorKindTag::N26Business => ConnectorKind::N26Business,
        ConnectorKindTag::Datev => ConnectorKind::Datev,
        ConnectorKindTag::Lexoffice => ConnectorKind::Lexoffice,
        ConnectorKindTag::DhlBusiness => ConnectorKind::DhlBusiness,
        ConnectorKindTag::Otto => ConnectorKind::Otto,
        ConnectorKindTag::Zalando => ConnectorKind::Zalando,
        ConnectorKindTag::DeutschePost => ConnectorKind::DeutschePost,
        ConnectorKindTag::Personio => ConnectorKind::Personio,
        ConnectorKindTag::SevDesk => ConnectorKind::SevDesk,
        ConnectorKindTag::Billomat => ConnectorKind::Billomat,
        ConnectorKindTag::Qonto => ConnectorKind::Qonto,
        ConnectorKindTag::Pennylane => ConnectorKind::Pennylane,
        ConnectorKindTag::PayFit => ConnectorKind::PayFit,
        ConnectorKindTag::Colissimo => ConnectorKind::Colissimo,
        ConnectorKindTag::Cdiscount => ConnectorKind::Cdiscount,
        ConnectorKindTag::MangoPay => ConnectorKind::MangoPay,
        ConnectorKindTag::Sendinblue => ConnectorKind::Sendinblue,
        ConnectorKindTag::OvhCloud => ConnectorKind::OvhCloud,
        ConnectorKindTag::Alan => ConnectorKind::Alan,
        ConnectorKindTag::Swile => ConnectorKind::Swile,
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
        ConnectorKind::QuickBooks => ConnectorKindTag::QuickBooks,
        ConnectorKind::Xero => ConnectorKindTag::Xero,
        ConnectorKind::Stripe => ConnectorKindTag::Stripe,
        ConnectorKind::Shopify => ConnectorKindTag::Shopify,
        ConnectorKind::Airtable => ConnectorKindTag::Airtable,
        ConnectorKind::GitLab => ConnectorKindTag::GitLab,
        ConnectorKind::Bitbucket => ConnectorKindTag::Bitbucket,
        ConnectorKind::Trello => ConnectorKindTag::Trello,
        ConnectorKind::Miro => ConnectorKindTag::Miro,
        ConnectorKind::DocuSign => ConnectorKindTag::DocuSign,
        ConnectorKind::Dropbox => ConnectorKindTag::Dropbox,
        ConnectorKind::Box => ConnectorKindTag::Box,
        ConnectorKind::SharePoint => ConnectorKindTag::SharePoint,
        ConnectorKind::Teams => ConnectorKindTag::Teams,
        ConnectorKind::Discord => ConnectorKindTag::Discord,
        ConnectorKind::Zoom => ConnectorKindTag::Zoom,
        ConnectorKind::GoogleCalendar => ConnectorKindTag::GoogleCalendar,
        ConnectorKind::GoogleDocs => ConnectorKindTag::GoogleDocs,
        ConnectorKind::GoogleSheets => ConnectorKindTag::GoogleSheets,
        ConnectorKind::GoogleMeet => ConnectorKindTag::GoogleMeet,
        ConnectorKind::Salesforce => ConnectorKindTag::Salesforce,
        ConnectorKind::ServiceNow => ConnectorKindTag::ServiceNow,
        ConnectorKind::Zendesk => ConnectorKindTag::Zendesk,
        ConnectorKind::Linear => ConnectorKindTag::Linear,
        ConnectorKind::Asana => ConnectorKindTag::Asana,
        ConnectorKind::Monday => ConnectorKindTag::Monday,
        ConnectorKind::ClickUp => ConnectorKindTag::ClickUp,
        ConnectorKind::Freshdesk => ConnectorKindTag::Freshdesk,
        ConnectorKind::Intercom => ConnectorKindTag::Intercom,
        ConnectorKind::Pipedrive => ConnectorKindTag::Pipedrive,
        // Singapore/Thailand/SEA connectors
        ConnectorKind::Line => ConnectorKindTag::Line,
        ConnectorKind::Grab => ConnectorKindTag::Grab,
        ConnectorKind::Gojek => ConnectorKindTag::Gojek,
        ConnectorKind::Talenox => ConnectorKindTag::Talenox,
        ConnectorKind::OdooSea => ConnectorKindTag::OdooSea,
        ConnectorKind::Fastwork => ConnectorKindTag::Fastwork,
        ConnectorKind::TrueMoney => ConnectorKindTag::TrueMoney,
        ConnectorKind::ScbEasy => ConnectorKindTag::ScbEasy,
        ConnectorKind::PromptPay => ConnectorKindTag::PromptPay,
        ConnectorKind::Tokopedia => ConnectorKindTag::Tokopedia,
        // Vietnam connectors (WS5).
        ConnectorKind::Zalo => ConnectorKindTag::Zalo,
        ConnectorKind::VNPay => ConnectorKindTag::VNPay,
        ConnectorKind::MoMo => ConnectorKindTag::MoMo,
        ConnectorKind::Tiki => ConnectorKindTag::Tiki,
        ConnectorKind::ShopeeVN => ConnectorKindTag::ShopeeVN,
        ConnectorKind::LazadaVN => ConnectorKindTag::LazadaVN,
        ConnectorKind::ViettelPost => ConnectorKindTag::ViettelPost,
        ConnectorKind::KiotViet => ConnectorKindTag::KiotViet,
        ConnectorKind::Sapo => ConnectorKindTag::Sapo,
        ConnectorKind::BaseVN => ConnectorKindTag::BaseVN,
        // GCC / Middle East connectors
        ConnectorKind::Careem => ConnectorKindTag::Careem,
        ConnectorKind::Talabat => ConnectorKindTag::Talabat,
        ConnectorKind::Noon => ConnectorKindTag::Noon,
        ConnectorKind::AmazonAE => ConnectorKindTag::AmazonAE,
        ConnectorKind::Tabby => ConnectorKindTag::Tabby,
        ConnectorKind::Foodics => ConnectorKindTag::Foodics,
        ConnectorKind::Zoho => ConnectorKindTag::Zoho,
        ConnectorKind::Bayt => ConnectorKindTag::Bayt,
        ConnectorKind::Fetchr => ConnectorKindTag::Fetchr,
        ConnectorKind::Payfort => ConnectorKindTag::Payfort,
        ConnectorKind::MonzoBusiness => ConnectorKindTag::MonzoBusiness,
        ConnectorKind::RevolutBusiness => ConnectorKindTag::RevolutBusiness,
        ConnectorKind::FreeAgent => ConnectorKindTag::FreeAgent,
        ConnectorKind::GoCardless => ConnectorKindTag::GoCardless,
        ConnectorKind::RoyalMail => ConnectorKindTag::RoyalMail,
        ConnectorKind::Deliveroo => ConnectorKindTag::Deliveroo,
        ConnectorKind::JustEat => ConnectorKindTag::JustEat,
        ConnectorKind::CompaniesHouse => ConnectorKindTag::CompaniesHouse,
        ConnectorKind::HmrcMtd => ConnectorKindTag::HmrcMtd,
        ConnectorKind::Starling => ConnectorKindTag::Starling,
        ConnectorKind::N26Business => ConnectorKindTag::N26Business,
        ConnectorKind::Datev => ConnectorKindTag::Datev,
        ConnectorKind::Lexoffice => ConnectorKindTag::Lexoffice,
        ConnectorKind::DhlBusiness => ConnectorKindTag::DhlBusiness,
        ConnectorKind::Otto => ConnectorKindTag::Otto,
        ConnectorKind::Zalando => ConnectorKindTag::Zalando,
        ConnectorKind::DeutschePost => ConnectorKindTag::DeutschePost,
        ConnectorKind::Personio => ConnectorKindTag::Personio,
        ConnectorKind::SevDesk => ConnectorKindTag::SevDesk,
        ConnectorKind::Billomat => ConnectorKindTag::Billomat,
        ConnectorKind::Qonto => ConnectorKindTag::Qonto,
        ConnectorKind::Pennylane => ConnectorKindTag::Pennylane,
        ConnectorKind::PayFit => ConnectorKindTag::PayFit,
        ConnectorKind::Colissimo => ConnectorKindTag::Colissimo,
        ConnectorKind::Cdiscount => ConnectorKindTag::Cdiscount,
        ConnectorKind::MangoPay => ConnectorKindTag::MangoPay,
        ConnectorKind::Sendinblue => ConnectorKindTag::Sendinblue,
        ConnectorKind::OvhCloud => ConnectorKindTag::OvhCloud,
        ConnectorKind::Alan => ConnectorKindTag::Alan,
        ConnectorKind::Swile => ConnectorKindTag::Swile,
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
            ConnectorKindTag::QuickBooks,
            ConnectorKindTag::Xero,
            ConnectorKindTag::Stripe,
            ConnectorKindTag::Shopify,
            ConnectorKindTag::Airtable,
            ConnectorKindTag::GitLab,
            ConnectorKindTag::Bitbucket,
            ConnectorKindTag::Trello,
            ConnectorKindTag::Miro,
            ConnectorKindTag::DocuSign,
            ConnectorKindTag::Dropbox,
            ConnectorKindTag::Box,
            ConnectorKindTag::SharePoint,
            ConnectorKindTag::Teams,
            ConnectorKindTag::Discord,
            ConnectorKindTag::Zoom,
            ConnectorKindTag::GoogleCalendar,
            ConnectorKindTag::GoogleDocs,
            ConnectorKindTag::GoogleSheets,
            ConnectorKindTag::GoogleMeet,
            ConnectorKindTag::Salesforce,
            ConnectorKindTag::ServiceNow,
            ConnectorKindTag::Zendesk,
            ConnectorKindTag::Linear,
            ConnectorKindTag::Asana,
            ConnectorKindTag::Monday,
            ConnectorKindTag::ClickUp,
            ConnectorKindTag::Freshdesk,
            ConnectorKindTag::Intercom,
            ConnectorKindTag::Pipedrive,
            // Singapore/Thailand/SEA connectors
            ConnectorKindTag::Line,
            ConnectorKindTag::Grab,
            ConnectorKindTag::Gojek,
            ConnectorKindTag::Talenox,
            ConnectorKindTag::OdooSea,
            ConnectorKindTag::Fastwork,
            ConnectorKindTag::TrueMoney,
            ConnectorKindTag::ScbEasy,
            ConnectorKindTag::PromptPay,
            ConnectorKindTag::Tokopedia,
            // Vietnam connectors (WS5).
            ConnectorKindTag::Zalo,
            ConnectorKindTag::VNPay,
            ConnectorKindTag::MoMo,
            ConnectorKindTag::Tiki,
            ConnectorKindTag::ShopeeVN,
            ConnectorKindTag::LazadaVN,
            ConnectorKindTag::ViettelPost,
            ConnectorKindTag::KiotViet,
            ConnectorKindTag::Sapo,
            ConnectorKindTag::BaseVN,
            // GCC / Middle East connectors
            ConnectorKindTag::Careem,
            ConnectorKindTag::Talabat,
            ConnectorKindTag::Noon,
            ConnectorKindTag::AmazonAE,
            ConnectorKindTag::Tabby,
            ConnectorKindTag::Foodics,
            ConnectorKindTag::Zoho,
            ConnectorKindTag::Bayt,
            ConnectorKindTag::Fetchr,
            ConnectorKindTag::Payfort,
            ConnectorKindTag::MonzoBusiness,
            ConnectorKindTag::RevolutBusiness,
            ConnectorKindTag::FreeAgent,
            ConnectorKindTag::GoCardless,
            ConnectorKindTag::RoyalMail,
            ConnectorKindTag::Deliveroo,
            ConnectorKindTag::JustEat,
            ConnectorKindTag::CompaniesHouse,
            ConnectorKindTag::HmrcMtd,
            ConnectorKindTag::Starling,
            ConnectorKindTag::N26Business,
            ConnectorKindTag::Datev,
            ConnectorKindTag::Lexoffice,
            ConnectorKindTag::DhlBusiness,
            ConnectorKindTag::Otto,
            ConnectorKindTag::Zalando,
            ConnectorKindTag::DeutschePost,
            ConnectorKindTag::Personio,
            ConnectorKindTag::SevDesk,
            ConnectorKindTag::Billomat,
            ConnectorKindTag::Qonto,
            ConnectorKindTag::Pennylane,
            ConnectorKindTag::PayFit,
            ConnectorKindTag::Colissimo,
            ConnectorKindTag::Cdiscount,
            ConnectorKindTag::MangoPay,
            ConnectorKindTag::Sendinblue,
            ConnectorKindTag::OvhCloud,
            ConnectorKindTag::Alan,
            ConnectorKindTag::Swile,
            ConnectorKindTag::GenericWebhook,
        ];
        for tag in all {
            assert_eq!(framework_kind_to_ffi(connector_kind_to_framework(tag)), tag);
        }
        // Guard against silently dropping a variant from `all`: bump this
        // count when adding a `ConnectorKindTag` (mirrors the exhaustive
        // `KNOWN_PROVIDER_IDS` check in `webhook.rs`).
        assert_eq!(all.len(), 101);
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
            ConnectorKind::GenericWebhook,
        ];
        for kind in all_kinds {
            // Stability assertion: the tag must not be empty and
            // must round-trip as ASCII so the evidence store's
            // FTS5 column doesn't have to deal with unicode.
            let tag = connector_source_tag(kind);
            assert!(!tag.is_empty());
            assert!(tag.is_ascii());
        }
        // Bump this count when adding a `ConnectorKind` so a new variant
        // can't silently skip the per-tag stability assertions above
        // (mirrors `kind_translation_round_trips` and `webhook.rs`).
        assert_eq!(all_kinds.len(), 101);
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

    /// `lookup_connector_handle` must disambiguate the three runtime
    /// states the host can observe:
    ///
    /// * **Instance + Arc both present** → returns the cloned handle.
    /// * **Instance present, Arc absent** → returns `Unavailable`. This
    ///   models the post-rehydrate case where `build_connector` failed
    ///   (e.g. `http-client` feature off, or transport construction
    ///   failed at `open_store` time). The host can still see the
    ///   instance through `list_connectors` and re-create it once the
    ///   transport recovers.
    /// * **Instance absent** → returns `NotFound`.
    ///
    /// Before the helper existed both `sync_connector` and
    /// `authenticate_connector` returned `NotFound` for the middle
    /// arm, which contradicted the doc comments on those functions
    /// (which promise `Unavailable` for the missing-transport case)
    /// and gave the host a misleading "instance unknown" signal when
    /// the row is plainly visible in `list_connectors`.
    #[test]
    fn lookup_connector_handle_returns_unavailable_when_arc_missing() {
        use connector_framework::{AuthKind, ConnectorConfig, ConnectorInstance};
        use evidence_store::ScopeId;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let master_key_hex = "a5".repeat(32);
        let handle = crate::open_store(path.to_string_lossy().into_owned(), master_key_hex)
            .expect("open_store");

        let instance_id = ConnectorInstanceId::new_v4();
        let scope_id = ScopeId::new_v4();
        let cfg = ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, scope_id);
        let instance = ConnectorInstance {
            id: instance_id,
            config: cfg,
            sync_state: SyncState::new(instance_id),
        };

        // Stage the "instance present, Arc absent" state directly
        // through `with_runtime` — this models what `rehydrate_connectors`
        // does when `build_connector` returns `Unavailable`: the
        // instance lives in `connector_instances` for observability but
        // no entry is created in `connectors`.
        crate::runtime::with_runtime(handle, |rt| {
            rt.connector_instances.insert(instance_id, instance.clone());
            // Deliberately NOT inserting into rt.connectors.

            // `Arc<dyn Connector>` is not `Debug`, so we can't use
            // `expect_err` here — match the `Result` directly.
            match lookup_connector_handle(rt, instance_id, "irrelevant") {
                Ok(_) => panic!("instance present without Arc must error"),
                Err(FfiError::Unavailable { subsystem }) => {
                    assert_eq!(subsystem, "connector");
                }
                Err(other) => {
                    panic!("expected Unavailable {{ subsystem: \"connector\" }}; got {other:?}")
                }
            }

            // Sanity: the absent-instance arm still returns NotFound.
            let other_id = ConnectorInstanceId::new_v4();
            match lookup_connector_handle(rt, other_id, "missing-id") {
                Ok(_) => panic!("absent instance must error"),
                Err(FfiError::NotFound { kind, id }) => {
                    assert_eq!(kind, "connector");
                    assert_eq!(id, "missing-id");
                }
                Err(other) => panic!("expected NotFound for absent instance; got {other:?}"),
            }
            Ok(())
        })
        .expect("with_runtime");

        crate::close_store(handle).expect("close_store");
        // Keep `dir` alive until after `close_store`.
        drop(dir);
    }

    /// `refresh_connector_token` must reject a malformed UUID the
    /// same way every other connector entry point does — with
    /// `FfiError::InvalidId`, not by silently parsing into a zero
    /// UUID and surfacing a confusing `NotFound`.
    #[test]
    fn refresh_connector_token_rejects_non_uuid_instance_id() {
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let master_key_hex = "a5".repeat(32);
        let handle = crate::open_store(path.to_string_lossy().into_owned(), master_key_hex)
            .expect("open_store");

        let err = crate::refresh_connector_token(handle, "not-a-uuid".to_string())
            .expect_err("must fail");
        assert!(matches!(err, FfiError::InvalidId { .. }));

        crate::close_store(handle).expect("close_store");
        drop(dir);
    }

    /// `refresh_connector_token` against an unknown instance must
    /// surface `NotFound { kind: "connector" }`. Mirrors the
    /// contract that every other instance-keyed FFI function
    /// (`authenticate_connector`, `sync_connector`,
    /// `remove_connector`) has.
    #[test]
    fn refresh_connector_token_returns_not_found_when_instance_absent() {
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let master_key_hex = "a5".repeat(32);
        let handle = crate::open_store(path.to_string_lossy().into_owned(), master_key_hex)
            .expect("open_store");

        let unknown = ConnectorInstanceId::new_v4().0.to_string();
        let err =
            crate::refresh_connector_token(handle, unknown.clone()).expect_err("absent must fail");
        match err {
            FfiError::NotFound { kind, id } => {
                assert_eq!(kind, "connector");
                assert_eq!(id, unknown);
            }
            other => panic!("expected NotFound(connector); got {other:?}"),
        }

        crate::close_store(handle).expect("close_store");
        drop(dir);
    }

    /// `refresh_token_three_phase` is meant to short-circuit
    /// (zero network I/O, zero SQLCipher writes) when the cached
    /// token is still fresh. This is the hot path —
    /// `sync_connector` invokes the helper unconditionally with a
    /// conservative skew, and the typical "sync against a fresh
    /// token" run must NOT pay the refresh overhead.
    ///
    /// Pins the short-circuit by passing a token whose
    /// `expires_at` is far enough in the future that
    /// `is_expiring_within(now, skew)` returns false. The helper
    /// must return `(original_token, false)` without touching the
    /// vault or the persistence layer.
    #[test]
    #[cfg(feature = "http-client")]
    fn refresh_token_three_phase_short_circuits_when_token_not_expiring() {
        use chrono::Duration;
        use connector_framework::OAuth2Token;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let master_key_hex = "a5".repeat(32);
        let handle = crate::open_store(path.to_string_lossy().into_owned(), master_key_hex)
            .expect("open_store");

        let instance = ConnectorInstanceId::new_v4();
        let scope_id = ScopeId::new_v4();
        let config = ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, scope_id);
        let now = Utc::now();
        // Token expires far outside the skew → no refresh should
        // happen. We deliberately use `new_without_refresh` here:
        // if the helper short-circuits correctly it never even
        // inspects the refresh_token field.
        let fresh_token =
            OAuth2Token::new_without_refresh("FRESH-AT", now + Duration::hours(1), "read");

        let (returned, refreshed) = refresh_token_three_phase(
            handle,
            instance,
            "irrelevant",
            fresh_token.clone(),
            &config,
            Some(Duration::seconds(60)),
            now,
        )
        .expect("short-circuit must succeed");
        assert!(!refreshed, "fresh token must NOT trigger a refresh");
        assert_eq!(
            returned.access_token.expose(),
            fresh_token.access_token.expose(),
            "short-circuit must return the original token verbatim",
        );

        crate::close_store(handle).expect("close_store");
        drop(dir);
    }

    /// `refresh_token_three_phase` must reject (without a network
    /// call) the "expiring token with no refresh_token" case
    /// — POSTing `refresh_token=` to the provider is doomed and
    /// would surface as a generic `invalid_grant`. The substrate
    /// short-circuits with an actionable diagnostic instead.
    ///
    /// This pins the contract documented on
    /// [`refresh_token_three_phase`]'s "no refresh_token in the
    /// cached bundle → re-auth required" path.
    #[test]
    #[cfg(feature = "http-client")]
    fn refresh_token_three_phase_errors_when_no_refresh_token() {
        use chrono::Duration;
        use connector_framework::OAuth2Token;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let master_key_hex = "a5".repeat(32);
        let handle = crate::open_store(path.to_string_lossy().into_owned(), master_key_hex)
            .expect("open_store");

        let instance = ConnectorInstanceId::new_v4();
        let scope_id = ScopeId::new_v4();
        let config = ConnectorConfig::new(ConnectorKind::Slack, AuthKind::OAuth2, scope_id);
        let now = Utc::now();
        // Token expiring within the skew, AND no refresh_token
        // stored — substrate must refuse to POST `refresh_token=`.
        let expiring_token =
            OAuth2Token::new_without_refresh("EXPIRING-AT", now + Duration::seconds(5), "read");

        let err = refresh_token_three_phase(
            handle,
            instance,
            "instance-display-xyz",
            expiring_token,
            &config,
            Some(Duration::seconds(60)),
            now,
        )
        .expect_err("missing refresh_token must error");
        match err {
            FfiError::Connector { message } => {
                assert!(
                    message.contains("no refresh_token stored")
                        && message.contains("instance-display-xyz")
                        && message.contains("re-authorisation required"),
                    "expected substrate-side `no refresh_token stored` diagnostic naming the \
                     instance + recovery path; got {message:?}",
                );
            }
            other => panic!("expected Connector(no refresh_token); got {other:?}"),
        }

        crate::close_store(handle).expect("close_store");
        drop(dir);
    }

    /// `refresh_token_three_phase` under `not(http-client)` must
    /// unconditionally surface `Unavailable { subsystem:
    /// "connector-http-client" }` — same recovery contract as
    /// `build_connector` / `lookup_connector_handle`. This is
    /// what `sync_connector` / `refresh_connector_token` rely on
    /// to keep the no-feature-flag binary observable.
    #[test]
    #[cfg(not(feature = "http-client"))]
    fn refresh_token_three_phase_unavailable_when_http_client_off() {
        use chrono::Duration;
        use connector_framework::OAuth2Token;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let master_key_hex = "a5".repeat(32);
        let handle = crate::open_store(path.to_string_lossy().into_owned(), master_key_hex)
            .expect("open_store");

        let instance = ConnectorInstanceId::new_v4();
        let scope_id = ScopeId::new_v4();
        let config = ConnectorConfig::new(ConnectorKind::Slack, AuthKind::OAuth2, scope_id);
        let now = Utc::now();
        let token = OAuth2Token::new("AT", "RT", now + Duration::seconds(5), "read");

        let err = refresh_token_three_phase(
            handle,
            instance,
            "irrelevant",
            token,
            &config,
            Some(Duration::seconds(60)),
            now,
        )
        .expect_err("not(http-client) must always error Unavailable");
        match err {
            FfiError::Unavailable { subsystem } => {
                assert_eq!(subsystem, "connector-http-client");
            }
            other => panic!("expected Unavailable(connector-http-client); got {other:?}"),
        }

        crate::close_store(handle).expect("close_store");
        drop(dir);
    }
}
