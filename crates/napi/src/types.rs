//! N-API request / config wrapper types.
//!
//! These mirror the FFI surface argument tuples but bundled into
//! single JSON-shaped objects, because Electron / Node hosts marshal
//! arguments through `napi-rs` as a single JS object.

use serde::{Deserialize, Serialize};

use ffi::types::{FfiImportanceClass, ScopeIdString, SourceKind};

/// One-time initialization config the Electron host passes via
/// `init(JSON.stringify(config))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitConfig {
    /// Filesystem directory holding the encrypted SQLCipher
    /// database, model artefacts, and audit log.
    pub data_dir: String,
    /// Logging verbosity (`"trace"` / `"debug"` / `"info"` /
    /// `"warn"` / `"error"`).
    pub log_level: String,
}

/// JSON-shaped argument object for [`super::ingest_message`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestRequest {
    /// UUID-string scope id.
    pub scope_id: ScopeIdString,
    /// Plaintext UTF-8 message body.
    pub body: String,
    /// Source connector kind.
    pub source: SourceKind,
    /// Importance classification. Defaults to `Important` when
    /// absent from the JSON payload.
    #[serde(default = "default_importance")]
    pub importance: FfiImportanceClass,
}

fn default_importance() -> FfiImportanceClass {
    FfiImportanceClass::Important
}

/// JSON-shaped argument object for [`super::query`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// UUID-string scope id.
    pub scope_id: ScopeIdString,
    /// Free-text query string.
    pub query_text: String,
    /// Maximum number of rows to return.
    pub limit: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_config_round_trips_via_serde() {
        let cfg = InitConfig {
            data_dir: "/tmp/k".into(),
            log_level: "info".into(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: InitConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
}
