//! Router configuration.

use serde::{Deserialize, Serialize};

/// Default warm-up prompt used by [`crate::InferenceRouter::warm_up`].
/// Picked to be short, deterministic, and trigger token cache fill.
pub const WARM_UP_PROMPT: &str = "knowledge substrate boot probe";

/// Default idle-unload timeout — adapters whose last call is older
/// than this are unloaded from memory by
/// [`crate::InferenceRouter::sweep_idle_adapters`].
pub const IDLE_UNLOAD_TIMEOUT_SECS: u64 = 60;

/// Default loopback URL of the bundled llama.cpp server, used when no
/// `KNOWLEDGE_SLM_SERVER_URL` override is supplied.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8081";

/// Default on-disk path of the SLM model artifact, used when no
/// `KNOWLEDGE_SLM_MODEL_PATH` override is supplied.
pub const DEFAULT_MODEL_PATH: &str = "/var/lib/knowledge/slm.gguf";

/// RAM threshold (in bytes) below which the device is classified
/// as [`DeviceTier::Low`]. 2 GiB is chosen because SLM inference
/// models typically require >2 GiB of working memory.
pub const LOW_TIER_RAM_THRESHOLD: u64 = 2 * 1024 * 1024 * 1024;

/// RAM threshold (in bytes) at or above which the device is
/// classified as [`DeviceTier::High`]. 8 GiB allows comfortable
/// full SLM synthesis with model + context window in memory.
pub const HIGH_TIER_RAM_THRESHOLD: u64 = 8 * 1024 * 1024 * 1024;

/// Device tier — drives which adapters / tasks are admitted.
///
/// Per `docs/technical/architecture.md` §3 a `Low`-tier device runs only the
/// encoder-only [`crate::FallbackAdapter`]; `Medium` adds llama.cpp
/// classification; `High` enables full SLM synthesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceTier {
    /// Low-end mobile or embedded — encoder-only, no SLM.
    Low,
    /// Mid-range — runs llama.cpp classification but not synthesis.
    Medium,
    /// High-end — runs full SLM synthesis on-device.
    High,
}

impl DeviceTier {
    /// Stable string tag for diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Auto-detect device tier from available system RAM.
    ///
    /// Heuristic:
    /// - < 2 GiB → `Low` (encoder-only, no SLM)
    /// - 2–8 GiB → `Medium` (llama.cpp classification)
    /// - ≥ 8 GiB → `High` (full SLM synthesis)
    ///
    /// Falls back to `Medium` if RAM cannot be determined (e.g.
    /// unsupported platform, sandboxed environment).
    ///
    /// The result is cached in a process-global [`std::sync::OnceLock`]
    /// so repeated calls (including multiple `RouterConfig::new()`
    /// constructions) pay the syscall cost at most once.
    pub fn auto_detect() -> Self {
        static CACHED: std::sync::OnceLock<DeviceTier> = std::sync::OnceLock::new();
        *CACHED.get_or_init(|| match detect_total_ram_bytes() {
            Some(bytes) if bytes < LOW_TIER_RAM_THRESHOLD => Self::Low,
            Some(bytes) if bytes >= HIGH_TIER_RAM_THRESHOLD => Self::High,
            Some(_) => Self::Medium,
            None => {
                tracing::debug!("could not detect system RAM; falling back to DeviceTier::Medium");
                Self::Medium
            }
        })
    }
}

/// Attempt to read total *available* RAM in bytes from the OS.
///
/// Uses direct syscalls / kernel interfaces — no subprocess is
/// spawned, making this safe for sandboxed environments and fast
/// enough for hot-path construction. On Linux, the result is
/// cgroup-aware: if a memory limit is set (Docker, Kubernetes,
/// systemd), the returned value is `min(host_ram, cgroup_limit)`.
///
/// Returns `None` on unsupported platforms or if the query fails.
#[allow(unsafe_code)] // FFI calls to libc / kernel32 — trivially safe.
fn detect_total_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `sysconf` is safe to call with any `_SC_*`
        // constant; it returns -1 on error (handled below).
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
        let host_bytes = if pages > 0 && page_size > 0 {
            u64::try_from(pages)
                .ok()?
                .checked_mul(u64::try_from(page_size).ok()?)?
        } else {
            return None;
        };
        // In containers, sysconf reports the *host* RAM, not the
        // cgroup limit. Read the cgroup memory cap and take the
        // minimum so we respect container budgets.
        let cgroup_limit = cgroup_memory_limit_bytes();
        Some(cgroup_limit.map_or(host_bytes, |limit| limit.min(host_bytes)))
    }
    #[cfg(target_os = "macos")]
    {
        // sysctlbyname("hw.memsize") returns a u64 directly — no
        // subprocess required.
        let mut memsize: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let name = b"hw.memsize\0";
        // SAFETY: `memsize` is a properly-aligned, initialized u64;
        // `size` is its byte length. The pointer casts are valid
        // because the kernel writes exactly `size_of::<u64>()` bytes
        // into the output buffer. `name` is a NUL-terminated byte
        // literal. We pass null for newp/newlen (read-only query).
        let ret = unsafe {
            libc::sysctlbyname(
                name.as_ptr().cast(),
                (&mut memsize as *mut u64).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if ret == 0 && memsize > 0 {
            Some(memsize)
        } else {
            None
        }
    }
    #[cfg(target_os = "windows")]
    {
        // GlobalMemoryStatusEx — available since Windows XP, no
        // deprecated `wmic` subprocess.
        #[repr(C)]
        #[allow(non_snake_case)]
        struct MEMORYSTATUSEX {
            dwLength: u32,
            dwMemoryLoad: u32,
            ullTotalPhys: u64,
            ullAvailPhys: u64,
            ullTotalPageFile: u64,
            ullAvailPageFile: u64,
            ullTotalVirtual: u64,
            ullAvailVirtual: u64,
            ullAvailExtendedVirtual: u64,
        }
        extern "system" {
            fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
        }
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            dwMemoryLoad: 0,
            ullTotalPhys: 0,
            ullAvailPhys: 0,
            ullTotalPageFile: 0,
            ullAvailPageFile: 0,
            ullTotalVirtual: 0,
            ullAvailVirtual: 0,
            ullAvailExtendedVirtual: 0,
        };
        // SAFETY: `status` is fully initialized with `dwLength`
        // set to the struct's size. `GlobalMemoryStatusEx` writes
        // only within the struct's bounds and returns 0 on failure.
        let ret = unsafe { GlobalMemoryStatusEx(&mut status) };
        if ret != 0 && status.ullTotalPhys > 0 {
            Some(status.ullTotalPhys)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Read the cgroup memory limit, trying cgroup v2 first, then v1.
///
/// Returns `None` when not running in a cgroup or if the limit is
/// "max" (unbounded).
#[cfg(target_os = "linux")]
fn cgroup_memory_limit_bytes() -> Option<u64> {
    // cgroup v2: /sys/fs/cgroup/memory.max ("max" = unbounded)
    if let Ok(contents) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        let trimmed = contents.trim();
        if trimmed != "max" {
            if let Ok(limit) = trimmed.parse::<u64>() {
                return Some(limit);
            }
        }
        return None;
    }
    // cgroup v1: /sys/fs/cgroup/memory/memory.limit_in_bytes
    // A very large value (close to i64::MAX or page-aligned) means
    // "no limit set", which we treat as None.
    if let Ok(contents) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        if let Ok(limit) = contents.trim().parse::<u64>() {
            // The kernel reports PAGE_ALIGN(i64::MAX) when no limit
            // is set — on a 4 KiB-page system that's 0x7FFF_FFFF_FFFF_F000.
            // Treat anything above 2^62 as "effectively unlimited".
            if limit < (1u64 << 62) {
                return Some(limit);
            }
        }
    }
    None
}

/// Default sampling seed. A *fixed* seed is the heart of the
/// reproducibility fix: with `llama-server`'s default seed (`-1`)
/// every `/completion` draws a fresh RNG state, so the same
/// `(model, prompt)` produces a different bundle run-to-run even at a
/// near-zero temperature. Pinning the seed makes synthesis
/// byte-reproducible for a fixed `(model, prompt)`.
pub const DEFAULT_SAMPLING_SEED: i64 = 0;

/// Default sampling temperature. `0.0` selects greedy decoding (the
/// most-likely token every step). Synthesis here is extraction-like —
/// a faithful condensation of evidence, not creative generation — so
/// greedy is a feature, not a limitation: it removes the last source
/// of run-to-run variance the seed alone does not (ties resolved by
/// RNG).
pub const DEFAULT_SAMPLING_TEMPERATURE: f32 = 0.0;

/// Default `top_k`. `1` keeps only the single most-likely token,
/// reinforcing the greedy/extraction posture. Raising it (with a
/// non-zero temperature) re-introduces controlled diversity.
pub const DEFAULT_SAMPLING_TOP_K: u32 = 1;

/// Default `top_p` (nucleus sampling cutoff). Inert under greedy
/// decoding (`top_k = 1`); carried so a host that opts into a
/// non-zero temperature gets a sane nucleus without having to set
/// every knob.
pub const DEFAULT_SAMPLING_TOP_P: f32 = 0.9;

/// Default `min_p` (minimum-probability floor). Inert under greedy
/// decoding; like [`DEFAULT_SAMPLING_TOP_P`] it is a sensible default
/// for hosts that raise the temperature.
pub const DEFAULT_SAMPLING_MIN_P: f32 = 0.05;

/// Default repeat penalty. A mild `1.1` discourages the degenerate
/// token loops a 2-bit-quantised small model is prone to (the
/// "rambling meta-commentary" failure mode) without distorting the
/// faithful-condensation objective.
pub const DEFAULT_SAMPLING_REPEAT_PENALTY: f32 = 1.1;

/// Default `n_predict` cap (token budget for one bundle). Mirrors the
/// adapters' historical default; kept as the floor so the adaptive
/// budget in the synthesis pipeline can only ever raise it.
pub const DEFAULT_SAMPLING_N_PREDICT: u32 = 512;

/// Environment variable overriding the sampling seed
/// ([`SamplingConfig::seed`]).
pub const ENV_SLM_SEED: &str = "KNOWLEDGE_SLM_SEED";
/// Environment variable overriding the sampling temperature.
pub const ENV_SLM_TEMPERATURE: &str = "KNOWLEDGE_SLM_TEMPERATURE";
/// Environment variable overriding the `top_k` cutoff.
pub const ENV_SLM_TOP_K: &str = "KNOWLEDGE_SLM_TOP_K";
/// Environment variable overriding the `top_p` nucleus cutoff.
pub const ENV_SLM_TOP_P: &str = "KNOWLEDGE_SLM_TOP_P";
/// Environment variable overriding the `min_p` floor.
pub const ENV_SLM_MIN_P: &str = "KNOWLEDGE_SLM_MIN_P";
/// Environment variable overriding the repeat penalty.
pub const ENV_SLM_REPEAT_PENALTY: &str = "KNOWLEDGE_SLM_REPEAT_PENALTY";
/// Environment variable overriding the `n_predict` token budget.
pub const ENV_SLM_N_PREDICT: &str = "KNOWLEDGE_SLM_N_PREDICT";

/// Deterministic, tunable SLM sampling parameters.
///
/// Every field maps 1:1 onto a `llama-server` `/completion` sampling
/// parameter (and, where the OpenAI surface supports it, onto a
/// managed-cloud `/chat/completions` field). The
/// [`Self::synthesis_default`] preset is greedy + fixed-seed so a
/// `(model, prompt)` pair is byte-reproducible; every field is
/// overridable from a `KNOWLEDGE_SLM_*` environment variable
/// (see [`Self::from_env`]) following the same convention as
/// [`DEFAULT_SERVER_URL`] / [`DEFAULT_MODEL_PATH`].
///
/// `f32` fields mean this type is `PartialEq` but not `Eq`; that is
/// why [`RouterConfig`] (which embeds it) is likewise `PartialEq`
/// only.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SamplingConfig {
    /// RNG seed fed to `llama-server` as `seed`. A fixed value makes
    /// generation reproducible; `-1` restores `llama-server`'s
    /// non-deterministic default.
    pub seed: i64,
    /// Sampling temperature (`temperature`). `0.0` = greedy.
    pub temperature: f32,
    /// `top_k` cutoff — keep only the `k` most-likely tokens.
    pub top_k: u32,
    /// `top_p` nucleus cutoff.
    pub top_p: f32,
    /// `min_p` minimum-probability floor.
    pub min_p: f32,
    /// `repeat_penalty` applied to already-emitted tokens.
    pub repeat_penalty: f32,
    /// `n_predict` — maximum tokens to generate for one payload.
    pub n_predict: u32,
}

impl SamplingConfig {
    /// Synthesis-appropriate preset: greedy decoding (`temperature =
    /// 0.0`, `top_k = 1`) with a fixed [`DEFAULT_SAMPLING_SEED`], so a
    /// fixed `(model, prompt)` yields byte-identical output every run.
    pub const fn synthesis_default() -> Self {
        Self {
            seed: DEFAULT_SAMPLING_SEED,
            temperature: DEFAULT_SAMPLING_TEMPERATURE,
            top_k: DEFAULT_SAMPLING_TOP_K,
            top_p: DEFAULT_SAMPLING_TOP_P,
            min_p: DEFAULT_SAMPLING_MIN_P,
            repeat_penalty: DEFAULT_SAMPLING_REPEAT_PENALTY,
            n_predict: DEFAULT_SAMPLING_N_PREDICT,
        }
    }

    /// Build a config from the `KNOWLEDGE_SLM_*` environment, falling
    /// back to [`Self::synthesis_default`] for any unset / malformed
    /// variable.
    ///
    /// Reads the process-global environment; the parsing itself lives
    /// in [`Self::from_env_values`] so it can be unit-tested without
    /// the thread-unsafe `std::env::set_var` (which is `unsafe` from
    /// the 2024 edition).
    pub fn from_env() -> Self {
        Self::from_env_values(
            std::env::var(ENV_SLM_SEED).ok().as_deref(),
            std::env::var(ENV_SLM_TEMPERATURE).ok().as_deref(),
            std::env::var(ENV_SLM_TOP_K).ok().as_deref(),
            std::env::var(ENV_SLM_TOP_P).ok().as_deref(),
            std::env::var(ENV_SLM_MIN_P).ok().as_deref(),
            std::env::var(ENV_SLM_REPEAT_PENALTY).ok().as_deref(),
            std::env::var(ENV_SLM_N_PREDICT).ok().as_deref(),
        )
    }

    /// Pure core of [`Self::from_env`]: each argument is the raw
    /// (untrimmed) value of the corresponding `KNOWLEDGE_SLM_*`
    /// variable, or `None` when unset. A value that fails to parse (or
    /// is empty / whitespace) is ignored and the
    /// [`Self::synthesis_default`] for that field is kept, so a typo
    /// in one variable can never silently disable determinism for the
    /// others.
    #[allow(clippy::too_many_arguments)]
    pub fn from_env_values(
        seed: Option<&str>,
        temperature: Option<&str>,
        top_k: Option<&str>,
        top_p: Option<&str>,
        min_p: Option<&str>,
        repeat_penalty: Option<&str>,
        n_predict: Option<&str>,
    ) -> Self {
        let d = Self::synthesis_default();
        Self {
            seed: parse_env(seed).unwrap_or(d.seed),
            temperature: parse_env_f32(temperature).unwrap_or(d.temperature),
            top_k: parse_env(top_k).unwrap_or(d.top_k),
            top_p: parse_env_f32(top_p).unwrap_or(d.top_p),
            min_p: parse_env_f32(min_p).unwrap_or(d.min_p),
            repeat_penalty: parse_env_f32(repeat_penalty).unwrap_or(d.repeat_penalty),
            n_predict: parse_env(n_predict).unwrap_or(d.n_predict),
        }
    }

    /// Override the `n_predict` token budget, returning the updated
    /// config. Used by the synthesis pipeline's adaptive-budget logic.
    #[must_use]
    pub const fn with_n_predict(mut self, n_predict: u32) -> Self {
        self.n_predict = n_predict;
        self
    }
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self::synthesis_default()
    }
}

/// Parse a trimmed environment value into `T`, treating an empty /
/// whitespace-only / unparseable value as "absent" (`None`).
fn parse_env<T: std::str::FromStr>(raw: Option<&str>) -> Option<T> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<T>().ok()
}

/// Parse a trimmed `f32` environment value, additionally rejecting
/// non-finite values. `f32::from_str` accepts `"nan"` / `"inf"` /
/// `"-inf"`, but `serde_json` serialises a non-finite float as `null`,
/// which `llama-server` / OpenAI endpoints reject — so a non-finite
/// override is treated as "absent" and the caller keeps the
/// deterministic [`SamplingConfig::synthesis_default`] for that field,
/// matching how a typo is handled rather than poisoning the wire.
fn parse_env_f32(raw: Option<&str>) -> Option<f32> {
    parse_env::<f32>(raw).filter(|v| v.is_finite())
}

/// Router configuration. Held by [`crate::InferenceRouter`] and
/// passed verbatim to each adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Loopback URL of the llama.cpp server, e.g. `http://127.0.0.1:8081`.
    pub server_url: String,
    /// Filesystem path to the SLM model artifact (GGUF for llama.cpp,
    /// MLX archive for the MLX adapter).
    pub model_path: String,
    /// Idle timeout in seconds — adapters not used for this long are
    /// unloaded.
    pub idle_timeout_secs: u64,
    /// Warm-up prompt sent on first dispatch.
    pub warm_up_prompt: String,
    /// Detected device tier — cached at boot so the router can refuse
    /// to admit tasks that exceed the tier's budget.
    pub device_tier: DeviceTier,
    /// Optional URL the SLM weights are lazily downloaded from on first
    /// synthesis when [`Self::model_path`] is absent.
    ///
    /// `None` (the default) means the host provisions weights
    /// out-of-band — the only option on mobile, where the network
    /// transport is deliberately not compiled in to keep the artifact
    /// small. Desktop / server builds set a platform-appropriate URL
    /// (GGUF for Windows, MLX for macOS). `#[serde(default)]` keeps
    /// older serialized configs (written before this field existed)
    /// deserialisable.
    #[serde(default)]
    pub model_download_url: Option<String>,
    /// Pinned lowercase-hex SHA-256 of the artifact at
    /// [`Self::model_download_url`]. When set, a downloaded artifact
    /// whose hash does not match is deleted and the download fails
    /// (see [`crate::model_download`]). MUST be set whenever
    /// `model_download_url` points at a public CDN — for a 5000-tenant
    /// fleet it is the line between lazy-loading a model and executing
    /// attacker-substituted weights.
    #[serde(default)]
    pub model_sha256: Option<String>,
    /// Sampling parameters threaded into every SLM `/completion`
    /// request. Defaults to the deterministic
    /// [`SamplingConfig::synthesis_default`] preset so synthesis is
    /// reproducible out of the box. `#[serde(default)]` keeps configs
    /// serialized before this field existed deserialisable.
    #[serde(default)]
    pub sampling: SamplingConfig,
}

impl RouterConfig {
    /// Construct a fresh config with sensible defaults.
    ///
    /// The device tier is auto-detected from available system RAM
    /// via [`DeviceTier::auto_detect`]. Use
    /// [`Self::with_device_tier`] to override, or [`Self::with_tier`]
    /// to supply the tier at construction without the RAM probe.
    pub fn new(server_url: impl Into<String>, model_path: impl Into<String>) -> Self {
        Self::with_tier(server_url, model_path, DeviceTier::auto_detect())
    }

    /// Construct a config for an already-resolved [`DeviceTier`],
    /// skipping the RAM probe [`Self::new`] performs.
    ///
    /// Callers that have already classified the device use this so the
    /// tier is resolved exactly once. The FFI runtime is the motivating
    /// case: it classifies the device a single time and feeds the same
    /// [`DeviceTier`] to both the evidence store's low-memory decision
    /// and this router config, so the two subsystems cannot disagree
    /// and the (syscall-backed) auto-detection isn't run twice per
    /// `open_store`.
    pub fn with_tier(
        server_url: impl Into<String>,
        model_path: impl Into<String>,
        tier: DeviceTier,
    ) -> Self {
        Self {
            server_url: server_url.into(),
            model_path: model_path.into(),
            idle_timeout_secs: IDLE_UNLOAD_TIMEOUT_SECS,
            warm_up_prompt: WARM_UP_PROMPT.into(),
            // Seed with the caller's resolved `tier`; the trailing
            // `with_device_tier(tier)` then installs that tier's
            // derived profile (warm-up prompt, idle timeout). Seeding
            // with `tier` rather than a fixed placeholder keeps the
            // struct literal honest even before the override runs.
            device_tier: tier,
            model_download_url: None,
            model_sha256: None,
            // Read sampling overrides from the environment so a
            // `KNOWLEDGE_SLM_*` deployment knob takes effect even on
            // the `with_tier` path the FFI runtime uses. Defaults to
            // the deterministic synthesis preset when unset.
            sampling: SamplingConfig::from_env(),
        }
        .with_device_tier(tier)
    }

    /// Configure lazy SLM-weight download from `url`, verified against
    /// the pinned lowercase-hex SHA-256 `sha256`.
    ///
    /// Pass `Some(hash)` for any public-CDN URL — the hash is what
    /// makes the lazy download safe to consume. `None` accepts the
    /// bytes unverified and is appropriate only for trusted-LAN /
    /// development sources.
    pub fn with_model_download(mut self, url: impl Into<String>, sha256: Option<String>) -> Self {
        self.model_download_url = Some(url.into());
        self.model_sha256 = sha256;
        self
    }

    /// Override the device tier, applying the tier's memory profile.
    ///
    /// The warm-up prompt and idle timeout are *tier-derived* defaults,
    /// so this sets **both** deterministically from `tier` rather than
    /// only mutating on the way *into* [`DeviceTier::Low`]. That makes
    /// the call idempotent and order-independent: re-applying a
    /// different tier later fully installs the new tier's profile
    /// instead of leaving a prior tier's values stuck. This matters
    /// because [`Self::new`] auto-detects a tier at construction, and a
    /// caller (e.g. `router_config_from_env`) may then override it from
    /// `KNOWLEDGE_SLM_DEVICE_TIER` — a host that auto-detects `Low`
    /// (small cgroup memory limit) but is configured `High` must end up
    /// with the High profile, not Low's empty warm-up / immediate
    /// unload.
    ///
    /// * [`DeviceTier::Low`] (encoder-only, no on-device SLM) empties
    ///   the warm-up prompt and sets `idle_timeout_secs` to `0`. The
    ///   warm-up prompt is dispatched on first use to page model
    ///   weights into memory; a Low-tier host never admits a synthesis
    ///   adapter, so warming up would either no-op against the encoder
    ///   fallback or waste a dispatch. `idle_timeout_secs = 0` makes
    ///   [`crate::InferenceRouter::sweep_idle_adapters`] unload an
    ///   adapter as soon as it goes idle, reclaiming RAM immediately on
    ///   a device where every megabyte counts.
    /// * [`DeviceTier::Medium`] / [`DeviceTier::High`] install the
    ///   standard [`WARM_UP_PROMPT`] and [`IDLE_UNLOAD_TIMEOUT_SECS`]
    ///   so an SLM adapter is pre-warmed and kept resident across short
    ///   idle gaps (avoiding load/unload churn).
    ///
    /// An explicit [`Self::with_idle_timeout`] chained *after* this
    /// call still wins, so the tier tuning is a default, not a floor.
    pub fn with_device_tier(mut self, tier: DeviceTier) -> Self {
        self.device_tier = tier;
        match tier {
            DeviceTier::Low => {
                self.warm_up_prompt = String::new();
                self.idle_timeout_secs = 0;
            }
            DeviceTier::Medium | DeviceTier::High => {
                self.warm_up_prompt = WARM_UP_PROMPT.into();
                self.idle_timeout_secs = IDLE_UNLOAD_TIMEOUT_SECS;
            }
        }
        self
    }

    /// Override the idle timeout.
    pub fn with_idle_timeout(mut self, secs: u64) -> Self {
        self.idle_timeout_secs = secs;
        self
    }

    /// Override the [`SamplingConfig`]. Lets a caller that builds its
    /// own config (e.g. a host that already resolved sampling knobs
    /// from a config file) install them without going through the
    /// environment.
    #[must_use]
    pub fn with_sampling(mut self, sampling: SamplingConfig) -> Self {
        self.sampling = sampling;
        self
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self::new(DEFAULT_SERVER_URL, DEFAULT_MODEL_PATH)
    }
}

/// Construct a [`DeviceTier`] from a raw byte count — exposed for
/// testing and for callers that already know the device's RAM
/// budget without hitting the OS.
impl DeviceTier {
    /// Classify a device tier from a known RAM byte count.
    pub fn from_ram_bytes(bytes: u64) -> Self {
        if bytes < LOW_TIER_RAM_THRESHOLD {
            Self::Low
        } else if bytes >= HIGH_TIER_RAM_THRESHOLD {
            Self::High
        } else {
            Self::Medium
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_json() {
        let cfg = RouterConfig::new("http://x", "/y/z.gguf").with_device_tier(DeviceTier::High);
        let s = serde_json::to_string(&cfg).expect("ser");
        let back: RouterConfig = serde_json::from_str(&s).expect("deser");
        assert_eq!(cfg, back);
    }

    #[test]
    fn device_tier_string_tags_are_stable() {
        assert_eq!(DeviceTier::Low.as_str(), "low");
        assert_eq!(DeviceTier::Medium.as_str(), "medium");
        assert_eq!(DeviceTier::High.as_str(), "high");
    }

    #[test]
    fn default_uses_loopback_server() {
        let cfg = RouterConfig::default();
        assert!(cfg.server_url.starts_with("http://127.0.0.1"));
        // The warm-up / idle knobs are tier-derived (see
        // `with_device_tier`): a Low-tier host empties the warm-up and
        // unloads immediately, while Medium/High keep the defaults.
        // Branch on the auto-detected tier so the assertion holds on
        // any runner regardless of its RAM budget.
        match cfg.device_tier {
            DeviceTier::Low => {
                assert_eq!(cfg.idle_timeout_secs, 0);
                assert!(cfg.warm_up_prompt.is_empty());
            }
            DeviceTier::Medium | DeviceTier::High => {
                assert_eq!(cfg.idle_timeout_secs, IDLE_UNLOAD_TIMEOUT_SECS);
                assert_eq!(cfg.warm_up_prompt, WARM_UP_PROMPT);
            }
        }
    }

    #[test]
    fn low_tier_empties_warm_up_and_unloads_immediately() {
        let cfg = RouterConfig::new("http://x", "/y/z.gguf").with_device_tier(DeviceTier::Low);
        assert_eq!(cfg.device_tier, DeviceTier::Low);
        assert!(
            cfg.warm_up_prompt.is_empty(),
            "Low tier must empty the warm-up prompt"
        );
        assert_eq!(
            cfg.idle_timeout_secs, 0,
            "Low tier must unload adapters immediately on idle"
        );
    }

    #[test]
    fn medium_and_high_tiers_keep_warm_up_defaults() {
        for tier in [DeviceTier::Medium, DeviceTier::High] {
            let cfg = RouterConfig::new("http://x", "/y/z.gguf").with_device_tier(tier);
            assert_eq!(cfg.warm_up_prompt, WARM_UP_PROMPT);
            assert_eq!(cfg.idle_timeout_secs, IDLE_UNLOAD_TIMEOUT_SECS);
        }
    }

    #[test]
    fn overriding_low_to_higher_tier_restores_slm_profile() {
        // Regression for the env-override path: `RouterConfig::new`
        // auto-detects a tier at construction, then a caller such as
        // `router_config_from_env` overrides it from
        // `KNOWLEDGE_SLM_DEVICE_TIER`. A host that auto-detects `Low`
        // (small cgroup memory limit) but is configured `High`/`Medium`
        // must end up with the SLM profile restored — not Low's empty
        // warm-up / immediate-unload left stuck on an SLM-capable host
        // (which would cause model load/unload churn on every sweep).
        //
        // This builds the Low profile explicitly so the assertion holds
        // on any runner regardless of its detected RAM tier.
        let low = RouterConfig::new("http://x", "/y/z.gguf").with_device_tier(DeviceTier::Low);
        assert!(low.warm_up_prompt.is_empty());
        assert_eq!(low.idle_timeout_secs, 0);

        for tier in [DeviceTier::Medium, DeviceTier::High] {
            let restored = low.clone().with_device_tier(tier);
            assert_eq!(restored.device_tier, tier);
            assert_eq!(
                restored.warm_up_prompt, WARM_UP_PROMPT,
                "{tier:?} must restore the warm-up prompt after Low"
            );
            assert_eq!(
                restored.idle_timeout_secs, IDLE_UNLOAD_TIMEOUT_SECS,
                "{tier:?} must restore the idle-unload timeout after Low"
            );
        }
    }

    #[test]
    fn explicit_idle_override_after_low_tier_wins() {
        // `with_idle_timeout` chained after `with_device_tier(Low)`
        // must still take effect — the tier tuning sets a *default*,
        // not a hard floor.
        let cfg = RouterConfig::new("http://x", "/y/z.gguf")
            .with_device_tier(DeviceTier::Low)
            .with_idle_timeout(30);
        assert_eq!(cfg.idle_timeout_secs, 30);
    }

    #[test]
    fn auto_detect_returns_a_valid_tier() {
        let tier = DeviceTier::auto_detect();
        // On any real machine this should succeed (not panic).
        assert!(
            matches!(
                tier,
                DeviceTier::Low | DeviceTier::Medium | DeviceTier::High
            ),
            "auto_detect must return a valid tier"
        );
    }

    #[test]
    fn from_ram_bytes_classifies_correctly() {
        assert_eq!(DeviceTier::from_ram_bytes(1_000_000_000), DeviceTier::Low);
        assert_eq!(
            DeviceTier::from_ram_bytes(4_000_000_000),
            DeviceTier::Medium
        );
        assert_eq!(DeviceTier::from_ram_bytes(16_000_000_000), DeviceTier::High);
        // Boundary: exactly at LOW threshold → Medium
        assert_eq!(
            DeviceTier::from_ram_bytes(LOW_TIER_RAM_THRESHOLD),
            DeviceTier::Medium
        );
        // Boundary: exactly at HIGH threshold → High
        assert_eq!(
            DeviceTier::from_ram_bytes(HIGH_TIER_RAM_THRESHOLD),
            DeviceTier::High
        );
    }

    #[test]
    fn sampling_default_is_greedy_and_fixed_seed() {
        // The reproducibility contract: a fixed seed + greedy decode.
        // If any of these defaults drift, synthesis stops being
        // byte-reproducible for a fixed (model, prompt).
        let s = SamplingConfig::synthesis_default();
        assert_eq!(s.seed, 0, "seed must be fixed for reproducibility");
        // Bit-exact float compares (workspace denies `clippy::float_cmp`).
        assert_eq!(
            s.temperature.to_bits(),
            0.0_f32.to_bits(),
            "default decode must be greedy"
        );
        assert_eq!(s.top_k, 1);
        assert_eq!(s.top_p.to_bits(), 0.9_f32.to_bits());
        assert_eq!(s.min_p.to_bits(), 0.05_f32.to_bits());
        assert_eq!(s.repeat_penalty.to_bits(), 1.1_f32.to_bits());
        assert_eq!(s.n_predict, 512);
        assert_eq!(SamplingConfig::default(), s);
    }

    #[test]
    fn sampling_from_env_values_parses_every_field() {
        let s = SamplingConfig::from_env_values(
            Some("42"),
            Some("0.7"),
            Some("40"),
            Some("0.95"),
            Some("0.1"),
            Some("1.3"),
            Some("768"),
        );
        assert_eq!(s.seed, 42);
        assert_eq!(s.temperature.to_bits(), 0.7_f32.to_bits());
        assert_eq!(s.top_k, 40);
        assert_eq!(s.top_p.to_bits(), 0.95_f32.to_bits());
        assert_eq!(s.min_p.to_bits(), 0.1_f32.to_bits());
        assert_eq!(s.repeat_penalty.to_bits(), 1.3_f32.to_bits());
        assert_eq!(s.n_predict, 768);
    }

    #[test]
    fn sampling_from_env_values_ignores_unset_and_malformed() {
        // A typo in one knob must not silently disable determinism for
        // the rest: each bad/unset field falls back to the synthesis
        // default independently.
        let d = SamplingConfig::synthesis_default();
        let s = SamplingConfig::from_env_values(
            None,        // unset → default seed
            Some("  "),  // whitespace → default temperature
            Some("abc"), // unparseable → default top_k
            None,
            None,
            None,
            Some("1024"), // the one valid override
        );
        assert_eq!(s.seed, d.seed);
        assert_eq!(s.temperature.to_bits(), d.temperature.to_bits());
        assert_eq!(s.top_k, d.top_k);
        assert_eq!(s.n_predict, 1024);
    }

    #[test]
    fn sampling_from_env_values_rejects_non_finite_floats() {
        // `f32::from_str` accepts "nan"/"inf"/"-inf", but a non-finite
        // float serialises as JSON `null` and would be rejected at the
        // wire. Each non-finite f32 override must fall back to its
        // deterministic default instead of poisoning the request body.
        let d = SamplingConfig::synthesis_default();
        let s = SamplingConfig::from_env_values(
            None,
            Some("nan"),  // → default temperature
            None,
            Some("inf"),  // → default top_p
            Some("-inf"), // → default min_p
            Some("NaN"),  // → default repeat_penalty
            None,
        );
        assert_eq!(s.temperature.to_bits(), d.temperature.to_bits());
        assert_eq!(s.top_p.to_bits(), d.top_p.to_bits());
        assert_eq!(s.min_p.to_bits(), d.min_p.to_bits());
        assert_eq!(s.repeat_penalty.to_bits(), d.repeat_penalty.to_bits());
        // A finite override on the same field still applies.
        let ok = SamplingConfig::from_env_values(
            None,
            Some("0.42"),
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(ok.temperature.to_bits(), 0.42_f32.to_bits());
    }

    #[test]
    fn sampling_with_n_predict_only_changes_budget() {
        let s = SamplingConfig::synthesis_default().with_n_predict(900);
        assert_eq!(s.n_predict, 900);
        // Determinism knobs are untouched.
        assert_eq!(s.seed, DEFAULT_SAMPLING_SEED);
        assert_eq!(
            s.temperature.to_bits(),
            DEFAULT_SAMPLING_TEMPERATURE.to_bits()
        );
    }

    #[test]
    fn router_config_carries_sampling_through_json() {
        let cfg = RouterConfig::new("http://x", "/y/z.gguf")
            .with_sampling(SamplingConfig::synthesis_default().with_n_predict(640));
        let s = serde_json::to_string(&cfg).expect("ser");
        let back: RouterConfig = serde_json::from_str(&s).expect("deser");
        assert_eq!(cfg, back);
        assert_eq!(back.sampling.n_predict, 640);
    }
}
