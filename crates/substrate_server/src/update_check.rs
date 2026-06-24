//! Optional, opt-in release-update check.
//!
//! Compares the running binary's version (`env!("CARGO_PKG_VERSION")`,
//! stamped at compile time) against the latest GitHub Release tag for
//! the project repository. The result is surfaced on demand via
//! `GET /internal/update_check`; **nothing here runs at startup**, so
//! a misconfigured or unreachable GitHub never delays or blocks the
//! server boot path.
//!
//! ## HTTP conventions
//!
//! This module does **not** depend on `reqwest` directly. It speaks
//! to GitHub through the workspace's
//! [`connector_framework::http::HttpTransport`] trait — the same
//! abstraction every connector uses. Production builds wire in the
//! reqwest-backed [`connector_framework::http::BlockingHttpTransport`]
//! (only linked under the `http-client` feature, exactly like the
//! connector runtime); unit tests inject the in-memory
//! [`connector_framework::http::MockHttpTransport`]. The core logic in
//! [`check_with_transport`] is transport-agnostic and fully testable
//! offline.
//!
//! ## Opt-in
//!
//! The check is disabled unless [`ENV_ENABLED`] is set to a truthy
//! value (`1`/`true`/`yes`/`on`). When disabled the endpoint returns a
//! cheap `{ "enabled": false, … }` body without touching the network.

use axum::extract::State;
use axum::Json;
use connector_framework::http::{HttpRequest, HttpTransport};
use serde::Serialize;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Environment variable that opts the update check in. Truthy values
/// are `1`, `true`, `yes`, `on` (case-insensitive); anything else
/// (including unset) leaves the check disabled.
pub const ENV_ENABLED: &str = "KNOWLEDGE_UPDATE_CHECK_ENABLED";
/// Environment variable overriding the `owner/name` repository slug
/// the latest-release lookup targets. Defaults to [`DEFAULT_REPO`].
pub const ENV_REPO: &str = "KNOWLEDGE_UPDATE_CHECK_REPO";
/// Environment variable overriding the GitHub API base URL. Defaults
/// to [`DEFAULT_API_BASE_URL`]. Primarily a test / GitHub-Enterprise
/// seam — operators on public GitHub never need to set it.
pub const ENV_API_BASE: &str = "KNOWLEDGE_UPDATE_CHECK_API_BASE";

/// Default `owner/name` slug. Kept in sync with the workspace
/// `repository` field in the root `Cargo.toml`.
pub const DEFAULT_REPO: &str = "kennguy3n/knowledge";
/// Default GitHub REST API base (no trailing slash).
pub const DEFAULT_API_BASE_URL: &str = "https://api.github.com";

/// Errors raised while performing an update check. `Disabled` is
/// modelled explicitly so the caller can distinguish "operator turned
/// this off" from a genuine upstream failure.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The check was invoked while disabled by configuration.
    #[error("update check is disabled")]
    Disabled,
    /// The HTTP transport failed (DNS, TLS, timeout, retry budget
    /// exhausted). Wraps the transport's own message.
    #[error("update check transport error: {0}")]
    Transport(String),
    /// GitHub answered with a non-success, non-404 status. 404 is
    /// treated as "no releases yet" rather than an error (see
    /// [`check_with_transport`]).
    #[error("update check upstream returned HTTP {status}")]
    Upstream {
        /// The HTTP status code GitHub returned.
        status: u16,
    },
    /// The response body could not be parsed as the expected GitHub
    /// release JSON (missing/empty `tag_name`, invalid UTF-8, …).
    #[error("update check could not parse release metadata: {0}")]
    Parse(String),
}

impl From<UpdateError> for ApiError {
    fn from(e: UpdateError) -> Self {
        use ffi::FfiError;
        match e {
            // Disabled is handled before any FFI mapping in the
            // handler, but map it defensively to a 503 anyway.
            UpdateError::Disabled => ApiError(FfiError::Unavailable {
                subsystem: "update-check (disabled)".to_string(),
            }),
            UpdateError::Transport(message) => ApiError(FfiError::Unavailable {
                subsystem: format!("update-check transport: {message}"),
            }),
            // Upstream/parse failures mean we reached GitHub but it
            // misbehaved — a gateway-class fault from our point of
            // view, mirroring how connector upstream errors map.
            UpdateError::Upstream { status } => ApiError(FfiError::Connector {
                message: format!("update-check upstream HTTP {status}"),
            }),
            UpdateError::Parse(message) => ApiError(FfiError::Connector {
                message: format!("update-check parse: {message}"),
            }),
        }
    }
}

/// Resolved update-check configuration. Cheap to clone (three small
/// `String`s) so it can live on [`crate::config::ServerConfig`] and be
/// handed to a `spawn_blocking` closure by value.
#[derive(Debug, Clone)]
pub struct UpdateCheckConfig {
    /// Whether the check is enabled. When `false` the endpoint never
    /// touches the network.
    pub enabled: bool,
    /// `owner/name` repository slug the latest-release lookup targets.
    pub repo: String,
    /// GitHub REST API base URL (no trailing slash).
    pub api_base_url: String,
    /// The running binary's version, from `CARGO_PKG_VERSION`.
    pub current_version: String,
}

impl Default for UpdateCheckConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: DEFAULT_REPO.to_string(),
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl UpdateCheckConfig {
    /// Build the config from the process environment, layering env
    /// overrides over the [`Default`] values. Never fails: an unset or
    /// empty variable falls back to the default, so the worst a
    /// misconfiguration can do is leave the (already opt-in) check
    /// disabled.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            enabled: std::env::var(ENV_ENABLED)
                .ok()
                .is_some_and(|v| is_truthy(&v)),
            // Both feed directly into `latest_release_url`, so trim
            // surrounding whitespace: a stray-space override like
            // " owner/name " would otherwise produce a URL with embedded
            // spaces and a confusing transport error rather than a clean
            // lookup.
            repo: non_blank_env(ENV_REPO)
                .map(|s| s.trim().to_string())
                .unwrap_or(defaults.repo),
            api_base_url: non_blank_env(ENV_API_BASE)
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .unwrap_or(defaults.api_base_url),
            current_version: defaults.current_version,
        }
    }

    /// The absolute URL of the `releases/latest` endpoint for [`Self::repo`].
    #[must_use]
    pub fn latest_release_url(&self) -> String {
        format!("{}/repos/{}/releases/latest", self.api_base_url, self.repo)
    }
}

/// Outcome of a successful update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    /// The running binary's version.
    pub current_version: String,
    /// The latest published release tag (with any leading `v`
    /// stripped), or `None` when the repository has no releases yet.
    pub latest_version: Option<String>,
    /// `true` iff a strictly-newer release than [`Self::current_version`]
    /// is available.
    pub update_available: bool,
}

/// Body returned by the `GET /internal/update_check` endpoint. Flattens
/// [`UpdateStatus`] so the wire shape stays flat (`enabled`,
/// `current_version`, `latest_version`, `update_available`).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheckResponse {
    /// Whether the check is enabled. When `false`, `latest_version` is
    /// `null` and `update_available` is `false` — the network was not
    /// touched.
    pub enabled: bool,
    /// The running binary's version.
    pub current_version: String,
    /// Latest release tag, or `null` when disabled / no releases.
    pub latest_version: Option<String>,
    /// Whether a newer release is available.
    pub update_available: bool,
}

impl UpdateCheckResponse {
    /// The cheap "disabled" response — no network access performed.
    #[must_use]
    pub fn disabled(cfg: &UpdateCheckConfig) -> Self {
        Self {
            enabled: false,
            current_version: cfg.current_version.clone(),
            latest_version: None,
            update_available: false,
        }
    }

    /// Wrap a successful [`UpdateStatus`] as an enabled response.
    #[must_use]
    pub fn from_status(status: UpdateStatus) -> Self {
        Self {
            enabled: true,
            current_version: status.current_version,
            latest_version: status.latest_version,
            update_available: status.update_available,
        }
    }
}

/// Run the update check against `transport`, comparing GitHub's latest
/// release tag for `cfg.repo` against `cfg.current_version`.
///
/// A `404` from GitHub is treated as "the repository has no releases
/// yet" — a normal pre-release state, not an error — and yields
/// `latest_version: None, update_available: false`.
///
/// # Errors
///
/// * [`UpdateError::Disabled`] if `cfg.enabled` is `false`.
/// * [`UpdateError::Transport`] if the transport could not complete the
///   request.
/// * [`UpdateError::Upstream`] for any non-success status other than
///   `404`.
/// * [`UpdateError::Parse`] if the release JSON lacks a usable
///   `tag_name`.
pub fn check_with_transport(
    transport: &dyn HttpTransport,
    cfg: &UpdateCheckConfig,
) -> Result<UpdateStatus, UpdateError> {
    if !cfg.enabled {
        return Err(UpdateError::Disabled);
    }

    let request = HttpRequest::get(cfg.latest_release_url())
        // GitHub requires a User-Agent on every API request.
        .with_header("User-Agent", user_agent(&cfg.current_version))
        .with_header("Accept", "application/vnd.github+json")
        .with_header("X-GitHub-Api-Version", "2022-11-28");

    let response = transport
        .execute(request)
        .map_err(|e| UpdateError::Transport(e.to_string()))?;

    // A repo with no published releases returns 404 — a normal state,
    // not a failure.
    if response.status == 404 {
        return Ok(UpdateStatus {
            current_version: cfg.current_version.clone(),
            latest_version: None,
            update_available: false,
        });
    }
    if !response.is_success() {
        return Err(UpdateError::Upstream {
            status: response.status,
        });
    }

    let tag = parse_latest_tag(&response.body)?;
    let latest = normalize_version(&tag).to_string();
    let update_available = is_newer(&latest, &cfg.current_version);

    Ok(UpdateStatus {
        current_version: cfg.current_version.clone(),
        latest_version: Some(latest),
        update_available,
    })
}

/// The `User-Agent` GitHub sees. Identifies the substrate + version so
/// abuse reports are actionable, per GitHub's API guidelines.
fn user_agent(version: &str) -> String {
    format!("knowledge-substrate/{version} (+{DEFAULT_REPO})")
}

/// Extract a non-empty `tag_name` from a GitHub `releases/latest` JSON
/// body.
fn parse_latest_tag(body: &[u8]) -> Result<String, UpdateError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| UpdateError::Parse(e.to_string()))?;
    let tag = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| UpdateError::Parse("missing or empty `tag_name`".to_string()))?;
    Ok(tag.to_string())
}

/// Strip a single leading `v`/`V` from a version/tag string
/// (`v1.2.3` → `1.2.3`). Tags without the prefix pass through
/// unchanged.
#[must_use]
pub fn normalize_version(tag: &str) -> &str {
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
}

/// Numeric `(major, minor, patch)` core of a SemVer string, ignoring
/// any pre-release (`-rc.1`) or build (`+meta`) suffix. Returns `None`
/// if the three leading dotted components are not all valid integers.
fn parse_semver_core(version: &str) -> Option<(u64, u64, u64)> {
    // Drop build metadata and pre-release: `1.2.3-rc.1+build` → `1.2.3`.
    let core = version.split(['+', '-']).next().unwrap_or(version).trim();
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    // Reject trailing junk like `1.2.3.4` so we don't silently treat a
    // malformed tag as equal to its truncated prefix.
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// A single pre-release identifier (SemVer §9). Per §11.4.3 a numeric
/// identifier always has lower precedence than an alphanumeric one;
/// numeric identifiers compare numerically and alphanumeric ones
/// compare lexically in ASCII order.
#[derive(Debug, PartialEq, Eq)]
enum PreReleaseId {
    Numeric(u64),
    AlphaNumeric(String),
}

impl Ord for PreReleaseId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        use PreReleaseId::{AlphaNumeric, Numeric};
        match (self, other) {
            (Numeric(a), Numeric(b)) => a.cmp(b),
            (AlphaNumeric(a), AlphaNumeric(b)) => a.cmp(b),
            (Numeric(_), AlphaNumeric(_)) => Ordering::Less,
            (AlphaNumeric(_), Numeric(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for PreReleaseId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A SemVer version reduced to the pieces that drive precedence: the
/// numeric `major.minor.patch` core and the ordered pre-release
/// identifiers (empty for a normal release). Build metadata is parsed
/// off and ignored, per SemVer §10.
struct SemVer {
    core: (u64, u64, u64),
    pre: Vec<PreReleaseId>,
}

impl PartialEq for SemVer {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for SemVer {}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A normal version outranks a pre-release of the same
                // core (SemVer §11.3): `1.2.3` > `1.2.3-rc.1`.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                // Both pre-release: compare identifiers left to right;
                // a larger set of identifiers wins when all the
                // preceding ones are equal (SemVer §11.4).
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Parse a normalized version string into a [`SemVer`] for precedence
/// comparison. Returns `None` for anything that is not a valid
/// `major.minor.patch[-pre]` (build metadata is allowed and ignored).
fn parse_semver(version: &str) -> Option<SemVer> {
    let version = version.trim();
    // Strip build metadata (`+…`) first — it never affects precedence.
    let without_build = version.split('+').next().unwrap_or(version);
    // Separate the core from the optional pre-release at the first `-`.
    let (core_str, pre_str) = match without_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (without_build, None),
    };
    let core = parse_semver_core(core_str)?;
    let pre = match pre_str {
        None => Vec::new(),
        // A trailing `-` with no identifiers is malformed.
        Some("") => return None,
        Some(pre) => {
            let mut ids = Vec::new();
            for ident in pre.split('.') {
                // Empty identifier (e.g. `rc..1`) is malformed.
                if ident.is_empty() {
                    return None;
                }
                if ident.bytes().all(|b| b.is_ascii_digit()) {
                    // SemVer §9 forbids leading zeros on numeric ids.
                    if ident.len() > 1 && ident.starts_with('0') {
                        return None;
                    }
                    ids.push(PreReleaseId::Numeric(ident.parse::<u64>().ok()?));
                } else {
                    ids.push(PreReleaseId::AlphaNumeric(ident.to_string()));
                }
            }
            ids
        }
    };
    Some(SemVer { core, pre })
}

/// Whether `latest` is a strictly newer release than `current`,
/// following SemVer 2.0.0 precedence (§11) including pre-releases:
///
/// * The numeric `major.minor.patch` core is compared first.
/// * With equal cores, a normal release outranks a pre-release — so a
///   running `1.2.3-rc.1` *does* see the stable `1.2.3` as an update,
///   while `1.2.3-rc.1` is **not** newer than the already-running
///   `1.2.3`.
/// * Two pre-releases of the same core compare identifier by
///   identifier (`-rc.2` is newer than `-rc.1`).
///
/// Build metadata is ignored. A single leading `v`/`V` is tolerated on
/// either argument (via [`normalize_version`]) so this `pub` helper is
/// robust for external callers that pass a raw tag like `v1.2.3`. If
/// either side still fails to parse we conservatively report "no update"
/// rather than nagging on a tag we do not understand.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (
        parse_semver(normalize_version(latest)),
        parse_semver(normalize_version(current)),
    ) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Truthy parse for the opt-in env var. Accepts `1`, `true`, `yes`,
/// `on` (case-insensitive); everything else is false.
fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Read an environment variable, treating unset or *blank* (empty /
/// whitespace-only) as `None`. The returned value is untrimmed; only
/// the blank test ignores whitespace. Leniency is safe here because
/// these vars merely choose between an override and a hard-coded
/// default — there is no downstream validator to rob of a useful error.
///
/// Named distinctly from [`crate::config`]'s `non_empty_env`, which
/// rejects only the empty string and passes whitespace through verbatim
/// to preserve diagnostics for a misconfigured `bind_addr` / master key
/// (see its doc comment). The differing names make the differing
/// whitespace contract obvious at every call site.
fn non_blank_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// `GET /internal/update_check` — report whether a newer release than
/// the running binary is published on GitHub.
///
/// When disabled (the default) this returns immediately with
/// `{ "enabled": false, … }` and performs no network I/O, so it is
/// always safe to probe. When enabled it dispatches the (blocking)
/// HTTP lookup on the blocking thread pool so the async runtime is
/// never stalled.
pub async fn update_check_handler(
    State(st): State<AppState>,
) -> ApiResult<Json<UpdateCheckResponse>> {
    let cfg = st.config.update_check.clone();
    if !cfg.enabled {
        return Ok(Json(UpdateCheckResponse::disabled(&cfg)));
    }
    run_enabled_check(cfg).await
}

/// Production path: build the reqwest-backed transport and run the
/// check on the blocking pool. Only compiled when the `http-client`
/// feature links the transport in (exactly as the connector handlers
/// gate their real transport).
#[cfg(feature = "http-client")]
async fn run_enabled_check(cfg: UpdateCheckConfig) -> ApiResult<Json<UpdateCheckResponse>> {
    use ffi::FfiError;

    let status = tokio::task::spawn_blocking(move || -> Result<UpdateStatus, UpdateError> {
        let transport = connector_framework::http::BlockingHttpTransport::new()
            .map_err(|e| UpdateError::Transport(e.to_string()))?;
        check_with_transport(&transport, &cfg)
    })
    .await
    .map_err(|join| {
        ApiError(FfiError::Unavailable {
            subsystem: format!("update-check blocking-pool join failure: {join}"),
        })
    })??;
    Ok(Json(UpdateCheckResponse::from_status(status)))
}

/// Offline / cross-compile path: no reqwest transport is linked in, so
/// the endpoint reports the subsystem as unavailable — mirroring the
/// connector handlers' `not(http-client)` behaviour.
#[cfg(not(feature = "http-client"))]
#[allow(clippy::unused_async)]
async fn run_enabled_check(_cfg: UpdateCheckConfig) -> ApiResult<Json<UpdateCheckResponse>> {
    Err(ApiError(ffi::FfiError::Unavailable {
        subsystem: "update-check-http-client".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use connector_framework::http::{HttpMethod, MockHttpTransport, MockResponse};

    fn cfg(version: &str) -> UpdateCheckConfig {
        UpdateCheckConfig {
            enabled: true,
            repo: "kennguy3n/knowledge".to_string(),
            api_base_url: "https://api.github.com".to_string(),
            current_version: version.to_string(),
        }
    }

    fn release_body(tag: &str) -> Vec<u8> {
        format!(r#"{{"tag_name":"{tag}","name":"release"}}"#).into_bytes()
    }

    #[test]
    fn normalize_strips_single_leading_v() {
        assert_eq!(normalize_version("v1.2.3"), "1.2.3");
        assert_eq!(normalize_version("V1.2.3"), "1.2.3");
        assert_eq!(normalize_version("1.2.3"), "1.2.3");
        // Only one prefix char is stripped.
        assert_eq!(normalize_version("vv1.2.3"), "v1.2.3");
    }

    #[test]
    fn semver_core_ignores_prerelease_and_build() {
        assert_eq!(parse_semver_core("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver_core("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_semver_core("1.2.3+build.7"), Some((1, 2, 3)));
        assert_eq!(parse_semver_core("10.0.0-rc.1+meta"), Some((10, 0, 0)));
    }

    #[test]
    fn semver_core_rejects_malformed() {
        assert_eq!(parse_semver_core("1.2"), None);
        assert_eq!(parse_semver_core("1.2.3.4"), None);
        assert_eq!(parse_semver_core("not-a-version"), None);
        assert_eq!(parse_semver_core(""), None);
    }

    #[test]
    fn is_newer_compares_numeric_components() {
        assert!(is_newer("1.2.4", "1.2.3"));
        assert!(is_newer("1.3.0", "1.2.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.2.2", "1.2.3"));
        // Numeric, not lexicographic: 10 > 9.
        assert!(is_newer("1.10.0", "1.9.0"));
        // Unparseable → conservatively not-newer.
        assert!(!is_newer("garbage", "1.2.3"));
        assert!(!is_newer("1.2.4", "garbage"));
    }

    #[test]
    fn is_newer_honours_prerelease_precedence() {
        // A normal release outranks a pre-release of the same core, so
        // a host running an rc *is* offered the stable release.
        assert!(is_newer("1.2.3", "1.2.3-rc.1"));
        // ...but the rc is not newer than the already-running stable.
        assert!(!is_newer("1.2.3-rc.1", "1.2.3"));
        // Later pre-release of the same core is newer.
        assert!(is_newer("1.2.3-rc.2", "1.2.3-rc.1"));
        assert!(!is_newer("1.2.3-rc.1", "1.2.3-rc.2"));
        // Equal pre-releases are not newer.
        assert!(!is_newer("1.2.3-rc.1", "1.2.3-rc.1"));
        // Numeric identifiers rank below alphanumeric ones (§11.4.3),
        // and a longer identifier set wins when prefixes match (§11.4.4).
        assert!(is_newer("1.0.0-alpha.1", "1.0.0-alpha"));
        assert!(is_newer("1.0.0-beta", "1.0.0-alpha.99"));
        // A newer core always wins regardless of pre-release suffix.
        assert!(is_newer("2.0.0-rc.1", "1.9.9"));
        // Build metadata never affects precedence (§10).
        assert!(!is_newer("1.2.3+build.9", "1.2.3+build.1"));
    }

    #[test]
    fn is_newer_tolerates_v_prefixed_tags() {
        // A `pub` caller may pass a raw release tag; the leading `v`/`V`
        // is normalized away on either side rather than failing to parse.
        assert!(is_newer("v1.2.4", "1.2.3"));
        assert!(is_newer("v1.2.4", "v1.2.3"));
        assert!(is_newer("1.2.4", "V1.2.3"));
        assert!(!is_newer("v1.2.3", "v1.2.3"));
        assert!(!is_newer("v1.2.2", "1.2.3"));
    }

    #[test]
    fn parse_semver_rejects_malformed_prerelease() {
        assert!(parse_semver("1.2.3-").is_none());
        assert!(parse_semver("1.2.3-rc..1").is_none());
        // Leading zero on a numeric identifier is invalid (§9).
        assert!(parse_semver("1.2.3-rc.01").is_none());
        // Well-formed pre-release parses.
        assert!(parse_semver("1.2.3-rc.1").is_some());
    }

    #[test]
    fn disabled_config_short_circuits() {
        let mut c = cfg("1.0.0");
        c.enabled = false;
        let mock = MockHttpTransport::new();
        let err = check_with_transport(&mock, &c).unwrap_err();
        assert!(matches!(err, UpdateError::Disabled));
        // No request should have been dispatched.
        assert!(mock.recorded().is_empty());
    }

    #[test]
    fn detects_available_update() {
        let c = cfg("1.0.0");
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            c.latest_release_url(),
            MockResponse::ok_json(release_body("v1.2.0")),
        );
        let status = check_with_transport(&mock, &c).unwrap();
        assert_eq!(status.current_version, "1.0.0");
        assert_eq!(status.latest_version.as_deref(), Some("1.2.0"));
        assert!(status.update_available);

        // Assert we hit the right endpoint with a User-Agent.
        let reqs = mock.recorded();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url, c.latest_release_url());
        assert!(reqs[0].headers.iter().any(
            |(k, v)| k.eq_ignore_ascii_case("user-agent") && v.contains("knowledge-substrate")
        ));
    }

    #[test]
    fn reports_up_to_date_when_equal() {
        let c = cfg("1.2.0");
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            c.latest_release_url(),
            MockResponse::ok_json(release_body("v1.2.0")),
        );
        let status = check_with_transport(&mock, &c).unwrap();
        assert_eq!(status.latest_version.as_deref(), Some("1.2.0"));
        assert!(!status.update_available);
    }

    #[test]
    fn running_ahead_is_not_an_update() {
        let c = cfg("2.0.0");
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            c.latest_release_url(),
            MockResponse::ok_json(release_body("v1.9.9")),
        );
        let status = check_with_transport(&mock, &c).unwrap();
        assert!(!status.update_available);
    }

    #[test]
    fn no_releases_yet_is_not_an_error() {
        let c = cfg("1.0.0");
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            c.latest_release_url(),
            MockResponse::status(404, br#"{"message":"Not Found"}"#.to_vec()),
        );
        let status = check_with_transport(&mock, &c).unwrap();
        assert_eq!(status.latest_version, None);
        assert!(!status.update_available);
    }

    #[test]
    fn upstream_error_surfaces() {
        let c = cfg("1.0.0");
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            c.latest_release_url(),
            MockResponse::status(500, b"boom".to_vec()),
        );
        let err = check_with_transport(&mock, &c).unwrap_err();
        assert!(matches!(err, UpdateError::Upstream { status: 500 }));
    }

    #[test]
    fn malformed_body_is_parse_error() {
        let c = cfg("1.0.0");
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            c.latest_release_url(),
            MockResponse::ok_json(br#"{"name":"no tag here"}"#.to_vec()),
        );
        let err = check_with_transport(&mock, &c).unwrap_err();
        assert!(matches!(err, UpdateError::Parse(_)));
    }

    #[test]
    fn empty_tag_is_parse_error() {
        let c = cfg("1.0.0");
        let mock = MockHttpTransport::new();
        mock.expect(
            HttpMethod::Get,
            c.latest_release_url(),
            MockResponse::ok_json(release_body("")),
        );
        let err = check_with_transport(&mock, &c).unwrap_err();
        assert!(matches!(err, UpdateError::Parse(_)));
    }

    #[test]
    fn response_helpers_round_trip() {
        let c = cfg("1.0.0");
        let disabled = UpdateCheckResponse::disabled(&c);
        assert!(!disabled.enabled);
        assert_eq!(disabled.current_version, "1.0.0");
        assert_eq!(disabled.latest_version, None);
        assert!(!disabled.update_available);

        let status = UpdateStatus {
            current_version: "1.0.0".to_string(),
            latest_version: Some("1.1.0".to_string()),
            update_available: true,
        };
        let enabled = UpdateCheckResponse::from_status(status);
        assert!(enabled.enabled);
        assert!(enabled.update_available);
        assert_eq!(enabled.latest_version.as_deref(), Some("1.1.0"));
    }
}
