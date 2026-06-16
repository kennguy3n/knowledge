//! `knowledge_ffi` — UniFFI surface for iOS / Android platform bindings.
//!
//! Per `docs/technical/architecture.md` §3 ("Platform integration plane") and
//! `docs/technical/design.md` §2 ("On-device runtime"), the knowledge substrate
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
//! # Known limitations
//!
//! These are deliberate constraints of the current FFI surface:
//! * **Ingest hardcodes `ImportanceClass::Important`.** The
//!   evidence store supports `Important` / `Useful` / `Noise` (with
//!   different storage routing, including the noise ring buffer);
//!   the FFI surface does not yet expose that knob.
//! * **`query` first forwards `query_text` verbatim to SQLite FTS5.**
//!   FTS5 has its own query grammar (`AND` / `OR` / `NOT` / `NEAR` /
//!   column filters). If the verbatim parse fails, ordinary
//!   search-box text (e.g. business identifiers like `BR-2505` whose
//!   punctuation trips the parser) is rescued by a literal-token
//!   fallback so it still returns results; a query that uses explicit
//!   FTS5 expression syntax but is genuinely malformed (unclosed
//!   phrase quote, grouping/`NEAR(` paren, bare boolean/`NEAR`
//!   keyword) instead surfaces as [`FfiError::InvalidQuery`].
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

// STABLE
pub mod connector;
// STABLE
pub mod error;
// STABLE
pub mod health;
// STABLE
pub mod key_storage;
// UNSTABLE — internal metrics; signatures may change.
#[doc(hidden)]
pub mod metrics;
// STABLE
pub mod reasoning;
// STABLE
pub mod runtime;
// STABLE
pub mod sync_scheduler;
// STABLE
pub mod synthesis;
pub(crate) mod synthesis_rate;
// STABLE
#[cfg(feature = "tracing-subscriber")]
pub mod tracing_init;
// STABLE
pub mod types;
// STABLE
pub mod webhook;

// STABLE
pub use connector::{
    authenticate_connector, clear_oauth_client_secret_resolver, connector_status, create_connector,
    list_connectors, refresh_connector_token, remove_connector, set_oauth_client_secret_resolver,
    sync_connector, OAuthClientSecretResolver,
};
// STABLE
pub use error::{FfiError, FfiResult};
// STABLE
pub use health::{
    health_check, AdapterReport, HealthStatus, SlmLatencyReport, SubsystemHealth, SubsystemStatus,
};
// STABLE
pub use key_storage::{clear_key_storage_resolver, set_key_storage_resolver, KeyStorageResolver};
// UNSTABLE — internal metrics; signatures may change.
#[doc(hidden)]
pub use metrics::{
    open_store_duration_histogram, slm_dispatch_histograms, snapshot as metrics_snapshot,
    ErrorCounters, HistogramView, MetricsSnapshot, SlmDispatchHistogram,
};
// STABLE
pub use runtime::{close_store, open_store, open_store_with_resolver, RuntimeHandle};
// STABLE
pub use sync_scheduler::{
    clear_sync_schedule, configure_sync_auto_synthesize, configure_sync_schedule,
    start_sync_scheduler, start_sync_scheduler_for_platform, stop_sync_scheduler,
    sync_scheduler_status, DEFAULT_SYNC_INTERVAL_SECS, DEFAULT_SYNC_MAX_BACKOFF_SECS,
    DEFAULT_SYNC_TICK_SECS, MOBILE_SYNC_INTERVAL_SECS, MOBILE_SYNC_TICK_SECS,
};
// STABLE
pub use synthesis::{
    admit_approved_document, configure_synthesis_engine, list_approved_documents,
    list_recent_syntheses, list_synthesis_versions, replace_approved_document, replay_synthesis,
    revoke_approved_document, synthesis_status, trigger_server_synthesis,
    LIST_RECENT_SYNTHESES_CAP, MAX_APPROVED_DOCUMENTS_PER_DISPATCH, MAX_APPROVED_DOCUMENT_BYTES,
    MAX_APPROVED_DOCUMENT_METADATA_BYTES, MAX_SYNTHESIS_OUTPUT_BYTES,
    MAX_SYNTHESIS_VERSIONS_PER_WINDOW, PER_SCOPE_COOLDOWN_SECS, WINDOW_RETENTION_CAP_PER_SCOPE,
};
// STABLE
#[cfg(feature = "tracing-subscriber")]
pub use tracing_init::try_init_tracing;
// STABLE
pub use types::{
    ApprovedDocumentSummary, ConnectorHealthRecord, ConnectorKindTag, ConnectorStatus,
    EvidenceRecord, FfiImportanceClass, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord,
    MemoryState, PlatformHint, QueryResult, RefreshReport, ScopeIdString, SourceKind, SyncModeKind,
    SyncReport, SyncSchedulerStatus, SyncStatusKind, SynthesisEngineConfig, SynthesisStatusRecord,
    SynthesisTierKind, SynthesisTrigger, SynthesisVersionSummary, WebhookServerHandle,
    WebhookServerSummary,
};
// STABLE
pub use webhook::{
    list_webhook_servers, register_webhook_dispatch, start_webhook_server, stop_webhook_server,
    unregister_webhook_dispatch,
};
// STABLE — the wire-flat concept-graph view returned by
// [`get_concept_graph`]; re-exported so the substrate server can name
// the type without depending on `concept_graph` directly.
pub use concept_graph::GraphView;
// STABLE — reasoning-plane queries (contradictions / drift / query-plan
// rationale) and their wire DTOs, re-exported so the substrate server
// can name the types without depending on `reasoning_engine` directly.
pub use reasoning::{
    reasoning_contradictions, reasoning_drift, reasoning_explain_query, ContradictionView,
    DriftView, ExplainStepView, QueryExplanationView,
};

use concept_graph::{
    project_memory_graph, subgraph_for_scope, AllowAllScopes, ConceptGraph, MemoryProjection,
    ViewFilter, DEFAULT_MAX_NODES,
};
use crypto::{
    decrypt_aead, encrypt_aead, forgetting, signer_backend::MlDsa65Signer, AeadNonce,
    AEAD_NONCE_LEN,
};
use evidence_store::{EvidenceError, EvidenceId, ImportanceClass, ScopeId};
// `TryRng` is the fallible RNG trait in rand 0.10 (which renamed
// `TryRngCore` to `TryRng` and `OsRng` to `SysRng`). See SECURITY.md
// §"Random number generation" for the rationale behind the
// workspace-wide OS-RNG-for-everything policy.
use rand::TryRng;

use runtime::with_runtime;

/// Run `apply` while holding this store handle's runtime lock, then
/// re-open the connection so the spliced changes become visible.
///
/// Every other FFI entry point ([`ingest_message`], [`query`], …)
/// serialises on the same per-handle mutex via `with_runtime`, so a
/// closure invoked here cannot overlap any in-flight SQLite read or
/// write on the same connection. The standby replicator uses this to
/// splice raw WAL page images into the database file *underneath* an
/// open SQLCipher connection without racing a reader.
///
/// SQLite normally notices an external write to the file via the
/// page-1 change counter and drops its page cache on the next read
/// transaction. That mechanism is **unavailable** here: the primary
/// runs in `journal_mode=WAL` (so it produces the `-wal` the shipper
/// reads), and WAL mode freezes the legacy page-1 change counter for
/// the life of the WAL — the shipped frames usually do not even
/// include page 1. The standby's long-lived read connection would
/// therefore keep serving its stale cached pages forever. To close
/// that gap, this helper re-opens the connection after `apply`
/// returns ([`EvidenceStore::reopen_connection`]), forcing the next
/// read to fault every page back in from the freshly spliced file.
/// The re-open uses a raw key blob (no PBKDF2), so it is cheap enough
/// to run after each applied segment.
///
/// `apply` must not call back into any FFI function on the same
/// `handle` — it already holds the lock, so re-entry would deadlock.
/// It should only touch the database file directly.
///
/// # Errors
///
/// [`FfiError::Unavailable`] if [`open_store`] has not been called for
/// `handle`, or [`FfiError::Evidence`] if re-opening the connection
/// fails after the splice. The closure's own return value is otherwise
/// passed through verbatim.
pub fn with_store_file_locked<R>(handle: RuntimeHandle, apply: impl FnOnce() -> R) -> FfiResult<R> {
    with_runtime(handle, |rt| {
        let out = apply();
        rt.store_mut()
            .reopen_connection()
            .map_err(|e| FfiError::Evidence {
                message: format!("re-opening store connection after WAL splice: {e}"),
            })?;
        Ok(out)
    })
}

/// Report the SQLite journal mode of this store handle's open
/// connection (e.g. `"delete"`, `"truncate"`, `"wal"`), lower-cased.
///
/// The standby replicator uses this to assert at startup that the
/// read-serving connection is in a *rollback-journal* mode. Its raw WAL
/// page splicing (see [`with_store_file_locked`]) writes page images
/// straight into the main database file; a standby connection in WAL
/// mode would instead consult its own `-wal` sidecar — which
/// replication never writes — and serve stale pages until a checkpoint,
/// even after the post-splice connection re-open. Surfacing the mode
/// lets the caller fail fast if the standby's open path ever switched
/// to `journal_mode=WAL`.
///
/// # Errors
///
/// [`FfiError::Unavailable`] if [`open_store`] has not been called for
/// `handle`, or [`FfiError::Evidence`] if the pragma query fails.
pub fn store_journal_mode(handle: RuntimeHandle) -> FfiResult<String> {
    with_runtime(handle, |rt| {
        let mode: String = rt
            .store
            .raw_conn()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(|e| FfiError::Evidence {
                message: format!("reading journal_mode: {e}"),
            })?;
        Ok(mode.to_ascii_lowercase())
    })
}

/// Report the SQLCipher page size (`PRAGMA cipher_page_size`) of this
/// store handle's open connection.
///
/// The standby replicator splices raw WAL page images straight into the
/// database file at `(page_number - 1) * page_size`, so it is bound to
/// the store's on-disk page geometry. Surfacing the cipher page size
/// lets the standby assert that each shipped segment's `page_size`
/// matches the local store before writing, so a future change to
/// `cipher_page_size` aborts with a clear error instead of silently
/// writing misaligned pages.
///
/// # Errors
///
/// [`FfiError::Unavailable`] if [`open_store`] has not been called for
/// `handle`, or [`FfiError::Evidence`] if the pragma query fails.
pub fn store_cipher_page_size(handle: RuntimeHandle) -> FfiResult<u32> {
    with_runtime(handle, |rt| {
        // SQLCipher returns `cipher_page_size` as TEXT (unlike SQLite's
        // own integer pragmas), so read it as a string and parse it.
        let raw: String = rt
            .store
            .raw_conn()
            .pragma_query_value(None, "cipher_page_size", |row| row.get(0))
            .map_err(|e| FfiError::Evidence {
                message: format!("reading cipher_page_size: {e}"),
            })?;
        raw.trim().parse::<u32>().map_err(|_| FfiError::Evidence {
            message: format!("implausible cipher_page_size {raw:?}"),
        })
    })
}

/// Switch this store handle's connection into `journal_mode=WAL` and
/// disable SQLite's automatic checkpointing.
///
/// A node acting as **primary** must run in WAL mode so SQLite produces
/// the `-wal` sidecar the replication shipper reads frames from — in the
/// default rollback-journal mode no `-wal` is ever created and the
/// shipper would publish nothing. Auto-checkpointing is turned off
/// (`wal_autocheckpoint=0`) because an automatic checkpoint folds
/// committed frames back into the main database and truncates the
/// `-wal` out from under the shipper, which would silently drop frames
/// that were never shipped. The shipper itself drives checkpoints (via
/// [`drain_wal`]) so it only ever truncates frames it has already read.
///
/// Returns the resulting journal mode, lower-cased (expected: `"wal"`).
///
/// # Errors
///
/// [`FfiError::Unavailable`] if [`open_store`] has not been called for
/// `handle`, or [`FfiError::Evidence`] if a pragma fails.
pub fn store_set_journal_wal(handle: RuntimeHandle) -> FfiResult<String> {
    with_runtime(handle, |rt| {
        let conn = rt.store.raw_conn();
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| FfiError::Evidence {
                message: format!("setting journal_mode=WAL: {e}"),
            })?;
        conn.pragma_update(None, "wal_autocheckpoint", 0_i64)
            .map_err(|e| FfiError::Evidence {
                message: format!("disabling wal_autocheckpoint: {e}"),
            })?;
        Ok(mode.to_ascii_lowercase())
    })
}

/// Checkpoint and switch this store handle's connection back to a
/// rollback-journal mode (`journal_mode=DELETE`).
///
/// Used when an auto-mode node is **demoted** from primary to standby:
/// any frames still in the `-wal` are folded into the main database and
/// the `-wal`/`-shm` sidecars are removed (`wal_checkpoint(TRUNCATE)`),
/// then the connection leaves WAL mode so the standby's raw page
/// splicing + page-1 change-counter cache invalidation works again (it
/// only holds outside WAL mode — see [`with_store_file_locked`]).
///
/// Returns the resulting journal mode, lower-cased.
///
/// # Errors
///
/// [`FfiError::Unavailable`] if [`open_store`] has not been called for
/// `handle`, or [`FfiError::Evidence`] if a pragma fails.
pub fn store_set_journal_rollback(handle: RuntimeHandle) -> FfiResult<String> {
    with_runtime(handle, |rt| {
        let conn = rt.store.raw_conn();
        // Fold any outstanding WAL frames into the main file and drop
        // the sidecars before leaving WAL mode. Ignore the returned
        // (busy, log, checkpointed) row — on a primary about to stop
        // there are no competing readers, and any residual frames are
        // re-derived from the bus by the standby regardless.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| FfiError::Evidence {
                message: format!("checkpointing WAL: {e}"),
            })?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
            .map_err(|e| FfiError::Evidence {
                message: format!("setting journal_mode=DELETE: {e}"),
            })?;
        Ok(mode.to_ascii_lowercase())
    })
}

/// Atomically read the `-wal` sidecar at `wal_path` and checkpoint it.
///
/// Runs under the store handle's runtime lock so no FFI write (every
/// `ingest_*` serialises on the same mutex) can append frames between
/// the read and the checkpoint. The whole `-wal` is read into memory,
/// then `wal_checkpoint(TRUNCATE)` folds those exact frames into the
/// main database and resets the sidecar — so every committed frame is
/// captured for shipping before it is truncated, and the `-wal` cannot
/// grow without bound. The returned bytes are the WAL generation the
/// caller must ship; the next write begins a fresh generation (new
/// salts) which the shipper re-ships from frame zero.
///
/// A missing `-wal` (no writes yet, or already drained) yields an empty
/// vector and performs no checkpoint.
///
/// # Errors
///
/// [`FfiError::Unavailable`] if [`open_store`] has not been called for
/// `handle`, or [`FfiError::Evidence`] if the read or checkpoint fails.
pub fn drain_wal(handle: RuntimeHandle, wal_path: &str) -> FfiResult<Vec<u8>> {
    with_runtime(handle, |rt| {
        let bytes = match std::fs::read(wal_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(FfiError::Evidence {
                    message: format!("reading WAL sidecar {wal_path}: {e}"),
                });
            }
        };
        if !bytes.is_empty() {
            rt.store
                .raw_conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .map_err(|e| FfiError::Evidence {
                    message: format!("checkpointing WAL after drain: {e}"),
                })?;
        }
        Ok(bytes)
    })
}

/// Fold this store handle's live SQLCipher database into a standalone,
/// fully self-contained backup copy at `dest_path`.
///
/// This is the host-facing entry point for
/// [`EvidenceStore::snapshot_to`] — the building block a single-file-DB
/// consumer (a mobile / Electron app holding the stores open) uses to
/// fold the otherwise-live database into a backup/restore cycle without
/// closing it.
///
/// The copy is produced with `VACUUM INTO` in a single implicit
/// transaction against the live connection (serialised behind the
/// per-handle runtime mutex, so no concurrent `ingest_*` write can tear
/// it), so the result is internally consistent even while the store
/// keeps serving reads and writes. It keeps the *same* SQLCipher page
/// key, so the copy re-opens with the identical `master_key` this store
/// was opened with — it is a backup, not a rekey — and is standalone
/// (no `-journal` / `-wal` sidecar to copy alongside it).
///
/// `dest_path` MUST NOT already exist: SQLite refuses to vacuum into a
/// present, non-empty file. Hosts should write to a fresh temp path and
/// atomically move it into place once this returns.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called for
///   `handle`.
/// * [`FfiError::Evidence`] if `dest_path` already exists, is not valid
///   UTF-8, or the underlying `VACUUM INTO` fails.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the boundary.
#[uniffi::export]
pub fn snapshot_store_to(handle: RuntimeHandle, dest_path: String) -> FfiResult<()> {
    metrics::instrument(metrics::inc_snapshot_store_to, || {
        with_runtime(handle, |rt| {
            rt.store()
                .snapshot_to(std::path::Path::new(&dest_path))
                .map_err(|e| FfiError::Evidence {
                    message: format!("snapshot store to {dest_path}: {e}"),
                })
        })
    })
}

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
            // Run language detection at the production
            // write boundary so every persistent row stamps a
            // BCP-47 primary subtag onto the `language_tag` column
            // (schema v13). `detect_language` is fail-closed: empty
            // / pure-punctuation / pure-emoji / unreliable-short
            // input returns `None` and the column stays NULL,
            // which is the correct "language unknown" state for
            // downstream consumers (the lexicon registry
            // reads this column on every retrieval). Detection
            // runs unconditionally — including on the noise path
            // that gets routed to the ring buffer by
            // `ingest_with_language` — because the ~microsecond
            // trigram analysis is cheaper than threading a
            // sensitivity-class lookahead through here, and noise
            // rows do not retain the tag anyway (the ring buffer
            // is plaintext-only, append-and-evict).
            let detection = observation_engine::detect_language(&body);
            let language_tag = detection.as_ref().map(|d| d.tag.as_str());
            let result = rt
                .store_mut()
                .ingest_with_language(
                    scope,
                    body.as_bytes(),
                    Some(source_kind_tag(&source)),
                    ffi_importance_to_internal(importance),
                    language_tag,
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            Ok(result.evidence_id.to_string())
        })
    })
}

/// Map an [`EvidenceError`] raised by a search into the FFI error
/// contract, preserving the client-vs-server distinction the store
/// makes: a malformed FTS5 query expression
/// ([`EvidenceError::InvalidQuery`]) becomes [`FfiError::InvalidQuery`]
/// (`400`), while every other failure remains [`FfiError::Evidence`]
/// (`500`). Keeping this in one helper means the read path
/// ([`query`]) and any future search surface map identically.
fn map_query_error(e: EvidenceError) -> FfiError {
    match e {
        EvidenceError::InvalidQuery(message) => FfiError::InvalidQuery { message },
        other => FfiError::Evidence {
            message: other.to_string(),
        },
    }
}

/// Rewrite free-text `query_text` into an FTS5 expression that cannot
/// trip the MATCH parser, used as a fallback when the verbatim query is
/// rejected as malformed (see [`query`]).
///
/// Each whitespace-separated run becomes a quoted FTS5 *string* (a
/// literal phrase), with embedded `"` doubled per the FTS5 escape
/// convention. The quoted runs are space-joined, which FTS5 reads as an
/// implicit `AND`. Quoting neutralises every operator character a user
/// might type inside a token — `-` (column-filter / `no such column`
/// errors), `,`, `:`, `*`, stray `"`, bare `AND`/`OR`/`NEAR` — so a
/// business identifier like `BR-2505` or `FA-2025-0411` matches the
/// adjacent indexed tokens instead of erroring.
///
/// Returns `None` when `query_text` has no tokens (whitespace only), in
/// which case there is nothing to retry.
fn fts_literal_token_fallback(query_text: &str) -> Option<String> {
    let mut expr = String::with_capacity(query_text.len() + 2);
    for token in query_text.split_whitespace() {
        if !expr.is_empty() {
            expr.push(' ');
        }
        expr.push('"');
        expr.push_str(&token.replace('"', "\"\""));
        expr.push('"');
    }
    if expr.is_empty() {
        None
    } else {
        Some(expr)
    }
}

/// Run a hybrid (FTS) query against a scope.
///
/// Returns up to `limit` rows ordered by FTS5 rank.
///
/// # Query syntax
///
/// `query_text` is first tried verbatim against SQLite FTS5's `MATCH`
/// operator (via a parameterised query, so SQL injection is not a
/// concern). This preserves the full FTS5 grammar — `AND` / `OR` /
/// `NOT` / `NEAR` / column filters / phrase quoting / prefix matching —
/// for callers that pass a deliberate expression.
///
/// If FTS5 rejects the verbatim text as malformed, the query is **not**
/// failed: it is retried once as a sanitised, literal-token expression
/// (see [`fts_literal_token_fallback`]) so a search box returns results
/// instead of a parser error. This is what users expect when they type
/// into a search box — a hyphenated identifier like `BR-2505` or
/// `FA-2025-0411` (whose `-` reads as a column filter), a stray or
/// unbalanced `"`, a dangling `revenue AND`, an incomplete `NEAR(`, or
/// an unmatched `(` all search for the literal words rather than
/// surfacing an FTS5 syntax error. The fallback quotes each
/// whitespace-separated token, keeping implicit-`AND` semantics across
/// tokens, so it narrows rather than broadens the result set.
///
/// Well-formed FTS5 expressions still run verbatim, so deliberate
/// `AND` / `OR` / `NOT` / `NEAR` / phrase / prefix queries are
/// unaffected — only a *malformed* expression is rewritten. The single
/// remaining rejection is input with no searchable tokens at all
/// (whitespace-only), which surfaces as [`FfiError::InvalidQuery`]
/// because there is nothing to search for.
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
/// * [`FfiError::InvalidQuery`] if `query_text` has no searchable
///   tokens (whitespace-only) — a client error (`400` at the HTTP
///   edge). Malformed but token-bearing input is rescued, not rejected.
/// * [`FfiError::Evidence`] if the underlying search hits a genuine
///   storage fault (I/O, corruption, …).
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
            let hits = match rt.store().search_fts(scope, &query_text, limit as usize) {
                Ok(hits) => hits,
                // FTS5 rejected the verbatim text — a hyphenated
                // identifier read as a column filter, an unbalanced
                // phrase quote, a dangling boolean operator, an
                // incomplete `NEAR(`, an unmatched `(`, … Retry once as
                // a sanitised literal-token expression so a search box
                // returns results instead of bouncing a 400 at someone
                // who simply typed punctuation or an unclosed quote.
                // Only whitespace-only input has no tokens to retry, so
                // its original error stands.
                Err(EvidenceError::InvalidQuery(orig)) => {
                    match fts_literal_token_fallback(&query_text) {
                        Some(sanitised) => {
                            // Record that the recovery path fired so operators
                            // can see how load-bearing it is (see
                            // `query_fts_fallback_total`).
                            metrics::inc_query_fts_fallback();
                            rt.store()
                                .search_fts(scope, &sanitised, limit as usize)
                                .map_err(map_query_error)?
                        }
                        None => return Err(FfiError::InvalidQuery { message: orig }),
                    }
                }
                Err(other) => return Err(map_query_error(other)),
            };
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
                // Forward the BCP-47 primary subtag the substrate
                // stamped on the row at ingest (schema v13). NULL
                // stays NULL across the bridge so host
                // shells can distinguish "no detection" from a
                // concrete tag like `"en"`. `row` is owned and not
                // borrowed after this expression, and `source_ref`'s
                // borrow above has already been consumed by
                // `map_or`, so a partial move of `row.language_tag`
                // (rather than a clone of the inner `String`) is
                // sound and saves the allocation.
                language_tag: row.language_tag,
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

/// Create a new user-memory observation in `scope_id` and return the
/// resulting [`MemoryRecord`].
///
/// This is the write counterpart to [`get_user_memory`] /
/// [`list_memories`]. It appends a fresh `Candidate` memory object to
/// the per-scope [`UserMemoryObject`](memory_manager::UserMemoryObject)
/// via [`UserMemoryObject::add_observation`](memory_manager::UserMemoryObject::add_observation),
/// then persists the bundle to the encrypted evidence plane (the same
/// `flush_user_memory` path [`pin`] / [`unpin`] / [`run_decay_sweep`]
/// use) so the row survives an `open_store` / `close_store` cycle. The
/// newly-created record is returned so the caller can render it
/// without a follow-up [`list_memories`] round-trip.
///
/// `observation_type` is a free-form tag (e.g. `"preference"`,
/// `"task"`, `"fact"`) recorded in the object metadata; `content` is
/// the human-readable memory text; `sensitivity` drives the decay
/// schedule (it mirrors the storage-tier importance classes —
/// `Critical` never passively decays, `Noise` is never promoted).
///
/// Only the **user** memory tier is writable through this surface.
/// The channel / domain / tenant tiers are owned by the synthesis
/// pipeline and have no caller-facing write path — keeping this entry
/// point user-only is the FFI half of the gateway's fail-closed tier
/// authorisation.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID, or if
///   `observation_type` / `content` is blank after trimming
///   (fail-closed on empty input).
/// * [`FfiError::NotFound`] if the scope has been cryptographically
///   forgotten — a destroyed-DEK scope must never accept a new write
///   that could never be read back.
/// * [`FfiError::Memory`] / [`FfiError::Evidence`] if persisting the
///   bundle to the encrypted plane fails.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings/structs across the language boundary on every call.
#[uniffi::export]
pub fn add_user_memory(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    observation_type: String,
    content: String,
    sensitivity: FfiImportanceClass,
) -> FfiResult<MemoryRecord> {
    metrics::instrument(metrics::inc_add_user_memory, || {
        let scope = parse_scope_id(&scope_id)?;
        if observation_type.trim().is_empty() {
            return Err(FfiError::InvalidId {
                message: "observation_type must not be empty".into(),
            });
        }
        if content.trim().is_empty() {
            return Err(FfiError::InvalidId {
                message: "content must not be empty".into(),
            });
        }
        let sensitivity = ffi_importance_to_sensitivity(sensitivity);
        with_runtime(handle, |rt| {
            // A cryptographically-forgotten scope must never accept a
            // new write: its DEK is destroyed, so the row could never
            // be read back, and silently dropping it would leave the
            // caller believing the write succeeded.
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope_id.clone(),
                });
            }
            // Durably persist the scope DEK before writing, exactly as
            // `ingest_message` does. `flush_user_memory` encrypts the
            // bundle under `scope_key`, which falls back to an
            // in-memory-only HKDF key for an unregistered scope. Without
            // registering, a brand-new scope's blob is sealed under that
            // ephemeral key; after a restart `ensure_scope_dek` finds no
            // persisted DEK and no `evidence` row (it does not inspect
            // `memory_objects`), mints a fresh random DEK, and the blob
            // becomes permanently unreadable. Registering writes the DEK
            // to `scope_deks` so the key survives the round-trip.
            rt.ensure_scope_registered(scope)?;
            let umo = rt.user_memory_mut(scope);
            let id = umo.add_observation(observation_type, content, sensitivity);
            let record = umo
                .read(&id)
                .map(memory_object_to_record)
                .expect("object just inserted by add_observation must be present");
            rt.flush_user_memory(scope)?;
            Ok(record)
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
/// Steps 2–9 are all *secondary* cleanups against state that the
/// tombstone already makes unreachable through the public read
/// path (`open_store` recovery + `is_scope_forgotten` guards).
/// They are each independently important for the
/// cryptographic-forgetting contract — in particular step 9 drops
/// **plaintext OAuth2 bearer tokens** out of process memory, which
/// is the highest-sensitivity secondary state in the substrate.
/// Letting one failing secondary cleanup short-circuit the others
/// would leave orphaned plaintext credentials in `token_vault` for
/// a forgotten scope, which violates the contract this helper
/// exists to enforce.
///
/// So steps 2–9 are run *unconditionally* — every step is attempted
/// regardless of earlier failures, errors are accumulated, and the
/// first error encountered is returned to the caller after every
/// cleanup has had a chance to run. Errors from earlier secondary
/// steps do NOT mask later secondary steps in any way: each step
/// owns its own piece of state and runs against that state directly,
/// so a SQLCipher I/O failure on step 3 does not affect the
/// in-memory connector teardown on step 9.
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
/// 5. Persisted approved-document payload row
///    deletion (`approved_document_payloads` table). Best-effort
///    — failure logs WARN and accumulates the error; the payload
///    ciphertext is sealed under the scope DEK that step 1 just
///    destroyed so the bytes are cryptographically unrecoverable.
///    The row delete keeps the table bounded and prevents
///    `open_store`'s rehydration pass from loading orphan
///    metadata for a forgotten scope.
/// 6. In-memory memory map purge (infallible — `HashMap::remove`).
/// 7. Persisted connector instance row deletion (`connector_instances`
///    table). Best-effort — failure logs WARN and accumulates the
///    error; the row's AEAD ciphertext is sealed under the scope
///    DEK that step 1 just destroyed, so the payload is
///    cryptographically unrecoverable. `open_store`'s rehydration
///    sweep also picks up any orphaned row on next boot.
/// 8. Persisted OAuth2 token row deletion (`connector_tokens`
///    table). Same best-effort discipline as step 7 — the token
///    ciphertext is sealed under the destroyed scope DEK so
///    failure to delete the row does not leak plaintext
///    credentials.
/// 9. Connector lifecycle purge — every in-memory
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
    // Defense in depth against the synthesis-windows sentinel
    // collision: `parse_scope_id` already rejects the nil UUID at the host
    // boundary, but internal callers (tests, future refactors)
    // can still synthesise a `ScopeId` directly. Forgetting the
    // sentinel scope would call `delete_memory_blobs_for_scope`
    // and wipe the entire `SynthesisWindowManager` row, which is
    // a substrate-wide data-loss event masquerading as a scoped
    // cleanup. Refuse loudly so the caller fixes the bug instead
    // of silently corrupting state.
    if scope == crate::runtime::synthesis_windows_scope() {
        return Err(FfiError::InvalidId {
            message: "scope_id: nil UUID is reserved as the synthesis-windows sentinel; \
                      refusing to forget the substrate-internal scope"
                .into(),
        });
    }

    // 1. Atomic in-memory + on-disk forgetting (see
    //    `FfiRuntime::forget_scope` for the rationale). Bail on
    //    failure: if this fails the scope is still readable and
    //    running the secondary cleanups would prematurely tear down
    //    state the host still has the right to read.
    rt.forget_scope(scope)?;

    // Steps 2–9 are best-effort secondary cleanups. Every step
    // MUST be attempted regardless of earlier failures so that the
    // in-memory connector teardown (step 9 — which drops plaintext
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

    // 5. Delete persisted approved-document metadata rows for the
    //     scope. As of v12 these are
    //     metadata-only — the actual payload bytes live in
    //     `body_store` and the per-scope CEK wrap (already destroyed
    //     in step 3 by `purge_body_key_wraps_for_scope`, which also
    //     GCs the body row when its last wrap goes away). Even if
    //     this DELETE fails, the bytes are cryptographically
    //     unrecoverable because step 1 destroyed the scope DEK that
    //     wraps the CEK and step 3 destroyed the wrap itself. We
    //     still attempt the delete so the metadata row count stays
    //     bounded and `open_store`'s rehydration pass does not load
    //     orphan metadata for a forgotten scope. Best-effort,
    //     accumulates the first error so the caller sees the gap
    //     without interrupting the remaining steps.
    match rt
        .store()
        .delete_approved_document_payloads_for_scope(scope)
    {
        Ok(deleted) => {
            if deleted > 0 {
                tracing::debug!(
                    scope = %scope.as_uuid(),
                    deleted,
                    "forget_scope_state: purged approved-document payload rows",
                );
            }
        }
        Err(e) => {
            let err = FfiError::Evidence {
                message: e.to_string(),
            };
            tracing::warn!(
                scope = %scope.as_uuid(),
                error = %err,
                "forget_scope_state: delete_approved_document_payloads_for_scope failed; \
                 ciphertext remains unrecoverable because the scope DEK is already destroyed",
            );
            first_error.get_or_insert(err);
        }
    }

    // 5b. earlier: delete archived synthesis-object version
    //     rows for the scope. The ciphertext was sealed under the
    //     scope DEK that step 1 destroyed, so even if this SQL
    //     DELETE fails the bytes are cryptographically
    //     unrecoverable. The row bytes themselves are still useful
    //     to clear so the table stays bounded across the lifetime
    //     of the substrate and `open_store`'s history-table
    //     rehydration does not surface stale versions for a
    //     forgotten scope. Best-effort, accumulates the first
    //     error.
    match rt.store().delete_synthesis_object_versions_for_scope(scope) {
        Ok(deleted) => {
            if deleted > 0 {
                tracing::debug!(
                    scope = %scope.as_uuid(),
                    deleted,
                    "forget_scope_state: purged synthesis-object version rows",
                );
            }
        }
        Err(e) => {
            let err = FfiError::Evidence {
                message: e.to_string(),
            };
            tracing::warn!(
                scope = %scope.as_uuid(),
                error = %err,
                "forget_scope_state: delete_synthesis_object_versions_for_scope failed; \
                 ciphertext remains unrecoverable because the scope DEK is already destroyed",
            );
            first_error.get_or_insert(err);
        }
    }

    // 6. In-memory memory maps. Infallible.
    rt.user_memories.remove(&scope);
    rt.channel_memories.remove(&scope);

    // 6b. Synthesis state teardown — cryptographic-forgetting
    //     contract requires every synthesis artefact bound to the
    //     forgotten scope (domain/tenant memory objects, in-flight
    //     synthesis windows, completed synthesis objects, cooldown
    //     bookkeeping) to become unrecoverable in-memory at the
    //     same time the on-disk row deletion lands.
    //
    //     The corresponding on-disk rows are already covered by
    //     step 4 above — `delete_memory_blobs_for_scope` deletes
    //     every memory_objects row keyed by the scope regardless
    //     of `kind`, so domain_memory / tenant_memory /
    //     synthesis_object blobs are removed by the same SQL DELETE.
    //
    //     Infallible: every operation is an in-memory map mutation
    //     or a `HashMap::remove` over freshly collected ids.
    rt.domain_memories.remove(&scope);
    rt.tenant_memories.remove(&scope);
    // Per-(scope, tier) cooldown map — strip every entry whose
    // first component matches the forgotten scope. Mirrors the
    // semantics of the prior `remove(&scope)` over the legacy
    // scope-only map. `retain` is O(N) over total cooldown
    // entries which is bounded by 2 × active scopes (Domain +
    // Tenant tiers) so the cost stays linear in the live runtime.
    rt.synthesis_cooldowns.retain(|(s, _), _| *s != scope);
    // Drop the whole sub-map for the forgotten scope in one O(1)
    // outer-map removal. An earlier nested shape walked every window
    // id owned by the scope and removed each from the flat map; the
    // current shape addresses the scope's entire object set as a
    // single value keyed by `scope`. Window ids stay
    // globally unique so no other scope's objects can be caught by
    // this — but as a defense-in-depth measure (and to match the
    // documented invariant in the runtime's `synthesis_objects`
    // rustdoc) we never touch other scopes' sub-maps here.
    rt.synthesis_objects.remove(&scope);
    rt.synthesis_windows.remove_windows_for_scope(scope);
    // The window-manager mutation only persists if we flush.
    //
    // Important: the `SynthesisWindowManager` is persisted under the
    // nil-UUID sentinel scope (see
    // `crate::runtime::synthesis_windows_scope`), NOT under the
    // forgotten scope. Step 1's DEK destruction therefore does NOT
    // make the stale row unreadable — the sentinel-scope DEK is
    // untouched and the blob will still decrypt on the next
    // `open_store`.
    //
    // A flush failure here is recoverable in two ways:
    //
    // 1. The in-memory manager has already been pruned, so no
    //    in-process FFI call can observe the forgotten window
    //    (`is_scope_forgotten` short-circuits every entry point
    //    that operates on the scope).
    // 2. `open_store` runs a tombstone-aware purge over the
    //    rehydrated manager (`tombstoned_scopes` walk after the
    //    `load_memory_blob`) and rewrites the sentinel blob on
    //    disk, so the stale window is dropped on the next restart
    //    even if this flush never lands.
    //
    // Best-effort warn is sufficient.
    if let Err(e) = rt.flush_synthesis_windows() {
        tracing::warn!(
            scope = %scope.as_uuid(),
            error = ?e,
            "forget_scope_state: flush_synthesis_windows failed; in-memory state is clean and \
             open_store will purge the stale window row on next restart via the \
             tombstone-aware rehydration cleanup",
        );
    }

    // 7. Delete persisted connector instance rows for the scope.
    //    Best-effort: even if the SQL DELETE fails, the rows are
    //    AEAD-encrypted under the scope DEK that step 1 destroyed,
    //    so the payload is cryptographically unrecoverable. The
    //    dangling rows are also picked up on the next `open_store`'s
    //    rehydration sweep (which checks `tombstones.contains` and
    //    deletes any row bound to a forgotten scope). We accumulate
    //    the first error so callers see the gap while still running
    //    step 9's infallible in-memory teardown unconditionally.
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

    // 8. Delete persisted OAuth2 token rows for the scope. Same
    //    best-effort discipline as step 7 — the token ciphertext is
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

    // 9. Connector lifecycle: every `ConnectorInstance` row, live
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
        // Mirror the `remove_connector` cleanup hook
        // (`crates/ffi/src/connector.rs`) so a `forget_scope` on a
        // scope whose connectors have per-instance scheduler
        // policies does not leave stale `SchedulePolicy` /
        // `InstanceAccounting` entries in the scheduler maps.
        // No-op when no scheduler is running. Inside the
        // `with_runtime` closure so the canonical runtime-mutex
        // → scheduler-state-mutex acquisition order documented at
        // `sync_scheduler::run_one_tick` is preserved.
        crate::sync_scheduler::prune_instance(rt, id);
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

/// Build the per-scope **concept graph** by projecting the scope's
/// live user-memory observations through
/// [`concept_graph::project_memory_graph`], and return a wire-flat
/// [`GraphView`] the UI can render directly.
///
/// This is a pure read: the graph is *derived* from the same
/// per-scope [`UserMemoryObject`](memory_manager::UserMemoryObject)
/// that [`list_memories`] reads and the decay sweep mutates, so the
/// graph can never disagree with memory and needs no separate
/// persisted store or CRDT sync to stay correct (see
/// [`concept_graph::projection`] for the rationale). Each live
/// (non-`Deleted`) observation becomes a node carrying its lifecycle
/// state, and every resolved supersession pointer becomes a typed
/// `Supersedes` edge — so a freshly-written memory shows up as a
/// `Candidate` node and a decayed one dims to `Superseded`,
/// reflecting the state machine in real time.
///
/// The returned graph is bounded to [`DEFAULT_MAX_NODES`] nodes — a
/// deliberate render budget so a pathologically large scope can never
/// ship an unbounded payload across the FFI / wire boundary into a
/// browser force-directed layout (a real concern across the SME
/// fleet). The cap is passed explicitly rather than relying on
/// [`ViewFilter`]'s implicit default so this surface's contract does
/// not silently change if that default is ever retuned. When a scope
/// has more live observations than the budget, the lowest-priority
/// nodes are dropped and [`GraphView::truncation`] reports
/// [`TruncationReason::NodeLimitReached`] so the UI can surface a
/// "showing first N concepts" hint instead of silently lying.
///
/// Only the **user** tier is projected: the graph is bound to the
/// requested `scope_id` and gated by [`AllowAllScopes`] *after* the
/// projection already restricted nodes to that scope, so a caller can
/// never read another scope's concepts through this surface. A
/// cryptographically-forgotten scope projects to an empty graph.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
///
/// # FFI surface
///
/// This is a plain-Rust entry point consumed by the substrate server
/// (which serialises the [`GraphView`] straight to JSON for the Go
/// gateway → UI read path) and mirrored on the N-API surface by
/// [`crate::js_get_concept_graph`] for Electron/desktop hosts. It is
/// intentionally **not** a `#[uniffi::export]`: [`GraphView`] is a
/// rich nested `concept_graph` type, and exporting it over UniFFI
/// would require making the whole `concept_graph` visualization
/// taxonomy a set of UniFFI records/enums — heavy coupling for a
/// surface today's mobile hosts do not render (they read memories
/// directly via [`list_memories`]). The graph is exposed as JSON at
/// the boundaries that actually consume it.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned strings across the language boundary on every call.
pub fn get_concept_graph(handle: RuntimeHandle, scope_id: ScopeIdString) -> FfiResult<GraphView> {
    metrics::instrument(metrics::inc_get_concept_graph, || {
        let scope = parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            let graph = if rt.is_scope_forgotten(scope) {
                ConceptGraph::new()
            } else if let Some(umo) = rt.user_memory(scope) {
                let projections = umo
                    .list(&memory_manager::MemoryFilter::any())
                    .into_iter()
                    .filter_map(memory_object_to_projection);
                project_memory_graph(projections)
            } else {
                ConceptGraph::new()
            };
            // The projection already bound every node to `scope`; the
            // scope-restricted view + `AllowAllScopes` gate is the
            // belt-and-braces second pass that keeps the read
            // structurally single-scope. The node budget is set
            // explicitly (not left to the implicit default) so the
            // wire/render bound is part of this surface's contract;
            // `GraphView::truncation` signals when it bites.
            let filter = ViewFilter {
                max_nodes: Some(DEFAULT_MAX_NODES),
                ..ViewFilter::default()
            };
            Ok(subgraph_for_scope(&graph, scope, &filter, &AllowAllScopes))
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

/// Report the current state of the lazy SLM-weight download as a JSON
/// object, so a host can poll for and render a one-time download
/// progress bar **without** repeatedly invoking [`trigger_synthesis`]
/// (which would otherwise be the only way to observe the
/// [`FfiError::ModelDownloading`] percentage).
///
/// The returned JSON is the serialised
/// [`inference_router::ModelDownloadState`], tagged by a `state` field:
///
/// ```json
/// {"state":"idle"}
/// {"state":"in_progress","pct":42}
/// {"state":"complete"}
/// {"state":"failed","message":"model checksum mismatch: …"}
/// ```
///
/// `idle` means no download is in flight — either the weights are
/// already present, or this build provisions them out-of-band (mobile,
/// where the network transport is intentionally not compiled in).
///
/// This pull-based accessor is the deliberate counterpart to a push
/// callback: the multi-hundred-MB transfer runs on the bootstrap
/// thread, so a host paints its progress bar by polling this every
/// frame instead of absorbing a hot per-chunk callback across the
/// language boundary.
///
/// # Errors
/// * [`FfiError::InvalidId`] — never (kept for signature symmetry with
///   the other handle-scoped accessors); resolving the handle is the
///   only fallible step.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the boundary.
#[uniffi::export]
pub fn model_download_status(handle: RuntimeHandle) -> FfiResult<String> {
    metrics::instrument(metrics::inc_model_download_status, || {
        let state =
            with_runtime(handle, |rt| Ok(rt.inference_router_arc()))?.model_download_state();
        // `ModelDownloadState` is a fixed, internally-tagged enum, so
        // serialisation cannot fail; map defensively rather than `expect`
        // to keep the FFI boundary panic-free.
        serde_json::to_string(&state).map_err(|e| FfiError::Synthesis {
            message: format!("serialise model download status: {e}"),
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
/// Because runs without the per-handle mutex, a host that
/// calls [`close_store`](crate::close_store) **concurrently** with
/// `trigger_synthesis` can land its close between and Step 3:
///
/// * captures the [`Arc<InferenceRouter>`] and drops the
///   mutex.
/// * issues the SLM dispatch. The host calls
///   [`close_store`](crate::close_store) on a different thread, which
///   removes the handle from the registry and (after the drain loop
///   completes — see the docs on
///   [`close_store`](crate::close_store)) drops the runtime.
/// * returns successfully with a parsed [`SummaryBundle`].
/// * Step 3's [`with_runtime`] re-lookup fails with
///   [`FfiError::Unavailable`] because the handle is no longer in
///   the registry.
///
/// In that scenario the SLM did real work (and burned real wall
/// clock / GPU time) but the recap is **discarded** — the host
/// observes `Unavailable` even though synthesis "happened". This is
/// a *safe* race is the only phase that writes to the
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

    // ─────────────────── Step 1: gather (locked) ───────────────────
    //
    // Returns the prompt to dispatch plus an owned `Arc` clone of the
    // router so the unlocked phase below can operate without re-entering
    // `with_runtime`.
    let (router, prompt, salient, row_count) = with_runtime(handle, |rt| {
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

        // Salient evidence terms + row count drive the deterministic
        // verify-and-retry policy below (coverage scoring + adaptive
        // `n_predict` budget). Computed here, under the lock, while the
        // decrypted `bodies` are still in hand. Only *derived* values
        // leave this frame — the lowercased salient tokens, the row
        // count, and the rendered `prompt`; the raw decrypted `bodies`
        // Vec is dropped when the closure returns. The prompt already
        // embeds the evidence text, so it (not these tokens) is the
        // plaintext-bearing value that crosses into the unlocked dispatch
        // phase — the salient tokens carry no evidence the prompt doesn't
        // already, and the lock boundary is unchanged by computing them
        // here rather than after the dispatch.
        let salient =
            synthesis_pipeline::salient_terms_from_texts(bodies.iter().map(String::as_str));
        let row_count = bodies.len();

        let combined = bodies.join("\n\n");
        let prompt = InferenceTask::SynthSummary
            .prompt_template()
            .replace("{body}", &combined);
        // Clone the `Arc` while still holding the runtime mutex so the
        // unlocked dispatch phase below has a stable handle that
        // outlives the `with_runtime` frame. The clone itself is one
        // atomic increment.
        Ok((rt.inference_router_arc(), prompt, salient, row_count))
    })?;

    // ───────────────── Step 2: dispatch (UNLOCKED) ─────────────────
    //
    // The per-handle `FfiRuntime` mutex is released for the duration of
    // these calls so concurrent `ingest_message` / `query` /
    // `get_channel_memory` on the same handle can run in parallel with
    // the (potentially multi-second) SLM dispatch.
    //
    // `open_store` no longer probes adapters eagerly (lazy-load: the
    // probe is deferred until the first synthesis request to keep the
    // open path — and ingest/query-only hosts — off the probe cost).
    // Kick the background probe off here the first time, then wait for
    // it to finish so a host that calls `trigger_synthesis` does not
    // race the probe. Both calls are no-ops once probing has started /
    // completed on a prior synthesis.
    router.ensure_bootstrap_started();
    // Bootstrap performs the lazy SLM-weight download (when configured
    // and the weights are absent) on a background thread. Inspect the
    // download state *before* `wait_for_bootstrap` — waiting would block
    // this caller for the entire multi-hundred-MB transfer, which is
    // exactly the lazy-download UX we must avoid.
    //
    // `ensure_bootstrap_started` publishes `InProgress` synchronously
    // when a download is pending, so the very first caller observes it
    // deterministically here (no race against the background thread).
    match router.model_download_state() {
        // Download in flight: the subsystem is healthy; the host should
        // render a one-time progress bar and retry, not an error banner.
        inference_router::ModelDownloadState::InProgress { pct } => {
            return Err(FfiError::ModelDownloading { progress_pct: pct });
        }
        // A prior download attempt failed. Reset the bootstrap latch so
        // the *next* `trigger_synthesis` re-attempts the download + probe
        // from scratch (host-paced retry — no tight loop), and surface
        // the failure now as a transient `Unavailable` so the host can
        // offer a "retry" affordance instead of treating it as permanent.
        inference_router::ModelDownloadState::Failed { message } => {
            router.reset_model_download_for_retry();
            return Err(FfiError::Unavailable {
                subsystem: format!("synthesis: SLM weight download failed: {message}"),
            });
        }
        // `Idle` (no download configured / weights already present) or
        // `Complete`: the probe is the only remaining bootstrap work, so
        // block on it and dispatch normally.
        inference_router::ModelDownloadState::Idle
        | inference_router::ModelDownloadState::Complete => {
            router.wait_for_bootstrap();
        }
    }
    // Per-call sampling: start from the router's deterministic synthesis
    // defaults (greedy + fixed seed) and let the verify-and-retry policy
    // override only `n_predict` per attempt via the adaptive budget. Using
    // `dispatch_with_sampling` (not the plain `dispatch`) is what carries
    // the seed + sampling knobs onto the wire so the on-device path is
    // byte-reproducible for a fixed (model, prompt).
    let base_sampling = router.config().sampling;

    // Deterministic verify-and-retry, shared with `LlamaCppSynthesizer`
    // (`synthesis_pipeline::verify_and_retry`): the orchestration owns the
    // *decision* (adaptive budget, quality scoring against `salient`, the
    // single bounded retry) while the closure below owns *transport*
    // (dispatch + truncation-aware salvage parse). The shared piece is the
    // scoring/retry *policy* (`score_bundle_with_terms` + `verify_and_retry`
    // + the budget constants) — that is what no longer drifts between
    // on-device and server synthesis. The salient-term *inputs* to that
    // policy are derived per-path and are not identical: here we feed the
    // decrypted evidence `bodies`, while the pipeline feeds observation
    // contents plus `inputs.recap_seed`. This is benign — `recap_seed` is
    // empty for real `LlamaCppSynthesizer` calls (it is a test-only seed
    // for `NoOpSynthesizer`), so both paths derive terms from the same
    // evidence text in practice — but the contract that is unified is the
    // scorer, not its inputs.
    let synthesis_pipeline::VerifiedSynthesis {
        bundle,
        recap_chars,
        low_quality,
        retried,
        retry_failed,
        truncated_attempts,
        exemplar_leaks_stripped,
    } = synthesis_pipeline::verify_and_retry(
        &prompt,
        row_count,
        &salient,
        |attempt_prompt: &str, n_predict: u32| -> Result<synthesis_pipeline::Attempt, FfiError> {
            let raw = router
                .dispatch_with_sampling(
                    InferenceTask::SynthSummary,
                    attempt_prompt,
                    &base_sampling.with_n_predict(n_predict),
                )
                .map_err(|e| match e {
                    // The model ran but produced an unusable result —
                    // hosts need to distinguish this from "no adapter
                    // available" to drive their own retry policy. See
                    // `FfiError::InferenceFailure` docs for the contract.
                    RouterError::InferenceFailure(message) => FfiError::InferenceFailure {
                        message: format!("synthesis: {message}"),
                    },
                    // `Unavailable`, `TierTooLow`, and `NotProbed` all
                    // mean "no adapter on this build can serve the task";
                    // surface them uniformly as a transient-unavailable
                    // subsystem so hosts can probe again once their
                    // environment changes.
                    other => FfiError::Unavailable {
                        subsystem: format!("synthesis: {other}"),
                    },
                })?;
            // The grammar constrains the *shape* but not the *length*: a
            // small model can be cut off at the adapter's `n_predict` cap
            // mid-string. `from_slm_str_salvaged` does the strict parse
            // once and reports whether a truncated prefix had to be
            // salvaged (closing the open string + brackets) — surfaced via
            // `Attempt::truncated` so the budget pressure is observable
            // rather than silently swallowed, without this caller running
            // its own redundant strict parse first.
            // Mapped to `InferenceFailure` (not `Evidence`) because the
            // failure mode is "the model ran but produced unusable JSON":
            // the evidence store never ran, so misclassifying as `Evidence`
            // would route the host to the wrong remediation.
            let (bundle, truncated) = SummaryBundle::from_slm_str_salvaged(&raw).map_err(|e| {
                FfiError::InferenceFailure {
                    message: format!("synthesis: malformed SummaryBundle JSON: {e}"),
                }
            })?;
            Ok(synthesis_pipeline::Attempt { bundle, truncated })
        },
    )?;

    // Quality telemetry for the on-device path (mirrors the pipeline's
    // `SynthesisMetrics`): retry/low-quality/truncation counters plus the
    // recap-length signal. Counters move only on real events so a flat
    // series means "first attempt was clean, full-length, on-budget".
    if low_quality {
        metrics::inc_synthesis_lowquality();
    }
    if retried {
        metrics::inc_synthesis_retry();
    }
    if retry_failed {
        // Graceful degradation: the retry dispatch errored, so the
        // first (mediocre but usable) bundle was kept rather than
        // failing the whole synthesis. Surface it — a counter plus a
        // warn — so a flaky adapter that fails only on the retry path
        // leaves a diagnostic trace instead of disappearing silently.
        metrics::inc_synthesis_retry_failed();
        tracing::warn!(
            "on-device synthesis retry dispatch failed; kept the first \
             (low-quality) attempt for scope"
        );
    }
    for _ in 0..truncated_attempts {
        metrics::inc_synthesis_truncated();
    }
    // The 2-bit model copied the synthesis prompt's one-shot exemplar
    // placeholder (`EXAMPLE_DECISION` / `EXAMPLE_TASK`) into a real
    // bundle's structured lists; the quality gate scrubbed it before it
    // could reach the channel memory. This should be rare now the exemplar
    // is abstract. We fold the count into the scrapeable
    // `synthesis_exemplar_leaks_stripped_total` counter (no-op at 0) so a
    // leaking prompt is observable on the Prometheus surface across the
    // tenant fleet, and additionally emit a `warn!` for the rare event so
    // it surfaces in logs without waiting for a scrape.
    metrics::add_synthesis_exemplar_leaks_stripped(usize::from(exemplar_leaks_stripped));
    if exemplar_leaks_stripped > 0 {
        tracing::warn!(
            stripped = exemplar_leaks_stripped,
            "on-device synthesis stripped leaked exemplar placeholder entries before persistence"
        );
    }
    metrics::observe_synthesis_recap_chars(recap_chars);

    // ─────────────────── Step 3: apply (locked) ────────────────────
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
            // See SECURITY.md §"Random number generation" for why
            // the substrate uses the OS RNG (`SysRng`, not the
            // userspace `ThreadRng`) for every per-encrypt AEAD
            // nonce, even on the hot FFI path.
            rand::rngs::SysRng
                .try_fill_bytes(&mut nonce)
                .expect("OS RNG failure");
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
/// drifted into duplication — they
/// were consolidated here so a future change to scope-id validation
/// touches exactly one site. `pub(crate)` visibility intentionally
/// keeps it out of the FFI surface (UniFFI/N-API hosts call the
/// public entry points, never this helper directly).
///
/// # Nil UUID rejection
///
/// The nil UUID (`00000000-0000-0000-0000-000000000000`) is
/// reserved as a substrate-internal sentinel scope under which the
/// global [`synthesis_pipeline::SynthesisWindowManager`] is
/// flushed (see [`crate::runtime::synthesis_windows_scope`]).
/// Accepting it from a host-supplied string would let a caller
/// collide with that sentinel: for example, `forget_scope` on the
/// nil scope would call `delete_memory_blobs_for_scope` and wipe
/// the entire synthesis window history on the next `open_store`.
/// `Uuid::new_v4()` never produces the nil UUID, so rejecting it
/// at the FFI boundary closes the collision without removing any
/// legitimate scope id from the host's namespace.
pub(crate) fn parse_scope_id(s: &str) -> FfiResult<ScopeId> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
        message: format!("scope_id: {e}"),
    })?;
    if uuid.is_nil() {
        return Err(FfiError::InvalidId {
            message: "scope_id: nil UUID is reserved as a substrate sentinel; \
                      hosts MUST supply a non-nil v4 UUID"
                .into(),
        });
    }
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
    MemoryRecord {
        id: obj.id.to_string(),
        scope_id: obj.scope_id.to_string(),
        summary: memory_summary(obj),
        state,
        retention_score: obj.retention_score,
        created_at: obj.created_at.timestamp(),
        last_reinforced_at: obj.last_accessed_at.timestamp(),
    }
}

/// Human-readable text for a memory object: the `metadata.content`
/// string if present, otherwise the whole metadata blob rendered as
/// JSON (or empty for a null blob). Shared by [`memory_object_to_record`]
/// and [`memory_object_to_projection`] so the list view and the
/// concept-graph node carry identical labels.
fn memory_summary(obj: &memory_manager::MemoryObject) -> String {
    obj.metadata
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
        )
}

/// Map an internal [`memory_manager::MemoryState`] onto the coarser
/// concept-graph [`NodeState`](concept_graph::NodeState).
///
/// The graph taxonomy is deliberately coarser than the memory state
/// machine — it only distinguishes *live* concepts (`Candidate` /
/// `Canonical`) from *non-live* ones (`Superseded`) — so several
/// memory states collapse onto one node state. The precise memory
/// state is preserved in the node's `metadata.memory_state` so nothing
/// is lost:
///
/// * `Candidate` / `Reinforced` / `Consolidated` → `Candidate`
///   (in the working set, not yet promoted to canonical).
/// * `Canonical` → `Canonical`.
/// * `Superseded` → `Superseded` (kept so a supersession edge has a
///   target) and `Archived` → `Superseded` (decayed out by TTL but
///   retained, rendered dimmed).
/// * `Deleted` → `None`: tombstones are never projected into the graph.
fn memory_state_to_node_state(
    state: memory_manager::MemoryState,
) -> Option<concept_graph::NodeState> {
    use concept_graph::NodeState as N;
    use memory_manager::MemoryState as M;
    match state {
        M::Candidate | M::Reinforced | M::Consolidated => Some(N::Candidate),
        M::Canonical => Some(N::Canonical),
        M::Superseded | M::Archived => Some(N::Superseded),
        M::Deleted => None,
    }
}

/// Flatten a live [`memory_manager::MemoryObject`] into the
/// [`MemoryProjection`] the concept graph projects into a node.
///
/// Returns `None` for a `Deleted` tombstone (never projected). The
/// `metadata` blob preserves the precise underlying memory state,
/// retention score, pin count, and observation type so a node-detail
/// panel can render them without a second round-trip.
pub(crate) fn memory_object_to_projection(
    obj: &memory_manager::MemoryObject,
) -> Option<MemoryProjection> {
    let state = memory_state_to_node_state(obj.state)?;
    let summary = memory_summary(obj);
    let metadata = serde_json::json!({
        "source": "user_memory",
        "memory_state": obj.state,
        "retention_score": obj.retention_score,
        "pin_count": obj.pin_count,
        "observation_type": obj
            .metadata
            .get("observation_type")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    });
    Some(MemoryProjection {
        id: obj.id,
        scope_id: obj.scope_id,
        label: summary.clone(),
        definition: summary,
        state,
        superseded_by: obj.superseded_by,
        created_at: obj.created_at,
        updated_at: obj.last_accessed_at,
        metadata,
    })
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

/// Map the wire-flat [`FfiImportanceClass`] onto the memory-layer
/// [`SensitivityClass`](memory_manager::SensitivityClass) that drives
/// the decay schedule. The two enums share the same four-way taxonomy
/// (`Critical` / `Important` / `Useful` / `Noise`); a single mapping
/// keeps the storage-tier and memory-tier classifications aligned.
fn ffi_importance_to_sensitivity(ffi: FfiImportanceClass) -> memory_manager::SensitivityClass {
    match ffi {
        FfiImportanceClass::Critical => memory_manager::SensitivityClass::Critical,
        FfiImportanceClass::Important => memory_manager::SensitivityClass::Important,
        FfiImportanceClass::Useful => memory_manager::SensitivityClass::Useful,
        FfiImportanceClass::Noise => memory_manager::SensitivityClass::Noise,
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
    /// New scopes get a fresh random DEK drawn from the OS RNG
    /// (`rand::rngs::SysRng`, see `SECURITY.md`). Existing scopes
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
        // destroy registry size. The health envelope reads
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

    /// Regression test for : the nil
    /// UUID is reserved as the substrate's synthesis-windows
    /// sentinel scope, so `parse_scope_id` MUST refuse it at the
    /// FFI boundary. Without this check, a host that passes
    /// `00000000-0000-0000-0000-000000000000` to `forget_scope`
    /// would wipe the entire `SynthesisWindowManager` row on the
    /// next `open_store`.
    #[test]
    fn parse_scope_id_rejects_nil_uuid() {
        let err = parse_scope_id("00000000-0000-0000-0000-000000000000").unwrap_err();
        match err {
            FfiError::InvalidId { message } => {
                assert!(
                    message.contains("nil UUID"),
                    "error message should call out the nil-UUID rejection, got: {message}",
                );
            }
            other => panic!("expected InvalidId, got {other:?}"),
        }
    }

    /// Defense-in-depth check: even if an internal caller manages
    /// to construct a sentinel-scoped `ScopeId` (bypassing the
    /// host-facing `parse_scope_id` guard above),
    /// `forget_scope_state` MUST refuse rather than silently wipe
    /// the global synthesis-windows row.
    #[test]
    fn forget_scope_state_refuses_synthesis_sentinel() {
        let (h, _dir) = fresh_store();
        let sentinel = crate::runtime::synthesis_windows_scope();
        let err = crate::runtime::with_runtime(h, |rt| forget_scope_state(rt, sentinel))
            .expect_err("must refuse sentinel scope");
        assert!(
            matches!(err, FfiError::InvalidId { ref message } if message.contains("sentinel")),
            "expected InvalidId with `sentinel` message, got {err:?}",
        );
        teardown(h);
    }

    /// The standby replicator's raw page splicing depends on the store
    /// running in a rollback-journal mode (not WAL). This pins the
    /// invariant `store_journal_mode` exists to assert: a freshly opened
    /// store must report a rollback mode so the standby's startup check
    /// passes. If `evidence_store`'s open path ever switched to WAL, this
    /// test would catch it alongside the standby's runtime guard.
    #[test]
    fn store_journal_mode_is_rollback_not_wal() {
        let (h, _dir) = fresh_store();
        let mode = store_journal_mode(h).expect("journal_mode");
        assert_ne!(
            mode, "wal",
            "standby raw applies require a rollback-journal mode"
        );
        assert!(
            matches!(
                mode.as_str(),
                "delete" | "truncate" | "persist" | "memory" | "off"
            ),
            "unexpected journal mode {mode:?}",
        );
        teardown(h);
    }

    #[test]
    fn store_journal_mode_rejects_unknown_handle() {
        let err = store_journal_mode(RuntimeHandle::NONE).expect_err("unknown handle");
        assert!(
            matches!(err, FfiError::Unavailable { .. }),
            "expected Unavailable, got {err:?}",
        );
    }

    /// `snapshot_store_to` must be wired end-to-end: a host can fold the
    /// live store into a standalone backup file, re-open that file with
    /// the same master key, and read back every row it ingested — all
    /// without closing the source store. Regression guard against the
    /// `EvidenceStore::snapshot_to` building block being left unexposed
    /// through the FFI surface (Devin Review flag on #220).
    #[test]
    fn snapshot_store_to_produces_reopenable_backup() {
        let (h, dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        ingest_message(
            h,
            scope.clone(),
            "quarterly revenue summary BR-2505".to_string(),
            SourceKind::Email,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");

        // Snapshot the *live* store (still open) into a fresh path.
        let dest = dir.path().join("backup.db");
        snapshot_store_to(h, dest.to_string_lossy().into_owned()).expect("snapshot_store_to");
        assert!(dest.exists(), "snapshot must create the destination file");

        // The source store keeps working after the snapshot.
        query(h, scope.clone(), "revenue".into(), 10).expect("source store still queryable");

        // Re-open the backup with the SAME master key and confirm the
        // ingested row round-tripped (same page key => no rekey needed).
        let key_hex = "a5".repeat(32);
        let restored =
            open_store(dest.to_string_lossy().into_owned(), key_hex).expect("reopen backup");
        let hits = query(restored, scope, "revenue".into(), 10).expect("query backup");
        assert!(!hits.is_empty(), "backup must contain the ingested row");

        teardown(restored);
        teardown(h);
    }

    /// `snapshot_store_to` refuses a destination that already exists,
    /// surfacing it as a recoverable `Evidence` error rather than
    /// panicking or silently clobbering. The guard exercised here is
    /// `EvidenceStore::snapshot_to`'s own `dest_path.exists()` pre-check
    /// (which fires before the `VACUUM INTO` and yields the friendly
    /// "already exists" message); SQLite's own refusal of a present,
    /// non-empty target is the redundant backstop behind it.
    #[test]
    fn snapshot_store_to_rejects_existing_destination() {
        let (h, dir) = fresh_store();
        // The live DB file itself already exists at this path.
        let dest = dir.path().join("evidence.db");
        let err = snapshot_store_to(h, dest.to_string_lossy().into_owned())
            .expect_err("must refuse a pre-existing destination");
        assert!(
            matches!(err, FfiError::Evidence { ref message } if message.contains("already exists")),
            "expected Evidence/already-exists, got {err:?}",
        );
        teardown(h);
    }

    #[test]
    fn snapshot_store_to_rejects_unknown_handle() {
        let err = snapshot_store_to(RuntimeHandle::NONE, "/tmp/never-written.db".into())
            .expect_err("unknown handle");
        assert!(
            matches!(err, FfiError::Unavailable { .. }),
            "expected Unavailable, got {err:?}",
        );
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
    fn fts_literal_token_fallback_quotes_each_token() {
        // Hyphen / comma identifiers and multi-word input each become
        // space-joined quoted phrases; embedded quotes are doubled.
        assert_eq!(
            fts_literal_token_fallback("BR-2505").as_deref(),
            Some("\"BR-2505\"")
        );
        assert_eq!(
            fts_literal_token_fallback("FA-2025-0411  12,4").as_deref(),
            Some("\"FA-2025-0411\" \"12,4\"")
        );
        assert_eq!(
            fts_literal_token_fallback(r#"sa"id"#).as_deref(),
            Some(r#""sa""id""#)
        );
        // Whitespace-only input has no tokens — nothing to retry.
        assert_eq!(fts_literal_token_fallback("   \t\n"), None);
        assert_eq!(fts_literal_token_fallback(""), None);
    }

    #[test]
    fn query_recovers_from_malformed_identifier_input() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // A realistic business record keyed by a hyphenated lot number
        // and a comma decimal — exactly the shape that trips FTS5's
        // MATCH parser ("no such column: 2505") when passed verbatim.
        let body = "Lot BR-2505 rejected: humidity 12,4% over the 9% spec.";
        let evidence_id = ingest_message(
            h,
            scope.clone(),
            body.to_string(),
            SourceKind::Email,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");

        // Searching the lot number the way a user actually types it must
        // not 400 — the literal-token fallback rescues it.
        let hits = query(h, scope.clone(), "BR-2505".into(), 10)
            .expect("hyphenated identifier query must not error");
        assert_eq!(hits.len(), 1, "lot-number search should find the record");
        assert_eq!(hits[0].evidence_id, evidence_id);

        // A comma decimal is the other common malformed-MATCH case.
        let hits =
            query(h, scope.clone(), "12,4".into(), 10).expect("comma decimal query must not error");
        assert_eq!(hits.len(), 1, "comma-decimal search should find the record");

        // Multi-token free text keeps implicit-AND semantics: both
        // tokens are present, so the record matches.
        let hits = query(h, scope.clone(), "BR-2505 humidity".into(), 10)
            .expect("multi-token query must not error");
        assert_eq!(hits.len(), 1, "AND of two present tokens should match");

        // Regression guard: a deliberate FTS5 `OR` expression is valid
        // verbatim, so the fallback never engages and power-user syntax
        // still works.
        let hits = query(h, scope, "BR OR missingtoken".into(), 10)
            .expect("valid FTS5 OR expression must work verbatim");
        assert_eq!(hits.len(), 1, "OR with one present term should match");
    }

    #[test]
    fn query_rescues_malformed_structured_fts_expression() {
        // A search box must never surface an FTS5 syntax error: input
        // that uses explicit-but-broken expression syntax — an
        // unbalanced phrase quote, a dangling boolean operator, an
        // incomplete `NEAR(`, an unmatched `(` — is rescued by the
        // literal-token fallback and returns results (a `200`) rather
        // than `InvalidQuery` (a `400`). Regression guard for the
        // gateway bouncing a 400 at a user who simply typed a stray
        // quote — the contract behind substrate_server's
        // `query_rescues_malformed_fts_with_200`.
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // Seed a row so the scope is non-empty; the rescue must not
        // depend on whether the scope happens to hold matching data.
        ingest_message(
            h,
            scope.clone(),
            "the quarterly revenue report is ready".to_string(),
            SourceKind::Email,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");

        // None of these may error — each must fall back to a literal
        // search instead of rejecting the input.
        for bad in ["\"unbalanced", "revenue AND", "NEAR(", "(grouped"] {
            query(h, scope.clone(), bad.to_string(), 10)
                .unwrap_or_else(|e| panic!("malformed query {bad:?} must be rescued, got {e:?}"));
        }

        // The rescue also *matches*: an unbalanced quote on a word that
        // is present returns the seeded row, proving the fallback
        // engaged and searched literally rather than returning a hollow
        // empty result.
        let hits = query(h, scope.clone(), "\"revenue".into(), 10)
            .expect("unbalanced quote must be rescued");
        assert_eq!(hits.len(), 1, "rescued query should match the seeded row");

        // The happy path against the same scope still returns results —
        // the rescue does not regress well-formed queries.
        let hits = query(h, scope, "revenue".into(), 10).expect("well-formed query must succeed");
        assert_eq!(hits.len(), 1, "well-formed query should still match");

        teardown(h);
    }

    #[test]
    fn query_rescues_stray_embedded_quote_in_plain_text() {
        // A quote *embedded* mid-token (not at a phrase boundary) is not
        // a phrase opener — it is ordinary search-box text that trips the
        // verbatim parser, so it must stay rescued by the literal-token
        // fallback rather than 400. Regression guard against the
        // unclosed-phrase heuristic over-matching any stray `"`.
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        ingest_message(
            h,
            scope.clone(),
            "record helloworld combined token".to_string(),
            SourceKind::Email,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");

        // Must not error — the fallback quotes each token literally.
        query(h, scope, "hello\"world".into(), 10)
            .expect("stray embedded quote must be rescued, not rejected");

        teardown(h);
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

    /// With evidence in the scope but no *reachable* SLM adapter that
    /// supports `SynthSummary`, the router cannot dispatch the task and
    /// `trigger_synthesis` surfaces `Unavailable { subsystem:
    /// synthesis: … }`.
    ///
    /// On a non-mobile test build the llama.cpp adapter IS compiled in
    /// (see the `http_client_wired` cfg / `build_inference_router`),
    /// but no `llama-server` sidecar is running and no MLX runtime is
    /// linked, so every `SynthSummary`-capable adapter probes as
    /// unavailable and dispatch falls through to `Unavailable`. This
    /// pins the contract that synthesis surfaces `Unavailable` (rather
    /// than panicking or hanging) when the on-device model is wired but
    /// not currently serving.
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
            matches!(err,
                FfiError::Unavailable { ref subsystem } if subsystem.starts_with("synthesis")
            ),
            "expected Unavailable {{ subsystem: synthesis* }}, got {err:?}"
        );
        teardown(h);
    }

    /// On a default-configured store (no `model_download_url`), the
    /// lazy-download machinery stays dormant: `model_download_status`
    /// reports the internally-tagged `{"state":"idle"}` rather than
    /// erroring or reporting a phantom in-progress download. This pins
    /// the "host provisions weights out-of-band / weights already
    /// present" path that mobile and every non-download build take.
    #[test]
    fn model_download_status_reports_idle_when_no_download_configured() {
        let (h, _dir) = fresh_store();
        // Per CONTRIBUTING.md every public FFI entry point must drive a
        // `<name>_total` counter via `metrics::instrument`. Snapshot
        // before/after to pin that `model_download_status` is wired up.
        let before = metrics::snapshot().model_download_status_total;
        let status = model_download_status(h).expect("model_download_status");
        assert_eq!(
            status, r#"{"state":"idle"}"#,
            "a store with no model_download_url must report idle"
        );
        let after = metrics::snapshot().model_download_status_total;
        assert!(
            after > before,
            "model_download_status must increment its metrics counter \
             (before={before}, after={after})"
        );
        teardown(h);
    }

    /// `add_user_memory` is the public write counterpart to
    /// `get_user_memory`: it appends a `Candidate` observation,
    /// persists it, and returns the created record. This pins the
    /// create→read round-trip and the metrics wiring.
    #[test]
    fn add_user_memory_creates_candidate_and_lists_back() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let before = metrics::snapshot().add_user_memory_total;

        let created = add_user_memory(
            h,
            scope.clone(),
            "preference".into(),
            "prefers async standups".into(),
            FfiImportanceClass::Useful,
        )
        .expect("add_user_memory");
        assert_eq!(created.scope_id, scope);
        assert_eq!(created.summary, "prefers async standups");
        assert_eq!(created.state, MemoryState::Candidate);

        let after = metrics::snapshot().add_user_memory_total;
        assert!(after > before, "add_user_memory must bump its counter");

        let listed = get_user_memory(h, scope).expect("get_user_memory");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        teardown(h);
    }

    /// Fail-closed: blank `observation_type` / `content` is rejected
    /// with `InvalidId` and never creates a row.
    #[test]
    fn add_user_memory_rejects_blank_fields() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();

        let err = add_user_memory(
            h,
            scope.clone(),
            "   ".into(),
            "content".into(),
            FfiImportanceClass::Useful,
        )
        .unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));

        let err = add_user_memory(
            h,
            scope.clone(),
            "note".into(),
            "\t\n".into(),
            FfiImportanceClass::Useful,
        )
        .unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));

        assert!(get_user_memory(h, scope).expect("list").is_empty());
        teardown(h);
    }

    /// A cryptographically-forgotten scope must reject new writes with
    /// `NotFound { kind: "scope" }` — the DEK is destroyed, so the row
    /// could never be read back.
    #[test]
    fn add_user_memory_rejects_forgotten_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let evidence_id = ingest_message(
            h,
            scope.clone(),
            "seed evidence so the scope exists".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest");
        forget(h, evidence_id).expect("forget");

        let err = add_user_memory(
            h,
            scope.clone(),
            "note".into(),
            "should be rejected".into(),
            FfiImportanceClass::Useful,
        )
        .unwrap_err();
        assert!(
            matches!(err, FfiError::NotFound { ref kind, .. } if kind == "scope"),
            "expected NotFound {{ kind: scope }}, got {err:?}"
        );
        teardown(h);
    }

    /// Regression: `add_user_memory` must durably persist the scope DEK
    /// (via `ensure_scope_registered`), exactly as `ingest_message`
    /// does. A memory-only scope (no evidence rows) seals its blob under
    /// whatever key `scope_key` resolves at write time. Without a
    /// persisted `scope_deks` row, that is an in-memory-only HKDF key.
    /// If a later `ensure_scope_dek` ever resolves the scope on a cold
    /// cache — it inspects `scope_deks` and the `evidence` table, never
    /// `memory_objects` — it mints a fresh *random* DEK and persists it,
    /// permanently shadowing the HKDF key the blob was sealed under. On
    /// the next open the blob can no longer be decrypted and the memory
    /// is silently dropped.
    ///
    /// The sequence below reproduces that data loss deterministically:
    /// write → restart → cold-cache resolution (evict + ingest) →
    /// restart → read. It fails before the `ensure_scope_registered`
    /// fix (the memory vanishes) and passes after (the DEK is persisted
    /// at write time, so every later resolution agrees on the key).
    #[test]
    fn add_user_memory_survives_cold_cache_dek_resolution() {
        let dir = tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("evidence.db")
            .to_string_lossy()
            .into_owned();
        let key_hex = "a5".repeat(32);
        let scope = uuid::Uuid::new_v4().to_string();
        let scope_id = parse_scope_id(&scope).expect("scope id");

        // 1. Write a memory to a brand-new, evidence-free scope.
        let h = open_store(path.clone(), key_hex.clone()).expect("open_store");
        let created = add_user_memory(
            h,
            scope.clone(),
            "preference".into(),
            "prefers async standups".into(),
            FfiImportanceClass::Useful,
        )
        .expect("add_user_memory");
        close_store(h).expect("close_store");

        // 2. Restart, then resolve the scope DEK on a cold cache (evict
        //    the rehydrated key, then let an ingest call
        //    `ensure_scope_dek`). This models a key resolution that
        //    races ahead of the memory-loading path.
        let h2 = open_store(path.clone(), key_hex.clone()).expect("reopen_store");
        with_runtime(h2, |rt| {
            rt.store().evict_cached_scope_key(scope_id);
            Ok(())
        })
        .expect("evict cached scope key");
        ingest_message(
            h2,
            scope.clone(),
            "unrelated evidence after restart".into(),
            SourceKind::Manual,
            FfiImportanceClass::Useful,
        )
        .expect("ingest after restart");
        close_store(h2).expect("close_store");

        // 3. Restart again and read the memory back. The blob must still
        //    decrypt under the persisted DEK.
        let h3 = open_store(path, key_hex).expect("reopen_store again");
        let listed = get_user_memory(h3, scope).expect("get_user_memory after restart");
        assert_eq!(
            listed.len(),
            1,
            "memory written before the cold-cache DEK resolution must remain readable"
        );
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].summary, "prefers async standups");
        teardown(h3);
    }

    #[test]
    fn pin_and_unpin_round_trip_through_user_memory() {
        let (h, _dir) = fresh_store();
        let scope_uuid = uuid::Uuid::new_v4();
        let scope_str = scope_uuid.to_string();
        // Seed a memory object through the public write surface so the
        // pin / unpin round-trip operates on a real persisted row.
        let mem_id = add_user_memory(
            h,
            scope_str.clone(),
            "fact".into(),
            "Sara owns the rollout".into(),
            FfiImportanceClass::Useful,
        )
        .expect("seed memory object")
        .id
        .parse::<uuid::Uuid>()
        .expect("created id is a uuid");

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
    fn get_concept_graph_is_empty_for_fresh_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let before = metrics::snapshot().get_concept_graph_total;
        let view = get_concept_graph(h, scope).expect("get_concept_graph");
        assert!(view.nodes.is_empty());
        assert!(view.edges.is_empty());
        let after = metrics::snapshot().get_concept_graph_total;
        assert!(
            after > before,
            "get_concept_graph must increment its metrics counter \
             (before={before}, after={after})"
        );
        teardown(h);
    }

    #[test]
    fn get_concept_graph_projects_each_observation_as_a_candidate_node() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let ids = runtime::with_runtime(h, |rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let a = umo.add_observation(
                "fact",
                "Sara owns the rollout",
                memory_manager::SensitivityClass::Useful,
            );
            let b = umo.add_observation(
                "preference",
                "prefers dark mode",
                memory_manager::SensitivityClass::Useful,
            );
            Ok((a, b))
        })
        .expect("seed");

        let view = get_concept_graph(h, scope).expect("get_concept_graph");
        assert_eq!(view.nodes.len(), 2);
        assert!(view.edges.is_empty(), "no supersession pointers were set");
        for node in &view.nodes {
            assert_eq!(node.state, concept_graph::NodeState::Candidate);
        }
        // Node ids reuse the memory ids verbatim.
        let node_ids: std::collections::HashSet<String> = view
            .nodes
            .iter()
            .map(|n| n.id.as_uuid().to_string())
            .collect();
        assert!(node_ids.contains(&ids.0.to_string()));
        assert!(node_ids.contains(&ids.1.to_string()));
        teardown(h);
    }

    /// A decayed (archived) observation must surface in the graph with
    /// the non-live `Superseded` node state, so the concept-graph view
    /// reflects the decay state machine rather than disagreeing with
    /// it. This is the read-side counterpart to `run_decay_sweep`.
    #[test]
    fn get_concept_graph_reflects_decay_state() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        runtime::with_runtime(h, |rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let _ = umo.add_observation(
                "fact",
                "stale fact",
                memory_manager::SensitivityClass::Useful,
            );
            // Age the working set far past any TTL so the candidate
            // archives, then prove the projection dims it.
            let _ = umo.decay_sweep(chrono::Utc::now() + chrono::Duration::days(3650));
            Ok(())
        })
        .expect("seed + decay");

        let view = get_concept_graph(h, scope).expect("get_concept_graph");
        assert_eq!(view.nodes.len(), 1);
        assert_eq!(view.nodes[0].state, concept_graph::NodeState::Superseded);
        teardown(h);
    }

    #[test]
    fn get_concept_graph_is_empty_after_forget_scope() {
        let (h, _dir) = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        runtime::with_runtime(h, |rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let _ = umo.add_observation(
                "fact",
                "to be forgotten",
                memory_manager::SensitivityClass::Useful,
            );
            Ok(())
        })
        .expect("seed");
        assert_eq!(
            get_concept_graph(h, scope.clone())
                .expect("pre-forget")
                .nodes
                .len(),
            1
        );

        forget_scope(h, scope.clone()).expect("forget_scope");
        let view = get_concept_graph(h, scope).expect("post-forget");
        assert!(view.nodes.is_empty());
        assert!(view.edges.is_empty());
        teardown(h);
    }

    #[test]
    fn get_concept_graph_rejects_malformed_scope() {
        let (h, _dir) = fresh_store();
        let err = get_concept_graph(h, "not-a-uuid".into()).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
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
        assert_eq!(before_synth, after_synth,
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
                .query_row("SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1 AND scope_id = ?2",
                    rusqlite::params![PHRASE, scope.as_uuid().as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            assert_eq!(raw_term_count, 1,
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
                .query_row("SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1 AND scope_id = ?2",
                    rusqlite::params![PHRASE, scope.as_uuid().as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            assert_eq!(raw_term_count, 0,
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

    /// C10 integration regression: same crash-recovery contract as
    /// [`open_store_repurges_fts_for_persisted_tombstones`], but
    /// with a body that contains CJK Han / Hiragana / Katakana
    /// codepoints so the row is also written to
    /// `evidence_fts_cjk` (the trigram-tokenised companion
    /// schema v14). The Latin-only test
    /// above cannot exercise this code path because `unicode61`
    /// produces tokens for Latin but `script::contains_cjk_or_thai`
    /// returns false, so the row is never inserted into
    /// `evidence_fts_cjk` and the post-reopen purge of that table
    /// is a no-op for that fixture.
    ///
    /// This test pins the dual-table purge contract end-to-end
    /// through the FFI: both `evidence_fts` and
    /// `evidence_fts_cjk` rows must survive the tombstone-only
    /// pre-reopen state (pre-condition), and both must be empty
    /// after the next `open_store` runs the re-purge. Closes the
    /// coverage gap.
    #[test]
    fn open_store_repurges_evidence_fts_cjk_for_persisted_tombstones() {
        // The body intentionally contains a long CJK substring so
        // it is well above the FTS5 trigram tokeniser's
        // 3-codepoint floor and reliably matches as a substring
        // probe against `evidence_fts_cjk`.
        const CJK_BODY: &str = "今日の重要な会議の議事録";
        // Latin probe pulled separately because `unicode61` does
        // not produce tokens for CJK codepoints — querying for
        // the CJK substring through `evidence_fts` would always
        // return zero rows regardless of purge state, so we use
        // it only against `evidence_fts_cjk`.
        const CJK_PROBE: &str = "重要な会議";

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let scope_str = uuid::Uuid::new_v4().to_string();

        let h1 =
            open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open_store");

        let evidence_id = ingest_message(
            h1,
            scope_str.clone(),
            CJK_BODY.into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");
        assert!(!evidence_id.is_empty());

        // Sanity: the public query surface routes the CJK probe
        // through the dual-table union and finds the row.
        let hits =
            query(h1, scope_str.clone(), CJK_PROBE.into(), 10).expect("query pre-forget cjk");
        assert_eq!(
            hits.len(),
            1,
            "FTS5 dual-table union must surface the seeded CJK phrase before forgetting"
        );

        // Persist the scope tombstone *without* running the FTS
        // purge, modelling a crash between the tombstone write and
        // the `purge_fts_for_scope_in_tx` call.
        runtime::with_runtime(h1, |rt| {
            let scope = parse_scope_id(&scope_str)?;
            rt.store_mut()
                .record_forgotten_scope(scope)
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            Ok(())
        })
        .expect("seed cjk tombstone without FTS purge");

        // Pre-condition: BOTH FTS5 shadow tables must still contain
        // the row for the forgotten scope. This is the crash state
        // the re-purge has to repair, and the assertion that makes
        // the rest of the test meaningful (a no-op test that
        // happens to pass against an empty `evidence_fts_cjk`
        // would never catch a regression where the cjk-purge is
        // skipped).
        runtime::with_runtime(h1, |rt| {
            let scope = parse_scope_id(&scope_str)?;
            let scope_bytes = scope.as_uuid().as_bytes().to_vec();
            // Probe by scope_id alone (no `MATCH`) so the
            // unicode61-vs-trigram tokeniser asymmetry doesn't
            // leak into the assertion: `unicode61` would never
            // match a pure-CJK probe regardless of purge state,
            // so a MATCH-based count would conflate "row absent"
            // with "row present but tokeniser produced no tokens
            // for this query". A scope-only `COUNT(*)` directly
            // tests row presence in each shadow table.
            let unicode61_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts WHERE scope_id = ?1",
                    rusqlite::params![scope_bytes.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            let trigram_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts_cjk WHERE scope_id = ?1",
                    rusqlite::params![scope_bytes.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            // schema v15: the bigram shadow also
            // holds a row for CJK bodies (precomputed-bigram
            // recall lane for 2-codepoint queries). Pre-condition
            // sanity-checks that the re-purge has a non-empty
            // bigram shadow to actually clean up.
            let bigram_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts_bigram WHERE scope_id = ?1",
                    rusqlite::params![scope_bytes.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            assert_eq!(
                unicode61_count, 1,
                "pre-condition: evidence_fts must still hold the row for the tombstoned CJK \
                 scope so the test exercises the re-purge"
            );
            assert_eq!(
                trigram_count, 1,
                "pre-condition: evidence_fts_cjk must still hold the row for the tombstoned CJK \
                 scope so the test exercises the dual-table re-purge"
            );
            assert_eq!(
                bigram_count, 1,
                "pre-condition: evidence_fts_bigram must still hold the row for the tombstoned \
                 CJK scope so the test exercises the three-table re-purge introduced in \
                  / schema v15"
            );
            Ok(())
        })
        .expect("probe pre-reopen three-table fts");

        // Restart cycle. The next `open_store` is where the
        // dual-table re-purge runs.
        close_store(h1).expect("close_store");
        let h2 = open_store(path.to_string_lossy().into_owned(), key_hex).expect("re-open_store");

        // All THREE FTS5 shadow tables must be empty for the
        // forgotten scope after the re-purge. Asserting on the
        // bigram table is what's new here vs the dual-table
        // sibling test — the three-table atomic transaction
        // invariant / schema v15 is what
        // this regression guards.
        runtime::with_runtime(h2, |rt| {
            let scope = parse_scope_id(&scope_str)?;
            let scope_bytes = scope.as_uuid().as_bytes().to_vec();
            let unicode61_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts WHERE scope_id = ?1",
                    rusqlite::params![scope_bytes.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            let trigram_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts_cjk WHERE scope_id = ?1",
                    rusqlite::params![scope_bytes.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            let bigram_count: i64 = rt
                .store()
                .raw_conn()
                .query_row(
                    "SELECT COUNT(*) FROM evidence_fts_bigram WHERE scope_id = ?1",
                    rusqlite::params![scope_bytes.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| FfiError::Evidence {
                    message: e.to_string(),
                })?;
            assert_eq!(
                unicode61_count, 0,
                "open_store must re-purge evidence_fts rows for every persisted tombstone, \
                 including CJK-routed rows"
            );
            assert_eq!(
                trigram_count, 0,
                "open_store must re-purge evidence_fts_cjk rows for every persisted tombstone \
                 (regression guard)"
            );
            assert_eq!(
                bigram_count, 0,
                "open_store must re-purge evidence_fts_bigram rows for every persisted \
                 tombstone (schema v15 three-table atomicity invariant)"
            );
            Ok(())
        })
        .expect("probe post-reopen three-table fts");

        // Public query surface mirrors the raw dual-table probe.
        let hits_after =
            query(h2, scope_str.clone(), CJK_PROBE.into(), 10).expect("query post-reopen cjk");
        assert!(
            hits_after.is_empty(),
            "post-reopen CJK query must return no rows for the previously-tombstoned scope"
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
