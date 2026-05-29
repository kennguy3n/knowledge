//! `knowledge_ffi` — UniFFI surface for iOS / Android platform bindings.
//!
//! Per `ARCHITECTURE.md` §3 ("Platform integration plane") and
//! `docs/DESIGN.md` §2 ("On-device runtime"), the knowledge substrate
//! ships as a Rust core with two foreign-language adapters:
//!
//! * **UniFFI** (this crate) — Swift on iOS, Kotlin on Android.
//! * **N-API** (the sibling `napi` crate) — Node / Electron on
//!   macOS / Windows desktop.
//!
//! Both adapters surface the **same** logical contract, defined here
//! as plain Rust structs / enums / functions. The contract has four
//! sections:
//!
//! 1. **Lifecycle** — [`open_store`] / [`close_store`].
//! 2. **Evidence store** — [`ingest_message`], [`query`], [`get_evidence`].
//! 3. **Memory manager** — [`get_user_memory`], [`pin`], [`unpin`],
//!    [`forget`], [`list_memories`], [`run_decay_sweep`].
//! 4. **Synthesis pipeline** — [`get_channel_memory`],
//!    [`trigger_synthesis`].
//! 5. **Crypto** — [`generate_keypair`], [`encrypt`], [`decrypt`].
//!
//! # Status
//!
//! * [`open_store`] / [`close_store`] / [`ingest_message`] / [`query`] /
//!   [`get_evidence`] / [`forget`] / [`encrypt`] / [`decrypt`] /
//!   [`generate_keypair`] are **wired through to the underlying internal
//!   crates** (`evidence_store`, `crypto`).
//! * [`get_user_memory`] / [`pin`] / [`unpin`] / [`list_memories`] /
//!   [`run_decay_sweep`] / [`get_channel_memory`] are wired through to
//!   the in-process [`memory_manager::UserMemoryObject`] /
//!   [`memory_manager::ChannelMemoryObject`] CRUD layer. Memory
//!   objects are persisted to the encrypted `memory_objects` table
//!   (schema v7) and rehydrated on [`open_store`].
//! * [`trigger_synthesis`] is **wired through to the on-device SLM
//!   router**: reads the recent-evidence window for the scope, renders
//!   the [`inference_router::InferenceTask::SynthSummary`] prompt,
//!   dispatches it through the [`inference_router::InferenceRouter`]
//!   stored on the runtime, parses the
//!   [`inference_router::SummaryBundle`] response, and persists the
//!   recap + extracted decisions / open questions / active tasks into
//!   the scope's [`memory_manager::ChannelMemoryObject`]. Returns the
//!   synthesis window UUID. Surfaces
//!   [`FfiError::Unavailable`] only when no adapter that supports
//!   [`inference_router::InferenceTask::SynthSummary`] is linked into
//!   this build (e.g. neither MLX nor the `http-client`-gated
//!   llama.cpp loopback is registered). See the function-level doc on
//!   [`trigger_synthesis`] for the full failure-mode table.
//!
//! All wired functions require a prior successful call to [`open_store`].
//! Calling any other function first returns
//! [`FfiError::Unavailable { subsystem: "evidence_store" }`].
//!
//! # Known simplifications
//!
//! These are deliberate to keep the unblocker PR small. Each one is
//! a clean follow-up:
//! * **Ingest hardcodes `ImportanceClass::Important`.** The
//!   evidence store supports `Important` / `Useful` / `Noise` (with
//!   different storage routing, including the noise ring buffer);
//!   exposing that knob through the FFI surface is a follow-up.
//! * **`query` forwards `query_text` verbatim to SQLite FTS5.**
//!   FTS5 has its own query grammar (`AND` / `OR` / `NOT` / `NEAR` /
//!   column filters). Hosts that want to treat user input as an
//!   opaque phrase must quote / escape it before calling here.
//!   Malformed expressions surface as [`FfiError::Evidence`].
//! * **Scores are ordering-only.** `score` and `fts_score` on
//!   [`QueryResult`] are a monotone position in `[0, 1]` over the
//!   actual result set, not a calibrated relevance signal.
//!   `recency_score` and `vector_score` stay at `0.0` until the
//!   embedding pipeline is wired.

#![deny(missing_docs)]
// Public FFI entry points below carry per-function
// `#[allow(clippy::needless_pass_by_value)]` rather than a
// module-level blanket. Each public function in this file is shaped
// for the UniFFI (Swift / Kotlin) and N-API (Electron) bindings,
// which hand ownership of `String` / `Vec<u8>` across the language
// boundary on every call; borrowed equivalents would force an extra
// copy in generated code (or simply not compile under UniFFI's
// proc-macro constraints). Keeping the allow local lets clippy keep
// catching inadvertent by-value taking in *internal* helpers in this
// file, which do not cross the FFI boundary and should idiomatically
// borrow.

// UniFFI scaffolding — emits the `extern "C"` shims, the
// per-type lift / lower codecs, the metadata blob the
// `uniffi-bindgen` binary walks when generating Swift / Kotlin
// from the compiled `staticlib` / `cdylib`, and the
// `<crate>_uniffi_contract_version()` / `<crate>_checksum_*()`
// guards that the platform bindings verify on load.
//
// MUST be invoked exactly once per UniFFI-exposed crate, at the
// crate root. The proc-macros (`#[uniffi::export]`,
// `#[derive(uniffi::Record / Enum / Error)]`,
// `uniffi::custom_newtype!`) refuse to compile if scaffolding is
// absent (or if it is invoked under a different crate name than
// the one the proc-macros stash in their metadata blob — see
// `https://mozilla.github.io/uniffi-rs/internals/lifting_and_lowering.html`
// for the upstream design note).
uniffi::setup_scaffolding!();

pub mod connector;
pub mod error;
pub mod health;
pub mod metrics;
pub mod runtime;
#[cfg(feature = "tracing-subscriber")]
pub mod tracing_init;
pub mod types;
pub mod webhook;

pub use connector::{
    authenticate_connector, clear_oauth_client_secret_resolver, create_connector, list_connectors,
    refresh_connector_token, remove_connector, set_oauth_client_secret_resolver, sync_connector,
    OAuthClientSecretResolver,
};
pub use error::{FfiError, FfiResult};
pub use health::{health_check, AdapterReport, HealthStatus, SubsystemHealth, SubsystemStatus};
pub use metrics::{snapshot as metrics_snapshot, ErrorCounters, MetricsSnapshot};
pub use runtime::{close_store, open_store, RuntimeHandle};
#[cfg(feature = "tracing-subscriber")]
pub use tracing_init::try_init_tracing;
pub use types::{
    ConnectorKindTag, ConnectorStatus, EvidenceRecord, FfiImportanceClass, FfiKeypair,
    FfiSignature, MemoryFilter, MemoryRecord, MemoryState, QueryResult, RefreshReport,
    ScopeIdString, SourceKind, SyncModeKind, SyncReport, SyncStatusKind, SynthesisTrigger,
    WebhookServerHandle, WebhookServerSummary,
};
pub use webhook::{
    list_webhook_servers, register_webhook_dispatch, start_webhook_server, stop_webhook_server,
    unregister_webhook_dispatch,
};

use crypto::{
    decrypt_aead, encrypt_aead, forgetting, signer_backend::MlDsa65Signer, AeadNonce,
    AEAD_NONCE_LEN,
};
use evidence_store::{EvidenceId, ImportanceClass, ScopeId};
use rand::RngCore;

use runtime::with_runtime;

// ─────────────────────────── Evidence store ──────────────────────────

/// Ingest a message into the encrypted evidence plane.
///
/// `scope_id` is a UUID string identifying the scope (channel,
/// thread, profile, …). `body` is plaintext UTF-8 to encrypt.
/// `source` is the connector tag (`"Slack"`, `"Email"`,
/// `"Manual"`, …). `importance` controls the storage tier: see
/// [`FfiImportanceClass`](types::FfiImportanceClass) for details.
///
/// Returns the new evidence row's UUID as a string on success.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::Evidence`] if the underlying evidence store fails.
/// * [`FfiError::NotFound`] if `scope_id` has been cryptographically
///   forgotten via [`forget`].
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn ingest_message(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    body: String,
    source: SourceKind,
    importance: FfiImportanceClass,
) -> FfiResult<String> {
    metrics::instrument(metrics::inc_ingest, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope_id.clone(),
                });
            }
            rt.ensure_scope_registered(scope)?;
            let result = rt
                .store_mut()
                .ingest(
                    scope,
                    body.as_bytes(),
                    Some(source_kind_tag(&source)),
                    ffi_importance_to_internal(importance),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            Ok(result.evidence_id.to_string())
        })
    })
}

/// Run a hybrid (FTS) query against a scope.
///
/// Returns up to `limit` rows ordered by FTS5 rank.
///
/// # Query syntax
///
/// `query_text` is forwarded verbatim to SQLite FTS5's `MATCH`
/// operator (via a parameterised query, so SQL injection is not a
/// concern). FTS5 has its own query grammar — `AND` / `OR` / `NOT` /
/// `NEAR` / column filters / phrase quoting / prefix matching. Hosts
/// that want to treat untrusted user input as a single opaque phrase
/// **must** quote it (`"…"`) and escape embedded quotes themselves
/// before calling here. Malformed expressions surface as
/// [`FfiError::Evidence`].
///
/// # Scoring
///
/// `score` and `fts_score` are a monotone position in `[0, 1]` over
/// the actual returned set, not calibrated relevance. `recency_score`
/// and `vector_score` stay at `0.0` until the embedding pipeline wires
/// `HybridRetriever` through this surface.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::Evidence`] if the underlying search fails (this
///   covers malformed FTS5 query syntax).
///
/// Returns an empty vector if `scope_id` has been forgotten — this is
/// a deliberate "soft" semantic so callers can treat forgotten scopes
/// the same as scopes that simply have no matching rows.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn query(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    query_text: String,
    limit: u32,
) -> FfiResult<Vec<QueryResult>> {
    metrics::instrument(metrics::inc_query, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(Vec::new());
            }
            let hits = rt
                .store()
                .search_fts(scope, &query_text, limit as usize)
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            // Capture the actual hit count up front so the score
            // denominator reflects the result set (not the requested
            // ceiling). Otherwise small result sets cluster near 1.0 —
            // e.g. 3 hits with limit=100 would yield 1.0 / 0.99 / 0.98
            // rather than 1.0 / 0.67 / 0.33.
            let hits_len = hits.len();
            let mut out = Vec::with_capacity(hits_len);
            let denom = hits_len.max(1) as f64;
            for (rank, evidence_id) in hits.into_iter().enumerate() {
                let snippet = rt
                    .store()
                    .read_body(evidence_id)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|s| snippet_clip(&s, 160))
                    .unwrap_or_default();
                // FTS5 ranks are negative (lower is better). We don't
                // currently expose ranking weights here — surface the
                // monotone position in [0, 1] as `fts_score` for callers
                // that only need ordering, and leave the recency /
                // vector components at 0.0 until the embedding pipeline (real ONNX
                // embeddings) and the dedicated `HybridRetriever` are
                // wired through this surface.
                let fts_score = 1.0 - (rank as f64 / denom).min(1.0);
                out.push(QueryResult {
                    evidence_id: evidence_id.to_string(),
                    score: fts_score,
                    fts_score,
                    recency_score: 0.0,
                    vector_score: 0.0,
                    snippet,
                });
            }
            Ok(out)
        })
    })
}

/// Escape user-supplied text for safe use in an FTS5 `MATCH` clause.
///
/// FTS5 interprets bare keywords, `AND`/`OR`/`NOT`/`NEAR`, prefix
/// globs (`*`), column filters (`:`) and phrase quotes as query
/// syntax. Passing raw user input directly to [`query`] can produce
/// parse errors or unintended Boolean logic.
///
/// This function wraps the input in double quotes (making it a
/// literal phrase) and escapes any embedded double quotes by
/// doubling them (`"` → `""`), which is the FTS5 escape convention.
///
/// ```text
/// escape_fts_query(r#"hello "world""#) => r#""hello ""world""""#
/// ```
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn escape_fts_query(input: String) -> String {
    // Pure string transform, infallible — no `metrics::instrument`
    // wrapper (which only fits `FfiResult<T>` for `Err` routing),
    // just the per-call counter. Increment before the body runs so
    // semantics match every other entry point ("calls initiated").
    crate::metrics::inc_escape_fts_query();
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

fn snippet_clip(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        body.to_string()
    } else {
        let mut out: String = body.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Fetch a single evidence row by id (returns decrypted plaintext).
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `evidence_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if the evidence row does not exist or
///   belongs to a forgotten scope.
/// * [`FfiError::Evidence`] if reading or decrypting the body fails.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn get_evidence(handle: RuntimeHandle, evidence_id: String) -> FfiResult<EvidenceRecord> {
    metrics::instrument(metrics::inc_get_evidence, || {
        let id = parse_evidence_id(&evidence_id)?;
        with_runtime(handle, |rt| {
            let row = rt
                .store()
                .get(id)
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?
                .ok_or_else(|| FfiError::NotFound {
                    kind: "evidence".into(),
                    id: evidence_id.clone(),
                })?;
            if rt.is_scope_forgotten(row.scope_id) {
                return Err(FfiError::NotFound {
                    kind: "evidence".into(),
                    id: evidence_id.clone(),
                });
            }
            let body_bytes = rt.store().read_body(id).map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
            let body = String::from_utf8(body_bytes).map_err(|_| FfiError::Evidence {
                message: "evidence body is not valid utf-8".into(),
            })?;
            Ok(EvidenceRecord {
                id: row.id.to_string(),
                scope_id: row.scope_id.to_string(),
                body,
                source: row
                    .source_ref
                    .as_deref()
                    .map_or(SourceKind::Other, parse_source_kind),
                created_at: row.created_at,
            })
        })
    })
}

// ───────────────────────── Memory manager ─────────────────────────
//
// Wired through to the in-process `UserMemoryObject` / `ChannelMemoryObject`
// CRUD layer in the `memory_manager` crate. Persistence to the
// encrypted evidence plane is not yet wired; the contract surfaced
// here is stable across the upcoming persistence work.

/// Fetch the per-user memory bundle for `scope_id`.
///
/// Returns the per-scope [`UserMemoryObject`](memory_manager::UserMemoryObject)'s
/// owned memory objects as wire-flat [`MemoryRecord`]s, ordered by
/// insertion. Returns an empty vector if the scope has been
/// cryptographically forgotten via [`forget`].
///
/// # Current simplification
///
/// The user memory layer is in-process only — `open_store` /
/// `close_store` cycles drop it. Persistence to the encrypted
/// evidence plane is not yet wired.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn get_user_memory(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
) -> FfiResult<Vec<MemoryRecord>> {
    metrics::instrument(metrics::inc_get_user_memory, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(Vec::new());
            }
            let Some(umo) = rt.user_memory(scope) else {
                return Ok(Vec::new());
            };
            Ok(umo.objects.iter().map(memory_object_to_record).collect())
        })
    })
}

/// Mark a memory record as `Pinned` (decay-immune) by its id.
///
/// The runtime walks every per-scope [`UserMemoryObject`] to find
/// the owning scope; the memory layer keeps an in-process index so
/// this is `O(scopes * objects-per-scope)` in the worst case, which
/// is fine for current working set sizes.
///
/// If the owning scope has been cryptographically forgotten (Gap 4
/// tombstone in `forgotten_scopes`), the pin is rejected with
/// `NotFound { kind: "memory" }` — the same shape every read path
/// (`get_user_memory`, `list_memories`) presents for that scope.
/// Mutating an object whose owning DEK has been destroyed would
/// leave host caches in an observably inconsistent state.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `id` is not a valid UUID.
/// * [`FfiError::NotFound`] if no memory object has that id in any
///   open scope, or if the owning scope has been forgotten.
/// * [`FfiError::Memory`] if the underlying state-machine transition
///   rejects the pin (e.g. the object is in a terminal state).
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn pin(handle: RuntimeHandle, id: String) -> FfiResult<()> {
    metrics::instrument(metrics::inc_pin, || {
        let uuid = parse_uuid(&id)?;
        with_runtime(handle, |rt| {
            let owning_scope =
                locate_owning_scope(rt, &uuid).ok_or_else(|| FfiError::NotFound {
                    kind: "memory".into(),
                    id: id.clone(),
                })?;
            if rt.is_scope_forgotten(owning_scope) {
                return Err(FfiError::NotFound {
                    kind: "memory".into(),
                    id: id.clone(),
                });
            }
            let umo = rt
                .user_memories
                .get_mut(&owning_scope)
                .expect("owning scope located above must still exist");
            umo.pin(&uuid).map_err(|e| FfiError::Memory {
                message: e.to_string(),
            })?;
            rt.flush_user_memory(owning_scope)
        })
    })
}

/// Lift a previously-applied pin so the row resumes ageing under the
/// decay state machine.
///
/// If the owning scope has been cryptographically forgotten (Gap 4
/// tombstone in `forgotten_scopes`), the unpin is rejected with
/// `NotFound { kind: "memory" }` — see [`pin`] for the rationale.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `id` is not a valid UUID.
/// * [`FfiError::NotFound`] if no memory object has that id in any
///   open scope, or if the owning scope has been forgotten.
/// * [`FfiError::Memory`] if the underlying state-machine rejects.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn unpin(handle: RuntimeHandle, id: String) -> FfiResult<()> {
    metrics::instrument(metrics::inc_unpin, || {
        let uuid = parse_uuid(&id)?;
        with_runtime(handle, |rt| {
            let owning_scope =
                locate_owning_scope(rt, &uuid).ok_or_else(|| FfiError::NotFound {
                    kind: "memory".into(),
                    id: id.clone(),
                })?;
            if rt.is_scope_forgotten(owning_scope) {
                return Err(FfiError::NotFound {
                    kind: "memory".into(),
                    id: id.clone(),
                });
            }
            let umo = rt
                .user_memories
                .get_mut(&owning_scope)
                .expect("owning scope located above must still exist");
            umo.unpin(&uuid).map_err(|e| FfiError::Memory {
                message: e.to_string(),
            })?;
            rt.flush_user_memory(owning_scope)
        })
    })
}

/// Cryptographically forget every evidence row in the scope that
/// owns the row identified by `id`.
///
/// `id` is the UUID-string of an evidence row. The runtime resolves
/// it to its owning [`ScopeId`] and destroys the in-memory scope DEK
/// via [`crypto::forgetting::destroy_scope_dek`], adding a tombstone
/// to the per-process [`crypto::forgetting::DekRegistry`].
///
/// After this call, every subsequent [`query`] / [`get_evidence`] /
/// [`ingest_message`] / [`encrypt`] / [`decrypt`] for the same scope
/// short-circuits with [`FfiError::NotFound`] (or an empty result, in
/// the case of [`query`]).
///
/// # Durability
///
/// The tombstone is **persisted** to the
/// `forgotten_scopes` table on the encrypted evidence database, and
/// the FTS5 / embedding secondary indexes are purged inline. On the
/// next [`open_store`], the runtime replays every persisted
/// tombstone into a fresh in-memory `DekRegistry`, so subsequent
/// calls for the same scope continue to short-circuit with
/// [`FfiError::NotFound`].
///
/// The encrypted **inline** bodies in the `evidence` table are
/// intentionally not deleted — the append-only trigger forbids it,
/// and without the per-scope DEK the ciphertexts are unrecoverable.
/// For **body-table** rows (`body_store`), `forget()` destroys the
/// per-scope CEK wraps (`body_store_key_wraps`). When no scope
/// retains a wrap for a given content hash the body row is garbage-
/// collected. Hosts that need to scrub surviving inline ciphertexts
/// must perform a VACUUM-style rebuild at a higher layer.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `id` is not a valid UUID.
/// * [`FfiError::NotFound`] if no evidence row has that id (e.g. if
///   the caller passed a *scope* UUID directly — there is no
///   `forget_scope` surface yet).
/// * [`FfiError::Evidence`] if persisting the tombstone or purging
///   the FTS / embedding indexes fails. The in-memory DEK
///   destruction is still effective in this case, but the next
///   `open_store` will not see the tombstone and the FTS index may
///   still contain plaintext for the affected scope.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn forget(handle: RuntimeHandle, id: String) -> FfiResult<()> {
    metrics::instrument(metrics::inc_forget, || {
        let evidence_id = parse_evidence_id(&id)?;
        with_runtime(handle, |rt| {
            let row = rt
                .store()
                .get(evidence_id)
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?
                .ok_or_else(|| FfiError::NotFound {
                    kind: "evidence".into(),
                    id: id.clone(),
                })?;
            // Both `forget` (by evidence id) and `forget_scope` (by
            // scope uuid) share the *exact* cryptographic-forgetting
            // sequence — DEK destroy + tombstone + FTS purge +
            // body-key wraps purge + memory blob delete + in-memory
            // memory purge + connector lifecycle purge. Routing both
            // entry points through `forget_scope_state` is what keeps
            // the contract honest: any new piece of scope-bound state
            // added in the future only has to be torn down in *one*
            // place, and both forget paths inherit the fix.
            forget_scope_state(rt, row.scope_id)
        })
    })
}

/// Cryptographically forget a scope by its UUID directly.
///
/// Unlike [`forget`] which resolves an evidence ID to find the
/// scope, this function accepts a scope UUID directly. This is
/// the preferred API when the caller already knows the scope —
/// see `crates/ffi/src/lib.rs` doc header §"Direct scope
/// operations".
///
/// The mechanics mirror [`forget`]: the per-scope DEK is destroyed
/// in memory, a tombstone is persisted, and the FTS5 / body-store
/// CEK wraps are purged.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::Evidence`] if persisting the tombstone or purging
///   secondary indexes fails.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn forget_scope(handle: RuntimeHandle, scope_id: String) -> FfiResult<()> {
    metrics::instrument(metrics::inc_forget_scope, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| forget_scope_state(rt, scope))
    })
}

/// Shared cryptographic-forgetting sequence executed by both
/// [`forget`] (by evidence id) and [`forget_scope`] (by scope uuid).
///
/// # Failure model
///
/// Step 1 (in-memory DEK destruction + on-disk tombstone) is the
/// only load-bearing atomic op: if it fails the scope is NOT
/// forgotten and the caller MUST see the error, so we bail out
/// immediately and skip the secondary cleanups (they would race
/// against a scope that is still readable).
///
/// Steps 2–8 are all *secondary* cleanups against state that the
/// tombstone already makes unreachable through the public read
/// path (`open_store` recovery + `is_scope_forgotten` guards).
/// They are each independently important for the
/// cryptographic-forgetting contract — in particular step 8 drops
/// **plaintext OAuth2 bearer tokens** out of process memory, which
/// is the highest-sensitivity secondary state in the substrate.
/// Letting one failing secondary cleanup short-circuit the others
/// would leave orphaned plaintext credentials in `token_vault` for
/// a forgotten scope, which violates the contract this helper
/// exists to enforce.
///
/// So steps 2–8 are run *unconditionally* — every step is attempted
/// regardless of earlier failures, errors are accumulated, and the
/// first error encountered is returned to the caller after every
/// cleanup has had a chance to run. Errors from earlier secondary
/// steps do NOT mask later secondary steps in any way: each step
/// owns its own piece of state and runs against that state directly,
/// so a SQLCipher I/O failure on step 3 does not affect the
/// in-memory connector teardown on step 8.
///
/// The ordered sequence:
///
/// 1. In-memory DEK destruction + on-disk tombstone (atomic via
///    `FfiRuntime::forget_scope` → `TombstoneStore`). **Bail on
///    failure — the scope is not forgotten.**
/// 2. Best-effort SQLCipher DEK deletion (tombstone still blocks
///    access on failure; `open_store`'s recovery path retries).
/// 3. FTS5 + body-key-wrap purge so plaintext-derived secondary
///    payloads cannot be recovered post-forget.
/// 4. Persisted memory blob deletion so forgotten-scope memory
///    state does not survive the next `open_store`.
/// 5. In-memory memory map purge (infallible — `HashMap::remove`).
/// 6. Persisted connector instance row deletion (`connector_instances`
///    table). Best-effort — failure logs WARN and accumulates the
///    error; the row's AEAD ciphertext is sealed under the scope
///    DEK that step 1 just destroyed, so the payload is
///    cryptographically unrecoverable. `open_store`'s rehydration
///    sweep also picks up any orphaned row on next boot.
/// 7. Persisted OAuth2 token row deletion (`connector_tokens`
///    table). Same best-effort discipline as step 6 — the token
///    ciphertext is sealed under the destroyed scope DEK so
///    failure to delete the row does not leak plaintext
///    credentials.
/// 8. Connector lifecycle purge — every in-memory
///    `ConnectorInstance` row, live `Arc<dyn Connector>` handle,
///    and cached OAuth2 token bound to the forgotten scope is
///    dropped so a later `sync_connector` (or any token-vault
///    dump) cannot resurrect plaintext provider credentials and
///    re-emit fresh evidence onto a tombstoned scope.
///    **Infallible** — purely in-memory `HashMap::remove` +
///    `OAuth2TokenVault::remove`, both idempotent.
///
/// Any new piece of scope-bound state added in the future MUST be
/// torn down here — routing both forget entry points through this
/// helper is what keeps the cryptographic-forgetting contract
/// honest.
fn forget_scope_state(rt: &mut crate::runtime::FfiRuntime, scope: ScopeId) -> FfiResult<()> {
    // 1. Atomic in-memory + on-disk forgetting (see
    //    `FfiRuntime::forget_scope` for the rationale). Bail on
    //    failure: if this fails the scope is still readable and
    //    running the secondary cleanups would prematurely tear down
    //    state the host still has the right to read.
    rt.forget_scope(scope)?;

    // Steps 2–8 are best-effort secondary cleanups. Every step
    // MUST be attempted regardless of earlier failures so that the
    // in-memory connector teardown (step 8 — which drops plaintext
    // OAuth2 tokens) is never skipped because a SQLCipher purge
    // happened to fail upstream. We accumulate the first error
    // encountered and surface it to the caller after every step
    // has run.
    let mut first_error: Option<FfiError> = None;

    // 2. Best-effort SQLCipher DEK deletion: if this fails the
    //    tombstone still blocks access and `open_store`'s recovery
    //    path will retry the deletion on next startup. Tracing-only
    //    on failure — we do NOT promote this to `first_error`
    //    because the documented recovery path handles it.
    if let Err(e) = rt.store_mut().delete_scope_dek(scope) {
        tracing::warn!(
            scope = %scope.as_uuid(),
            error = %e,
            "failed to delete scope DEK; will retry on next open_store",
        );
    }

    // 3. Purge the FTS5 index + body-store CEK wraps so
    //    plaintext-derived secondary payloads cannot be recovered
    //    post-forget. Capture the first failure but DO NOT
    //    short-circuit — the steps below own disjoint state and
    //    are equally important to the forgetting contract.
    if let Err(e) = rt.store_mut().purge_fts_for_scope(scope) {
        let err = FfiError::Evidence {
            message: e.to_string(),
        };
        tracing::warn!(
            scope = %scope.as_uuid(),
            error = %err,
            "forget_scope_state: purge_fts_for_scope failed; continuing to subsequent cleanups",
        );
        first_error.get_or_insert(err);
    }
    if let Err(e) = rt.store_mut().purge_body_key_wraps_for_scope(scope) {
        let err = FfiError::Evidence {
            message: e.to_string(),
        };
        tracing::warn!(
            scope = %scope.as_uuid(),
            error = %err,
            "forget_scope_state: purge_body_key_wraps_for_scope failed; continuing to subsequent cleanups",
        );
        first_error.get_or_insert(err);
    }

    // 4. Delete persisted memory blobs so forgotten-scope memory
    //    state does not survive the next `open_store`.
    if let Err(e) = rt.store().delete_memory_blobs_for_scope(scope) {
        let err = FfiError::Evidence {
            message: e.to_string(),
        };
        tracing::warn!(
            scope = %scope.as_uuid(),
            error = %err,
            "forget_scope_state: delete_memory_blobs_for_scope failed; continuing to connector teardown",
        );
        first_error.get_or_insert(err);
    }

    // 5. In-memory memory maps. Infallible.
    rt.user_memories.remove(&scope);
    rt.channel_memories.remove(&scope);

    // 6. Delete persisted connector instance rows for the scope.
    //    Best-effort: even if the SQL DELETE fails, the rows are
    //    AEAD-encrypted under the scope DEK that step 1 destroyed,
    //    so the payload is cryptographically unrecoverable. The
    //    dangling rows are also picked up on the next `open_store`'s
    //    rehydration sweep (which checks `tombstones.contains` and
    //    deletes any row bound to a forgotten scope). We accumulate
    //    the first error so callers see the gap while still running
    //    step 8's infallible in-memory teardown unconditionally.
    if let Err(e) = rt.store().delete_connector_instances_for_scope(scope) {
        let err = FfiError::Evidence {
            message: e.to_string(),
        };
        tracing::warn!(
            scope = %scope.as_uuid(),
            error = %err,
            "forget_scope_state: delete_connector_instances_for_scope failed; rows will be cleaned up on next open_store",
        );
        first_error.get_or_insert(err);
    }

    // 7. Delete persisted OAuth2 token rows for the scope. Same
    //    best-effort discipline as step 6 — the token ciphertext is
    //    sealed under the destroyed scope DEK so failure to delete
    //    the row does not leak plaintext credentials.
    if let Err(e) = rt.store().delete_connector_tokens_for_scope(scope) {
        let err = FfiError::Evidence {
            message: e.to_string(),
        };
        tracing::warn!(
            scope = %scope.as_uuid(),
            error = %err,
            "forget_scope_state: delete_connector_tokens_for_scope failed; rows will be cleaned up on next open_store",
        );
        first_error.get_or_insert(err);
    }

    // 8. Connector lifecycle: every `ConnectorInstance` row, live
    //    `Arc<dyn Connector>` handle, and cached OAuth2 token bound
    //    to the forgotten scope MUST become unrecoverable.
    //    Infallible — purely in-memory `HashMap::remove` and
    //    `OAuth2TokenVault::remove` (which treats a missing entry
    //    as a benign no-op).
    //
    //    Collect first to release the immutable borrow on
    //    `connector_instances` before the removal loop takes mutable
    //    borrows on the same map plus disjoint `connectors` /
    //    `token_vault` borrows.
    let connector_ids_to_remove: Vec<connector_framework::ConnectorInstanceId> = rt
        .connector_instances
        .iter()
        .filter_map(|(id, inst)| {
            if inst.config.scope_id == scope {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    for id in connector_ids_to_remove {
        rt.connector_instances.remove(&id);
        rt.connectors.remove(&id);
        // `OAuth2TokenVault::remove` returns
        // `ConnectorError::TokenNotFound` if no token was cached
        // for the instance (e.g. a connector created but never
        // authenticated). Treat that as a benign no-op so the
        // forgetting path is idempotent.
        let _ = rt.token_vault.remove(id);
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// List memory records for a scope, optionally filtered by state.
///
/// Returns rows from the per-scope [`UserMemoryObject`] matching
/// the supplied [`MemoryFilter`]. Returns an empty vector if the
/// scope has been cryptographically forgotten.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings/structs across the language boundary on every call.
#[uniffi::export]
pub fn list_memories(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    filter: MemoryFilter,
) -> FfiResult<Vec<MemoryRecord>> {
    metrics::instrument(metrics::inc_list_memories, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(Vec::new());
            }
            let Some(umo) = rt.user_memory(scope) else {
                return Ok(Vec::new());
            };
            let mm_filter = ffi_filter_to_memory_filter(&filter, scope);
            // `MemoryState::Pinned` has no native internal state — it is
            // a pin-count predicate layered on top of the underlying
            // state machine. The call-site filter must apply whenever
            // the caller asked for pinned rows either through the
            // explicit `pinned_only` flag *or* by selecting the
            // `Pinned` state. Gating only on `pinned_only` silently
            // dropped the `state = Some(Pinned)` filter.
            let require_pinned = filter.pinned_only || filter.state == Some(MemoryState::Pinned);
            let out: Vec<MemoryRecord> = umo
                .list(&mm_filter)
                .into_iter()
                .filter(|o| !require_pinned || o.pin_count > 0)
                .map(memory_object_to_record)
                .collect();
            Ok(out)
        })
    })
}

/// Run a decay sweep over `scope_id`. Returns the count of rows
/// transitioned to `Archived` (Candidate → Archived plus
/// Superseded → Archived) by this sweep.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn run_decay_sweep(handle: RuntimeHandle, scope_id: ScopeIdString) -> FfiResult<u32> {
    metrics::instrument(metrics::inc_decay_sweep, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(0);
            }
            // Only run decay and flush if the scope has an existing UMO.
            // `user_memory_mut` would create an empty one, and flushing
            // it would persist an orphan empty blob.
            if rt.user_memory(scope).is_none() {
                return Ok(0);
            }
            let umo = rt.user_memory_mut(scope);
            let report = umo.decay_sweep(chrono::Utc::now());
            // `candidates_archived + superseded_archived` are `usize`
            // counters; saturate at `u32::MAX` for the FFI return rather
            // than wrapping. The substrate's working sets are bounded
            // well below `u32::MAX` per scope so this is a defensive
            // cast, not a behavioural one.
            let count = u32::try_from(report.candidates_archived + report.superseded_archived)
                .unwrap_or(u32::MAX);
            rt.flush_user_memory(scope)?;
            Ok(count)
        })
    })
}

// ──────────────────────── Synthesis pipeline ───────────────────────

/// Fetch the channel-level synthesis memory for `scope_id`.
///
/// Returns the latest channel recap (as a [`MemoryRecord`]) if any
/// has been produced for this scope, or `None` if synthesis has
/// never run.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn get_channel_memory(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
) -> FfiResult<Option<MemoryRecord>> {
    metrics::instrument(metrics::inc_get_channel_memory, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(None);
            }
            let Some(cmo) = rt.channel_memory(scope) else {
                return Ok(None);
            };
            if cmo.recap.is_empty() {
                return Ok(None);
            }
            Ok(Some(MemoryRecord {
                id: cmo.id.to_string(),
                scope_id: cmo.scope_id.to_string(),
                summary: cmo.recap.clone(),
                state: MemoryState::Reinforced,
                retention_score: 1.0,
                created_at: cmo.created_at.timestamp(),
                last_reinforced_at: cmo.updated_at.timestamp(),
            }))
        })
    })
}

/// Trigger synthesis on `scope_id` with the given trigger reason.
///
/// # Behaviour
///
/// Reads the most recent
/// [`SYNTHESIS_EVIDENCE_WINDOW`] evidence rows for `scope_id`,
/// decrypts their bodies, builds an [`InferenceTask::SynthSummary`]
/// prompt, dispatches it through the runtime's
/// [`InferenceRouter`], parses the resulting [`SummaryBundle`] JSON,
/// and writes the recap + extracted decisions / open questions /
/// active tasks into the scope's [`ChannelMemoryObject`]. The
/// channel memory is then flushed to the encrypted
/// `memory_objects` table so the recap survives process restarts.
///
/// Returns the synthesis window id (UUID) as a string. The same
/// id is stored on the channel memory's `last_synthesis_window`
/// field so the host can correlate the recap with the call that
/// produced it.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called,
///   or if the synthesis subsystem has no adapter that supports
///   [`InferenceTask::SynthSummary`] available in this build
///   (e.g. neither MLX nor the llama.cpp loopback is linked).
///   Hosts should treat this as a transient / setup-time failure
///   and retry after the platform shell registers its adapters.
/// * [`FfiError::InferenceFailure`] if an adapter was selected and
///   ran but produced an unusable result (grammar violation, model
///   error, transport failure mid-stream). Hosts SHOULD NOT
///   silently retry the same prompt — see
///   [`FfiError::InferenceFailure`] for the contract.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `scope_id` has been forgotten or
///   has no evidence to summarise.
/// * [`FfiError::Evidence`] if the underlying store fails (read
///   or memory-blob flush).
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
#[uniffi::export]
pub fn trigger_synthesis(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    trigger: SynthesisTrigger,
) -> FfiResult<String> {
    metrics::instrument(metrics::inc_synthesis_triggered, || {
        let scope = parse_scope_id(&scope_id)?;
        tracing::info!(
            scope = %scope.as_uuid(),
            trigger = ?trigger,
            "trigger_synthesis: dispatching SynthSummary",
        );
        synthesize_scope(handle, scope, &scope_id).map_err(|err| {
            tracing::warn!(
                scope = %scope.as_uuid(),
                error = ?err,
                "trigger_synthesis: failed",
            );
            err
        })
    })
}

/// Window size (in evidence rows) used by [`trigger_synthesis`] to
/// build the SLM prompt. Picked to fit comfortably inside the
/// 4 K-token context of the SLMs the substrate targets (the
/// `recap` field in the produced bundle is a 2–4 sentence headline
/// per the [`InferenceTask::SynthSummary`] prompt template, so
/// every additional row crowds the model). Public so integration
/// tests can assert against the exact same window the production
/// path uses.
pub const SYNTHESIS_EVIDENCE_WINDOW: usize = 50;

/// Core synthesis implementation called by [`trigger_synthesis`]
/// once the scope-id has been parsed.
///
/// # Locking discipline
///
/// Split into three phases so the per-handle [`FfiRuntime`] mutex is
/// **released** for the duration of the SLM dispatch:
///
/// 1. **Gather (locked).** [`with_runtime`] acquires the mutex.
///    Validates the scope, reads the recent-evidence window, decrypts
///    bodies (skip-and-warn on per-row failure — see the body-decode
///    loop below), renders the [`InferenceTask::SynthSummary`] prompt,
///    and clones an [`Arc`] handle to the inference router. Returns
///    the handle + prompt; **drops the mutex.**
/// 2. **Dispatch (unlocked).** Calls [`InferenceRouter::wait_for_bootstrap`]
///    and [`InferenceRouter::dispatch`] against the cloned `Arc`. The
///    runtime mutex is NOT held — concurrent
///    [`ingest_message`](crate::ingest_message) / [`query`](crate::query)
///    / [`get_channel_memory`](crate::get_channel_memory) calls on the
///    same handle run in parallel with the SLM. This is the key
///    correctness contract: an SLM dispatch can take seconds (especially
///    on the `http-client`-backed llama.cpp adapter waiting on the
///    bootstrap probe + the actual generation), and serialising every
///    FFI call behind that latency would freeze ingest / query for the
///    entire window.
/// 3. **Apply (locked).** [`with_runtime`] re-acquires the mutex.
///    Re-checks `is_scope_forgotten` (TOCTOU defence: another thread may
///    have called [`forget_scope`](crate::forget_scope) during the
///    unlocked phase — racing the recap onto a forgotten scope would
///    resurrect deleted state). Allocates / fetches the
///    [`ChannelMemoryObject`], merges the [`SummaryBundle`] via the
///    `*_dedup` helpers, and flushes to disk.
///
/// The `Arc<InferenceRouter>` keeps the router alive across the
/// unlocked phase even though no [`with_runtime`] frame is pinning the
/// `FfiRuntime` (the surrounding `close_store` drain loop blocks on
/// outstanding `Arc<Mutex<FfiRuntime>>` clones from `WITH_RUNTIME_STACK`,
/// so the runtime itself is not at risk of disappearing — but stripping
/// the lifetime tie is what makes the split structurally legal).
///
/// # Lost-work race with `close_store`
///
/// Because Phase 2 runs without the per-handle mutex, a host that
/// calls [`close_store`](crate::close_store) **concurrently** with
/// `trigger_synthesis` can land its close between Phase 2 and Phase 3:
///
/// * Phase 1 captures the [`Arc<InferenceRouter>`] and drops the
///   mutex.
/// * Phase 2 issues the SLM dispatch. The host calls
///   [`close_store`](crate::close_store) on a different thread, which
///   removes the handle from the registry and (after the drain loop
///   completes — see the docs on
///   [`close_store`](crate::close_store)) drops the runtime.
/// * Phase 2 returns successfully with a parsed [`SummaryBundle`].
/// * Phase 3's [`with_runtime`] re-lookup fails with
///   [`FfiError::Unavailable`] because the handle is no longer in
///   the registry.
///
/// In that scenario the SLM did real work (and burned real wall
/// clock / GPU time) but the recap is **discarded** — the host
/// observes `Unavailable` even though synthesis "happened". This is
/// a *safe* race — Phase 3 is the only phase that writes to the
/// store, so no partial state is ever persisted — and the
/// alternative (holding the mutex across the multi-second SLM
/// dispatch) would freeze every other FFI call on the handle.
/// Hosts that need to guarantee no synthesis runs are lost across
/// shutdown should await the result of `trigger_synthesis` before
/// calling [`close_store`](crate::close_store); the substrate's
/// recommended close path drains pending FFI calls externally rather
/// than letting them race the close.
fn synthesize_scope(
    handle: RuntimeHandle,
    scope: ScopeId,
    scope_id: &ScopeIdString,
) -> FfiResult<String> {
    use inference_router::{InferenceTask, RouterError, SummaryBundle};

    // ─────────────────── Phase 1: gather (locked) ───────────────────
    //
    // Returns the prompt to dispatch plus an owned `Arc` clone of the
    // router so the unlocked phase below can operate without re-entering
    // `with_runtime`.
    let (router, prompt) = with_runtime(handle, |rt| {
        if rt.is_scope_forgotten(scope) {
            return Err(FfiError::NotFound {
                kind: "scope".into(),
                id: scope_id.clone(),
            });
        }
        let recent_ids = rt
            .store()
            .recent_evidence_ids_for_scope(scope, SYNTHESIS_EVIDENCE_WINDOW)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        if recent_ids.is_empty() {
            return Err(FfiError::NotFound {
                kind: "evidence".into(),
                id: scope.as_uuid().to_string(),
            });
        }

        // Decrypt each row in reverse so the prompt reads chronologically
        // (oldest message first → newest last), matching the natural
        // reading order the prompt template asks the SLM to summarise.
        // `recent_evidence_ids_for_scope` returns newest-first; reverse
        // before reading.
        //
        // Skip-and-warn (NOT fail-fast) on per-row read failures: a
        // single corrupted body row (missing body-table entry, DEK
        // destroyed mid-flight, etc.) must not block an otherwise-useful
        // recap. This matches `EvidenceStore::search_hybrid`, which
        // demotes corrupted rows to `vector_score = 0.0` rather than
        // failing the whole search. The synthesis path inherits the
        // same contract: every readable row contributes to the prompt;
        // unreadable rows are logged and dropped. Only if EVERY row in
        // the window is unreadable do we surface `FfiError::Evidence`
        // to the host — otherwise the recap proceeds against the
        // readable subset.
        let mut bodies: Vec<String> = Vec::with_capacity(recent_ids.len());
        let mut skipped: usize = 0;
        for evidence_id in recent_ids.iter().rev() {
            match rt.store().read_body(*evidence_id) {
                Ok(body) => {
                    // Lossy decode is intentional: evidence bodies are
                    // UTF-8 in practice (ingest is gated through
                    // `String` at the FFI surface) but a malformed row
                    // should still be summarisable rather than failing
                    // the entire synthesis call.
                    bodies.push(String::from_utf8_lossy(&body).into_owned());
                }
                Err(e) => {
                    skipped = skipped.saturating_add(1);
                    tracing::warn!(
                        scope = %scope.as_uuid(),
                        evidence = %evidence_id.as_uuid(),
                        error = %e,
                        "trigger_synthesis: skipping unreadable evidence row",
                    );
                }
            }
        }
        if bodies.is_empty() {
            return Err(FfiError::Evidence {
                message: format!(
                    "synthesis: every evidence row in the {SYNTHESIS_EVIDENCE_WINDOW}-row \
                     window for scope {} was unreadable ({skipped} skipped)",
                    scope.as_uuid()
                ),
            });
        }
        if skipped > 0 {
            tracing::warn!(
                scope = %scope.as_uuid(),
                skipped,
                kept = bodies.len(),
                "trigger_synthesis: proceeding with partial evidence window",
            );
        }

        let combined = bodies.join("\n\n");
        let prompt = InferenceTask::SynthSummary
            .prompt_template()
            .replace("{body}", &combined);
        // Clone the `Arc` while still holding the runtime mutex so the
        // unlocked dispatch phase below has a stable handle that
        // outlives the `with_runtime` frame. The clone itself is one
        // atomic increment.
        Ok((rt.inference_router_arc(), prompt))
    })?;

    // ───────────────── Phase 2: dispatch (UNLOCKED) ─────────────────
    //
    // The per-handle `FfiRuntime` mutex is released for the duration of
    // these calls so concurrent `ingest_message` / `query` /
    // `get_channel_memory` on the same handle can run in parallel with
    // the (potentially multi-second) SLM dispatch.
    //
    // `open_store` spawns the adapter probe on a background thread to
    // keep the open path itself non-blocking; wait here until the
    // bootstrap finishes so a host that calls `trigger_synthesis`
    // immediately after `open_store` does not race the probe. The wait
    // is a no-op once probing has completed.
    router.wait_for_bootstrap();
    let raw = router
        .dispatch(InferenceTask::SynthSummary, &prompt)
        .map_err(|e| match e {
            // The model ran but produced an unusable result — hosts
            // need to distinguish this from "no adapter available"
            // to drive their own retry policy. See
            // `FfiError::InferenceFailure` docs for the contract.
            RouterError::InferenceFailure(message) => FfiError::InferenceFailure {
                message: format!("synthesis: {message}"),
            },
            // `Unavailable`, `TierTooLow`, and `NotProbed` all mean
            // "no adapter on this build can serve the task"; surface
            // them uniformly as a transient-unavailable subsystem so
            // hosts can probe again once their environment changes.
            other => FfiError::Unavailable {
                subsystem: format!("synthesis: {other}"),
            },
        })?;
    // Mapped to `InferenceFailure` (not `Evidence`) because the failure
    // mode is "the model ran but produced unusable JSON", which is the
    // same retry-policy class as `RouterError::InferenceFailure` above
    // — the evidence store never even ran, so misclassifying as
    // `Evidence` would route the host to the wrong remediation
    // (database recovery vs. retry / fall back to a different adapter
    // / re-prompt). The grammar constraint applied at dispatch time
    // should make this branch unreachable in practice, but a buggy /
    // unconstrained adapter could still feed us non-JSON.
    let bundle: SummaryBundle =
        serde_json::from_str(&raw).map_err(|e| FfiError::InferenceFailure {
            message: format!("synthesis: malformed SummaryBundle JSON: {e}"),
        })?;

    // ─────────────────── Phase 3: apply (locked) ────────────────────
    //
    // Re-acquire the mutex. We must re-check `is_scope_forgotten` here
    // because another thread may have called `forget_scope` while the
    // mutex was released — racing the recap onto a forgotten scope
    // would resurrect deleted state and break the
    // cryptographic-forgetting guarantee documented on
    // [`crate::forget_scope`].
    with_runtime(handle, |rt| {
        if rt.is_scope_forgotten(scope) {
            return Err(FfiError::NotFound {
                kind: "scope".into(),
                id: scope_id.clone(),
            });
        }

        let window_id = uuid::Uuid::new_v4();
        // Build the next channel-memory state OFF-THE-SIDE. We clone
        // any existing entry so the dedup helpers can compare against
        // the history, but the runtime's `channel_memories` map is
        // NOT mutated until `save_channel_memory` below succeeds at
        // writing the new state to disk.
        //
        // This pins the
        // `trigger_synthesis_failure_does_not_allocate_channel_memory`
        // invariant across every failure mode between the dispatch
        // result and the final flush — including future ones that
        // might be added between the `SummaryBundle` parse above and
        // the save below.
        let mut cmo = rt
            .channel_memory(scope)
            .cloned()
            .unwrap_or_else(|| memory_manager::ChannelMemoryObject::new(scope));
        cmo.update_recap(bundle.recap.clone(), Some(window_id));
        // `*_dedup` variants — the SLM re-emits the same decisions /
        // questions / tasks every synthesis window because each run
        // sees an overlapping evidence window. Naive `add_*` would
        // append the same surface text on every run; dedup preserves
        // the original entry's lifecycle state (resolved /
        // completed) and prevents unbounded growth across runs.
        for decision in bundle.decisions {
            cmo.add_decision_dedup(memory_manager::Decision::new(scope, decision));
        }
        for q in bundle.open_questions {
            cmo.add_open_question_dedup(memory_manager::OpenQuestion::new(scope, q));
        }
        for task_text in bundle.active_tasks {
            cmo.add_task_dedup(memory_manager::ActiveTask::new(scope, task_text));
        }
        rt.save_channel_memory(scope, cmo)?;
        Ok(window_id.to_string())
    })
}

// ──────────────────────────── Crypto ────────────────────────────

/// Generate a fresh ML-DSA-65 (FIPS 204) signing keypair.
///
/// The substrate's canonical post-quantum signature primitive — see
/// `crypto::signer_backend::MlDsa65Signer`.
///
/// # Errors
///
/// Currently infallible; the [`FfiResult`] wrap exists so future
/// hardware-backed key generators (Secure Enclave, StrongBox) can
/// surface failures without breaking the FFI contract.
#[uniffi::export]
pub fn generate_keypair() -> FfiResult<FfiKeypair> {
    metrics::instrument(metrics::inc_generate_keypair, || {
        let signer = MlDsa65Signer::generate();
        let encoded = signer.encode();
        Ok(FfiKeypair {
            algorithm: "ml-dsa-65".into(),
            public_key: <_ as AsRef<[u8]>>::as_ref(&encoded.verifying_key).to_vec(),
            private_key: <_ as AsRef<[u8]>>::as_ref(&encoded.signing_seed).to_vec(),
        })
    })
}

/// Encrypt `plaintext` for `scope_id` using XChaCha20-Poly1305 and
/// the scope-derived AEAD key. Returns a `nonce || ciphertext`
/// envelope (24-byte nonce prefix + AEAD ciphertext + Poly1305 tag).
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::Evidence`] if scope DEK registration fails (store
///   write error).
/// * [`FfiError::Crypto`] on AEAD or key-derivation failure.
/// * [`FfiError::NotFound`] if `scope_id` has been forgotten.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned byte buffers across the language boundary on every call.
#[uniffi::export]
pub fn encrypt(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    plaintext: Vec<u8>,
) -> FfiResult<Vec<u8>> {
    metrics::instrument(metrics::inc_encrypt, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope_id.clone(),
                });
            }
            // Auto-register so new scopes get a random DEK and
            // pre-v6 scopes adopt their HKDF key into the registry.
            rt.ensure_scope_registered(scope)?;
            let key = rt.scope_encrypt_key(scope)?;
            let mut nonce: AeadNonce = [0u8; AEAD_NONCE_LEN];
            rand::thread_rng().fill_bytes(&mut nonce);
            let aad = scope_aad(scope);
            let ciphertext =
                encrypt_aead(&key, &nonce, &plaintext, &aad).map_err(|e| FfiError::Crypto {
                    message: e.to_string(),
                })?;
            let mut out = Vec::with_capacity(AEAD_NONCE_LEN + ciphertext.len());
            out.extend_from_slice(&nonce);
            out.extend_from_slice(&ciphertext);
            Ok(out)
        })
    })
}

/// Inverse of [`encrypt`].
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::Crypto`] if the envelope is malformed or decryption
///   fails.
/// * [`FfiError::NotFound`] if `scope_id` has been forgotten.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned byte buffers across the language boundary on every call.
#[uniffi::export]
pub fn decrypt(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    ciphertext: Vec<u8>,
) -> FfiResult<Vec<u8>> {
    metrics::instrument(metrics::inc_decrypt, || {
        let scope = parse_scope_id(&scope_id)?;
        if ciphertext.len() < AEAD_NONCE_LEN {
            return Err(FfiError::Crypto {
                message: "ciphertext envelope shorter than nonce prefix".into(),
            });
        }
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope_id.clone(),
                });
            }
            let key = rt.scope_encrypt_key(scope)?;
            let mut nonce: AeadNonce = [0u8; AEAD_NONCE_LEN];
            nonce.copy_from_slice(&ciphertext[..AEAD_NONCE_LEN]);
            let body = &ciphertext[AEAD_NONCE_LEN..];
            let aad = scope_aad(scope);
            let plain = match decrypt_aead(&key, &nonce, body, &aad) {
                Ok(p) => p,
                Err(primary_err) => {
                    // The primary key failed — try the legacy HKDF key.
                    // Pre-v6 ciphertexts were encrypted under
                    // `scope:{uuid}:ffi-encrypt:v1`; after scope
                    // registration the primary key is the random DEK,
                    // so old ciphertexts need this fallback.
                    let legacy = rt.legacy_ffi_encrypt_key(scope)?;
                    if legacy == key {
                        // Same key — no point retrying.
                        return Err(FfiError::Crypto {
                            message: primary_err.to_string(),
                        });
                    }
                    decrypt_aead(&legacy, &nonce, body, &aad).map_err(|e| FfiError::Crypto {
                        message: e.to_string(),
                    })?
                }
            };
            Ok(plain)
        })
    })
}

// ─────────────────────────── Internals ────────────────────────────

/// Crate-wide UUID parser for the `ScopeId` newtype.
///
/// Originally this helper lived only in `lib.rs`; `connector.rs`
/// duplicated it with a near-identical implementation (different
/// error-message format, equivalent semantics). The two copies
/// drifted under Devin Review which flagged the duplication — they
/// were consolidated here so a future change to scope-id validation
/// touches exactly one site. `pub(crate)` visibility intentionally
/// keeps it out of the FFI surface (UniFFI/N-API hosts call the
/// public entry points, never this helper directly).
pub(crate) fn parse_scope_id(s: &str) -> FfiResult<ScopeId> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
        message: format!("scope_id: {e}"),
    })?;
    Ok(ScopeId::from_uuid(uuid))
}

fn parse_evidence_id(s: &str) -> FfiResult<EvidenceId> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
        message: format!("evidence_id: {e}"),
    })?;
    Ok(EvidenceId(uuid))
}

fn parse_uuid(s: &str) -> FfiResult<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
        message: format!("id: {e}"),
    })
}

/// Locate the scope whose [`UserMemoryObject`](memory_manager::UserMemoryObject)
/// owns a memory object with the given UUID. Returns `None` if no
/// scope currently holds that memory object.
///
/// Memory-object UUIDs are globally unique (`Uuid::new_v4()`), so at
/// most one scope can match. The split between this immutable
/// lookup and the subsequent mutable mutation in [`pin`] / [`unpin`]
/// is what lets the forgotten-scope check sit between the two
/// without violating Rust's aliasing rules.
fn locate_owning_scope(rt: &runtime::FfiRuntime, uuid: &uuid::Uuid) -> Option<ScopeId> {
    rt.user_memories
        .iter()
        .find(|(_, umo)| umo.read(uuid).is_some())
        .map(|(scope, _)| *scope)
}

/// Map an internal [`memory_manager::MemoryObject`] to the wire-flat
/// [`MemoryRecord`] surfaced through the FFI.
///
/// State mapping (internal → FFI):
///
/// * `Candidate` → `Candidate`
/// * `Reinforced` / `Consolidated` / `Canonical` → `Reinforced`
/// * `Superseded` → `Decaying`
/// * `Archived` / `Deleted` → `Archived`
/// * any object with `pin_count > 0` → `Pinned` (takes precedence
///   over the above so the host can render the pin lock icon
///   regardless of underlying state)
fn memory_object_to_record(obj: &memory_manager::MemoryObject) -> MemoryRecord {
    let state = if obj.pin_count > 0 {
        MemoryState::Pinned
    } else {
        match obj.state {
            memory_manager::MemoryState::Candidate => MemoryState::Candidate,
            memory_manager::MemoryState::Reinforced
            | memory_manager::MemoryState::Consolidated
            | memory_manager::MemoryState::Canonical => MemoryState::Reinforced,
            memory_manager::MemoryState::Superseded => MemoryState::Decaying,
            memory_manager::MemoryState::Archived | memory_manager::MemoryState::Deleted => {
                MemoryState::Archived
            }
        }
    };
    let summary = obj
        .metadata
        .get("content")
        .and_then(|v| v.as_str())
        .map_or_else(
            || {
                if obj.metadata.is_null() {
                    String::new()
                } else {
                    obj.metadata.to_string()
                }
            },
            str::to_string,
        );
    MemoryRecord {
        id: obj.id.to_string(),
        scope_id: obj.scope_id.to_string(),
        summary,
        state,
        retention_score: obj.retention_score,
        created_at: obj.created_at.timestamp(),
        last_reinforced_at: obj.last_accessed_at.timestamp(),
    }
}

/// Convert the FFI-side [`MemoryFilter`] into the internal
/// [`memory_manager::MemoryFilter`] shape. `pinned_only` is applied
/// at the call site because the internal filter has no native pin
/// predicate.
fn ffi_filter_to_memory_filter(
    filter: &MemoryFilter,
    scope: ScopeId,
) -> memory_manager::MemoryFilter {
    let mut mm = memory_manager::MemoryFilter::any().with_scope(scope);
    if let Some(state) = filter.state {
        match state {
            MemoryState::Candidate => {
                mm.states.push(memory_manager::MemoryState::Candidate);
            }
            MemoryState::Reinforced => {
                mm.states.push(memory_manager::MemoryState::Reinforced);
                mm.states.push(memory_manager::MemoryState::Consolidated);
                mm.states.push(memory_manager::MemoryState::Canonical);
            }
            MemoryState::Decaying => {
                mm.states.push(memory_manager::MemoryState::Superseded);
            }
            MemoryState::Archived => {
                mm.states.push(memory_manager::MemoryState::Archived);
                mm.states.push(memory_manager::MemoryState::Deleted);
            }
            MemoryState::Pinned => {
                // No direct internal state — the call-site filter on
                // `pin_count > 0` handles this. Leave states empty
                // so the internal filter does not over-restrict.
            }
        }
    }
    mm
}

fn ffi_importance_to_internal(ffi: FfiImportanceClass) -> ImportanceClass {
    match ffi {
        FfiImportanceClass::Critical => ImportanceClass::Critical,
        FfiImportanceClass::Important => ImportanceClass::Important,
        FfiImportanceClass::Useful => ImportanceClass::Useful,
        FfiImportanceClass::Noise => ImportanceClass::Noise,
    }
}

fn source_kind_tag(source: &SourceKind) -> &'static str {
    match source {
        SourceKind::Manual => "manual",
        SourceKind::Slack => "slack",
        SourceKind::Email => "email",
        SourceKind::MicrosoftGraph => "microsoft_graph",
        SourceKind::Atlassian => "atlassian",
        SourceKind::HubSpot => "hubspot",
        SourceKind::GoogleWorkspace => "google_workspace",
        SourceKind::Other => "other",
    }
}

fn parse_source_kind(tag: &str) -> SourceKind {
    match tag {
        "manual" => SourceKind::Manual,
        "slack" => SourceKind::Slack,
        "email" => SourceKind::Email,
        "microsoft_graph" => SourceKind::MicrosoftGraph,
        "atlassian" => SourceKind::Atlassian,
        "hubspot" => SourceKind::HubSpot,
        "google_workspace" => SourceKind::GoogleWorkspace,
        _ => SourceKind::Other,
    }
}

fn scope_aad(scope: ScopeId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(40);
    aad.extend_from_slice(b"ffi:encrypt:v1:");
    aad.extend_from_slice(scope.as_uuid().as_bytes());
    aad
}

// Helpers that need to reach into the runtime's master key + DEK
// registry. They live here rather than in `runtime.rs` so the
// `crypto`-specific knowledge (HKDF context labels, AEAD nonce
// length) stays co-located with the public functions that use them.
impl runtime::FfiRuntime {
    /// Look up the scope-specific AEAD key used by [`encrypt`] /
    /// [`decrypt`]. Tries the DEK registry first (populated during
    /// `open_store` or `ensure_scope_registered`). Falls back to the
    /// legacy HKDF derivation (`scope:{uuid}:ffi-encrypt:v1`) so
    /// that ciphertexts produced by pre-v6 `encrypt()` remain
    /// decryptable. Callers **must** check
    /// [`Self::is_scope_forgotten`] before invoking this.
    fn scope_encrypt_key(&self, scope: ScopeId) -> FfiResult<crypto::AeadKey> {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        if let Some(dek) = self.registry().get_scope_dek(registry_scope) {
            let key = dek.key().ok_or_else(|| FfiError::Crypto {
                message: "scope DEK has been destroyed".into(),
            })?;
            return Ok(*key);
        }
        // Legacy HKDF fallback: pre-v6 databases derived scope keys
        // from the master key with a per-surface label. This keeps
        // existing ciphertexts decryptable until the host explicitly
        // registers the scope (which adopts the HKDF key into the
        // registry via ensure_scope_dek's evidence check).
        let label = format!("scope:{}:ffi-encrypt:v1", scope.as_uuid());
        crypto::derive_key(self.master_key(), label.as_bytes()).map_err(|e| FfiError::Crypto {
            message: e.to_string(),
        })
    }

    /// Derive the legacy HKDF key for the `ffi-encrypt` surface. Used
    /// by [`decrypt`] as a fallback when the primary (DEK) key fails
    /// AEAD authentication — i.e. the ciphertext was produced by a
    /// pre-v6 `encrypt()`.
    fn legacy_ffi_encrypt_key(&self, scope: ScopeId) -> FfiResult<crypto::AeadKey> {
        let label = format!("scope:{}:ffi-encrypt:v1", scope.as_uuid());
        crypto::derive_key(self.master_key(), label.as_bytes()).map_err(|e| FfiError::Crypto {
            message: e.to_string(),
        })
    }

    /// Ensure the scope has an independently-generated DEK registered
    /// in the in-memory `DekRegistry` and persisted in the evidence
    /// store's `scope_deks` table.
    ///
    /// New scopes get a fresh random DEK via `OsRng`. Existing scopes
    /// (already in the registry) are a no-op.
    fn ensure_scope_registered(&mut self, scope: ScopeId) -> FfiResult<()> {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        if self.registry().get_scope_dek(registry_scope).is_some() {
            return Ok(());
        }
        // Generate an independently random DEK and persist it wrapped
        // in the evidence store's scope_deks table.
        let key = self
            .store_mut()
            .ensure_scope_dek(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        let dek = forgetting::ScopeDek::new(registry_scope, forgetting::EpochId::zero(), key);
        self.registry_mut().insert_scope_dek(dek);
        Ok(())
    }

    /// Destroy the in-memory scope DEK *and* persist the matching
    /// tombstone in the evidence store's `forgotten_scopes` /
    /// `epoch_tombstones` tables in one atomic operation.
    ///
    /// Returns [`FfiError::Evidence`] if the on-disk tombstone
    /// persistence fails. In that case the in-memory DEK is still
    /// zeroized (the destroy code path runs persistence after
    /// destruction) but the next `open_store` will not see the
    /// tombstone — callers SHOULD surface the error to their host
    /// so it can retry or alert.
    fn forget_scope(&mut self, scope: ScopeId) -> Result<(), FfiError> {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        // Split-borrow `&mut self` so we can hand both the registry
        // and the store to `destroy_scope_dek` at the same time.
        // Going through accessor methods would have to consume
        // `self` twice and the borrow checker would reject it.
        let runtime::FfiRuntime {
            registry, store, ..
        } = self;
        let mut adapter = runtime::EvidenceStoreTombstoneStore::new(store);
        forgetting::destroy_scope_dek(registry, registry_scope, Some(&mut adapter)).map_err(
            |e| FfiError::Evidence {
                message: e.to_string(),
            },
        )?;
        // Refresh the metrics tombstone gauge to match the post-
        // destroy registry size. The Phase 6 health envelope reads
        // this gauge on every `health_check` call.
        metrics::set_tombstone_count(self.registry().tombstones().count() as u64);
        Ok(())
    }

    fn is_scope_forgotten(&self, scope: ScopeId) -> bool {
        forgetting::is_scope_forgotten(self.registry(), forgetting::ScopeId(scope.as_uuid()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Helper: open a fresh temp-dir-backed store, returning the
    /// allocated [`RuntimeHandle`] and the owning `TempDir`. The
    /// `TempDir` must outlive the handle so the on-disk database
    /// is not garbage-collected while the runtime still holds it
    /// open.
    fn fresh_store() -> (RuntimeHandle, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let handle = open_store(path.to_string_lossy().into_owned(), key_hex).expect("open_store");
        (handle, dir)
    }

    fn teardown(handle: RuntimeHandle) {
        close_store(handle).expect("close_store");
    }

    #[test]
    fn open_store_rejects_invalid_hex_master_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evidence.db");
        let err = open_store(path.to_string_lossy().into_owned(), "not-hex".into()).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn open_store_rejects_wrong_length_master_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evidence.db");
        let err = open_store(path.to_string_lossy().into_owned(), "ab".repeat(16)).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn open_store_allocates_distinct_handles_for_independent_dbs() {
        // Two independent on-disk stores must produce two distinct
        // handles; calls on one must not affect the other.
        let (h1, _d1) = fresh_store();
        let (h2, _d2) = fresh_store();
        assert_ne!(h1, h2, "open_store must allocate distinct handles");

        let scope = uuid::Uuid::new_v4().to_string();
        let phrase = "storeoneisolationphrase";
        let _ = ingest_message(
            h1,
            scope.clone(),
            format!("body contains {phrase}"),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest into store 1");

        // Querying the same scope from store 2 must return zero hits
        // — store 2 has never seen this scope nor the FTS term.
        let hits = query(h2, scope, phrase.into(), 10).expect("query store 2");
        assert!(hits.is_empty(), "stores must be isolated by handle");

        teardown(h1);
        teardown(h2);
    }

    #[test]
    fn close_store_is_idempotent_for_unknown_handle() {
        // Closing an unknown handle returns Ok — hosts rely on this
        // in `try`/`finally` shutdown paths.
        close_store(RuntimeHandle::NONE).expect("close NONE");
        close_store(RuntimeHandle(u64::MAX)).expect("close unknown");
    }

    #[test]
    fn ingest_then_query_then_get_then_forget_round_trips() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // Single-token phrase (no punctuation) so the FTS5 `unicode61`
        // tokenizer indexes it verbatim and `MATCH` does not need
        // any phrase-quoting / escape gymnastics.
        let phrase = "xyzzyffiroundtripphrase";
        let body = format!("Reminder: please file the {phrase} report by Friday.");

        let evidence_id = ingest_message(
            h,
            scope.clone(),
            body.clone(),
            SourceKind::Slack,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");
        assert!(!evidence_id.is_empty());

        let hits = query(h, scope.clone(), phrase.into(), 10).expect("query");
        assert_eq!(hits.len(), 1, "FTS5 should surface the ingested phrase");
        assert_eq!(hits[0].evidence_id, evidence_id);
        assert!(hits[0].snippet.contains(phrase));

        let record = get_evidence(h, evidence_id.clone()).expect("get_evidence");
        assert_eq!(record.body, body);
        assert_eq!(record.source, SourceKind::Slack);
        assert_eq!(record.scope_id, scope);

        forget(h, evidence_id.clone()).expect("forget");

        let hits_after = query(h, scope.clone(), phrase.into(), 10).expect("query after forget");
        assert!(
            hits_after.is_empty(),
            "post-forget query must not return rows"
        );

        match get_evidence(h, evidence_id.clone()) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "evidence"),
            other => panic!("expected NotFound after forget, got {other:?}"),
        }

        match ingest_message(
            h,
            scope.clone(),
            "second message".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        ) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "scope"),
            other => panic!("expected NotFound after forget, got {other:?}"),
        }
        teardown(h);
    }

    #[test]
    fn encrypt_decrypt_round_trips_for_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let scope_id = parse_scope_id(&scope).unwrap();
        runtime::with_runtime(h, |rt| rt.ensure_scope_registered(scope_id)).expect("register");
        let plaintext = b"the quick brown fox".to_vec();
        let ct = encrypt(h, scope.clone(), plaintext.clone()).expect("encrypt");
        assert!(ct.len() > plaintext.len());
        let pt = decrypt(h, scope.clone(), ct).expect("decrypt");
        assert_eq!(pt, plaintext);
        teardown(h);
    }

    #[test]
    fn decrypt_rejects_short_envelope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let err = decrypt(h, scope, vec![0u8; 4]).unwrap_err();
        assert!(matches!(err, FfiError::Crypto { .. }));
        teardown(h);
    }

    #[test]
    fn decrypt_rejects_cross_scope_ciphertext() {
        let (h, _dir) = fresh_store();
        let scope_a = uuid::Uuid::new_v4().to_string();
        let scope_b = uuid::Uuid::new_v4().to_string();
        let scope_a_id = parse_scope_id(&scope_a).unwrap();
        let scope_b_id = parse_scope_id(&scope_b).unwrap();
        runtime::with_runtime(h, |rt| {
            rt.ensure_scope_registered(scope_a_id)?;
            rt.ensure_scope_registered(scope_b_id)
        })
        .expect("register");
        let ct = encrypt(h, scope_a, b"secret".to_vec()).expect("encrypt");
        let err = decrypt(h, scope_b, ct).unwrap_err();
        assert!(matches!(err, FfiError::Crypto { .. }));
        teardown(h);
    }

    #[test]
    fn generate_keypair_returns_ml_dsa_65() {
        let kp = generate_keypair().expect("generate_keypair");
        assert_eq!(kp.algorithm, "ml-dsa-65");
        // ML-DSA-65 verifying key is 1952 bytes (FIPS 204 §4.2).
        assert!(
            kp.public_key.len() >= 1500,
            "ml-dsa-65 verifying key suspiciously small: {}",
            kp.public_key.len()
        );
        // ml-dsa 0.1.0 represents the signing key as a 32-byte seed
        // (from which the full expanded key is derived at use time).
        assert_eq!(
            kp.private_key.len(),
            32,
            "ml-dsa-65 signing seed must be 32 bytes, got {}",
            kp.private_key.len()
        );
    }

    #[test]
    fn get_user_memory_is_empty_for_fresh_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let records = get_user_memory(h, scope).expect("get_user_memory");
        assert!(records.is_empty());
        teardown(h);
    }

    #[test]
    fn list_memories_is_empty_for_fresh_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let records =
            list_memories(h, scope.clone(), MemoryFilter::default()).expect("list_memories");
        assert!(records.is_empty());

        // Filtering by state on a fresh scope is also empty.
        let candidates = list_memories(
            h,
            scope,
            MemoryFilter {
                state: Some(MemoryState::Candidate),
                pinned_only: false,
            },
        )
        .expect("list_memories candidate filter");
        assert!(candidates.is_empty());
        teardown(h);
    }

    #[test]
    fn run_decay_sweep_is_zero_for_fresh_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let n = run_decay_sweep(h, scope).expect("run_decay_sweep");
        assert_eq!(n, 0);
        teardown(h);
    }

    #[test]
    fn get_channel_memory_is_none_until_synthesis_runs() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let cm = get_channel_memory(h, scope).expect("get_channel_memory");
        assert!(cm.is_none());
        teardown(h);
    }

    /// When the scope has no evidence at all, `trigger_synthesis`
    /// returns `NotFound { kind: "evidence" }` rather than dispatching
    /// an empty prompt to the SLM. Sending an empty prompt would
    /// waste an inference call and produce a nonsensical bundle.
    #[test]
    fn trigger_synthesis_returns_not_found_when_scope_has_no_evidence() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let err = trigger_synthesis(h, scope, SynthesisTrigger::ManualUserAction).unwrap_err();
        assert!(
            matches!(err, FfiError::NotFound { ref kind, .. } if kind == "evidence"),
            "expected NotFound {{ kind: evidence }}, got {err:?}"
        );
        teardown(h);
    }

    /// With evidence in the scope but no SLM adapter that supports
    /// `SynthSummary` (the default test build has neither MLX nor
    /// the `http-client` feature), the router cannot dispatch the
    /// task and `trigger_synthesis` surfaces `Unavailable { subsystem:
    /// synthesis: … }`.
    #[test]
    fn trigger_synthesis_returns_unavailable_when_no_synth_adapter() {
        let (h, _dir) = fresh_store();
        let scope_uuid = uuid::Uuid::new_v4();
        let scope = scope_uuid.to_string();
        ingest_message(
            h,
            scope.clone(),
            "hello world".into(),
            SourceKind::Manual,
            FfiImportanceClass::Useful,
        )
        .expect("ingest seed evidence");
        let err = trigger_synthesis(h, scope, SynthesisTrigger::ManualUserAction).unwrap_err();
        assert!(
            matches!(
                err,
                FfiError::Unavailable { ref subsystem } if subsystem.starts_with("synthesis")
            ),
            "expected Unavailable {{ subsystem: synthesis* }}, got {err:?}"
        );
        teardown(h);
    }

    #[test]
    fn pin_and_unpin_round_trip_through_user_memory() {
        let (h, _dir) = fresh_store();
        let scope_uuid = uuid::Uuid::new_v4();
        let scope_str = scope_uuid.to_string();
        // The pin / unpin surface needs an existing memory object;
        // there is no public FFI to seed one yet (observation
        // ingest through the FFI is not yet wired). Seed one
        // directly via the in-crate runtime hook so we still cover
        // the round-trip.
        let mem_id = runtime::with_runtime(h, |rt| {
            let scope = parse_scope_id(&scope_str)?;
            let umo = rt.user_memory_mut(scope);
            Ok(umo.add_observation(
                "fact",
                "Sara owns the rollout",
                memory_manager::SensitivityClass::Useful,
            ))
        })
        .expect("seed memory object");

        let records = get_user_memory(h, scope_str.clone()).expect("get_user_memory");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, mem_id.to_string());
        assert_eq!(records[0].state, MemoryState::Candidate);
        assert_eq!(records[0].summary, "Sara owns the rollout");

        pin(h, mem_id.to_string()).expect("pin");
        let pinned = get_user_memory(h, scope_str.clone()).expect("get_user_memory after pin");
        assert_eq!(pinned[0].state, MemoryState::Pinned);

        unpin(h, mem_id.to_string()).expect("unpin");
        let after_unpin = get_user_memory(h, scope_str).expect("get_user_memory after unpin");
        // pin_count back to 0 means the underlying state machine
        // controls the wire state again. The decay-state-machine
        // promotion in `pin()` lifted the object to Reinforced, so
        // the FFI mapping should now surface `Reinforced`.
        assert_eq!(after_unpin[0].state, MemoryState::Reinforced);
        teardown(h);
    }

    #[test]
    fn pin_unknown_id_reports_not_found() {
        let (h, _dir) = fresh_store();
        let bogus = uuid::Uuid::new_v4().to_string();
        let err = pin(h, bogus).unwrap_err();
        assert!(
            matches!(err, FfiError::NotFound { ref kind, .. } if kind == "memory"),
            "expected NotFound {{ kind: memory }}, got {err:?}"
        );
        teardown(h);
    }

    #[test]
    fn pin_rejects_malformed_id() {
        let (h, _dir) = fresh_store();
        let err = pin(h, "not-a-uuid".into()).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
        teardown(h);
    }

    #[test]
    fn get_user_memory_returns_empty_after_forget() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let phrase = "memorymanagerforgetphrase";
        let evidence_id = ingest_message(
            h,
            scope.clone(),
            phrase.into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest");

        // Seed a memory object into the same scope so we can prove
        // forget elides it.
        runtime::with_runtime(h, |rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let _ = umo.add_observation(
                "note",
                "tombstone candidate",
                memory_manager::SensitivityClass::Useful,
            );
            Ok(())
        })
        .expect("seed");

        assert_eq!(
            get_user_memory(h, scope.clone()).expect("pre-forget").len(),
            1
        );

        forget(h, evidence_id).expect("forget");
        assert!(get_user_memory(h, scope).expect("post-forget").is_empty());
        teardown(h);
    }

    #[test]
    fn list_memories_filters_by_state() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // Seed three candidate observations.
        runtime::with_runtime(h, |rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let _ = umo.add_observation("a", "one", memory_manager::SensitivityClass::Useful);
            let _ = umo.add_observation("b", "two", memory_manager::SensitivityClass::Useful);
            let _ = umo.add_observation("c", "three", memory_manager::SensitivityClass::Useful);
            Ok(())
        })
        .expect("seed");

        let all = list_memories(h, scope.clone(), MemoryFilter::default()).expect("list all");
        assert_eq!(all.len(), 3);

        let candidates = list_memories(
            h,
            scope.clone(),
            MemoryFilter {
                state: Some(MemoryState::Candidate),
                pinned_only: false,
            },
        )
        .expect("list candidates");
        assert_eq!(candidates.len(), 3);

        let reinforced = list_memories(
            h,
            scope,
            MemoryFilter {
                state: Some(MemoryState::Reinforced),
                pinned_only: false,
            },
        )
        .expect("list reinforced");
        assert!(reinforced.is_empty());
        teardown(h);
    }

    /// Regression: `list_memories` with `state = Some(Pinned)` must
    /// return only objects whose `pin_count > 0`, even when the
    /// caller leaves `pinned_only` at its default `false`.
    ///
    /// Previously the `Pinned` state arm of `ffi_filter_to_memory_filter`
    /// emitted an empty internal `states` vec (matching every internal
    /// state), and the call-site predicate was gated on
    /// `filter.pinned_only` instead of the `state` selector — so a
    /// caller asking for `state = Some(Pinned), pinned_only = false`
    /// got every object in the scope back, including unpinned ones.
    #[test]
    fn list_memories_state_pinned_returns_only_pinned() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // Seed two observations, pin one of them.
        let pinned_id = runtime::with_runtime(h, |rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let pinned =
                umo.add_observation("pinned", "kept", memory_manager::SensitivityClass::Useful);
            let _unpinned =
                umo.add_observation("loose", "decays", memory_manager::SensitivityClass::Useful);
            Ok(pinned)
        })
        .expect("seed");
        pin(h, pinned_id.to_string()).expect("pin");

        let only_pinned = list_memories(
            h,
            scope.clone(),
            MemoryFilter {
                state: Some(MemoryState::Pinned),
                pinned_only: false,
            },
        )
        .expect("list pinned");
        assert_eq!(
            only_pinned.len(),
            1,
            "state = Some(Pinned) must filter out unpinned rows even when pinned_only is false"
        );
        assert_eq!(only_pinned[0].id, pinned_id.to_string());
        assert_eq!(only_pinned[0].state, MemoryState::Pinned);

        teardown(h);
    }

    /// Regression: `pin` / `unpin` must reject mutations against
    /// objects whose owning scope has been cryptographically
    /// forgotten. The Gap 4 tombstone destroys the per-scope DEK,
    /// but the in-memory `UserMemoryObject` survives — a host that
    /// cached the memory id before `forget()` would otherwise be
    /// able to mutate an object that every read surface
    /// (`get_user_memory`, `list_memories`) reports as invisible.
    #[test]
    fn pin_after_forget_returns_not_found() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();

        // Seed one evidence row (so `forget` has a row to resolve to
        // a scope) and one memory object in the same scope.
        let evidence_id = ingest_message(
            h,
            scope.clone(),
            "pin-after-forget-seed-body".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest");
        let mem_id = runtime::with_runtime(h, |rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            Ok(umo.add_observation(
                "pinnable",
                "cache before forget",
                memory_manager::SensitivityClass::Useful,
            ))
        })
        .expect("seed memory");

        forget(h, evidence_id).expect("forget");

        // Pin must now return NotFound { kind: "memory" } — the same
        // shape the read surfaces present for the forgotten scope.
        let pin_err = pin(h, mem_id.to_string()).unwrap_err();
        assert!(
            matches!(pin_err, FfiError::NotFound { ref kind, .. } if kind == "memory"),
            "pin after forget must return NotFound {{ kind: memory }}, got {pin_err:?}"
        );

        // Same contract for unpin.
        let unpin_err = unpin(h, mem_id.to_string()).unwrap_err();
        assert!(
            matches!(unpin_err, FfiError::NotFound { ref kind, .. } if kind == "memory"),
            "unpin after forget must return NotFound {{ kind: memory }}, got {unpin_err:?}"
        );

        teardown(h);
    }

    /// Regression for the design follow-up: `get_user_memory` and
    /// `list_memories` must not lazily allocate a `UserMemoryObject`
    /// for scopes they observe but never mutate. A read for an
    /// unknown scope returns an empty bundle and leaves the
    /// per-scope `user_memories` map at its previous size.
    #[test]
    fn read_paths_do_not_allocate_user_memory_for_unknown_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();

        // Snapshot the map size before any read.
        let before = runtime::with_runtime(h, |rt| Ok(rt.user_memories.len())).expect("len before");

        let bundle = get_user_memory(h, scope.clone()).expect("get_user_memory");
        assert!(bundle.is_empty());
        let listed = list_memories(h, scope, MemoryFilter::default()).expect("list_memories");
        assert!(listed.is_empty());

        let after = runtime::with_runtime(h, |rt| Ok(rt.user_memories.len())).expect("len after");
        assert_eq!(
            before, after,
            "read paths must not allocate per-scope user_memory entries"
        );
        teardown(h);
    }

    /// Regression for the design rule: a synthesis call that fails
    /// before reaching the SLM must not allocate a
    /// `ChannelMemoryObject`. Allocating one would attach observable
    /// state to a call that never produces a recap.
    ///
    /// The two failure modes covered:
    /// 1. `NotFound { kind: "evidence" }` — scope has nothing to
    ///    summarise.
    /// 2. `Unavailable { subsystem: "synthesis: …" }` — router has
    ///    no adapter that supports `SynthSummary`.
    #[test]
    fn trigger_synthesis_failure_does_not_allocate_channel_memory() {
        let (h, _dir) = fresh_store();

        // Case 1: empty scope → NotFound, no allocation.
        let scope_empty = uuid::Uuid::new_v4().to_string();
        let before =
            runtime::with_runtime(h, |rt| Ok(rt.channel_memories.len())).expect("len before");
        match trigger_synthesis(h, scope_empty, SynthesisTrigger::ManualUserAction) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "evidence"),
            other => panic!("expected NotFound {{ kind: evidence }}, got {other:?}"),
        }
        let after_empty =
            runtime::with_runtime(h, |rt| Ok(rt.channel_memories.len())).expect("len after empty");
        assert_eq!(before, after_empty);

        // Case 2: scope has evidence but no SLM adapter → Unavailable,
        // no allocation.
        let scope_evidence = uuid::Uuid::new_v4().to_string();
        ingest_message(
            h,
            scope_evidence.clone(),
            "hello world".into(),
            SourceKind::Manual,
            FfiImportanceClass::Useful,
        )
        .expect("ingest seed evidence");
        let before_synth =
            runtime::with_runtime(h, |rt| Ok(rt.channel_memories.len())).expect("len before synth");
        match trigger_synthesis(h, scope_evidence, SynthesisTrigger::ManualUserAction) {
            Err(FfiError::Unavailable { subsystem }) => {
                assert!(
                    subsystem.starts_with("synthesis"),
                    "expected synthesis subsystem, got {subsystem}"
                );
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        let after_synth =
            runtime::with_runtime(h, |rt| Ok(rt.channel_memories.len())).expect("len after synth");
        assert_eq!(
            before_synth, after_synth,
            "trigger_synthesis must not allocate channel memory when returning a pre-dispatch error"
        );
        teardown(h);
    }

    #[test]
    fn calls_with_no_open_handle_report_unavailable() {
        // No prior `open_store` — every FFI function called with the
        // reserved `NONE` sentinel must surface the structured
        // `Unavailable { subsystem: "evidence_store" }` so hosts can
        // present a uniform "not initialised" UI.
        let err = ingest_message(
            RuntimeHandle::NONE,
            uuid::Uuid::new_v4().to_string(),
            "body".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .unwrap_err();
        assert!(
            matches!(err, FfiError::Unavailable { ref subsystem } if subsystem == "evidence_store")
        );
    }

    /// Durable cryptographic-forgetting tombstones
    /// must survive a `close_store` / `open_store` cycle. We ingest
    /// into a scope, forget it, close the store, re-open the same DB
    /// with the same master key, and assert that the scope still
    /// short-circuits with `NotFound { kind: "scope" }`.
    #[test]
    fn forget_survives_close_and_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let scope = uuid::Uuid::new_v4().to_string();

        let h1 =
            open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open_store");

        let evidence_id = ingest_message(
            h1,
            scope.clone(),
            "the persistent forgetting test body".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");
        forget(h1, evidence_id).expect("forget");

        // Round-trip the runtime. The in-memory `DekRegistry` is
        // dropped here; the next `open_store` must rebuild it from
        // the persisted `forgotten_scopes` table.
        close_store(h1).expect("close_store");
        let h2 = open_store(path.to_string_lossy().into_owned(), key_hex).expect("re-open_store");

        // The scope must still be rejected. We probe via
        // `ingest_message` because that's the canonical
        // `is_scope_forgotten` short-circuit path that hosts hit
        // first after a restart.
        match ingest_message(
            h2,
            scope,
            "second message after restart".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        ) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "scope"),
            other => panic!("expected NotFound {{ kind: \"scope\" }} after restart, got {other:?}"),
        }
        teardown(h2);
    }

    /// The FFI `forget()` path persists
    /// the tombstone *before* purging the FTS5 / embedding indexes.
    /// If the process crashes between those two steps the tombstone
    /// survives but the plaintext FTS terms persist on disk. Re-opening
    /// the store must re-run the per-scope FTS purge so the
    /// cryptographic-forgetting contract holds across crashes.
    ///
    /// We simulate the crash by writing the tombstone directly via the
    /// store handle (the public `forget()` would also purge FTS) and
    /// then closing / reopening through the FFI.
    #[test]
    fn open_store_repurges_fts_for_persisted_tombstones() {
        const PHRASE: &str = "openstoreftsrepurgeregressionphrase";

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let scope_str = uuid::Uuid::new_v4().to_string();

        let h1 =
            open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open_store");

        let evidence_id = ingest_message(
            h1,
            scope_str.clone(),
            PHRASE.into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");
        assert!(!evidence_id.is_empty());

        // Sanity: FTS5 surfaces the phrase before any forgetting.
        let hits = query(h1, scope_str.clone(), PHRASE.into(), 10).expect("query pre-forget");
        assert_eq!(hits.len(), 1, "FTS5 must surface the seeded phrase");

        // Simulate the crash window: persist the tombstone *without*
        // running `purge_fts_for_scope`. The public `forget()` would
        // do both — we reach into the store directly to model a crash
        // between steps 2 and 3.
        runtime::with_runtime(h1, |rt| {
            let scope = parse_scope_id(&scope_str)?;
            rt.store_mut()
                .record_forgotten_scope(scope)
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            Ok(())
        })
        .expect("seed tombstone without FTS purge");

        // Verify the crash state: the FTS index still contains the
        // phrase even though the tombstone is now on disk. This is
        // the security gap the re-purge closes.
        runtime::with_runtime(h1, |rt| {
            let scope = parse_scope_id(&scope_str)?;
            let raw_term_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1 AND scope_id = ?2",
                    rusqlite::params![PHRASE, scope.as_uuid().as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            assert_eq!(
                raw_term_count, 1,
                "pre-condition: FTS row must survive a tombstone-only forget so the test exercises the re-purge"
            );
            Ok(())
        })
        .expect("probe pre-reopen fts");

        // Restart cycle. The next `open_store` is where the re-purge
        // runs.
        close_store(h1).expect("close_store");
        let h2 = open_store(path.to_string_lossy().into_owned(), key_hex).expect("re-open_store");

        // After the re-purge, the raw FTS5 shadow tables must
        // contain no rows for the forgotten scope.
        runtime::with_runtime(h2, |rt| {
            let scope = parse_scope_id(&scope_str)?;
            let raw_term_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1 AND scope_id = ?2",
                    rusqlite::params![PHRASE, scope.as_uuid().as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            assert_eq!(
                raw_term_count, 0,
                "open_store must re-purge FTS rows for every persisted tombstone"
            );
            Ok(())
        })
        .expect("probe post-reopen fts");

        // Public query surface mirrors the raw probe — the canonical
        // host-visible signal that the cryptographic-forgetting
        // contract is now intact across crashes.
        let hits_after =
            query(h2, scope_str.clone(), PHRASE.into(), 10).expect("query post-reopen");
        assert!(
            hits_after.is_empty(),
            "post-reopen query must return no rows for the previously-tombstoned scope"
        );

        teardown(h2);
    }

    /// C10 integration test: memory state survives an open/close/open
    /// cycle via the encrypted `memory_objects` table.
    #[test]
    fn memory_persists_across_open_close_open() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);

        // First session: open, add a memory object, pin it, close.
        let h1 = open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open 1");
        let scope_uuid = uuid::Uuid::new_v4();
        let scope_str = scope_uuid.to_string();
        let scope = parse_scope_id(&scope_str).unwrap();

        // Ensure scope is registered so the DEK exists.
        runtime::with_runtime(h1, |rt| {
            rt.ensure_scope_registered(scope)?;
            Ok(())
        })
        .expect("ensure_scope_registered");

        // Insert a memory object and pin it.
        runtime::with_runtime(h1, |rt| {
            let umo = rt.user_memory_mut(scope);
            let obj = memory_manager::MemoryObject::new_candidate(
                scope,
                memory_manager::SensitivityClass::Important,
            );
            let obj_id = obj.id;
            umo.insert(obj);
            umo.pin(&obj_id).map_err(|e| FfiError::Memory {
                message: e.to_string(),
            })?;
            // Flush to disk.
            rt.flush_user_memory(scope)?;
            Ok(())
        })
        .expect("insert + pin");

        // Verify we can see the memory object before closing.
        let before_close = list_memories(h1, scope_str.clone(), MemoryFilter::default())
            .expect("list before close");
        assert_eq!(before_close.len(), 1, "one memory object before close");
        // Check pin count via the internal MemoryObject (not exposed
        // on the FFI MemoryRecord wire type).
        runtime::with_runtime(h1, |rt| {
            let umo = rt.user_memory(scope).expect("scope must exist");
            assert_eq!(umo.objects[0].pin_count, 1, "pinned once before close");
            Ok(())
        })
        .expect("pin count check");

        close_store(h1).expect("close 1");

        // Second session: re-open with same key.
        let h2 = open_store(path.to_string_lossy().into_owned(), key_hex).expect("open 2");

        // Memory object must be rehydrated from disk.
        let after_reopen = list_memories(h2, scope_str.clone(), MemoryFilter::default())
            .expect("list after reopen");
        assert_eq!(
            after_reopen.len(),
            1,
            "memory object must survive close/open cycle"
        );
        runtime::with_runtime(h2, |rt| {
            let umo = rt
                .user_memory(scope)
                .expect("scope must exist after reopen");
            assert_eq!(
                umo.objects[0].pin_count, 1,
                "pin count must survive close/open cycle"
            );
            Ok(())
        })
        .expect("pin count check after reopen");

        teardown(h2);
    }

    /// C10 integration test: forget_scope deletes persisted memory
    /// blobs so they do not reappear on reopen.
    #[test]
    fn forget_scope_deletes_persisted_memory() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);

        let h1 = open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open 1");
        let scope_uuid = uuid::Uuid::new_v4();
        let scope_str = scope_uuid.to_string();
        let scope = parse_scope_id(&scope_str).unwrap();

        runtime::with_runtime(h1, |rt| {
            rt.ensure_scope_registered(scope)?;
            Ok(())
        })
        .expect("ensure_scope_registered");

        // Insert a memory object and flush it.
        runtime::with_runtime(h1, |rt| {
            let umo = rt.user_memory_mut(scope);
            let obj = memory_manager::MemoryObject::new_candidate(
                scope,
                memory_manager::SensitivityClass::Useful,
            );
            umo.insert(obj);
            rt.flush_user_memory(scope)?;
            Ok(())
        })
        .expect("insert + flush");

        // Forget the scope.
        forget_scope(h1, scope_str.clone()).expect("forget_scope");

        close_store(h1).expect("close 1");

        // Reopen — memories for the forgotten scope must NOT reappear.
        let h2 = open_store(path.to_string_lossy().into_owned(), key_hex).expect("open 2");

        let after = list_memories(h2, scope_str.clone(), MemoryFilter::default())
            .expect("list after forget + reopen");
        assert!(
            after.is_empty(),
            "forgotten-scope memories must not reappear after reopen"
        );

        teardown(h2);
    }
}
