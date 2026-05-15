//! `knowledge_napi` — N-API addon skeleton for macOS / Windows
//! Electron desktop integration.
//!
//! Per `ARCHITECTURE.md` §3 ("Platform integration plane") and
//! `docs/DESIGN.md` §2 ("On-device runtime"), the desktop bridge ships
//! as a Node.js native addon that mirrors the iOS / Android UniFFI
//! surface (see the sibling `ffi` crate) but speaks JSON-over-N-API
//! instead of typed object handles.
//!
//! The Phase 1 deliverable is the **wire skeleton**:
//!
//! 1. JSON-shaped wrapper types (re-exported from `ffi::types` with
//!    a desktop-only [`InitConfig`] added).
//! 2. Function signatures matching the contract in `crate::ffi` —
//!    every call takes `serde_json::Value` arguments (because that
//!    is exactly how `napi-derive` will serialize the JS-side object
//!    arguments) and returns `serde_json::Value` on success.
//! 3. A round-trippable [`NapiError`] mapped from [`ffi::FfiError`]
//!    so the Electron host gets a stable JSON envelope.
//!
//! Real `#[napi]` proc-macros land when `napi` and `napi-derive` are
//! pinned. Until then this crate compiles as a regular Rust library
//! and is fully unit-testable from the workspace.

#![deny(missing_docs)]

pub mod error;
pub mod types;

pub use error::{NapiError, NapiResult};
pub use ffi::{
    EvidenceRecord, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord, MemoryState, QueryResult,
    ScopeIdString, SourceKind, SynthesisTrigger,
};
pub use types::{IngestRequest, InitConfig, QueryRequest};

/// Initialize the Rust core with a JSON config blob. Hosts call this
/// once during Electron's `app.whenReady` hook.
///
/// # Errors
///
/// Returns [`NapiError::InvalidConfig`] if `config_json` is not valid
/// JSON or does not match [`InitConfig`].
pub fn init(config_json: &str) -> NapiResult<()> {
    let _cfg: InitConfig =
        serde_json::from_str(config_json).map_err(|e| NapiError::InvalidConfig {
            message: e.to_string(),
        })?;
    Ok(())
}

/// Open the SQLCipher-backed evidence store at `path` using the
/// 32-byte master key encoded as `master_key_hex` (64 lower-case hex
/// chars). Mirrors [`ffi::open_store`].
///
/// # Errors
///
/// Forwards [`ffi::open_store`] errors as [`NapiError`].
pub fn open_store(path: String, master_key_hex: String) -> NapiResult<()> {
    ffi::open_store(path, master_key_hex).map_err(NapiError::from)
}

/// Drop the open evidence store. Mirrors [`ffi::close_store`].
///
/// # Errors
///
/// Forwards [`ffi::close_store`] errors as [`NapiError`].
pub fn close_store() -> NapiResult<()> {
    ffi::close_store().map_err(NapiError::from)
}

/// Ingest a chat / document message through the encrypted evidence
/// plane. Mirrors [`ffi::ingest_message`].
///
/// # Errors
///
/// Returns [`NapiError`] if the request body is malformed or the
/// underlying FFI surface returns an error.
pub fn ingest_message(req: IngestRequest) -> NapiResult<serde_json::Value> {
    ffi::ingest_message(req.scope_id, req.body, req.source)
        .map(|id| serde_json::json!({ "evidence_id": id }))
        .map_err(NapiError::from)
}

/// Hybrid query against a scope. Mirrors [`ffi::query`].
///
/// # Errors
///
/// Forwards [`ffi::query`] errors as [`NapiError`].
pub fn query(req: QueryRequest) -> NapiResult<Vec<QueryResult>> {
    ffi::query(req.scope_id, req.query_text, req.limit).map_err(NapiError::from)
}

/// Fetch a single evidence row. Mirrors [`ffi::get_evidence`].
///
/// # Errors
///
/// Forwards [`ffi::get_evidence`] errors as [`NapiError`].
pub fn get_evidence(evidence_id: String) -> NapiResult<EvidenceRecord> {
    ffi::get_evidence(evidence_id).map_err(NapiError::from)
}

/// Fetch the per-user memory bundle for a scope.
///
/// # Errors
///
/// Forwards [`ffi::get_user_memory`] errors as [`NapiError`].
pub fn get_user_memory(scope_id: ScopeIdString) -> NapiResult<Vec<MemoryRecord>> {
    ffi::get_user_memory(scope_id).map_err(NapiError::from)
}

/// Mark a memory record as `Pinned`.
///
/// # Errors
///
/// Forwards [`ffi::pin`] errors as [`NapiError`].
pub fn pin(id: String) -> NapiResult<()> {
    ffi::pin(id).map_err(NapiError::from)
}

/// Lift a previously-applied pin so the row resumes ageing.
///
/// # Errors
///
/// Forwards [`ffi::unpin`] errors as [`NapiError`].
pub fn unpin(id: String) -> NapiResult<()> {
    ffi::unpin(id).map_err(NapiError::from)
}

/// Force-archive a memory record (user-initiated forget).
///
/// # Errors
///
/// Forwards [`ffi::forget`] errors as [`NapiError`].
pub fn forget(id: String) -> NapiResult<()> {
    ffi::forget(id).map_err(NapiError::from)
}

/// List memory records for a scope, optionally filtered.
///
/// # Errors
///
/// Forwards [`ffi::list_memories`] errors as [`NapiError`].
pub fn list_memories(
    scope_id: ScopeIdString,
    filter: MemoryFilter,
) -> NapiResult<Vec<MemoryRecord>> {
    ffi::list_memories(scope_id, filter).map_err(NapiError::from)
}

/// Fetch the channel-level synthesis memory for a scope.
///
/// # Errors
///
/// Forwards [`ffi::get_channel_memory`] errors as [`NapiError`].
pub fn get_channel_memory(scope_id: ScopeIdString) -> NapiResult<Option<MemoryRecord>> {
    ffi::get_channel_memory(scope_id).map_err(NapiError::from)
}

/// Trigger synthesis for a scope.
///
/// # Errors
///
/// Forwards [`ffi::trigger_synthesis`] errors as [`NapiError`].
pub fn trigger_synthesis(scope_id: ScopeIdString, trigger: SynthesisTrigger) -> NapiResult<String> {
    ffi::trigger_synthesis(scope_id, trigger).map_err(NapiError::from)
}

/// Generate a fresh signing keypair (post-quantum baseline).
///
/// # Errors
///
/// Forwards [`ffi::generate_keypair`] errors as [`NapiError`].
pub fn generate_keypair() -> NapiResult<FfiKeypair> {
    ffi::generate_keypair().map_err(NapiError::from)
}

/// Encrypt `plaintext` for `scope_id` using the scope-derived AEAD
/// key. Returns the ciphertext envelope as a base64 string suitable
/// for transport over JSON.
///
/// # Errors
///
/// Forwards [`ffi::encrypt`] errors as [`NapiError`].
pub fn encrypt(scope_id: ScopeIdString, plaintext_b64: String) -> NapiResult<String> {
    let plaintext = decode_b64(&plaintext_b64)?;
    let cipher = ffi::encrypt(scope_id, plaintext).map_err(NapiError::from)?;
    Ok(encode_b64(&cipher))
}

/// Inverse of [`encrypt`].
///
/// # Errors
///
/// Forwards [`ffi::decrypt`] errors as [`NapiError`].
pub fn decrypt(scope_id: ScopeIdString, ciphertext_b64: String) -> NapiResult<String> {
    let cipher = decode_b64(&ciphertext_b64)?;
    let plain = ffi::decrypt(scope_id, cipher).map_err(NapiError::from)?;
    Ok(encode_b64(&plain))
}

const B64_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let v = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(B64_ALPHABET[((v >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[((v >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((v >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(v & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn decode_b64(s: &str) -> NapiResult<Vec<u8>> {
    let s = s.as_bytes();
    if s.len() % 4 != 0 {
        return Err(NapiError::InvalidArgument {
            message: "base64 input length must be a multiple of 4".into(),
        });
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.chunks(4) {
        let mut v = 0u32;
        let mut pad = 0;
        for &c in chunk {
            v <<= 6;
            if c == b'=' {
                pad += 1;
                continue;
            }
            let idx =
                B64_ALPHABET
                    .iter()
                    .position(|&x| x == c)
                    .ok_or(NapiError::InvalidArgument {
                        message: "invalid base64 character".into(),
                    })?;
            v |= idx as u32;
        }
        out.push(((v >> 16) & 0xFF) as u8);
        if pad < 2 {
            out.push(((v >> 8) & 0xFF) as u8);
        }
        if pad < 1 {
            out.push((v & 0xFF) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_accepts_valid_config() {
        let cfg = InitConfig {
            data_dir: "/tmp/knowledge".into(),
            log_level: "info".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        init(&json).unwrap();
    }

    #[test]
    fn init_rejects_invalid_json() {
        let err = init("not-json").unwrap_err();
        assert!(matches!(err, NapiError::InvalidConfig { .. }));
    }

    #[test]
    fn ingest_request_round_trips() {
        let req = IngestRequest {
            scope_id: "scope".into(),
            body: "hi".into(),
            source: SourceKind::Manual,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: IngestRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn ingest_message_forwards_invalid_id_for_malformed_scope() {
        // The FFI surface parses `scope_id` as a UUID. Hosts that
        // forget to validate the JS-side string should get a
        // structured `InvalidId` back rather than a panic.
        let req = IngestRequest {
            scope_id: "scope".into(),
            body: "hi".into(),
            source: SourceKind::Slack,
        };
        let err = ingest_message(req).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn query_request_round_trips() {
        let req = QueryRequest {
            scope_id: "scope".into(),
            query_text: "q".into(),
            limit: 10,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: QueryRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn query_forwards_invalid_id_for_malformed_scope() {
        let req = QueryRequest {
            scope_id: "scope".into(),
            query_text: "q".into(),
            limit: 10,
        };
        let err = query(req).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn pin_unpin_forward_invalid_id_for_malformed_id() {
        // `pin` / `unpin` validate the id as a UUID before walking
        // the memory layer, so malformed strings surface as
        // structured `InvalidId` rather than panicking through the
        // FFI bridge.
        for f in [pin, unpin] {
            let err = f("id".into()).unwrap_err();
            assert_eq!(err.kind(), "InvalidId");
        }
    }

    #[test]
    fn forget_forwards_invalid_id_for_malformed_id() {
        // `forget` is wired in Phase A: it validates the id is a
        // UUID before touching the runtime, so malformed ids surface
        // as `InvalidId`.
        let err = forget("id".into()).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn list_memories_forwards_invalid_id_for_malformed_scope() {
        // `list_memories` is wired in Phase A.5 — the surface
        // validates the scope id is a UUID before reaching the
        // memory layer.
        let err = list_memories(
            "scope".into(),
            MemoryFilter {
                state: Some(MemoryState::Reinforced),
                pinned_only: false,
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn synthesis_endpoints_forward_invalid_id_for_malformed_scope() {
        // `get_channel_memory` is wired; `trigger_synthesis` parses
        // the scope id before returning the Phase-A.5 `Unavailable`
        // marker. Both should report InvalidId for a malformed id.
        assert_eq!(
            get_channel_memory("scope".into()).unwrap_err().kind(),
            "InvalidId"
        );
        assert_eq!(
            trigger_synthesis("scope".into(), SynthesisTrigger::ManualUserAction)
                .unwrap_err()
                .kind(),
            "InvalidId"
        );
    }

    #[test]
    fn generate_keypair_returns_ml_dsa_65_envelope() {
        // Wired in Phase A. The N-API layer just forwards the
        // structured envelope; assert the envelope shape is
        // preserved across the bridge.
        let kp = generate_keypair().expect("generate_keypair");
        assert_eq!(kp.algorithm, "ml-dsa-65");
        assert!(!kp.public_key.is_empty());
        assert!(!kp.private_key.is_empty());
    }

    #[test]
    fn encrypt_decrypt_forward_invalid_id_for_malformed_scope() {
        // The N-API layer base64-decodes the payload and forwards to
        // FFI. With a malformed scope string FFI rejects with
        // InvalidId before any crypto work happens.
        let err = encrypt("scope".into(), encode_b64(&[1, 2, 3])).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
        let err = decrypt("scope".into(), encode_b64(&[1, 2, 3])).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn b64_codec_round_trips() {
        let inputs: &[&[u8]] = &[
            &[],
            &[0x00],
            &[0x00, 0x01],
            &[0x00, 0x01, 0x02],
            &[0x00, 0x01, 0x02, 0x03],
            b"hello world",
        ];
        for input in inputs {
            let s = encode_b64(input);
            let back = decode_b64(&s).unwrap();
            assert_eq!(*input, &back[..]);
        }
    }

    #[test]
    fn b64_decode_rejects_invalid_input() {
        assert!(decode_b64("AAA").is_err()); // wrong length
        assert!(decode_b64("AA!=").is_err()); // invalid char
    }
}
