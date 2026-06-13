//! Server configuration, read from the process environment.
//!
//! Mirrors the env-driven construction pattern used by
//! [`inference_router::RouterConfig`] (see
//! `crates/inference_router/src/config.rs`): a plain-data struct with
//! a fallible `from_env` constructor and documented defaults. Secrets
//! (the master key) are wrapped in [`zeroize::Zeroizing`] so the hex
//! representation is wiped from the allocator freelist on drop and is
//! never accidentally logged via the derived `Debug`.

use std::net::SocketAddr;
use std::path::Path;

use zeroize::Zeroizing;

use crypto::{MasterKey, MASTER_KEY_LEN};

/// Environment variable naming the loopback bind address.
pub const ENV_BIND_ADDR: &str = "KNOWLEDGE_SUBSTRATE_ADDR";
/// Environment variable naming the on-disk SQLCipher store path.
pub const ENV_STORE_PATH: &str = "KNOWLEDGE_STORE_PATH";
/// Environment variable carrying the 64-hex-char master key.
pub const ENV_MASTER_KEY: &str = "KNOWLEDGE_MASTER_KEY";
/// Environment variable naming the SQLCipher-backed permission-tuple
/// store path. When unset it defaults to a `permissions.db` sibling of
/// the evidence store (see [`ServerConfig::from_env`]).
pub const ENV_PERMISSIONS_PATH: &str = "KNOWLEDGE_PERMISSIONS_PATH";

/// Environment variable carrying the HTTPS URL of the managed synthesis
/// endpoint (the SLM/LLM service that backs server-side domain / tenant
/// synthesis). Setting this opts the substrate into installing the
/// [`ffi::configure_synthesis_engine`] HTTP engine at boot; leaving it
/// unset keeps the server-side synthesis routes returning `503`
/// (engine unavailable), preserving the prior behaviour for
/// deployments that only use the on-device channel tier.
pub const ENV_SYNTHESIS_URL: &str = "KNOWLEDGE_SYNTHESIS_URL";
/// Environment variable naming the *secret-store reference* (never the
/// raw key) for the synthesis endpoint's API key.
pub const ENV_SYNTHESIS_API_KEY_REF: &str = "KNOWLEDGE_SYNTHESIS_API_KEY_REF";
/// Environment variable carrying the synthesis model identifier
/// (e.g. `"slm-recap-v1"`).
pub const ENV_SYNTHESIS_MODEL_ID: &str = "KNOWLEDGE_SYNTHESIS_MODEL_ID";
/// Environment variable carrying the response token cap. Unset / `0`
/// falls back to the engine's `DEFAULT_MAX_TOKENS`.
pub const ENV_SYNTHESIS_MAX_TOKENS: &str = "KNOWLEDGE_SYNTHESIS_MAX_TOKENS";
/// Environment variable carrying the per-request timeout in
/// milliseconds. Unset / `0` falls back to the engine's default.
pub const ENV_SYNTHESIS_TIMEOUT_MS: &str = "KNOWLEDGE_SYNTHESIS_TIMEOUT_MS";
/// Environment variable carrying a comma-separated allow-list of scope
/// UUIDs the synthesis engine may serve. Unset disables binding (every
/// scope allowed); see [`SynthesisEngineSettings`].
pub const ENV_SYNTHESIS_SCOPE_BINDINGS: &str = "KNOWLEDGE_SYNTHESIS_SCOPE_BINDINGS";
/// Environment variable opting the deployment into single-tenant health
/// semantics (truthy = `1`/`true`/`yes`/`on`). Defaults to the
/// multi-tenant posture (`false`).
pub const ENV_SYNTHESIS_SINGLE_TENANT: &str = "KNOWLEDGE_SYNTHESIS_SINGLE_TENANT";
/// Environment variable carrying the per-endpoint requests-per-minute
/// cap. Unset uses the engine default (`DEFAULT_MAX_RPM`, 60); a very
/// large value effectively disables RPM limiting. `0` is rejected by
/// the engine and so is treated as "unset" here.
pub const ENV_SYNTHESIS_MAX_RPM: &str = "KNOWLEDGE_SYNTHESIS_MAX_RPM";
/// Environment variable carrying the global synthesis rate-limit
/// capacity (token-bucket burst). Unset / `0` uses the library default.
pub const ENV_SYNTHESIS_RATE_CAPACITY: &str = "KNOWLEDGE_SYNTHESIS_RATE_CAPACITY";
/// Environment variable carrying the global synthesis rate-limit refill
/// (tokens per second). Unset / `0` uses the library default.
pub const ENV_SYNTHESIS_RATE_REFILL_PER_SEC: &str = "KNOWLEDGE_SYNTHESIS_RATE_REFILL_PER_SEC";

/// Default loopback bind address — internal only, never exposed to
/// the public network. The Go API gateway is the only client.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:9090";
/// Default on-disk store path used when [`ENV_STORE_PATH`] is unset.
pub const DEFAULT_STORE_PATH: &str = "/var/lib/knowledge/substrate.db";
/// Exact required length of the master key in hex characters
/// (32 bytes → 64 hex chars). Mirrors `ffi::open_store`'s contract.
pub const MASTER_KEY_HEX_LEN: usize = 64;

/// Errors surfaced while assembling [`ServerConfig`] from the
/// environment.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required environment variable was unset or empty.
    #[error("required environment variable `{0}` is unset or empty")]
    Missing(&'static str),
    /// The bind address did not parse as a `host:port` socket addr.
    #[error("invalid bind address in `{var}`: {source}")]
    BadBindAddr {
        /// Offending environment variable name.
        var: &'static str,
        /// Underlying parse error.
        source: std::net::AddrParseError,
    },
    /// The master key was not exactly [`MASTER_KEY_HEX_LEN`] chars of
    /// ASCII hex. We validate length + alphabet here so a misconfig
    /// fails fast at boot rather than deep inside `ffi::open_store`.
    #[error("`{0}` must be exactly 64 lowercase/uppercase hex characters")]
    BadMasterKey(&'static str),
}

/// Loopback server configuration.
///
/// `Debug` is derived, but the master key is held in
/// [`Zeroizing<String>`] whose own `Debug` prints the wrapped value;
/// to avoid leaking the key into logs the struct's `Debug` is
/// hand-implemented below to redact it.
#[derive(Clone)]
pub struct ServerConfig {
    /// Address the axum server binds to (loopback only).
    pub bind_addr: SocketAddr,
    /// Filesystem path of the SQLCipher-backed evidence store.
    pub store_path: String,
    /// 64-hex-char master key forwarded verbatim to
    /// [`ffi::open_store`].
    pub master_key_hex: Zeroizing<String>,
    /// Filesystem path of the SQLCipher-backed permission-tuple store.
    /// Permission grants are mirrored here so they survive a restart.
    pub permissions_path: String,
    /// Opt-in release-update-check configuration. Disabled by default;
    /// the `/internal/update_check` endpoint never touches the network
    /// unless this is enabled via [`crate::update_check::ENV_ENABLED`].
    pub update_check: crate::update_check::UpdateCheckConfig,
    /// Optional server-side synthesis-engine configuration. `Some` when
    /// [`ENV_SYNTHESIS_URL`] is set, in which case the boot path
    /// installs the managed-endpoint engine; `None` leaves the
    /// `/synthesis/domain` and `/synthesis/tenant` routes returning
    /// `503` (engine unavailable).
    pub synthesis: Option<SynthesisEngineSettings>,
}

/// Decode a 64-hex-char string into a 32-byte [`MasterKey`]. The bytes
/// are returned in a [`Zeroizing`] wrapper so the parsed key is wiped
/// from the heap on drop. Returns `None` if `hex` is not exactly
/// [`MASTER_KEY_HEX_LEN`] ASCII-hex characters.
#[must_use]
pub fn decode_master_key(hex: &str) -> Option<Zeroizing<MasterKey>> {
    if hex.len() != MASTER_KEY_HEX_LEN {
        return None;
    }
    let mut out: MasterKey = [0u8; MASTER_KEY_LEN];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(Zeroizing::new(out))
}

/// Default permission-store path derived from the evidence-store path:
/// a `permissions.db` file in the same directory. Falls back to a bare
/// `permissions.db` when `store_path` has no parent component.
fn default_permissions_path(store_path: &str) -> String {
    Path::new(store_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(
            || "permissions.db".to_string(),
            |dir| dir.join("permissions.db").to_string_lossy().into_owned(),
        )
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("bind_addr", &self.bind_addr)
            .field("store_path", &self.store_path)
            .field("permissions_path", &self.permissions_path)
            .field("update_check", &self.update_check)
            .field("synthesis", &self.synthesis)
            .field("master_key_hex", &"<redacted>")
            .finish()
    }
}

/// Resolved server-side synthesis-engine configuration.
///
/// When present on [`ServerConfig::synthesis`], the boot path installs
/// the managed-endpoint synthesis engine via
/// [`crate::configure_synthesis`] / [`ffi::configure_synthesis_engine`],
/// making the `/synthesis/domain` and `/synthesis/tenant` routes
/// functional. When absent, those routes return `503` (engine
/// unavailable) — the on-device channel tier (`/synthesis/trigger`) is
/// unaffected since it does not use this engine slot.
///
/// This is a plain projection of the caller-input subset of
/// [`ffi::SynthesisEngineConfig`]; [`Self::to_ffi`] performs the
/// mapping. No secret material lives here: `api_key_ref` is a
/// *reference* into the secret store, resolved by the engine at
/// dispatch time, never the raw key.
#[derive(Debug, Clone)]
pub struct SynthesisEngineSettings {
    /// HTTPS URL of the synthesis endpoint.
    pub url: String,
    /// Secret-store reference for the endpoint API key (not the key).
    pub api_key_ref: String,
    /// Model identifier forwarded to the endpoint.
    pub model_id: String,
    /// Response token cap; `0` defers to the engine default.
    pub max_tokens: u32,
    /// Per-request timeout (ms); `0` defers to the engine default.
    pub timeout_ms: u64,
    /// Optional allow-list of scope UUID strings. `None` allows every
    /// scope; `Some(empty)` refuses every scope (matching the TEE
    /// worker's binding semantics).
    pub scope_bindings: Option<Vec<String>>,
    /// Relax the synthesis health probe for single-tenant / dev
    /// deployments that legitimately run without scope bindings.
    pub single_tenant: bool,
    /// Per-endpoint requests-per-minute cap. `None` defers to the
    /// engine default; `Some(0)` is rejected by the engine.
    pub max_requests_per_minute: Option<u64>,
    /// Global rate-limit burst capacity; `0` defers to the default.
    pub rate_capacity: u32,
    /// Global rate-limit refill (tokens/sec); `0` defers to the default.
    pub rate_refill_per_sec: f64,
}

impl SynthesisEngineSettings {
    /// Assemble from the environment, returning `None` when
    /// [`ENV_SYNTHESIS_URL`] is unset/empty (synthesis engine disabled).
    ///
    /// Numeric vars that are unset or fail to parse fall back to `0`
    /// (the "use engine default" sentinel) so a stray value degrades to
    /// the library default rather than aborting boot; the URL is the
    /// single switch that gates the whole feature.
    fn from_env() -> Option<Self> {
        let url = non_empty_env(ENV_SYNTHESIS_URL)?;
        let scope_bindings = non_empty_env(ENV_SYNTHESIS_SCOPE_BINDINGS).map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        });
        Some(Self {
            url,
            api_key_ref: non_empty_env(ENV_SYNTHESIS_API_KEY_REF).unwrap_or_default(),
            model_id: non_empty_env(ENV_SYNTHESIS_MODEL_ID).unwrap_or_default(),
            max_tokens: parse_env_or_zero(ENV_SYNTHESIS_MAX_TOKENS),
            timeout_ms: parse_env_or_zero(ENV_SYNTHESIS_TIMEOUT_MS),
            scope_bindings,
            single_tenant: env_truthy(ENV_SYNTHESIS_SINGLE_TENANT),
            max_requests_per_minute: non_empty_env(ENV_SYNTHESIS_MAX_RPM)
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|&n| n != 0),
            rate_capacity: parse_env_or_zero(ENV_SYNTHESIS_RATE_CAPACITY),
            rate_refill_per_sec: parse_env_or_zero(ENV_SYNTHESIS_RATE_REFILL_PER_SEC),
        })
    }

    /// Project into the FFI configuration record consumed by
    /// [`ffi::configure_synthesis_engine`].
    #[must_use]
    pub fn to_ffi(&self) -> ffi::SynthesisEngineConfig {
        ffi::SynthesisEngineConfig {
            url: self.url.clone(),
            api_key_ref: self.api_key_ref.clone(),
            model_id: self.model_id.clone(),
            max_tokens: self.max_tokens,
            timeout_ms: self.timeout_ms,
            grammar: None,
            scope_bindings: self.scope_bindings.clone(),
            single_tenant: self.single_tenant,
            max_requests_per_minute: self.max_requests_per_minute,
            rate_capacity: self.rate_capacity,
            rate_refill_per_sec: self.rate_refill_per_sec,
        }
    }
}

/// Parse an environment variable as a `T`, returning `T::default()`
/// (zero for the numeric uses here) when unset, empty, or unparseable.
fn parse_env_or_zero<T: std::str::FromStr + Default>(key: &str) -> T {
    non_empty_env(key)
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or_default()
}

/// Interpret an environment variable as a boolean flag. Truthy values
/// are `1`/`true`/`yes`/`on` (case-insensitive); anything else
/// (including unset) is `false`.
fn env_truthy(key: &str) -> bool {
    non_empty_env(key).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Return `true` if `s` is exactly 64 ASCII-hex characters.
fn is_valid_master_key_hex(s: &str) -> bool {
    s.len() == MASTER_KEY_HEX_LEN && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl ServerConfig {
    /// Build a [`ServerConfig`] from the process environment.
    ///
    /// * [`ENV_BIND_ADDR`] — optional, defaults to
    ///   [`DEFAULT_BIND_ADDR`].
    /// * [`ENV_STORE_PATH`] — optional, defaults to
    ///   [`DEFAULT_STORE_PATH`].
    /// * [`ENV_MASTER_KEY`] — **required**, must be 64 hex chars.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the master key is missing/malformed
    /// or the bind address fails to parse.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw =
            non_empty_env(ENV_BIND_ADDR).unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
        let bind_addr =
            bind_raw
                .parse::<SocketAddr>()
                .map_err(|source| ConfigError::BadBindAddr {
                    var: ENV_BIND_ADDR,
                    source,
                })?;

        let store_path =
            non_empty_env(ENV_STORE_PATH).unwrap_or_else(|| DEFAULT_STORE_PATH.to_string());

        let master_key_hex = Zeroizing::new(
            non_empty_env(ENV_MASTER_KEY).ok_or(ConfigError::Missing(ENV_MASTER_KEY))?,
        );
        if !is_valid_master_key_hex(&master_key_hex) {
            return Err(ConfigError::BadMasterKey(ENV_MASTER_KEY));
        }

        let permissions_path = non_empty_env(ENV_PERMISSIONS_PATH)
            .unwrap_or_else(|| default_permissions_path(&store_path));

        Ok(Self {
            bind_addr,
            store_path,
            master_key_hex,
            permissions_path,
            update_check: crate::update_check::UpdateCheckConfig::from_env(),
            synthesis: SynthesisEngineSettings::from_env(),
        })
    }
}

/// Read an environment variable, returning `None` only when it is
/// unset or the empty string. A non-empty but whitespace-only value is
/// **passed through unchanged** so it reaches the relevant validator
/// and surfaces an actionable error (a stray-whitespace `bind_addr`
/// fails [`ConfigError::BadBindAddr`] rather than silently falling back
/// to the default, and a whitespace master key fails
/// [`ConfigError::BadMasterKey`] rather than masquerading as unset).
///
/// This intentionally differs from [`crate::update_check`]'s
/// same-named helper, which *does* treat whitespace-only as blank:
/// there the value only selects between an override and a hard-coded
/// default (no validator downstream), so leniency is harmless, whereas
/// here verbatim pass-through preserves diagnostics for a
/// misconfigured deployment.
fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_master_key() {
        assert!(!is_valid_master_key_hex("deadbeef"));
    }

    #[test]
    fn rejects_non_hex_master_key() {
        let bad = "z".repeat(MASTER_KEY_HEX_LEN);
        assert!(!is_valid_master_key_hex(&bad));
    }

    #[test]
    fn accepts_exactly_64_hex_chars() {
        let good = "a".repeat(MASTER_KEY_HEX_LEN);
        assert!(is_valid_master_key_hex(&good));
        let mixed = "0123456789abcdefABCDEF0123456789abcdefABCDEF0123456789abcdef0123";
        assert_eq!(mixed.len(), MASTER_KEY_HEX_LEN);
        assert!(is_valid_master_key_hex(mixed));
    }

    #[test]
    fn debug_redacts_master_key() {
        let cfg = ServerConfig {
            bind_addr: DEFAULT_BIND_ADDR.parse().unwrap(),
            store_path: "/tmp/x.db".into(),
            master_key_hex: Zeroizing::new("a".repeat(MASTER_KEY_HEX_LEN)),
            permissions_path: "/tmp/permissions.db".into(),
            update_check: crate::update_check::UpdateCheckConfig::default(),
            synthesis: None,
        };
        let rendered = format!("{cfg:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(&"a".repeat(MASTER_KEY_HEX_LEN)));
    }

    #[test]
    fn decode_master_key_round_trips() {
        let key = decode_master_key(&"ab".repeat(32)).expect("valid hex");
        assert_eq!(*key, [0xAB; 32]);
        assert!(decode_master_key("ab").is_none());
        assert!(decode_master_key(&"zz".repeat(32)).is_none());
    }

    #[test]
    fn default_permissions_path_is_store_sibling() {
        assert_eq!(
            default_permissions_path("/var/lib/knowledge/substrate.db"),
            "/var/lib/knowledge/permissions.db"
        );
        assert_eq!(default_permissions_path("substrate.db"), "permissions.db");
    }

    #[test]
    fn synthesis_settings_project_to_ffi_verbatim() {
        let settings = SynthesisEngineSettings {
            url: "https://synth.example/v1".into(),
            api_key_ref: "SYNTH_KEY_REF".into(),
            model_id: "slm-recap-v1".into(),
            max_tokens: 512,
            timeout_ms: 90_000,
            scope_bindings: Some(vec!["a".into(), "b".into()]),
            single_tenant: true,
            max_requests_per_minute: Some(120),
            rate_capacity: 16,
            rate_refill_per_sec: 2.0,
        };
        let ffi_cfg = settings.to_ffi();
        assert_eq!(ffi_cfg.url, "https://synth.example/v1");
        assert_eq!(ffi_cfg.api_key_ref, "SYNTH_KEY_REF");
        assert_eq!(ffi_cfg.model_id, "slm-recap-v1");
        assert_eq!(ffi_cfg.max_tokens, 512);
        assert_eq!(ffi_cfg.timeout_ms, 90_000);
        assert_eq!(ffi_cfg.grammar, None);
        assert_eq!(
            ffi_cfg.scope_bindings,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert!(ffi_cfg.single_tenant);
        assert_eq!(ffi_cfg.max_requests_per_minute, Some(120));
        assert_eq!(ffi_cfg.rate_capacity, 16);
        assert!((ffi_cfg.rate_refill_per_sec - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_env_or_zero_falls_back_on_unset_and_garbage() {
        // Unique keys so the test is independent of ambient env.
        assert_eq!(parse_env_or_zero::<u32>("KNOWLEDGE_TEST_UNSET_NUM_XYZ"), 0);
        std::env::set_var("KNOWLEDGE_TEST_GARBAGE_NUM_XYZ", "not-a-number");
        assert_eq!(
            parse_env_or_zero::<u64>("KNOWLEDGE_TEST_GARBAGE_NUM_XYZ"),
            0
        );
        std::env::remove_var("KNOWLEDGE_TEST_GARBAGE_NUM_XYZ");
    }

    #[test]
    fn env_truthy_accepts_common_affirmatives() {
        for v in ["1", "true", "TRUE", "Yes", "on"] {
            std::env::set_var("KNOWLEDGE_TEST_FLAG_XYZ", v);
            assert!(
                env_truthy("KNOWLEDGE_TEST_FLAG_XYZ"),
                "{v} should be truthy"
            );
        }
        for v in ["0", "false", "no", "off", "maybe"] {
            std::env::set_var("KNOWLEDGE_TEST_FLAG_XYZ", v);
            assert!(
                !env_truthy("KNOWLEDGE_TEST_FLAG_XYZ"),
                "{v} should be falsey"
            );
        }
        std::env::remove_var("KNOWLEDGE_TEST_FLAG_XYZ");
        assert!(!env_truthy("KNOWLEDGE_TEST_FLAG_UNSET_XYZ"));
    }
}
