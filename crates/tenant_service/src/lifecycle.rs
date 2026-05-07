//! Tenant lifecycle state machine.

use serde::{Deserialize, Serialize};

use crate::error::{Result, TenantError};

/// The four states a tenant can occupy.
///
/// ```text
/// [*] -> Active
/// Active    -> Suspended  (admin / billing freeze)
/// Suspended -> Active     (admin / billing unfreeze)
/// Active    -> Deleted    (explicit deletion + key destruction)
/// Suspended -> Deleted    (explicit deletion + key destruction)
/// ```
///
/// `Deleted` is terminal — the tenant root key has been destroyed and
/// the catalog row stays around for audit only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    /// Active tenant accepting traffic.
    #[default]
    Active,
    /// Suspended tenant — no synthesis, no member changes, no
    /// connector traffic.
    Suspended,
    /// Deleted tenant — root key destroyed.
    Deleted,
}

impl TenantStatus {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    /// Whether this status is terminal.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Deleted)
    }

    /// Validate a transition `self -> to`. Returns
    /// [`TenantError::InvalidLifecycleTransition`] if the transition
    /// is not allowed.
    pub fn validate_transition(self, to: TenantStatus) -> Result<()> {
        let allowed = matches!(
            (self, to),
            (
                TenantStatus::Active,
                TenantStatus::Suspended | TenantStatus::Deleted
            ) | (
                TenantStatus::Suspended,
                TenantStatus::Active | TenantStatus::Deleted
            )
        );
        if !allowed {
            return Err(TenantError::InvalidLifecycleTransition { from: self, to });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_string_tag() {
        for s in [
            TenantStatus::Active,
            TenantStatus::Suspended,
            TenantStatus::Deleted,
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
    }

    #[test]
    fn lifecycle_transitions_are_enforced() {
        TenantStatus::Active
            .validate_transition(TenantStatus::Suspended)
            .unwrap();
        TenantStatus::Suspended
            .validate_transition(TenantStatus::Active)
            .unwrap();
        TenantStatus::Active
            .validate_transition(TenantStatus::Deleted)
            .unwrap();
        TenantStatus::Suspended
            .validate_transition(TenantStatus::Deleted)
            .unwrap();

        // Self-loops and exits from Deleted are forbidden.
        assert!(TenantStatus::Active
            .validate_transition(TenantStatus::Active)
            .is_err());
        assert!(TenantStatus::Deleted
            .validate_transition(TenantStatus::Active)
            .is_err());
        assert!(TenantStatus::Deleted
            .validate_transition(TenantStatus::Suspended)
            .is_err());
    }
}
