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
            .field("master_key_hex", &"<redacted>")
            .finish()
    }
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
        })
    }
}

/// Read an environment variable, returning `None` when it is unset or
/// blank (empty or whitespace-only), so an explicit empty/whitespace
/// value is treated the same as "unset" for default substitution. The
/// returned value is the original, untrimmed string — only the
/// emptiness test ignores surrounding whitespace. This mirrors the
/// identically-named helper in [`crate::update_check`].
fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
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
}
