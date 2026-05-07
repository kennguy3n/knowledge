//! Tenant member provisioning.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use permission_service::Relation;

/// Lifecycle state for a tenant membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantMemberStatus {
    /// Active membership.
    #[default]
    Active,
    /// Provisioning has been suspended (e.g. employee on leave).
    Suspended,
    /// Membership has been removed.
    Removed,
}

impl TenantMemberStatus {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Removed => "removed",
        }
    }
}

/// One tenant membership row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantMember {
    /// Tenant id.
    pub tenant_id: Uuid,
    /// User id.
    pub user_id: Uuid,
    /// Substrate role for this user.
    ///
    /// Maps to the [`permission_service::Relation`] tuple
    /// `(Tenant, tenant_id) # role @ (User, user_id)`.
    pub role: Relation,
    /// Lifecycle state.
    pub status: TenantMemberStatus,
    /// Wall-clock time at which the membership was provisioned.
    pub provisioned_at: DateTime<Utc>,
    /// Wall-clock time of the last status / role change.
    pub updated_at: DateTime<Utc>,
}

impl TenantMember {
    /// Construct a fresh `Active` membership.
    pub fn new(tenant_id: Uuid, user_id: Uuid, role: Relation) -> Self {
        let now = Utc::now();
        Self {
            tenant_id,
            user_id,
            role,
            status: TenantMemberStatus::Active,
            provisioned_at: now,
            updated_at: now,
        }
    }
}
