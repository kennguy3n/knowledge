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
//! # Status (Phase A.5 wiring, this PR)
//!
//! * [`open_store`] / [`close_store`] / [`ingest_message`] / [`query`] /
//!   [`get_evidence`] / [`forget`] / [`encrypt`] / [`decrypt`] /
//!   [`generate_keypair`] are **wired through to the underlying internal
//!   crates** (`evidence_store`, `crypto`).
//! * [`get_user_memory`] / [`pin`] / [`unpin`] / [`list_memories`] /
//!   [`run_decay_sweep`] / [`get_channel_memory`] are wired through to
//!   the in-process [`memory_manager::UserMemoryObject`] /
//!   [`memory_manager::ChannelMemoryObject`] CRUD layer. The memory
//!   plane is in-memory only in Phase A.5 — persistence to the
//!   encrypted evidence plane lands with Phase 2.
//! * [`trigger_synthesis`] returns [`FfiError::Unavailable`] with
//!   `subsystem = "synthesis"` until the on-device SLM router is
//!   wired through this surface in Phase C.
//!
//! All wired functions require a prior successful call to [`open_store`].
//! Calling any other function first returns
//! [`FfiError::Unavailable { subsystem: "evidence_store" }`].
//!
//! # Known Phase A simplifications
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
//!   is tracked for the post-Phase-A surface.
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
//!   `recency_score` and `vector_score` stay at `0.0` until
//!   Phase B.

#![deny(missing_docs)]

pub mod error;
pub mod runtime;
pub mod types;

pub use error::{FfiError, FfiResult};
pub use runtime::{close_store, open_store};
pub use types::{
    EvidenceRecord, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord, MemoryState, QueryResult,
    ScopeIdString, SourceKind, SynthesisTrigger,
};

use crypto::{
    decrypt_aead, derive_key, encrypt_aead, forgetting, signer_backend::MlDsa65Signer, AeadNonce,
    AEAD_NONCE_LEN,
};
use evidence_store::{EvidenceId, ImportanceClass, ScopeId};
use rand::RngCore;

use runtime::with_runtime;

// ─────────────────────────── Evidence store ──────────────────────────

/// Ingest a message into the encrypted evidence plane.
///
/// `scope_id` is a UUID string identifying the scope (channel,
/// thread, profile, …). `body` is plaintext UTF-8 to encrypt. `source`
/// is the connector tag (`"Slack"`, `"Email"`, `"Manual"`, …).
///
/// Returns the new evidence row's UUID as a string on success.
///
/// # Phase A simplification
///
/// Every ingest is currently routed at `ImportanceClass::Important`.
/// The underlying [`EvidenceStore`](evidence_store::EvidenceStore)
/// supports `Useful` and `Noise` (the latter goes to the ring
/// buffer), but exposing that knob through the FFI is tracked as a
/// follow-up — see the crate-level docs.
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
                ImportanceClass::Important,
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
/// and `vector_score` stay at `0.0` until Phase B wires the
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
            // vector components at 0.0 until Phase B (real ONNX
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
// encrypted evidence plane is Phase 2 work; the contract surfaced
// here is stable across the upcoming persistence work.

/// Fetch the per-user memory bundle for `scope_id`.
///
/// Returns the per-scope [`UserMemoryObject`](memory_manager::UserMemoryObject)'s
/// owned memory objects as wire-flat [`MemoryRecord`]s, ordered by
/// insertion. Returns an empty vector if the scope has been
/// cryptographically forgotten via [`forget`].
///
/// # Phase A.5 simplification
///
/// The user memory layer is in-process only — `open_store` /
/// `close_store` cycles drop it. Persistence to the encrypted
/// evidence plane is tracked under Phase 2.
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
        let umo = rt.user_memory_mut(scope);
        Ok(umo.objects.iter().map(memory_object_to_record).collect())
    })
}

/// Mark a memory record as `Pinned` (decay-immune) by its id.
///
/// The runtime walks every per-scope [`UserMemoryObject`] to find
/// the owning scope; the memory layer keeps an in-process index so
/// this is `O(scopes * objects-per-scope)` in the worst case, which
/// is fine for the Phase A.5 working set sizes.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `id` is not a valid UUID.
/// * [`FfiError::NotFound`] if no memory object has that id in any
///   open scope.
/// * [`FfiError::Memory`] if the underlying state-machine transition
///   rejects the pin (e.g. the object is in a terminal state).
pub fn pin(id: String) -> FfiResult<()> {
    let uuid = parse_uuid(&id)?;
    with_runtime(|rt| {
        for umo in rt.user_memories.values_mut() {
            if umo.read(&uuid).is_some() {
                return umo.pin(&uuid).map_err(|e| FfiError::Memory {
                    message: e.to_string(),
                });
            }
        }
        Err(FfiError::NotFound {
            kind: "memory".into(),
            id: id.clone(),
        })
    })
}

/// Lift a previously-applied pin so the row resumes ageing under the
/// decay state machine.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `id` is not a valid UUID.
/// * [`FfiError::NotFound`] if no memory object has that id in any
///   open scope.
/// * [`FfiError::Memory`] if the underlying state-machine rejects.
pub fn unpin(id: String) -> FfiResult<()> {
    let uuid = parse_uuid(&id)?;
    with_runtime(|rt| {
        for umo in rt.user_memories.values_mut() {
            if umo.read(&uuid).is_some() {
                return umo.unpin(&uuid).map_err(|e| FfiError::Memory {
                    message: e.to_string(),
                });
            }
        }
        Err(FfiError::NotFound {
            kind: "memory".into(),
            id: id.clone(),
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
/// # Durability — Phase A.5 semantics
///
/// As of Phase A.5 (Gap 4) the tombstone is **persisted** to the
/// `forgotten_scopes` table on the encrypted evidence database, and
/// the FTS5 / embedding secondary indexes are purged inline. On the
/// next [`open_store`], the runtime replays every persisted
/// tombstone into a fresh in-memory `DekRegistry`, so subsequent
/// calls for the same scope continue to short-circuit with
/// [`FfiError::NotFound`].
///
/// The encrypted **bodies** in `evidence` / `body_store` are
/// intentionally not deleted — the append-only trigger on
/// `evidence` forbids it, and without the per-scope DEK the
/// ciphertexts are unrecoverable anyway. Hosts that need to drop
/// the physical bytes must perform a VACUUM-style rebuild at a
/// higher layer.
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
        let umo = rt.user_memory_mut(scope);
        let mm_filter = ffi_filter_to_memory_filter(&filter, scope);
        let pinned_only = filter.pinned_only;
        let out: Vec<MemoryRecord> = umo
            .list(mm_filter)
            .into_iter()
            .filter(|o| !pinned_only || o.pin_count > 0)
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
        let umo = rt.user_memory_mut(scope);
        let report = umo.decay_sweep(chrono::Utc::now());
        Ok((report.candidates_archived + report.superseded_archived) as u32)
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
/// # Phase A.5 status
///
/// The synthesis pipeline requires an on-device SLM (the
/// `inference_router` + a `llama-server` adapter or equivalent).
/// The FFI runtime does not yet hold an `InferenceRouter` handle,
/// so this call currently returns
/// [`FfiError::Unavailable`] with `subsystem = "synthesis"`. The
/// wiring lands together with the on-device SLM bring-up — see
/// `docs/internal/PHASES.md` Phase C. The function signature and
/// validation behaviour (UUID parsing, forgotten-scope handling)
/// are stable; only the underlying call dispatch is deferred.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called,
///   or if the synthesis subsystem has not been wired through this
///   build (the Phase A.5 default).
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
        // Touch the channel memory so future calls observe a stable
        // identity even before a real synthesizer runs.
        let _ = rt.channel_memory_mut(scope);
        Err(FfiError::Unavailable {
            subsystem: "synthesis".into(),
        })
    })
}

// ──────────────────────────── Crypto ────────────────────────────

/// Generate a fresh ML-DSA-65 (FIPS 204) signing keypair.
///
/// The substrate's canonical post-quantum signature primitive — see
/// `crypto::signer_backend::MlDsa65Signer` and `docs/internal/PHASES.md` Phase 7.
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
        private_key: <_ as AsRef<[u8]>>::as_ref(&encoded.signing_key).to_vec(),
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
        let plain = decrypt_aead(&key, &nonce, body, &aad).map_err(|e| FfiError::Crypto {
            message: e.to_string(),
        })?;
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
    /// Derive the scope-specific AEAD key used by [`encrypt`] /
    /// [`decrypt`]. Callers **must** check
    /// [`Self::is_scope_forgotten`] before invoking this — the
    /// forgotten-scope short-circuit lives at the public-function
    /// layer so the error mapping (`NotFound { kind: "scope" }`)
    /// stays consistent across the surface.
    fn scope_encrypt_key(&self, scope: ScopeId) -> FfiResult<crypto::AeadKey> {
        let label = format!("scope:{}:ffi-encrypt:v1", scope.as_uuid());
        derive_key(self.master_key(), label.as_bytes()).map_err(|e| FfiError::Crypto {
            message: e.to_string(),
        })
    }

    fn ensure_scope_registered(&mut self, scope: ScopeId) -> FfiResult<()> {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        if self.registry().get_scope_dek(registry_scope).is_some() {
            return Ok(());
        }
        let label = format!("scope:{}:dek:v1", scope.as_uuid());
        let key =
            derive_key(self.master_key(), label.as_bytes()).map_err(|e| FfiError::Crypto {
                message: e.to_string(),
            })?;
        let dek = forgetting::ScopeDek::new(registry_scope, forgetting::EpochId::zero(), key);
        self.registry_mut().insert_scope_dek(dek);
        Ok(())
    }

    fn forget_scope(&mut self, scope: ScopeId) {
        let registry_scope = forgetting::ScopeId(scope.as_uuid());
        let _ = forgetting::destroy_scope_dek(self.registry_mut(), registry_scope);
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

        let evidence_id =
            ingest_message(scope.clone(), body.clone(), SourceKind::Slack).expect("ingest_message");
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

        match ingest_message(scope.clone(), "second message".into(), SourceKind::Manual) {
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
        let ct = encrypt(scope_a, b"secret".to_vec()).expect("encrypt");
        let err = decrypt(scope_b, ct).unwrap_err();
        assert!(matches!(err, FfiError::Crypto { .. }));
        teardown();
    }

    #[test]
    fn generate_keypair_returns_ml_dsa_65() {
        let kp = generate_keypair().expect("generate_keypair");
        assert_eq!(kp.algorithm, "ml-dsa-65");
        // ML-DSA-65 verifying key is 1952 bytes, signing key is 4032
        // bytes (FIPS 204 §4.2 / §4.3). We assert lower bounds rather
        // than exact equality so an upstream `ml-dsa` minor-version
        // bump that changes the wire encoding does not crater this
        // test gratuitously.
        assert!(
            kp.public_key.len() >= 1500,
            "ml-dsa-65 verifying key suspiciously small: {}",
            kp.public_key.len()
        );
        assert!(
            kp.private_key.len() >= 3500,
            "ml-dsa-65 signing key suspiciously small: {}",
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
        // there is no public FFI to seed one in Phase A.5 (Phase 2
        // adds observation ingest through the FFI). Seed one
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
        let evidence_id =
            ingest_message(scope.clone(), phrase.into(), SourceKind::Manual).expect("ingest");

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
        )
        .unwrap_err();
        assert!(
            matches!(err, FfiError::Unavailable { ref subsystem } if subsystem == "evidence_store")
        );
    }

    /// Phase A.5 (Gap 4) — durable cryptographic-forgetting tombstones
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
        ) {
            Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "scope"),
            other => panic!("expected NotFound {{ kind: \"scope\" }} after restart, got {other:?}"),
        }
        teardown();
    }
}
