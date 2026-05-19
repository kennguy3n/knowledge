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
//!   [`memory_manager::ChannelMemoryObject`] CRUD layer. The memory
//!   plane is in-memory only — persistence to the encrypted
//!   evidence plane is not yet wired.
//! * [`trigger_synthesis`] returns [`FfiError::Unavailable`] with
//!   `subsystem = "synthesis"` until the on-device SLM router is
//!   wired through this surface.
//!
//! All wired functions require a prior successful call to [`open_store`].
//! Calling any other function first returns
//! [`FfiError::Unavailable { subsystem: "evidence_store" }`].
//!
//! # Known simplifications
//!
//! These are deliberate to keep the unblocker PR small. Each one is
//! a clean follow-up:
//!
//! * **Cryptographic forgetting is session-scoped.** The
//!   [`forget`] tombstones live in an in-memory
//!   [`crypto::forgetting::DekRegistry`] that is dropped by
//!   [`close_store`]. A durable on-disk DEK registry lands with the
//!   broader forgetting work; until then, hosts that need durability
//!   across process restarts must layer their own persistence on
//!   top.
//! * **`forget` resolves a scope via an evidence id.** There is no
//!   way to forget a scope that has only ever been used via
//!   [`encrypt`] / [`decrypt`] without ingest. A `forget_scope` API
//!   is tracked as a follow-up.
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

pub mod error;
pub mod runtime;
pub mod types;

pub use error::{FfiError, FfiResult};
pub use runtime::{close_store, open_store};
pub use types::{
    EvidenceRecord, FfiImportanceClass, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord,
    MemoryState, QueryResult, ScopeIdString, SourceKind, SynthesisTrigger,
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
pub fn ingest_message(
    scope_id: ScopeIdString,
    body: String,
    source: SourceKind,
    importance: FfiImportanceClass,
) -> FfiResult<String> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
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
                Some(source_kind_tag(source)),
                ffi_importance_to_internal(importance),
            )
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        Ok(result.evidence_id.to_string())
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
pub fn query(
    scope_id: ScopeIdString,
    query_text: String,
    limit: u32,
) -> FfiResult<Vec<QueryResult>> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
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
pub fn escape_fts_query(input: String) -> String {
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
pub fn get_evidence(evidence_id: String) -> FfiResult<EvidenceRecord> {
    let id = parse_evidence_id(&evidence_id)?;
    with_runtime(|rt| {
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
pub fn get_user_memory(scope_id: ScopeIdString) -> FfiResult<Vec<MemoryRecord>> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
        if rt.is_scope_forgotten(scope) {
            return Ok(Vec::new());
        }
        let Some(umo) = rt.user_memory(scope) else {
            return Ok(Vec::new());
        };
        Ok(umo.objects.iter().map(memory_object_to_record).collect())
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
pub fn pin(id: String) -> FfiResult<()> {
    let uuid = parse_uuid(&id)?;
    with_runtime(|rt| {
        let owning_scope = locate_owning_scope(rt, &uuid).ok_or_else(|| FfiError::NotFound {
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
pub fn unpin(id: String) -> FfiResult<()> {
    let uuid = parse_uuid(&id)?;
    with_runtime(|rt| {
        let owning_scope = locate_owning_scope(rt, &uuid).ok_or_else(|| FfiError::NotFound {
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
pub fn forget(id: String) -> FfiResult<()> {
    let evidence_id = parse_evidence_id(&id)?;
    with_runtime(|rt| {
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
        let scope = row.scope_id;
        // 1. In-memory DEK destruction (immediate effect on this
        //    process). 2. Persist the tombstone so a process
        //    restart still rejects the scope. 3. Purge the FTS5 /
        //    embedding indexes so plaintext-derived secondary
        //    payloads cannot be recovered post-forget.
        rt.forget_scope(scope);
        rt.store_mut()
            .record_forgotten_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        rt.store_mut()
            .purge_fts_for_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        rt.store_mut()
            .purge_body_key_wraps_for_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        // Delete persisted memory blobs so forgotten-scope memory
        // state does not survive the next open_store.
        rt.store()
            .delete_memory_blobs_for_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        rt.user_memories.remove(&scope);
        rt.channel_memories.remove(&scope);
        Ok(())
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
pub fn forget_scope(scope_id: String) -> FfiResult<()> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
        rt.forget_scope(scope);
        rt.store_mut()
            .record_forgotten_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        rt.store_mut()
            .purge_fts_for_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        rt.store_mut()
            .purge_body_key_wraps_for_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        // Delete persisted memory blobs for the forgotten scope.
        rt.store()
            .delete_memory_blobs_for_scope(scope)
            .map_err(|e| FfiError::Evidence {
                message: e.to_string(),
            })?;
        rt.user_memories.remove(&scope);
        rt.channel_memories.remove(&scope);
        Ok(())
    })
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
pub fn list_memories(
    scope_id: ScopeIdString,
    filter: MemoryFilter,
) -> FfiResult<Vec<MemoryRecord>> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
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
            .list(mm_filter)
            .into_iter()
            .filter(|o| !require_pinned || o.pin_count > 0)
            .map(memory_object_to_record)
            .collect();
        Ok(out)
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
pub fn run_decay_sweep(scope_id: ScopeIdString) -> FfiResult<u32> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
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
        let count = (report.candidates_archived + report.superseded_archived) as u32;
        rt.flush_user_memory(scope)?;
        Ok(count)
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
pub fn get_channel_memory(scope_id: ScopeIdString) -> FfiResult<Option<MemoryRecord>> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
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
}

/// Trigger synthesis on `scope_id` with the given trigger reason.
///
/// # Status
///
/// The synthesis pipeline requires an on-device SLM (the
/// `inference_router` + a `llama-server` adapter or equivalent).
/// The FFI runtime does not yet hold an `InferenceRouter` handle,
/// so this call currently returns
/// [`FfiError::Unavailable`] with `subsystem = "synthesis"`. The
/// wiring lands together with the on-device SLM bring-up. The function signature and
/// validation behaviour (UUID parsing, forgotten-scope handling)
/// are stable; only the underlying call dispatch is deferred.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called,
///   or if the synthesis subsystem has not been wired through this
///   build (the current default).
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `scope_id` has been forgotten.
pub fn trigger_synthesis(scope_id: ScopeIdString, _trigger: SynthesisTrigger) -> FfiResult<String> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
        if rt.is_scope_forgotten(scope) {
            return Err(FfiError::NotFound {
                kind: "scope".into(),
                id: scope_id.clone(),
            });
        }
        // No `ChannelMemoryObject` allocation here — until a real
        // synthesizer runs, allocating one would attach observable
        // state to a call that never produces a recap. The
        // allocation moves into the synthesizer's success path
        // once the SLM router is wired through.
        Err(FfiError::Unavailable {
            subsystem: "synthesis".into(),
        })
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
pub fn generate_keypair() -> FfiResult<FfiKeypair> {
    let signer = MlDsa65Signer::generate();
    let encoded = signer.encode();
    Ok(FfiKeypair {
        algorithm: "ml-dsa-65".into(),
        public_key: <_ as AsRef<[u8]>>::as_ref(&encoded.verifying_key).to_vec(),
        private_key: <_ as AsRef<[u8]>>::as_ref(&encoded.signing_seed).to_vec(),
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
/// * [`FfiError::Crypto`] on AEAD or key-derivation failure.
/// * [`FfiError::NotFound`] if `scope_id` has been forgotten.
pub fn encrypt(scope_id: ScopeIdString, plaintext: Vec<u8>) -> FfiResult<Vec<u8>> {
    let scope = parse_scope_id(&scope_id)?;
    with_runtime(|rt| {
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
pub fn decrypt(scope_id: ScopeIdString, ciphertext: Vec<u8>) -> FfiResult<Vec<u8>> {
    let scope = parse_scope_id(&scope_id)?;
    if ciphertext.len() < AEAD_NONCE_LEN {
        return Err(FfiError::Crypto {
            message: "ciphertext envelope shorter than nonce prefix".into(),
        });
    }
    with_runtime(|rt| {
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
}

// ─────────────────────────── Internals ────────────────────────────

fn parse_scope_id(s: &str) -> FfiResult<ScopeId> {
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

fn source_kind_tag(source: SourceKind) -> &'static str {
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

    fn forget_scope(&mut self, scope: ScopeId) {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        let _ = forgetting::destroy_scope_dek(self.registry_mut(), registry_scope);
        // Delete the wrapped DEK from durable storage so the scope
        // key is truly unrecoverable even with the master key.
        let _ = self.store_mut().delete_scope_dek(scope);
    }

    fn is_scope_forgotten(&self, scope: ScopeId) -> bool {
        forgetting::is_scope_forgotten(self.registry(), forgetting::ScopeId(scope.as_uuid()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use tempfile::tempdir;

    // The FFI surface holds a process-global runtime singleton, so
    // tests that open / close it must be serialized. Each test that
    // touches the singleton starts by acquiring this mutex.
    fn test_lock() -> MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fresh_store() -> tempfile::TempDir {
        // Defensive: previous test could have panicked mid-flight,
        // leaving the singleton populated. `close_store` is
        // idempotent so this is safe.
        let _ = close_store();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        open_store(path.to_string_lossy().into_owned(), key_hex).expect("open_store");
        dir
    }

    fn teardown() {
        close_store().expect("close_store");
    }

    #[test]
    fn open_store_rejects_invalid_hex_master_key() {
        let _g = test_lock();
        let _ = close_store();
        let dir = tempdir().unwrap();
        let path = dir.path().join("evidence.db");
        let err = open_store(path.to_string_lossy().into_owned(), "not-hex".into()).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn open_store_rejects_wrong_length_master_key() {
        let _g = test_lock();
        let _ = close_store();
        let dir = tempdir().unwrap();
        let path = dir.path().join("evidence.db");
        let err = open_store(path.to_string_lossy().into_owned(), "ab".repeat(16)).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
    }

    #[test]
    fn ingest_then_query_then_get_then_forget_round_trips() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // Single-token phrase (no punctuation) so the FTS5 `unicode61`
        // tokenizer indexes it verbatim and `MATCH` does not need
        // any phrase-quoting / escape gymnastics.
        let phrase = "xyzzyffiroundtripphrase";
        let body = format!("Reminder: please file the {phrase} report by Friday.");

        let evidence_id = ingest_message(
            scope.clone(),
            body.clone(),
            SourceKind::Slack,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");
        assert!(!evidence_id.is_empty());

        let hits = query(scope.clone(), phrase.into(), 10).expect("query");
        assert_eq!(hits.len(), 1, "FTS5 should surface the ingested phrase");
        assert_eq!(hits[0].evidence_id, evidence_id);
        assert!(hits[0].snippet.contains(phrase));

        let record = get_evidence(evidence_id.clone()).expect("get_evidence");
        assert_eq!(record.body, body);
        assert_eq!(record.source, SourceKind::Slack);
        assert_eq!(record.scope_id, scope);

        forget(evidence_id.clone()).expect("forget");

        let hits_after = query(scope.clone(), phrase.into(), 10).expect("query after forget");
        assert!(
            hits_after.is_empty(),
            "post-forget query must not return rows"
        );

        match get_evidence(evidence_id.clone()) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "evidence"),
            other => panic!("expected NotFound after forget, got {other:?}"),
        }

        match ingest_message(
            scope.clone(),
            "second message".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        ) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "scope"),
            other => panic!("expected NotFound after forget, got {other:?}"),
        }
        teardown();
    }

    #[test]
    fn encrypt_decrypt_round_trips_for_scope() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let scope_id = parse_scope_id(&scope).unwrap();
        runtime::with_runtime(|rt| rt.ensure_scope_registered(scope_id)).expect("register");
        let plaintext = b"the quick brown fox".to_vec();
        let ct = encrypt(scope.clone(), plaintext.clone()).expect("encrypt");
        assert!(ct.len() > plaintext.len());
        let pt = decrypt(scope.clone(), ct).expect("decrypt");
        assert_eq!(pt, plaintext);
        teardown();
    }

    #[test]
    fn decrypt_rejects_short_envelope() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let err = decrypt(scope, vec![0u8; 4]).unwrap_err();
        assert!(matches!(err, FfiError::Crypto { .. }));
        teardown();
    }

    #[test]
    fn decrypt_rejects_cross_scope_ciphertext() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope_a = uuid::Uuid::new_v4().to_string();
        let scope_b = uuid::Uuid::new_v4().to_string();
        let scope_a_id = parse_scope_id(&scope_a).unwrap();
        let scope_b_id = parse_scope_id(&scope_b).unwrap();
        runtime::with_runtime(|rt| {
            rt.ensure_scope_registered(scope_a_id)?;
            rt.ensure_scope_registered(scope_b_id)
        })
        .expect("register");
        let ct = encrypt(scope_a, b"secret".to_vec()).expect("encrypt");
        let err = decrypt(scope_b, ct).unwrap_err();
        assert!(matches!(err, FfiError::Crypto { .. }));
        teardown();
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
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let records = get_user_memory(scope).expect("get_user_memory");
        assert!(records.is_empty());
        teardown();
    }

    #[test]
    fn list_memories_is_empty_for_fresh_scope() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let records = list_memories(scope.clone(), MemoryFilter::default()).expect("list_memories");
        assert!(records.is_empty());

        // Filtering by state on a fresh scope is also empty.
        let candidates = list_memories(
            scope,
            MemoryFilter {
                state: Some(MemoryState::Candidate),
                pinned_only: false,
            },
        )
        .expect("list_memories candidate filter");
        assert!(candidates.is_empty());
        teardown();
    }

    #[test]
    fn run_decay_sweep_is_zero_for_fresh_scope() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let n = run_decay_sweep(scope).expect("run_decay_sweep");
        assert_eq!(n, 0);
        teardown();
    }

    #[test]
    fn get_channel_memory_is_none_until_synthesis_runs() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let cm = get_channel_memory(scope).expect("get_channel_memory");
        assert!(cm.is_none());
        teardown();
    }

    #[test]
    fn trigger_synthesis_reports_unavailable_until_slm_is_wired() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let err = trigger_synthesis(scope, SynthesisTrigger::ManualUserAction).unwrap_err();
        assert!(
            matches!(err, FfiError::Unavailable { ref subsystem } if subsystem == "synthesis"),
            "expected Unavailable {{ subsystem: synthesis }}, got {err:?}"
        );
        teardown();
    }

    #[test]
    fn pin_and_unpin_round_trip_through_user_memory() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope_uuid = uuid::Uuid::new_v4();
        let scope_str = scope_uuid.to_string();
        // The pin / unpin surface needs an existing memory object;
        // there is no public FFI to seed one yet (observation
        // ingest through the FFI is not yet wired). Seed one
        // directly via the in-crate runtime hook so we still cover
        // the round-trip.
        let mem_id = runtime::with_runtime(|rt| {
            let scope = parse_scope_id(&scope_str)?;
            let umo = rt.user_memory_mut(scope);
            Ok(umo.add_observation(
                "fact",
                "Sara owns the rollout",
                memory_manager::SensitivityClass::Useful,
            ))
        })
        .expect("seed memory object");

        let records = get_user_memory(scope_str.clone()).expect("get_user_memory");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, mem_id.to_string());
        assert_eq!(records[0].state, MemoryState::Candidate);
        assert_eq!(records[0].summary, "Sara owns the rollout");

        pin(mem_id.to_string()).expect("pin");
        let pinned = get_user_memory(scope_str.clone()).expect("get_user_memory after pin");
        assert_eq!(pinned[0].state, MemoryState::Pinned);

        unpin(mem_id.to_string()).expect("unpin");
        let after_unpin = get_user_memory(scope_str).expect("get_user_memory after unpin");
        // pin_count back to 0 means the underlying state machine
        // controls the wire state again. The decay-state-machine
        // promotion in `pin()` lifted the object to Reinforced, so
        // the FFI mapping should now surface `Reinforced`.
        assert_eq!(after_unpin[0].state, MemoryState::Reinforced);
        teardown();
    }

    #[test]
    fn pin_unknown_id_reports_not_found() {
        let _g = test_lock();
        let _dir = fresh_store();
        let bogus = uuid::Uuid::new_v4().to_string();
        let err = pin(bogus).unwrap_err();
        assert!(
            matches!(err, FfiError::NotFound { ref kind, .. } if kind == "memory"),
            "expected NotFound {{ kind: memory }}, got {err:?}"
        );
        teardown();
    }

    #[test]
    fn pin_rejects_malformed_id() {
        let _g = test_lock();
        let _dir = fresh_store();
        let err = pin("not-a-uuid".into()).unwrap_err();
        assert!(matches!(err, FfiError::InvalidId { .. }));
        teardown();
    }

    #[test]
    fn get_user_memory_returns_empty_after_forget() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        let phrase = "memorymanagerforgetphrase";
        let evidence_id = ingest_message(
            scope.clone(),
            phrase.into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest");

        // Seed a memory object into the same scope so we can prove
        // forget elides it.
        runtime::with_runtime(|rt| {
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

        assert_eq!(get_user_memory(scope.clone()).expect("pre-forget").len(), 1);

        forget(evidence_id).expect("forget");
        assert!(get_user_memory(scope).expect("post-forget").is_empty());
        teardown();
    }

    #[test]
    fn list_memories_filters_by_state() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // Seed three candidate observations.
        runtime::with_runtime(|rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let _ = umo.add_observation("a", "one", memory_manager::SensitivityClass::Useful);
            let _ = umo.add_observation("b", "two", memory_manager::SensitivityClass::Useful);
            let _ = umo.add_observation("c", "three", memory_manager::SensitivityClass::Useful);
            Ok(())
        })
        .expect("seed");

        let all = list_memories(scope.clone(), MemoryFilter::default()).expect("list all");
        assert_eq!(all.len(), 3);

        let candidates = list_memories(
            scope.clone(),
            MemoryFilter {
                state: Some(MemoryState::Candidate),
                pinned_only: false,
            },
        )
        .expect("list candidates");
        assert_eq!(candidates.len(), 3);

        let reinforced = list_memories(
            scope,
            MemoryFilter {
                state: Some(MemoryState::Reinforced),
                pinned_only: false,
            },
        )
        .expect("list reinforced");
        assert!(reinforced.is_empty());
        teardown();
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
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        // Seed two observations, pin one of them.
        let pinned_id = runtime::with_runtime(|rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            let pinned =
                umo.add_observation("pinned", "kept", memory_manager::SensitivityClass::Useful);
            let _unpinned =
                umo.add_observation("loose", "decays", memory_manager::SensitivityClass::Useful);
            Ok(pinned)
        })
        .expect("seed");
        pin(pinned_id.to_string()).expect("pin");

        let only_pinned = list_memories(
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

        teardown();
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
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();

        // Seed one evidence row (so `forget` has a row to resolve to
        // a scope) and one memory object in the same scope.
        let evidence_id = ingest_message(
            scope.clone(),
            "pin-after-forget-seed-body".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest");
        let mem_id = runtime::with_runtime(|rt| {
            let s = parse_scope_id(&scope)?;
            let umo = rt.user_memory_mut(s);
            Ok(umo.add_observation(
                "pinnable",
                "cache before forget",
                memory_manager::SensitivityClass::Useful,
            ))
        })
        .expect("seed memory");

        forget(evidence_id).expect("forget");

        // Pin must now return NotFound { kind: "memory" } — the same
        // shape the read surfaces present for the forgotten scope.
        let pin_err = pin(mem_id.to_string()).unwrap_err();
        assert!(
            matches!(pin_err, FfiError::NotFound { ref kind, .. } if kind == "memory"),
            "pin after forget must return NotFound {{ kind: memory }}, got {pin_err:?}"
        );

        // Same contract for unpin.
        let unpin_err = unpin(mem_id.to_string()).unwrap_err();
        assert!(
            matches!(unpin_err, FfiError::NotFound { ref kind, .. } if kind == "memory"),
            "unpin after forget must return NotFound {{ kind: memory }}, got {unpin_err:?}"
        );

        teardown();
    }

    /// Regression for the design follow-up: `get_user_memory` and
    /// `list_memories` must not lazily allocate a `UserMemoryObject`
    /// for scopes they observe but never mutate. A read for an
    /// unknown scope returns an empty bundle and leaves the
    /// per-scope `user_memories` map at its previous size.
    #[test]
    fn read_paths_do_not_allocate_user_memory_for_unknown_scope() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();

        // Snapshot the map size before any read.
        let before = runtime::with_runtime(|rt| Ok(rt.user_memories.len())).expect("len before");

        let bundle = get_user_memory(scope.clone()).expect("get_user_memory");
        assert!(bundle.is_empty());
        let listed = list_memories(scope, MemoryFilter::default()).expect("list_memories");
        assert!(listed.is_empty());

        let after = runtime::with_runtime(|rt| Ok(rt.user_memories.len())).expect("len after");
        assert_eq!(
            before, after,
            "read paths must not allocate per-scope user_memory entries"
        );
        teardown();
    }

    /// Regression for the design follow-up: `trigger_synthesis` must
    /// not allocate a `ChannelMemoryObject` when returning
    /// `Unavailable`. Allocating one attaches observable state to a
    /// call that never produces a recap.
    #[test]
    fn trigger_synthesis_unavailable_does_not_allocate_channel_memory() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();

        let before = runtime::with_runtime(|rt| Ok(rt.channel_memories.len())).expect("len before");

        match trigger_synthesis(scope, SynthesisTrigger::ManualUserAction) {
            Err(FfiError::Unavailable { subsystem }) => assert_eq!(subsystem, "synthesis"),
            other => panic!("expected Unavailable {{ subsystem: synthesis }}, got {other:?}"),
        }

        let after = runtime::with_runtime(|rt| Ok(rt.channel_memories.len())).expect("len after");
        assert_eq!(
            before, after,
            "trigger_synthesis must not allocate channel memory when returning Unavailable"
        );
        teardown();
    }

    #[test]
    fn calls_before_open_store_report_unavailable() {
        let _g = test_lock();
        // Belt-and-suspenders: explicit close in case a previous test
        // left state behind (we use a process-global singleton).
        let _ = close_store();
        let err = ingest_message(
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
        let _g = test_lock();
        let _ = close_store();

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let scope = uuid::Uuid::new_v4().to_string();

        open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open_store");

        let evidence_id = ingest_message(
            scope.clone(),
            "the persistent forgetting test body".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");
        forget(evidence_id).expect("forget");

        // Round-trip the singleton. The in-memory `DekRegistry` is
        // dropped here; the next `open_store` must rebuild it from
        // the persisted `forgotten_scopes` table.
        close_store().expect("close_store");
        open_store(path.to_string_lossy().into_owned(), key_hex).expect("re-open_store");

        // The scope must still be rejected. We probe via
        // `ingest_message` because that's the canonical
        // `is_scope_forgotten` short-circuit path that hosts hit
        // first after a restart.
        match ingest_message(
            scope,
            "second message after restart".into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        ) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "scope"),
            other => panic!("expected NotFound {{ kind: \"scope\" }} after restart, got {other:?}"),
        }
        teardown();
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

        let _g = test_lock();
        let _ = close_store();

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let scope_str = uuid::Uuid::new_v4().to_string();

        open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open_store");

        let evidence_id = ingest_message(
            scope_str.clone(),
            PHRASE.into(),
            SourceKind::Manual,
            FfiImportanceClass::Important,
        )
        .expect("ingest_message");
        assert!(!evidence_id.is_empty());

        // Sanity: FTS5 surfaces the phrase before any forgetting.
        let hits = query(scope_str.clone(), PHRASE.into(), 10).expect("query pre-forget");
        assert_eq!(hits.len(), 1, "FTS5 must surface the seeded phrase");

        // Simulate the crash window: persist the tombstone *without*
        // running `purge_fts_for_scope`. The public `forget()` would
        // do both — we reach into the store directly to model a crash
        // between steps 2 and 3.
        runtime::with_runtime(|rt| {
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
        runtime::with_runtime(|rt| {
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
        close_store().expect("close_store");
        open_store(path.to_string_lossy().into_owned(), key_hex).expect("re-open_store");

        // After the re-purge, the raw FTS5 shadow tables must
        // contain no rows for the forgotten scope. We probe the raw
        // table directly so a future `search_fts` short-circuit on
        // the forgotten scope cannot hide a missing on-disk
        // delete.
        runtime::with_runtime(|rt| {
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
        let hits_after = query(scope_str.clone(), PHRASE.into(), 10).expect("query post-reopen");
        assert!(
            hits_after.is_empty(),
            "post-reopen query must return no rows for the previously-tombstoned scope"
        );

        teardown();
    }

    /// C10 integration test: memory state survives an open/close/open
    /// cycle via the encrypted `memory_objects` table.
    #[test]
    fn memory_persists_across_open_close_open() {
        let _g = test_lock();
        let _ = close_store();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);

        // First session: open, add a memory object, pin it, close.
        open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open 1");
        let scope_uuid = uuid::Uuid::new_v4();
        let scope_str = scope_uuid.to_string();
        let scope = parse_scope_id(&scope_str).unwrap();

        // Ensure scope is registered so the DEK exists.
        runtime::with_runtime(|rt| {
            rt.ensure_scope_registered(scope)?;
            Ok(())
        })
        .expect("ensure_scope_registered");

        // Insert a memory object and pin it.
        runtime::with_runtime(|rt| {
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
        let before_close =
            list_memories(scope_str.clone(), MemoryFilter::default()).expect("list before close");
        assert_eq!(before_close.len(), 1, "one memory object before close");
        // Check pin count via the internal MemoryObject (not exposed
        // on the FFI MemoryRecord wire type).
        runtime::with_runtime(|rt| {
            let umo = rt.user_memory(scope).expect("scope must exist");
            assert_eq!(umo.objects[0].pin_count, 1, "pinned once before close");
            Ok(())
        })
        .expect("pin count check");

        close_store().expect("close 1");

        // Second session: re-open with same key.
        open_store(path.to_string_lossy().into_owned(), key_hex).expect("open 2");

        // Memory object must be rehydrated from disk.
        let after_reopen =
            list_memories(scope_str.clone(), MemoryFilter::default()).expect("list after reopen");
        assert_eq!(
            after_reopen.len(),
            1,
            "memory object must survive close/open cycle"
        );
        runtime::with_runtime(|rt| {
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

        teardown();
    }

    /// C10 integration test: forget_scope deletes persisted memory
    /// blobs so they do not reappear on reopen.
    #[test]
    fn forget_scope_deletes_persisted_memory() {
        let _g = test_lock();
        let _ = close_store();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);

        open_store(path.to_string_lossy().into_owned(), key_hex.clone()).expect("open 1");
        let scope_uuid = uuid::Uuid::new_v4();
        let scope_str = scope_uuid.to_string();
        let scope = parse_scope_id(&scope_str).unwrap();

        runtime::with_runtime(|rt| {
            rt.ensure_scope_registered(scope)?;
            Ok(())
        })
        .expect("ensure_scope_registered");

        // Insert a memory object and flush it.
        runtime::with_runtime(|rt| {
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
        forget_scope(scope_str.clone()).expect("forget_scope");

        close_store().expect("close 1");

        // Reopen — memories for the forgotten scope must NOT reappear.
        open_store(path.to_string_lossy().into_owned(), key_hex).expect("open 2");

        let after = list_memories(scope_str.clone(), MemoryFilter::default())
            .expect("list after forget + reopen");
        assert!(
            after.is_empty(),
            "forgotten-scope memories must not reappear after reopen"
        );

        teardown();
    }
}
