//! `knowledge_ffi` — UniFFI surface for iOS / Android platform bindings.
//!
//! Per `ARCHITECTURE.md` §3 ("Platform integration plane") and
//! `PROPOSAL.md` §2 ("On-device runtime"), the knowledge substrate
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
//! # Status (Phase A wiring, this PR)
//!
//! * [`open_store`] / [`close_store`] / [`ingest_message`] / [`query`] /
//!   [`get_evidence`] / [`forget`] / [`encrypt`] / [`decrypt`] /
//!   [`generate_keypair`] are **wired through to the underlying internal
//!   crates** (`evidence_store`, `crypto`).
//! * Memory-manager and synthesis-pipeline calls still return
//!   [`FfiError::Unimplemented`] — they are unblocked by Phase B / C
//!   work (real ONNX embeddings + real on-device synthesis).
//!
//! All wired functions require a prior successful call to [`open_store`].
//! Calling any other function first returns
//! [`FfiError::Unavailable { subsystem: "evidence_store" }`].

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
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::Evidence`] if the underlying search fails.
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
        let mut out = Vec::with_capacity(hits.len());
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
            let fts_score = 1.0 - (rank as f64 / hits_len_for_score(limit) as f64).min(1.0);
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

fn hits_len_for_score(limit: u32) -> u32 {
    limit.max(1)
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
// These calls remain `Unimplemented` until Phase B / C wire the
// `memory_manager` crate through the FFI runtime.

/// Fetch the per-user memory bundle for `scope_id`.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] — the memory-manager surface
/// is not yet wired to the runtime; see Phase B.
pub fn get_user_memory(_scope_id: ScopeIdString) -> FfiResult<Vec<MemoryRecord>> {
    Err(FfiError::Unimplemented {
        method: "get_user_memory".into(),
    })
}

/// Mark a memory record as `Pinned` (decay-immune) by its id.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] — pending Phase B wiring.
pub fn pin(_id: String) -> FfiResult<()> {
    Err(FfiError::Unimplemented {
        method: "pin".into(),
    })
}

/// Lift a previously-applied pin so the row resumes ageing under the
/// decay state machine.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] — pending Phase B wiring.
pub fn unpin(_id: String) -> FfiResult<()> {
    Err(FfiError::Unimplemented {
        method: "unpin".into(),
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
/// the case of [`query`]). The bytes on disk are **not** wiped — that
/// requires per-scope SQLCipher rekeying and a redesign of the FTS5
/// secondary index, both tracked separately under
/// `crates/evidence_store/tests/forgetting_fts.rs`.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`open_store`] has not been called.
/// * [`FfiError::InvalidId`] if `id` is not a valid UUID.
/// * [`FfiError::NotFound`] if no evidence row has that id.
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
        rt.forget_scope(row.scope_id);
        Ok(())
    })
}

/// List memory records for a scope, optionally filtered by state.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] — pending Phase B wiring.
pub fn list_memories(
    _scope_id: ScopeIdString,
    _filter: MemoryFilter,
) -> FfiResult<Vec<MemoryRecord>> {
    Err(FfiError::Unimplemented {
        method: "list_memories".into(),
    })
}

/// Run a decay sweep over `scope_id`. Returns the count of rows
/// transitioned (Candidate → Reinforced, Reinforced → Decaying,
/// Decaying → Archived) by this sweep.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] — pending Phase B wiring.
pub fn run_decay_sweep(_scope_id: ScopeIdString) -> FfiResult<u32> {
    Err(FfiError::Unimplemented {
        method: "run_decay_sweep".into(),
    })
}

// ──────────────────────── Synthesis pipeline ───────────────────────

/// Fetch the channel-level synthesis memory for `scope_id`.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] — pending Phase C wiring.
pub fn get_channel_memory(_scope_id: ScopeIdString) -> FfiResult<Option<MemoryRecord>> {
    Err(FfiError::Unimplemented {
        method: "get_channel_memory".into(),
    })
}

/// Trigger synthesis on `scope_id` with the given trigger reason.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] — pending Phase C wiring.
pub fn trigger_synthesis(
    _scope_id: ScopeIdString,
    _trigger: SynthesisTrigger,
) -> FfiResult<String> {
    Err(FfiError::Unimplemented {
        method: "trigger_synthesis".into(),
    })
}

// ──────────────────────────── Crypto ────────────────────────────

/// Generate a fresh ML-DSA-65 (FIPS 204) signing keypair.
///
/// The substrate's canonical post-quantum signature primitive — see
/// `crypto::signer_backend::MlDsa65Signer` and `PHASES.md` Phase 7.
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
    fn scope_encrypt_key(&self, scope: ScopeId) -> FfiResult<crypto::AeadKey> {
        if self.is_scope_forgotten(scope) {
            return Err(FfiError::NotFound {
                kind: "scope".into(),
                id: scope.to_string(),
            });
        }
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
    fn unwired_surfaces_still_return_unimplemented() {
        let _g = test_lock();
        let _dir = fresh_store();
        let scope = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            get_user_memory(scope.clone()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            pin("00000000-0000-0000-0000-000000000000".into()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            unpin("00000000-0000-0000-0000-000000000000".into()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            list_memories(scope.clone(), MemoryFilter::default()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            run_decay_sweep(scope.clone()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            get_channel_memory(scope.clone()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            trigger_synthesis(scope, SynthesisTrigger::ManualUserAction).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
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
}
