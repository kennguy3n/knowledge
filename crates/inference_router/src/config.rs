//! Router configuration.

use serde::{Deserialize, Serialize};

/// Default warm-up prompt used by [`crate::InferenceRouter::warm_up`].
/// Picked to be short, deterministic, and trigger token cache fill.
pub const WARM_UP_PROMPT: &str = "knowledge substrate boot probe";

/// Default idle-unload timeout — adapters whose last call is older
/// than this are unloaded from memory by
/// [`crate::InferenceRouter::sweep_idle_adapters`].
pub const IDLE_UNLOAD_TIMEOUT_SECS: u64 = 60;

/// Device tier — drives which adapters / tasks are admitted.
///
/// Per `ARCHITECTURE.md` §3 a `Low`-tier device runs only the
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
    pub fn new(server_url: impl Into<String>, model_path: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            model_path: model_path.into(),
            idle_timeout_secs: IDLE_UNLOAD_TIMEOUT_SECS,
            warm_up_prompt: WARM_UP_PROMPT.into(),
            device_tier: DeviceTier::Medium,
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
}
