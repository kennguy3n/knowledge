//! Request / response bodies for the loopback API.
//!
//! Where the FFI surface already exposes a serde type (enums such as
//! [`ffi::SourceKind`], records such as [`ffi::MemoryFilter`]) we reuse
//! it directly so the JSON contract is identical on both sides of the
//! bridge — the Go tier serialises the same `PascalCase`-tagged enums
//! the FFI layer expects.

use ffi::{ConnectorKindTag, FfiImportanceClass, MemoryFilter, SourceKind, SynthesisTrigger};
use serde::{Deserialize, Serialize};

/// `POST /ingest` body.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestRequest {
    /// UUID-string scope id.
    pub scope_id: String,
    /// Plaintext UTF-8 body to encrypt + persist.
    pub body: String,
    /// Connector tag that produced the row.
    pub source: SourceKind,
    /// Storage-tier importance class.
    pub importance: FfiImportanceClass,
}

/// Generic `{ "id": "<uuid>" }` response used by create-style routes.
#[derive(Debug, Clone, Serialize)]
pub struct IdResponse {
    /// Newly created object's UUID string.
    pub id: String,
}

/// `POST /query` body.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryRequest {
    /// UUID-string scope id to search within.
    pub scope_id: String,
    /// FTS5 query expression (forwarded verbatim, parameterised).
    pub query_text: String,
    /// Maximum number of rows to return.
    pub limit: u32,
}

/// `POST /memories` body — list per-user memories for a scope.
#[derive(Debug, Clone, Deserialize)]
pub struct ListMemoriesRequest {
    /// UUID-string scope id.
    pub scope_id: String,
    /// Optional filter; defaults to "all states, not pinned-only".
    #[serde(default)]
    pub filter: MemoryFilter,
}

/// `POST /user_memory` body — create a new user-memory observation for
/// a scope.
///
/// This route writes the **user** memory tier only. The channel /
/// domain / tenant tiers are owned by the synthesis pipeline and have
/// no caller-facing write surface, so the body carries no tier
/// discriminator — there is structurally no way to target another
/// tier through this endpoint. That keeps tier authorisation
/// fail-closed: a caller can only ever write the user tier.
#[derive(Debug, Clone, Deserialize)]
pub struct AddUserMemoryRequest {
    /// UUID-string scope id.
    pub scope_id: String,
    /// Free-form observation tag (e.g. `"preference"`, `"task"`,
    /// `"fact"`) recorded in the object metadata.
    pub observation_type: String,
    /// Human-readable memory text.
    pub content: String,
    /// Sensitivity class driving the decay schedule. Defaults to
    /// [`FfiImportanceClass::Useful`] when omitted, matching the
    /// storage-tier default used by `ingest`.
    #[serde(default = "default_sensitivity")]
    pub sensitivity: FfiImportanceClass,
}

/// Default sensitivity for [`AddUserMemoryRequest`] — `Useful` keeps a
/// new observation in the working set under medium decay rather than
/// the never-promoted `Noise` tier.
fn default_sensitivity() -> FfiImportanceClass {
    FfiImportanceClass::Useful
}

/// `POST /forget_scope` body.
#[derive(Debug, Clone, Deserialize)]
pub struct ForgetScopeRequest {
    /// UUID-string scope id to cryptographically forget.
    pub scope_id: String,
}

/// `POST /pin` / `POST /unpin` body.
#[derive(Debug, Clone, Deserialize)]
pub struct IdRequest {
    /// UUID-string memory / evidence id.
    pub id: String,
}

/// `POST /synthesis/trigger` body.
#[derive(Debug, Clone, Deserialize)]
pub struct SynthesisTriggerRequest {
    /// UUID-string scope id to synthesise.
    pub scope_id: String,
    /// Why the cycle was triggered.
    pub trigger: SynthesisTrigger,
}

/// `POST /synthesis/recent` body.
#[derive(Debug, Clone, Deserialize)]
pub struct RecentSynthesisRequest {
    /// UUID-string scope id.
    pub scope_id: String,
}

/// `POST /connectors` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateConnectorRequest {
    /// Connector provider kind.
    pub kind: ConnectorKindTag,
    /// UUID-string scope id the connector ingests into.
    pub scope_id: String,
    /// Provider-specific auth config as a JSON string (forwarded
    /// verbatim to the connector framework).
    pub config_json: String,
}

/// `POST /connectors/{id}/authenticate` body.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthenticateRequest {
    /// OAuth2 authorization code from the provider callback.
    pub auth_code: String,
}

/// `POST /connector/fetch_content` body. The content-fetch endpoint
/// returns `501 Not Implemented` on this build, so the field set is
/// intentionally permissive.
#[derive(Debug, Clone, Deserialize)]
pub struct FetchContentRequest {
    /// UUID-string connector instance id.
    pub instance_id: String,
    /// Provider-native content reference (message id, file id, …).
    pub content_ref: String,
}
