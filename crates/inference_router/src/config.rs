//! Router configuration.

use serde::{Deserialize, Serialize};

/// Default warm-up prompt used by [`crate::InferenceRouter::warm_up`].
/// Picked to be short, deterministic, and trigger token cache fill.
pub const WARM_UP_PROMPT: &str = "knowledge substrate boot probe";

/// Default idle-unload timeout — adapters whose last call is older
/// than this are unloaded from memory by
/// [`crate::InferenceRouter::sweep_idle_adapters`].
pub const IDLE_UNLOAD_TIMEOUT_SECS: u64 = 60;

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

/// Router configuration. Held by [`crate::InferenceRouter`] and
/// passed verbatim to each adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl RouterConfig {
    /// Construct a fresh config with sensible defaults.
    ///
    /// The device tier is auto-detected from available system RAM
    /// via [`DeviceTier::auto_detect`]. Use
    /// [`Self::with_device_tier`] to override.
    pub fn new(server_url: impl Into<String>, model_path: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            model_path: model_path.into(),
            idle_timeout_secs: IDLE_UNLOAD_TIMEOUT_SECS,
            warm_up_prompt: WARM_UP_PROMPT.into(),
            device_tier: DeviceTier::auto_detect(),
        }
    }

    /// Override the device tier.
    pub fn with_device_tier(mut self, tier: DeviceTier) -> Self {
        self.device_tier = tier;
        self
    }

    /// Override the idle timeout.
    pub fn with_idle_timeout(mut self, secs: u64) -> Self {
        self.idle_timeout_secs = secs;
        self
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self::new("http://127.0.0.1:8081", "/var/lib/knowledge/slm.gguf")
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
        assert_eq!(cfg.idle_timeout_secs, IDLE_UNLOAD_TIMEOUT_SECS);
        assert_eq!(cfg.warm_up_prompt, WARM_UP_PROMPT);
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
}
