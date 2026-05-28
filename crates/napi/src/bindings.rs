//! N-API bindings entry point.
//!
//! This module exposes the substrate's desktop bridge as a Node.js
//! native addon via `napi-rs` [`#[napi]`] proc-macros. The pure-Rust
//! surface in [`crate`] stays the canonical Rust-facing API; every
//! `#[napi]`-annotated wrapper here is a thin adapter that:
//!
//! 1. Converts JS-side argument types (`BigInt`, plain JS objects)
//!    into the Rust-side types ([`NapiHandle`] is `u64`, complex
//!    requests come in as [`serde_json::Value`] and are
//!    `from_value`-parsed into the typed [`IngestRequest`] /
//!    [`QueryRequest`] / [`MemoryFilter`] / [`SynthesisTrigger`]).
//! 2. Forwards the call to the matching pure-Rust function.
//! 3. Maps any [`NapiError`] into a structured [`napi::Error`] whose
//!    `reason` is a JSON envelope `{"kind": "...", "detail": {...}}`
//!    so JavaScript callers can switch on `JSON.parse(e.message).kind`.
//!
//! The bindings are compiled in the `cdylib` artefact picked up by
//! `napi build`. They are also reachable from Rust (via the `rlib`
//! crate-type) so the unit tests in this file can exercise the same
//! JSON-envelope error path that JavaScript callers see at runtime.
//!
//! ## Naming
//!
//! `napi-derive` automatically renames Rust `snake_case` identifiers
//! to JS `camelCase` ones when generating the JS surface (e.g.
//! `open_store` → `openStore`). This is the documented behaviour;
//! see <https://napi.rs/docs/concepts/values#naming>.

#![allow(clippy::needless_pass_by_value)]
// napi-derive hands owned values across the JS boundary every call; borrowing would force an extra copy in generated code.
#![allow(unsafe_code)] // napi-derive's `#[napi]` proc-macro expands into FFI module-init stubs that necessarily touch raw C pointers (napi_env, napi_callback_info, napi_value). The expansion includes its own `#[allow(unsafe_code)]` on every generated `extern "C"` function; for the workspace-level `deny(unsafe_code)` to be overridable we mirror the allow here. The hand-written code in this module remains `unsafe`-free.

use napi::bindgen_prelude::{BigInt, Error as JsError, Result};
use napi_derive::napi;

#[cfg(test)]
use ffi::RuntimeHandle;
use ffi::{
    ConnectorKindTag, ConnectorStatus, MemoryFilter, MemoryRecord, SyncReport, SynthesisTrigger,
};

use crate::types::{IngestRequest, QueryRequest};
use crate::{NapiError, NapiHandle};

/// Convert a [`NapiError`] into a structured [`napi::Error`].
///
/// The JS-side `Error.message` is a JSON string of the form
/// `{"kind":"InvalidId","message":"...","detail":{...}}` where:
///
/// * `kind` is the flattened, finest-grained kind tag — for a
///   wrapped `Ffi(InvalidId)` it is `"InvalidId"`, not `"Ffi"`. This
///   is what JS callers switch on.
/// * `message` is the human-readable [`Display`] string suitable
///   for surfacing to the UI.
/// * `detail` is the full serialized [`NapiError`] (including the
///   outer `Ffi` wrapper) for callers that need the raw structure
///   for telemetry / logging.
///
/// Callers do:
///
/// ```js
/// try { core.openStore(...) }
/// catch (e) {
///   const env = JSON.parse(e.message);
///   if (env.kind === "InvalidId") { /* surface as user-input bug */ }
/// }
/// ```
///
/// We deliberately do NOT map kinds to napi's fixed `Status` enum
/// (e.g. `InvalidArg`) because that erases the finer-grained kind
/// surface (`InvalidId` vs `InvalidArgument` would both become
/// `InvalidArg`). The JSON envelope preserves full fidelity.
fn to_js_error(err: NapiError) -> JsError {
    let envelope = serde_json::json!({
        "kind": err.kind(),
        "message": err.to_string(),
        "detail": serde_json::to_value(&err).unwrap_or(serde_json::Value::Null),
    });
    JsError::from_reason(envelope.to_string())
}

/// Convert a JS `BigInt` handle (as marshalled by `napi-derive`) into
/// the substrate's [`NapiHandle`].
///
/// JS represents [`NapiHandle`] as a `BigInt` so the full 64-bit
/// width round-trips without precision loss (JS `Number` only has 53
/// bits of mantissa). napi-rs' `BigInt::get_u64()` returns
/// `(sign, value, lossless)` — we reject negative or too-large
/// BigInts so opaque host bugs can't smuggle a corrupted handle into
/// the runtime.
fn handle_from_bigint(handle: &BigInt) -> Result<NapiHandle> {
    let (sign, value, lossless) = handle.get_u64();
    if sign {
        return Err(to_js_error(NapiError::InvalidArgument {
            message: "handle must be a non-negative BigInt".into(),
        }));
    }
    if !lossless {
        return Err(to_js_error(NapiError::InvalidArgument {
            message: "handle does not fit in a 64-bit unsigned integer".into(),
        }));
    }
    Ok(value)
}

/// Parse a JSON-shaped argument into a typed Rust struct.
///
/// The argument arrives from JS as a plain `Object` which napi-rs
/// converts to [`serde_json::Value`] thanks to the `serde-json`
/// feature on the `napi` crate. From there, `serde_json::from_value`
/// produces the typed request struct used by the pure-Rust API.
fn parse_arg<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| {
        to_js_error(NapiError::InvalidArgument {
            message: format!("invalid request payload: {e}"),
        })
    })
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Initialise the Rust core. Mirrors [`crate::init`].
#[napi(js_name = "init")]
pub fn js_init(config_json: String) -> Result<()> {
    crate::init(&config_json).map_err(to_js_error)
}

/// Open the SQLCipher-backed evidence store. Mirrors
/// [`crate::open_store`]. Returns a JS `BigInt` handle the caller
/// passes back into every subsequent call.
#[napi(js_name = "openStore")]
pub fn js_open_store(path: String, master_key_hex: String) -> Result<BigInt> {
    let handle = crate::open_store(path, master_key_hex).map_err(to_js_error)?;
    Ok(BigInt::from(handle))
}

/// Drop the open evidence store. Mirrors [`crate::close_store`].
#[napi(js_name = "closeStore")]
pub fn js_close_store(handle: BigInt) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::close_store(h).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Evidence plane
// ---------------------------------------------------------------------------

/// Ingest a chat / document message. Mirrors
/// [`crate::ingest_message`]. The `req` argument is a plain JS
/// object shaped like [`IngestRequest`].
#[napi(js_name = "ingestMessage")]
pub fn js_ingest_message(handle: BigInt, req: serde_json::Value) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let typed: IngestRequest = parse_arg(req)?;
    crate::ingest_message(h, typed).map_err(to_js_error)
}

/// Hybrid query against a scope. Mirrors [`crate::query`].
#[napi(js_name = "query")]
pub fn js_query(handle: BigInt, req: serde_json::Value) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let typed: QueryRequest = parse_arg(req)?;
    let rows = crate::query(h, typed).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        // `Internal`, not `InvalidArgument`: the JS caller's input
        // has already cleared `parse_arg` and the underlying
        // `crate::query` call has already returned successfully.
        // A `to_value` failure here means the substrate has a
        // latent encoding bug on a `Serialize` impl that should
        // be infallible by construction — routing this to the
        // `Internal` bucket keeps caller-error telemetry clean.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise query rows: {e}"),
        })
    })
}

/// Fetch a single evidence row. Mirrors [`crate::get_evidence`].
#[napi(js_name = "getEvidence")]
pub fn js_get_evidence(handle: BigInt, evidence_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let row = crate::get_evidence(h, evidence_id).map_err(to_js_error)?;
    serde_json::to_value(row).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise evidence row: {e}"),
        })
    })
}

/// Escape a user-supplied FTS5 query. Mirrors
/// [`crate::escape_fts_query`].
#[napi(js_name = "escapeFtsQuery")]
pub fn js_escape_fts_query(input: String) -> String {
    crate::escape_fts_query(input)
}

// ---------------------------------------------------------------------------
// Memory plane
// ---------------------------------------------------------------------------

/// Fetch the per-user memory bundle for a scope. Mirrors
/// [`crate::get_user_memory`].
#[napi(js_name = "getUserMemory")]
pub fn js_get_user_memory(handle: BigInt, scope_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rows: Vec<MemoryRecord> = crate::get_user_memory(h, scope_id).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise memory rows: {e}"),
        })
    })
}

/// Pin a memory record. Mirrors [`crate::pin`].
#[napi(js_name = "pin")]
pub fn js_pin(handle: BigInt, id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::pin(h, id).map_err(to_js_error)
}

/// Lift a pin. Mirrors [`crate::unpin`].
#[napi(js_name = "unpin")]
pub fn js_unpin(handle: BigInt, id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::unpin(h, id).map_err(to_js_error)
}

/// Force-archive a memory record. Mirrors [`crate::forget`].
#[napi(js_name = "forget")]
pub fn js_forget(handle: BigInt, id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::forget(h, id).map_err(to_js_error)
}

/// Destroy all key material for a scope. Mirrors
/// [`crate::forget_scope`].
#[napi(js_name = "forgetScope")]
pub fn js_forget_scope(handle: BigInt, scope_id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::forget_scope(h, scope_id).map_err(to_js_error)
}

/// List memory records, optionally filtered. Mirrors
/// [`crate::list_memories`]. The `filter` argument is a JSON object
/// whose key set must match [`MemoryFilter`] exactly — `state` is
/// optional (`null` or omitted ⇒ no state filter) and `pinned_only`
/// is a required bool. The struct carries
/// `#[serde(deny_unknown_fields)]`, so a typo like `pinnedOnly`
/// (camelCase) or `Pinned_Only` errors out with a clear
/// `InvalidArgument` rather than silently defaulting — this catches
/// JS-side mistakes at the FFI boundary instead of letting them
/// surface as missing memory rows later in the pipeline.
#[napi(js_name = "listMemories")]
pub fn js_list_memories(
    handle: BigInt,
    scope_id: String,
    filter: serde_json::Value,
) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let typed: MemoryFilter = parse_arg(filter)?;
    let rows: Vec<MemoryRecord> = crate::list_memories(h, scope_id, typed).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise memory rows: {e}"),
        })
    })
}

/// Run the per-scope memory decay sweep. Mirrors
/// [`crate::run_decay_sweep`].
#[napi(js_name = "runDecaySweep")]
pub fn js_run_decay_sweep(handle: BigInt, scope_id: String) -> Result<u32> {
    let h = handle_from_bigint(&handle)?;
    crate::run_decay_sweep(h, scope_id).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Synthesis plane
// ---------------------------------------------------------------------------

/// Fetch the channel-level synthesis memory. Mirrors
/// [`crate::get_channel_memory`].
#[napi(js_name = "getChannelMemory")]
pub fn js_get_channel_memory(handle: BigInt, scope_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let row: Option<MemoryRecord> = crate::get_channel_memory(h, scope_id).map_err(to_js_error)?;
    serde_json::to_value(row).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise channel memory: {e}"),
        })
    })
}

/// Trigger synthesis for a scope. Mirrors
/// [`crate::trigger_synthesis`]. `trigger` is one of
/// `"ManualUserAction"` / `"BackgroundIdle"` / `"EvidenceThreshold"` /
/// `"ConnectorSyncCompleted"` (matching the
/// [`SynthesisTrigger`] enum's serde-rename rules).
#[napi(js_name = "triggerSynthesis")]
pub fn js_trigger_synthesis(handle: BigInt, scope_id: String, trigger: String) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    let trig: SynthesisTrigger = parse_arg(serde_json::Value::String(trigger))?;
    crate::trigger_synthesis(h, scope_id, trig).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Cryptography
// ---------------------------------------------------------------------------

/// Generate a fresh signing keypair. Mirrors
/// [`crate::generate_keypair`]. The returned object has `algorithm`,
/// `publicKey`, `privateKey` fields (the key fields are arrays of
/// byte integers in `[0, 255]`).
#[napi(js_name = "generateKeypair")]
pub fn js_generate_keypair() -> Result<serde_json::Value> {
    let kp = crate::generate_keypair().map_err(to_js_error)?;
    serde_json::to_value(kp).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise keypair: {e}"),
        })
    })
}

/// Encrypt a base64-encoded plaintext for a scope. Mirrors
/// [`crate::encrypt`].
#[napi(js_name = "encrypt")]
pub fn js_encrypt(handle: BigInt, scope_id: String, plaintext_b64: String) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    crate::encrypt(h, scope_id, plaintext_b64).map_err(to_js_error)
}

/// Decrypt a base64-encoded ciphertext envelope. Mirrors
/// [`crate::decrypt`].
#[napi(js_name = "decrypt")]
pub fn js_decrypt(handle: BigInt, scope_id: String, ciphertext_b64: String) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    crate::decrypt(h, scope_id, ciphertext_b64).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Health / version surface (consumed by the desktop status panel and the
// `health-check` exit-code probe shipped alongside the addon).
// ---------------------------------------------------------------------------

/// Return the package version of the Rust core baked into this
/// `.node` artefact. Mirrors [`crate::core_version`]. Lets the JS-side
/// bootstrapper assert against a known-good version before opening
/// any stores so a stale addon from a previous install doesn't
/// silently corrupt data.
#[napi(js_name = "coreVersion")]
pub fn js_core_version() -> String {
    crate::core_version()
}

/// Full health envelope. Mirrors [`crate::health_check`].
///
/// `handle` is optional:
/// * pass `0n` (or omit it via the TypeScript optional parameter)
///   to get a bridge-only envelope — useful immediately after
///   loading the addon, before any [`js_open_store`] call.
/// * pass a [`BigInt`] returned by [`js_open_store`] to get a full
///   probe (`bridge` + `evidence_store` + `crypto` + `memory_manager`
///   + `inference_router` subsystems, with real per-subsystem I/O).
///
/// Returns the [`ffi::HealthStatus`] envelope serialised to JSON.
/// JS hosts get a typed object via napi's `serde_json::Value`
/// transparent passthrough; the shape is documented in
/// `crates/napi/index.d.ts`.
#[napi(js_name = "healthCheck")]
pub fn js_health_check(handle: Option<BigInt>) -> Result<serde_json::Value> {
    let h = match handle.as_ref() {
        Some(bi) => Some(handle_from_bigint(bi)?),
        None => None,
    };
    let envelope = crate::health_check(h).map_err(to_js_error)?;
    serde_json::to_value(envelope).map_err(|e| {
        to_js_error(crate::NapiError::Internal {
            message: format!("failed to serialize HealthStatus: {e}"),
        })
    })
}

// ---------------------------------------------------------------------------
// Connector management surface — mirrors the five connector FFI functions
// from `crates/ffi/src/connector.rs` through to the JS host. The JS-side
// argument shape is:
//
//   * `kind` arrives as a JS string matching the snake_case serde tags on
//     `ConnectorKindTag` (e.g. `"google_drive"`, `"notion"`, `"slack"`).
//   * `configJson` is a JS string (NOT a JS object) carrying the provider's
//     OAuth2 config (`client_id`, `redirect_uri`, `token_url`, etc.) so the
//     same exact bytes get parsed once on the Rust side, matching the
//     UniFFI signature on iOS / Android.
//
// Return shapes:
//
//   * `createConnector` returns the new instance UUID as a string.
//   * `syncConnector` returns the `SyncReport` envelope serialised to JSON
//     for ergonomic JS-side destructuring.
//   * `listConnectors` returns the same `ConnectorStatus[]` envelope.
// ---------------------------------------------------------------------------

/// Instantiate a connector. Mirrors [`crate::create_connector`].
#[napi(js_name = "createConnector")]
pub fn js_create_connector(
    handle: BigInt,
    kind: serde_json::Value,
    scope_id: String,
    config_json: String,
) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    let kind_tag: ConnectorKindTag = parse_arg(kind)?;
    crate::create_connector(h, kind_tag, scope_id, config_json).map_err(to_js_error)
}

/// Run the OAuth2 `authorization_code` exchange for an existing
/// connector instance. Mirrors [`crate::authenticate_connector`].
#[napi(js_name = "authenticateConnector")]
pub fn js_authenticate_connector(
    handle: BigInt,
    instance_id: String,
    auth_code: String,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::authenticate_connector(h, instance_id, auth_code).map_err(to_js_error)
}

/// Run a connector sync and ingest emitted events into the evidence
/// store. Mirrors [`crate::sync_connector`]. Returns the
/// [`SyncReport`] envelope serialised as `serde_json::Value` for JS.
#[napi(js_name = "syncConnector")]
pub fn js_sync_connector(handle: BigInt, instance_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let report: SyncReport = crate::sync_connector(h, instance_id).map_err(to_js_error)?;
    serde_json::to_value(report).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("failed to serialize SyncReport: {e}"),
        })
    })
}

/// List configured connector instances. Mirrors
/// [`crate::list_connectors`]. Returns a JSON array of
/// [`ConnectorStatus`] objects.
#[napi(js_name = "listConnectors")]
pub fn js_list_connectors(handle: BigInt) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rows: Vec<ConnectorStatus> = crate::list_connectors(h).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("failed to serialize ConnectorStatus list: {e}"),
        })
    })
}

/// Tear down a connector. Mirrors [`crate::remove_connector`].
#[napi(js_name = "removeConnector")]
pub fn js_remove_connector(handle: BigInt, instance_id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::remove_connector(h, instance_id).map_err(to_js_error)
}

/// Install a global `tracing` subscriber filtered by the supplied
/// `RUST_LOG`-syntax directive (e.g. `"ffi=debug,evidence_store=info"`).
///
/// Mirrors [`crate::try_init_tracing`]. Idempotent: a second call is
/// a no-op (the substrate guards against installing competing
/// subscribers).
///
/// Available only when the addon was built with the
/// `tracing-subscriber` feature; without it this entry point is not
/// exposed (the underlying `crate::try_init_tracing` is also
/// feature-gated).
#[cfg(feature = "tracing-subscriber")]
#[napi(js_name = "initTracing")]
pub fn js_init_tracing(directive: String) -> Result<()> {
    // `crate::try_init_tracing` is the re-export of `ffi::try_init_tracing`
    // so its error type is `FfiError`. Funnel it through the napi error
    // mapper via the `From<FfiError> for NapiError` impl so callers see the
    // same `kind`/`message`/`detail` JSON envelope every other entry point
    // produces.
    crate::try_init_tracing(&directive).map_err(|e| to_js_error(crate::NapiError::from(e)))
}

#[cfg(test)]
mod tests {
    //! These tests exercise the JSON-envelope error path that
    //! JavaScript callers see at runtime. They run against the
    //! `rlib` build (not the cdylib) so they don't need a live
    //! Node.js host.

    use super::*;

    fn parse_envelope(err: &JsError) -> serde_json::Value {
        // `JsError::reason` is the JSON envelope we produced in
        // `to_js_error`. The error's `Display` impl prefixes the
        // status — read the reason directly to avoid that.
        serde_json::from_str(&err.reason).expect("error reason should be JSON")
    }

    #[test]
    fn handle_from_bigint_rejects_negative() {
        let bi = BigInt {
            sign_bit: true,
            words: vec![5],
        };
        let err = handle_from_bigint(&bi).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn handle_from_bigint_rejects_overflow() {
        // Two-word BigInt → does not fit in u64.
        let bi = BigInt {
            sign_bit: false,
            words: vec![1, 1],
        };
        let err = handle_from_bigint(&bi).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn handle_from_bigint_accepts_valid_u64() {
        let bi = BigInt {
            sign_bit: false,
            words: vec![42],
        };
        let h = handle_from_bigint(&bi).unwrap();
        assert_eq!(h, 42);
    }

    #[test]
    fn js_init_rejects_malformed_json() {
        let err = js_init("not a json object".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidConfig");
    }

    #[test]
    fn js_init_accepts_valid_config() {
        let cfg = r#"{"data_dir":"/tmp/x","log_level":"info"}"#;
        js_init(cfg.into()).expect("valid config should accept");
    }

    #[test]
    fn js_ingest_message_forwards_invalid_id_for_malformed_scope() {
        let req = serde_json::json!({
            "scope_id": "not-a-uuid",
            "body": "hi",
            "source": "Slack",
            "importance": "Important",
        });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_ingest_message(bi, req).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_query_round_trips_through_json_envelope() {
        let req = serde_json::json!({
            "scope_id": "scope",
            "query_text": "q",
            "limit": 10,
        });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_query(bi, req).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_list_memories_parses_filter_object() {
        let filter = serde_json::json!({ "state": "Reinforced", "pinned_only": false });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_list_memories(bi, "scope".into(), filter).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_list_memories_rejects_camelcase_pinned_only_typo() {
        // Companion to the FFI-level pin
        // (`crates/ffi/src/types.rs::memory_filter_rejects_camelcase_pinned_only_alias`).
        // Exercises the N-API wrapper end-to-end: a JS caller
        // shipping `{ pinnedOnly: false }` instead of
        // `{ pinned_only: false }` must come back through
        // [`to_js_error`] as an `InvalidArgument` envelope —
        // never `InvalidId` (which would imply we silently
        // defaulted past the typo and then choked downstream on
        // the bogus scope id).
        let filter = serde_json::json!({ "state": null, "pinnedOnly": false });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_list_memories(bi, "scope".into(), filter).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
        let msg = env["message"].as_str().expect("message is a string");
        assert!(
            msg.contains("pinnedOnly"),
            "expected the JS-facing error to name the offending key `pinnedOnly`, got {msg}"
        );
    }

    #[test]
    fn js_trigger_synthesis_parses_enum_string() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_trigger_synthesis(bi, "scope".into(), "ManualUserAction".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_trigger_synthesis_rejects_unknown_trigger() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_trigger_synthesis(bi, "scope".into(), "Bogus".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn js_generate_keypair_returns_ml_dsa_envelope() {
        let v = js_generate_keypair().expect("keypair");
        assert_eq!(v["algorithm"], "ml-dsa-65");
        assert!(v["public_key"].is_array() || v["public_key"].is_string());
    }

    #[test]
    fn js_escape_fts_query_matches_pure_rust_impl() {
        let input = r#"hello "world""#;
        let got = js_escape_fts_query(input.into());
        let want = crate::escape_fts_query(input.into());
        assert_eq!(got, want);
    }

    #[test]
    fn js_core_version_matches_cargo_pkg_version() {
        assert_eq!(js_core_version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn js_health_check_without_handle_returns_bridge_only_envelope() {
        let envelope = js_health_check(None).expect("bridge probe is infallible");
        let core_version = envelope
            .get("core_version")
            .and_then(|v| v.as_str())
            .expect("core_version is a string");
        assert_eq!(core_version, env!("CARGO_PKG_VERSION"));
        let subsystems = envelope
            .get("subsystems")
            .and_then(|v| v.as_array())
            .expect("subsystems is an array");
        assert_eq!(subsystems.len(), 1);
        assert_eq!(
            subsystems[0].get("name").and_then(|v| v.as_str()),
            Some("bridge")
        );
        assert_eq!(
            subsystems[0].get("status").and_then(|v| v.as_str()),
            Some("ok")
        );
        // tracing_initialized starts false in a fresh process; the
        // rlib test build does not link `tracing-subscriber` by
        // default. If a sibling test in the same binary has already
        // installed a subscriber the flag may flip to true, so we
        // only assert the field exists and is a boolean.
        assert!(envelope
            .get("tracing_initialized")
            .is_some_and(serde_json::Value::is_boolean));
        // The metrics snapshot is wire-flat — every counter / gauge
        // is a sibling field of the snapshot object. At minimum the
        // ingest counter and the boot timestamp must exist.
        let metrics = envelope.get("metrics").expect("metrics object");
        assert!(metrics
            .get("ingest_total")
            .and_then(serde_json::Value::as_u64)
            .is_some());
        assert!(metrics
            .get("boot_unix_secs")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|v| v > 0));
    }

    #[test]
    fn js_health_check_with_zero_bigint_is_equivalent_to_none() {
        let zero = BigInt {
            sign_bit: false,
            words: vec![0],
        };
        let envelope = js_health_check(Some(zero)).expect("zero handle == bridge-only");
        let subsystems = envelope
            .get("subsystems")
            .and_then(|v| v.as_array())
            .expect("subsystems is an array");
        assert_eq!(subsystems.len(), 1);
        assert_eq!(
            subsystems[0].get("name").and_then(|v| v.as_str()),
            Some("bridge")
        );
    }

    #[test]
    fn js_health_check_with_unknown_handle_returns_unavailable_envelope_error() {
        let bogus = BigInt {
            sign_bit: false,
            words: vec![u64::MAX],
        };
        let err = js_health_check(Some(bogus)).expect_err("unknown handle");
        let env = parse_envelope(&err);
        assert_eq!(
            env.get("kind").and_then(|v| v.as_str()),
            Some("Unavailable")
        );
    }

    #[test]
    fn js_close_store_accepts_none_sentinel() {
        // The NONE sentinel (handle 0) is allowed; close_store
        // documented as a no-op for unknown handles.
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        js_close_store(bi).expect("close NONE should succeed");
    }

    #[test]
    fn js_open_store_rejects_malformed_master_key() {
        // 64-char string but contains non-hex characters → InvalidArgument
        // forwarded from the FFI surface.
        let err = js_open_store(
            "/tmp/should-not-be-touched".into(),
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ".into(),
        )
        .unwrap_err();
        let env = parse_envelope(&err);
        // FFI surface flags this as InvalidArgument (hex decode failure).
        assert!(
            env["kind"] == "InvalidArgument" || env["kind"] == "InvalidId",
            "got kind = {}",
            env["kind"]
        );
    }

    #[test]
    fn js_get_evidence_forwards_invalid_id_for_malformed_id() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_get_evidence(bi, "not-a-uuid".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_pin_unpin_forget_forward_invalid_id() {
        type F = fn(BigInt, String) -> Result<()>;
        for f in [js_pin as F, js_unpin as F, js_forget as F] {
            let bi = BigInt::from(RuntimeHandle::NONE.0);
            let err = f(bi, "id".into()).unwrap_err();
            let env = parse_envelope(&err);
            assert_eq!(env["kind"], "InvalidId");
        }
    }

    #[test]
    fn js_forget_scope_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_forget_scope(bi, "not-a-uuid".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_get_user_memory_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_get_user_memory(bi, "scope".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_run_decay_sweep_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_run_decay_sweep(bi, "scope".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_get_channel_memory_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_get_channel_memory(bi, "scope".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_encrypt_decrypt_forward_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_encrypt(bi.clone(), "scope".into(), "AAEC".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");

        let err = js_decrypt(bi, "scope".into(), "AAEC".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn parse_arg_rejects_invalid_json_shape() {
        // An IngestRequest with missing required fields should yield
        // an `InvalidArgument` envelope rather than panicking.
        let req = serde_json::json!({"scope_id": "x"});
        let result: Result<IngestRequest> = parse_arg(req);
        let err = result.unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn js_init_round_trip_via_json() {
        // The full JSON round-trip exercise the same code path
        // JavaScript callers see: stringified config → parse →
        // accept. A regression here would silently break the
        // Electron bootstrapper.
        let cfg = crate::types::InitConfig {
            data_dir: "/tmp/k".into(),
            log_level: "warn".into(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        js_init(s).expect("config should round-trip through json");
    }
}
