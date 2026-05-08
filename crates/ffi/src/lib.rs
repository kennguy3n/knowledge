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
//! 1. **Evidence store** — `ingest_message`, `query`, `get_evidence`.
//! 2. **Memory manager** — `get_user_memory`, `pin`, `unpin`,
//!    `forget`, `list_memories`, `run_decay_sweep`.
//! 3. **Synthesis pipeline** — `get_channel_memory`,
//!    `trigger_synthesis`.
//! 4. **Crypto** — `generate_keypair`, `encrypt`, `decrypt`.
//!
//! The Phase 1 deliverable is the **interface skeleton**: types,
//! errors, function signatures, and round-trip tests for the wire
//! types. Real implementations land when the per-call orchestration
//! work in Phase 2 unblocks the consuming surfaces. Stubs return
//! `FfiError::Unimplemented` and are documented as such — callers
//! must check for that variant and degrade gracefully.

#![deny(missing_docs)]

pub mod error;
pub mod types;

pub use error::{FfiError, FfiResult};
pub use types::{
    EvidenceRecord, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord, MemoryState, QueryResult,
    ScopeIdString, SourceKind, SynthesisTrigger,
};

// ─────────────────────────── Evidence store ──────────────────────────

/// Ingest a message into the encrypted evidence plane.
///
/// `scope_id` is a UUID string identifying the scope (channel,
/// thread, profile, …). `body` is plaintext UTF-8 to encrypt. `source`
/// is the connector tag (`"slack"`, `"email"`, `"manual"`, …).
///
/// Returns the new evidence row's UUID as a string on success.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton —
/// platform consumers should wire this to the real
/// `evidence_store::EvidenceStore::write` once the Phase 2 service
/// boundary lands.
pub fn ingest_message(
    _scope_id: ScopeIdString,
    _body: String,
    _source: SourceKind,
) -> FfiResult<String> {
    Err(FfiError::Unimplemented {
        method: "ingest_message".into(),
    })
}

/// Run a hybrid (FTS + recency + semantic) query against a scope.
///
/// Returns up to `limit` rows ordered by descending hybrid score.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn query(
    _scope_id: ScopeIdString,
    _query_text: String,
    _limit: u32,
) -> FfiResult<Vec<QueryResult>> {
    Err(FfiError::Unimplemented {
        method: "query".into(),
    })
}

/// Fetch a single evidence row by id.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton or
/// [`FfiError::NotFound`] once wired.
pub fn get_evidence(_evidence_id: String) -> FfiResult<EvidenceRecord> {
    Err(FfiError::Unimplemented {
        method: "get_evidence".into(),
    })
}

// ───────────────────────── Memory manager ─────────────────────────

/// Fetch the per-user memory bundle for `scope_id`.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn get_user_memory(_scope_id: ScopeIdString) -> FfiResult<Vec<MemoryRecord>> {
    Err(FfiError::Unimplemented {
        method: "get_user_memory".into(),
    })
}

/// Mark a memory record as `Pinned` (decay-immune) by its id.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
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
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn unpin(_id: String) -> FfiResult<()> {
    Err(FfiError::Unimplemented {
        method: "unpin".into(),
    })
}

/// Force-archive a memory record (user-initiated forget).
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn forget(_id: String) -> FfiResult<()> {
    Err(FfiError::Unimplemented {
        method: "forget".into(),
    })
}

/// List memory records for a scope, optionally filtered by state.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
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
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
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
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn get_channel_memory(_scope_id: ScopeIdString) -> FfiResult<Option<MemoryRecord>> {
    Err(FfiError::Unimplemented {
        method: "get_channel_memory".into(),
    })
}

/// Trigger synthesis on `scope_id` with the given trigger reason.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn trigger_synthesis(
    _scope_id: ScopeIdString,
    _trigger: SynthesisTrigger,
) -> FfiResult<String> {
    Err(FfiError::Unimplemented {
        method: "trigger_synthesis".into(),
    })
}

// ──────────────────────────── Crypto ────────────────────────────

/// Generate a fresh ML-DSA-65 (Phase 7 baseline) signing keypair.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn generate_keypair() -> FfiResult<FfiKeypair> {
    Err(FfiError::Unimplemented {
        method: "generate_keypair".into(),
    })
}

/// Encrypt `plaintext` for `scope_id` using XChaCha20-Poly1305 and
/// the scope-derived AEAD key. Returns the encoded ciphertext envelope.
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn encrypt(_scope_id: ScopeIdString, _plaintext: Vec<u8>) -> FfiResult<Vec<u8>> {
    Err(FfiError::Unimplemented {
        method: "encrypt".into(),
    })
}

/// Inverse of [`encrypt`].
///
/// # Errors
///
/// Returns [`FfiError::Unimplemented`] in the Phase 1 skeleton.
pub fn decrypt(_scope_id: ScopeIdString, _ciphertext: Vec<u8>) -> FfiResult<Vec<u8>> {
    Err(FfiError::Unimplemented {
        method: "decrypt".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_message_returns_unimplemented_in_skeleton() {
        let err = ingest_message("scope".into(), "hi".into(), SourceKind::Manual).unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
    }

    #[test]
    fn query_returns_unimplemented_in_skeleton() {
        let err = query("scope".into(), "q".into(), 10).unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
    }

    #[test]
    fn get_evidence_returns_unimplemented_in_skeleton() {
        let err = get_evidence("id".into()).unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
    }

    #[test]
    fn get_user_memory_returns_unimplemented_in_skeleton() {
        let err = get_user_memory("scope".into()).unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
    }

    #[test]
    fn pin_unpin_forget_return_unimplemented_in_skeleton() {
        for f in [pin, unpin, forget] {
            let err = f("id".into()).unwrap_err();
            assert!(matches!(err, FfiError::Unimplemented { .. }));
        }
    }

    #[test]
    fn list_memories_returns_unimplemented_in_skeleton() {
        let err = list_memories(
            "scope".into(),
            MemoryFilter {
                state: Some(MemoryState::Reinforced),
                pinned_only: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
    }

    #[test]
    fn run_decay_sweep_returns_unimplemented_in_skeleton() {
        let err = run_decay_sweep("scope".into()).unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
    }

    #[test]
    fn synthesis_apis_return_unimplemented_in_skeleton() {
        let err = get_channel_memory("scope".into()).unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
        let err =
            trigger_synthesis("scope".into(), SynthesisTrigger::ManualUserAction).unwrap_err();
        assert!(matches!(err, FfiError::Unimplemented { .. }));
    }

    #[test]
    fn crypto_apis_return_unimplemented_in_skeleton() {
        assert!(matches!(
            generate_keypair().unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            encrypt("scope".into(), b"x".to_vec()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
        assert!(matches!(
            decrypt("scope".into(), b"x".to_vec()).unwrap_err(),
            FfiError::Unimplemented { .. }
        ));
    }
}
