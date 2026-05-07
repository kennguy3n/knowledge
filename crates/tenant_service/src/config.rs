//! Per-tenant configuration: encryption keys, storage, and synthesis.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Result, TenantError};

/// Opaque reference to a tenant-level encryption key. The actual key
/// material is held by the `crypto` crate; the tenant service only
/// stores the *handle*. `destroyed` flips to `true` when the key has
/// been destroyed as part of tenant deletion (cryptographic
/// forgetting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantKeyRef {
    /// Handle for the key (UUID v4).
    pub handle: Uuid,
    /// `true` if the key has been destroyed.
    pub destroyed: bool,
}

impl TenantKeyRef {
    /// Construct a fresh, non-destroyed key reference.
    pub fn new() -> Self {
        Self {
            handle: Uuid::new_v4(),
            destroyed: false,
        }
    }

    /// Destroy the key. Idempotent.
    pub fn destroy(&mut self) {
        self.destroyed = true;
    }
}

impl Default for TenantKeyRef {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-tenant storage budget / routing configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Soft cap on the tenant's total evidence-plane storage in
    /// bytes. `None` means "no soft cap".
    pub soft_cap_bytes: Option<u64>,
    /// Hard cap on the tenant's total evidence-plane storage. The
    /// hard cap must be `>=` the soft cap when both are set.
    pub hard_cap_bytes: Option<u64>,
    /// `true` if the tenant's evidence may be replicated to the
    /// server-side cold tier.
    pub server_cold_tier: bool,
}

impl StorageConfig {
    /// Validate the config. Returns
    /// [`TenantError::InvalidConfig`] if soft_cap > hard_cap.
    pub fn validate(&self) -> Result<()> {
        if let (Some(soft), Some(hard)) = (self.soft_cap_bytes, self.hard_cap_bytes) {
            if soft > hard {
                return Err(TenantError::InvalidConfig(
                    "soft_cap_bytes must be <= hard_cap_bytes".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Per-tenant synthesis configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SynthesisConfig {
    /// Whether tenant-level synthesis is enabled at all.
    pub tenant_synthesis_enabled: bool,
    /// Cadence (seconds) between tenant-level synthesis windows. The
    /// minimum is 60 seconds; values below the floor are rejected by
    /// [`Self::validate`].
    pub tenant_window_secs: u64,
    /// Cadence (seconds) between domain-level synthesis windows.
    pub domain_window_secs: u64,
    /// Optional managed-AI endpoint for tenant synthesis. `None`
    /// means "use on-device only".
    pub managed_endpoint: Option<String>,
}

impl Default for SynthesisConfig {
    fn default() -> Self {
        Self {
            tenant_synthesis_enabled: false,
            tenant_window_secs: 86_400, // daily
            domain_window_secs: 3_600,  // hourly
            managed_endpoint: None,
        }
    }
}

impl SynthesisConfig {
    /// Validate the config. Rejects sub-minute windows.
    pub fn validate(&self) -> Result<()> {
        if self.tenant_window_secs < 60 || self.domain_window_secs < 60 {
            return Err(TenantError::InvalidConfig(
                "synthesis windows must be at least 60 seconds".into(),
            ));
        }
        Ok(())
    }
}

/// One tenant's full configuration: keys + storage + synthesis.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantConfig {
    /// Tenant root key reference. Destroyed on tenant deletion.
    pub root_key: TenantKeyRef,
    /// Storage config.
    pub storage: StorageConfig,
    /// Synthesis config.
    pub synthesis: SynthesisConfig,
}

impl TenantConfig {
    /// Construct a fresh config with default storage / synthesis and
    /// a fresh root-key reference.
    pub fn new() -> Self {
        Self {
            root_key: TenantKeyRef::new(),
            storage: StorageConfig::default(),
            synthesis: SynthesisConfig::default(),
        }
    }

    /// Validate the whole config.
    pub fn validate(&self) -> Result<()> {
        self.storage.validate()?;
        self.synthesis.validate()?;
        Ok(())
    }
}

impl Default for TenantConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_config_rejects_inverted_caps() {
        let cfg = StorageConfig {
            soft_cap_bytes: Some(100),
            hard_cap_bytes: Some(10),
            server_cold_tier: false,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn synthesis_config_rejects_short_windows() {
        let cfg = SynthesisConfig {
            tenant_synthesis_enabled: true,
            tenant_window_secs: 30,
            domain_window_secs: 600,
            managed_endpoint: None,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn tenant_key_ref_destroy_is_idempotent() {
        let mut k = TenantKeyRef::new();
        assert!(!k.destroyed);
        k.destroy();
        assert!(k.destroyed);
        k.destroy();
        assert!(k.destroyed);
    }
}
