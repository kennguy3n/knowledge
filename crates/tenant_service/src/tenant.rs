//! [`Tenant`] data model and the in-memory [`TenantRegistry`].

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use permission_service::Relation;

use crate::config::TenantConfig;
use crate::error::{Result, TenantError};
use crate::lifecycle::TenantStatus;
use crate::member::{TenantMember, TenantMemberStatus};

/// Newtype wrapper for tenant ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TenantId(pub Uuid);

impl TenantId {
    /// Generate a fresh random tenant id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a raw [`Uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One tenant row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Tenant {
    /// Tenant id.
    pub id: TenantId,
    /// Display name (e.g. `"Uney Inc."`).
    pub name: String,
    /// Lifecycle status.
    pub status: TenantStatus,
    /// Tenant config (keys, storage, synthesis).
    pub config: TenantConfig,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock last update time.
    pub updated_at: DateTime<Utc>,
}

impl Tenant {
    /// Construct a fresh `Active` tenant with `config`.
    pub fn new(name: impl Into<String>, config: TenantConfig) -> Self {
        let now = Utc::now();
        Self {
            id: TenantId::new_v4(),
            name: name.into(),
            status: TenantStatus::Active,
            config,
            created_at: now,
            updated_at: now,
        }
    }
}

/// In-memory tenant catalog. Persistence (Postgres + key store) lands
/// in a future update.
#[derive(Debug, Clone, Default)]
pub struct TenantRegistry {
    tenants: HashMap<TenantId, Tenant>,
    /// `(tenant_id, user_id) -> member`. The pair is the natural
    /// primary key.
    members: HashMap<(TenantId, Uuid), TenantMember>,
}

impl TenantRegistry {
    /// Construct a fresh empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tenants in the registry (any status).
    pub fn len(&self) -> usize {
        self.tenants.len()
    }

    /// True iff the registry has no tenants.
    pub fn is_empty(&self) -> bool {
        self.tenants.is_empty()
    }

    /// Create a new tenant. Validates the config first and rejects on
    /// id collisions.
    pub fn create(&mut self, name: impl Into<String>, config: TenantConfig) -> Result<TenantId> {
        config.validate()?;
        let tenant = Tenant::new(name, config);
        let id = tenant.id;
        if self.tenants.contains_key(&id) {
            return Err(TenantError::AlreadyExists(id.0));
        }
        self.tenants.insert(id, tenant);
        Ok(id)
    }

    /// Look up a tenant.
    pub fn get(&self, id: TenantId) -> Result<&Tenant> {
        self.tenants.get(&id).ok_or(TenantError::NotFound(id.0))
    }

    /// Suspend a tenant. Only `Active` tenants can be suspended.
    pub fn suspend(&mut self, id: TenantId) -> Result<()> {
        self.transition(id, TenantStatus::Suspended)
    }

    /// Activate a previously suspended tenant.
    pub fn activate(&mut self, id: TenantId) -> Result<()> {
        self.transition(id, TenantStatus::Active)
    }

    /// Delete a tenant. Destroys the root key reference (cryptographic
    /// forgetting) and walks the lifecycle to `Deleted`. Idempotent
    /// is *not* the rule — calling delete twice errors with
    /// [`TenantError::InvalidLifecycleTransition`].
    pub fn delete(&mut self, id: TenantId) -> Result<()> {
        let tenant = self
            .tenants
            .get_mut(&id)
            .ok_or(TenantError::NotFound(id.0))?;
        tenant.status.validate_transition(TenantStatus::Deleted)?;
        tenant.config.root_key.destroy();
        tenant.status = TenantStatus::Deleted;
        tenant.updated_at = Utc::now();
        Ok(())
    }

    fn transition(&mut self, id: TenantId, to: TenantStatus) -> Result<()> {
        let tenant = self
            .tenants
            .get_mut(&id)
            .ok_or(TenantError::NotFound(id.0))?;
        tenant.status.validate_transition(to)?;
        tenant.status = to;
        tenant.updated_at = Utc::now();
        Ok(())
    }

    /// Provision a member for `tenant_id` with `role`. Only `Active`
    /// tenants accept membership mutations — `Suspended` tenants
    /// freeze membership per the lifecycle docs at
    /// `crate::lifecycle::TenantStatus::Suspended`, and `Deleted`
    /// tenants reject everything because the root key reference has
    /// been destroyed.
    ///
    /// A user whose previous membership row is `Removed` may be
    /// re-provisioned: the audit row is overwritten with a fresh
    /// membership starting at the current wall-clock time. (The
    /// audit log keeps the historical removal entry, so this does
    /// not erase the audit trail — it just reopens the surface for
    /// employees who leave and return.) A user whose membership row
    /// is currently `Active` is rejected with
    /// [`TenantError::MemberAlreadyProvisioned`].
    pub fn add_member(
        &mut self,
        tenant_id: TenantId,
        user_id: Uuid,
        role: Relation,
    ) -> Result<TenantMember> {
        self.require_active(tenant_id)?;
        let key = (tenant_id, user_id);
        if let Some(existing) = self.members.get(&key) {
            if existing.status != TenantMemberStatus::Removed {
                return Err(TenantError::MemberAlreadyProvisioned(user_id));
            }
        }
        let member = TenantMember::new(tenant_id.0, user_id, role);
        self.members.insert(key, member.clone());
        Ok(member)
    }

    /// Remove a member from `tenant_id`. The membership row is kept
    /// around with `status = Removed` so the audit log can prove the
    /// removal. Only `Active` tenants accept membership mutations —
    /// see the docs on [`TenantRegistry::add_member`]. Re-removing a
    /// member that is already `Removed` is rejected with
    /// [`TenantError::MemberAlreadyRemoved`] so the audit trail keeps
    /// a single removal timestamp.
    pub fn remove_member(&mut self, tenant_id: TenantId, user_id: Uuid) -> Result<()> {
        self.require_active(tenant_id)?;
        let key = (tenant_id, user_id);
        let member = self
            .members
            .get_mut(&key)
            .ok_or(TenantError::MemberNotProvisioned(user_id))?;
        if member.status == TenantMemberStatus::Removed {
            return Err(TenantError::MemberAlreadyRemoved(user_id));
        }
        member.status = TenantMemberStatus::Removed;
        member.updated_at = Utc::now();
        Ok(())
    }

    /// Update a member's role. Only `Active` tenants accept
    /// membership mutations — see the docs on
    /// [`TenantRegistry::add_member`]. Members whose status is
    /// `Removed` are kept as audit artefacts only and cannot be
    /// re-roled; calling [`TenantRegistry::update_role`] on them
    /// returns [`TenantError::MemberAlreadyRemoved`].
    pub fn update_role(
        &mut self,
        tenant_id: TenantId,
        user_id: Uuid,
        role: Relation,
    ) -> Result<()> {
        self.require_active(tenant_id)?;
        let key = (tenant_id, user_id);
        let member = self
            .members
            .get_mut(&key)
            .ok_or(TenantError::MemberNotProvisioned(user_id))?;
        if member.status == TenantMemberStatus::Removed {
            return Err(TenantError::MemberAlreadyRemoved(user_id));
        }
        member.role = role;
        member.updated_at = Utc::now();
        Ok(())
    }

    /// Helper: look up `tenant_id` and reject anything that is not
    /// [`TenantStatus::Active`]. Reused by every membership mutator
    /// so the lifecycle check stays in one place.
    fn require_active(&self, tenant_id: TenantId) -> Result<()> {
        let tenant = self
            .tenants
            .get(&tenant_id)
            .ok_or(TenantError::NotFound(tenant_id.0))?;
        if tenant.status != TenantStatus::Active {
            return Err(TenantError::InvalidLifecycleTransition {
                from: tenant.status,
                to: TenantStatus::Active,
            });
        }
        Ok(())
    }

    /// Look up a single membership row.
    pub fn get_member(&self, tenant_id: TenantId, user_id: Uuid) -> Result<&TenantMember> {
        self.members
            .get(&(tenant_id, user_id))
            .ok_or(TenantError::MemberNotProvisioned(user_id))
    }

    /// All members of `tenant_id`, in iteration order.
    pub fn list_members(&self, tenant_id: TenantId) -> Vec<&TenantMember> {
        self.members
            .iter()
            .filter_map(|((t, _), m)| if *t == tenant_id { Some(m) } else { None })
            .collect()
    }
}
