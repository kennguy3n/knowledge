//! `knowledge_napi` — N-API addon skeleton for macOS / Windows
//! Electron desktop integration.
//!
//! Per `ARCHITECTURE.md` §3 ("Platform integration plane") and
//! `docs/DESIGN.md` §2 ("On-device runtime"), the desktop bridge ships
//! as a Node.js native addon that mirrors the iOS / Android UniFFI
//! surface (see the sibling `ffi` crate) but speaks JSON-over-N-API
//! instead of typed object handles.
//!
//! The current deliverable is the **wire skeleton**:
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
//! Phase 4 — the [`#[napi]`] proc-macros are live. The cdylib that
//! `napi build` produces is loaded by Node via `require('./*.node')`.
//! The [`bindings`] module is the JS-facing surface; the freestanding
//! `pub fn`s in this file remain the canonical Rust-facing API so
//! unit tests and Rust callers can exercise the substrate without
//! going through the Node bridge.
//!
//! See [`bindings`] for the full JS API.

#![deny(missing_docs)]
// Most N-API entry points in this file forward their `String` /
// `Vec<u8>` arguments straight into the matching `ffi::*` call, which
// consumes them by value — clippy treats that as a genuine
// consumption and does not fire `needless_pass_by_value`. The
// exception is the `encrypt` / `decrypt` pair: they call helpers
// that only borrow their inputs, so a per-function
// `#[allow(clippy::needless_pass_by_value)]` is applied there with a
// comment explaining why the by-value signature is kept (napi-derive
// hands owned `String` / `Vec<u8>` across the JS boundary on every
// call; borrowing would force an extra copy in generated code).
// Keeping the allows local lets clippy still catch inadvertent
// by-value taking in internal helpers that don't cross the FFI
// boundary.

pub mod bindings;
pub mod error;
pub mod types;

pub use error::{NapiError, NapiResult};
pub use ffi::{
    EvidenceRecord, FfiImportanceClass, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord,
    MemoryState, QueryResult, RuntimeHandle, ScopeIdString, SourceKind, SynthesisTrigger,
};
pub use types::{IngestRequest, InitConfig, QueryRequest};

/// Wire-stable handle to an open store. Hosts receive this from
/// [`open_store`] and must pass it back into every subsequent call.
/// JavaScript represents this as a `bigint` to preserve the full
/// 64-bit width without loss of precision (N-API will marshal this
/// transparently once the `#[napi]` macros land).
///
/// # Sentinel
///
/// `0n` (BigInt zero) is the reserved "no handle" sentinel mirroring
/// [`RuntimeHandle::NONE`]. The handle allocator on the Rust side
/// starts at `1n` and never re-mints `0n`, so any call from JS that
/// passes `0n` is guaranteed to be rejected with
/// [`NapiError::Unavailable`] for the `evidence_store` subsystem.
/// Hosts should treat `0n` as "not yet opened" rather than as a
/// valid handle.
pub type NapiHandle = u64;

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
/// chars). Mirrors [`ffi::open_store`]. Returns the allocated
/// [`NapiHandle`] the host must pass back into every subsequent
/// call.
///
/// # Errors
///
/// Forwards [`ffi::open_store`] errors as [`NapiError`].
pub fn open_store(path: String, master_key_hex: String) -> NapiResult<NapiHandle> {
    ffi::open_store(path, master_key_hex)
        .map(|h| h.0)
        .map_err(NapiError::from)
}

/// Drop the open evidence store identified by `handle`. Mirrors
/// [`ffi::close_store`]. Calling with an unknown handle is a no-op
/// — hosts may invoke this in `try`/`finally` shutdown paths.
///
/// # Errors
///
/// Forwards [`ffi::close_store`] errors as [`NapiError`].
pub fn close_store(handle: NapiHandle) -> NapiResult<()> {
    ffi::close_store(RuntimeHandle(handle)).map_err(NapiError::from)
}

/// Ingest a chat / document message through the encrypted evidence
/// plane. Mirrors [`ffi::ingest_message`].
///
/// # Errors
///
/// Returns [`NapiError`] if the request body is malformed or the
/// underlying FFI surface returns an error.
pub fn ingest_message(handle: NapiHandle, req: IngestRequest) -> NapiResult<serde_json::Value> {
    ffi::ingest_message(
        RuntimeHandle(handle),
        req.scope_id,
        req.body,
        req.source,
        req.importance,
    )
    .map(|id| serde_json::json!({ "evidence_id": id }))
    .map_err(NapiError::from)
}

/// Hybrid query against a scope. Mirrors [`ffi::query`].
///
/// # Errors
///
/// Forwards [`ffi::query`] errors as [`NapiError`].
pub fn query(handle: NapiHandle, req: QueryRequest) -> NapiResult<Vec<QueryResult>> {
    ffi::query(
        RuntimeHandle(handle),
        req.scope_id,
        req.query_text,
        req.limit,
    )
    .map_err(NapiError::from)
}

/// Fetch a single evidence row. Mirrors [`ffi::get_evidence`].
///
/// # Errors
///
/// Forwards [`ffi::get_evidence`] errors as [`NapiError`].
pub fn get_evidence(handle: NapiHandle, evidence_id: String) -> NapiResult<EvidenceRecord> {
    ffi::get_evidence(RuntimeHandle(handle), evidence_id).map_err(NapiError::from)
}

/// Fetch the per-user memory bundle for a scope.
///
/// # Errors
///
/// Forwards [`ffi::get_user_memory`] errors as [`NapiError`].
pub fn get_user_memory(
    handle: NapiHandle,
    scope_id: ScopeIdString,
) -> NapiResult<Vec<MemoryRecord>> {
    ffi::get_user_memory(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Mark a memory record as `Pinned`.
///
/// # Errors
///
/// Forwards [`ffi::pin`] errors as [`NapiError`].
pub fn pin(handle: NapiHandle, id: String) -> NapiResult<()> {
    ffi::pin(RuntimeHandle(handle), id).map_err(NapiError::from)
}

/// Lift a previously-applied pin so the row resumes ageing.
///
/// # Errors
///
/// Forwards [`ffi::unpin`] errors as [`NapiError`].
pub fn unpin(handle: NapiHandle, id: String) -> NapiResult<()> {
    ffi::unpin(RuntimeHandle(handle), id).map_err(NapiError::from)
}

/// Force-archive a memory record (user-initiated forget).
///
/// # Errors
///
/// Forwards [`ffi::forget`] errors as [`NapiError`].
pub fn forget(handle: NapiHandle, id: String) -> NapiResult<()> {
    ffi::forget(RuntimeHandle(handle), id).map_err(NapiError::from)
}

/// Destroy all cryptographic material for `scope_id` so its evidence
/// and body-table data become permanently unrecoverable. Mirrors
/// [`ffi::forget_scope`].
///
/// # Errors
///
/// Forwards [`ffi::forget_scope`] errors as [`NapiError`].
pub fn forget_scope(handle: NapiHandle, scope_id: ScopeIdString) -> NapiResult<()> {
    ffi::forget_scope(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Escape a user-supplied string for safe use inside an FTS5 query.
/// Mirrors [`ffi::escape_fts_query`].
pub fn escape_fts_query(input: String) -> String {
    ffi::escape_fts_query(input)
}

/// List memory records for a scope, optionally filtered.
///
/// # Errors
///
/// Forwards [`ffi::list_memories`] errors as [`NapiError`].
pub fn list_memories(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    filter: MemoryFilter,
) -> NapiResult<Vec<MemoryRecord>> {
    ffi::list_memories(RuntimeHandle(handle), scope_id, filter).map_err(NapiError::from)
}

/// Fetch the channel-level synthesis memory for a scope.
///
/// # Errors
///
/// Forwards [`ffi::get_channel_memory`] errors as [`NapiError`].
pub fn get_channel_memory(
    handle: NapiHandle,
    scope_id: ScopeIdString,
) -> NapiResult<Option<MemoryRecord>> {
    ffi::get_channel_memory(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Run the per-scope memory decay sweep, demoting stale rows and
/// archiving anything that has aged out of the working set.
///
/// Mirrors [`ffi::run_decay_sweep`]. Returns the number of rows that
/// transitioned state during the sweep. Electron hosts call this on
/// idle ticks (typically every few minutes) to keep retention scores
/// fresh without blocking interactive paths.
///
/// # Errors
///
/// Forwards [`ffi::run_decay_sweep`] errors as [`NapiError`].
pub fn run_decay_sweep(handle: NapiHandle, scope_id: ScopeIdString) -> NapiResult<u32> {
    ffi::run_decay_sweep(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Trigger synthesis for a scope.
///
/// # Errors
///
/// Forwards [`ffi::trigger_synthesis`] errors as [`NapiError`].
pub fn trigger_synthesis(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    trigger: SynthesisTrigger,
) -> NapiResult<String> {
    ffi::trigger_synthesis(RuntimeHandle(handle), scope_id, trigger).map_err(NapiError::from)
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
#[allow(clippy::needless_pass_by_value)] // FFI: napi-derive hands owned strings across the JS boundary on every call; borrowing here would force an extra copy in generated code.
pub fn encrypt(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    plaintext_b64: String,
) -> NapiResult<String> {
    let plaintext = decode_b64(&plaintext_b64)?;
    let cipher =
        ffi::encrypt(RuntimeHandle(handle), scope_id, plaintext).map_err(NapiError::from)?;
    Ok(encode_b64(&cipher))
}

/// Inverse of [`encrypt`].
///
/// # Errors
///
/// Forwards [`ffi::decrypt`] errors as [`NapiError`].
#[allow(clippy::needless_pass_by_value)] // FFI: napi-derive hands owned strings across the JS boundary on every call; borrowing here would force an extra copy in generated code.
pub fn decrypt(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    ciphertext_b64: String,
) -> NapiResult<String> {
    let cipher = decode_b64(&ciphertext_b64)?;
    let plain = ffi::decrypt(RuntimeHandle(handle), scope_id, cipher).map_err(NapiError::from)?;
    Ok(encode_b64(&plain))
}

/// Return the semver of the Rust core baked into this build artefact.
///
/// Sourced from `CARGO_PKG_VERSION` at compile time, which mirrors
/// the workspace-level `version` in the root `Cargo.toml`. Hosts use
/// this to assert against a known-good core before opening any stores
/// so a stale addon from a previous install doesn't silently corrupt
/// data. The corresponding JS-facing wrapper is
/// [`bindings::js_core_version`].
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Lightweight "is the bridge alive?" probe.
///
/// Returns the string `"ok"` synchronously without touching any
/// subsystems. Phase 6 will replace this with a full `HealthStatus`
/// envelope sourced from the substrate's metrics + tracing layer; for
/// now it exists so callers (the desktop status panel and the
/// `health-check` exit-code probe shipped alongside the addon) can
/// confirm the FFI layer is reachable before any other call. The
/// corresponding JS-facing wrapper is [`bindings::js_health_check`].
pub fn health_check() -> String {
    "ok".to_string()
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
    if !s.len().is_multiple_of(4) {
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
            // `idx` is bounded by `B64_ALPHABET.len() == 64`, so the
            // conversion is lossless. `try_from` keeps the cast lints
            // happy without resorting to `as`.
            v |= u32::try_from(idx).expect("base64 alphabet index always fits in u32");
        }
        // Each byte extracted is masked to 0xFF before the cast, so
        // truncation is the intended semantic.
        #[allow(clippy::cast_possible_truncation)]
        {
            out.push(((v >> 16) & 0xFF) as u8);
            if pad < 2 {
                out.push(((v >> 8) & 0xFF) as u8);
            }
            if pad < 1 {
                out.push((v & 0xFF) as u8);
            }
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
            importance: FfiImportanceClass::Important,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: IngestRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);

        // Importance field defaults to Important when absent from JSON.
        let minimal = r#"{"scope_id":"s","body":"b","source":"Manual"}"#;
        let parsed: IngestRequest = serde_json::from_str(minimal).unwrap();
        assert_eq!(parsed.importance, FfiImportanceClass::Important);
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
            importance: FfiImportanceClass::Important,
        };
        let err = ingest_message(RuntimeHandle::NONE.0, req).unwrap_err();
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
        let err = query(RuntimeHandle::NONE.0, req).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn pin_unpin_forward_invalid_id_for_malformed_id() {
        // `pin` / `unpin` validate the id as a UUID before walking
        // the memory layer, so malformed strings surface as
        // structured `InvalidId` rather than panicking through the
        // FFI bridge.
        for f in [
            pin as fn(NapiHandle, String) -> NapiResult<()>,
            unpin as fn(NapiHandle, String) -> NapiResult<()>,
        ] {
            let err = f(RuntimeHandle::NONE.0, "id".into()).unwrap_err();
            assert_eq!(err.kind(), "InvalidId");
        }
    }

    #[test]
    fn forget_forwards_invalid_id_for_malformed_id() {
        // `forget` is wired: it validates the id is a
        // UUID before touching the runtime, so malformed ids surface
        // as `InvalidId`.
        let err = forget(RuntimeHandle::NONE.0, "id".into()).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn forget_scope_forwards_invalid_id_for_malformed_scope() {
        let err = forget_scope(RuntimeHandle::NONE.0, "not-a-uuid".into()).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn escape_fts_query_wraps_in_quotes() {
        let escaped = escape_fts_query(r#"hello "world""#.into());
        assert_eq!(escaped, r#""hello ""world""""#);
    }

    #[test]
    fn list_memories_forwards_invalid_id_for_malformed_scope() {
        // `list_memories` is wired — the surface
        // validates the scope id is a UUID before reaching the
        // memory layer.
        let err = list_memories(
            RuntimeHandle::NONE.0,
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
        // the scope id before returning the `Unavailable`
        // marker. Both should report InvalidId for a malformed id.
        assert_eq!(
            get_channel_memory(RuntimeHandle::NONE.0, "scope".into())
                .unwrap_err()
                .kind(),
            "InvalidId"
        );
        assert_eq!(
            trigger_synthesis(
                RuntimeHandle::NONE.0,
                "scope".into(),
                SynthesisTrigger::ManualUserAction
            )
            .unwrap_err()
            .kind(),
            "InvalidId"
        );
    }

    #[test]
    fn run_decay_sweep_forwards_invalid_id_for_malformed_scope() {
        // Mirrors the FFI-side run_decay_sweep contract: a malformed
        // scope id is rejected before the runtime is touched, so this
        // surfaces InvalidId rather than Unavailable.
        let err = run_decay_sweep(RuntimeHandle::NONE.0, "scope".into()).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn generate_keypair_returns_ml_dsa_65_envelope() {
        // Wired. The N-API layer just forwards the
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
        let err = encrypt(
            RuntimeHandle::NONE.0,
            "scope".into(),
            encode_b64(&[1, 2, 3]),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
        let err = decrypt(
            RuntimeHandle::NONE.0,
            "scope".into(),
            encode_b64(&[1, 2, 3]),
        )
        .unwrap_err();
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
