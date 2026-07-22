//! World model: tenants, users, scopes, and the permission graph.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use evidence_store::ScopeId;
use permission_service::{
    NamespaceRegistry, ObjectRef, Relation, RelationTuple, SubjectRef, SubjectType, TupleStore,
};

/// The kind of communication scope a [`SimScope`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ScopeKind {
    /// 1:1 direct message scope (2 members).
    DirectMessage,
    /// Small group scope (3–8 members).
    GroupMessage,
    /// Community / large channel scope (20–100 members).
    Community,
    /// Domain-level rollup scope.
    Domain,
    /// Tenant-level top scope.
    Tenant,
}

/// A simulated user.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimUser {
    /// Unique user UUID.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Primary language (BCP-47 primary subtag).
    pub language: String,
    /// Role within the tenant.
    pub role: UserRole,
}

/// User role within a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UserRole {
    /// Full control.
    Admin,
    /// Can ingest and manage.
    Editor,
    /// Can ingest and query.
    Member,
    /// Read-only.
    Viewer,
}

impl UserRole {
    /// Convert to a permission-service [`Relation`].
    pub fn to_relation(self) -> Relation {
        match self {
            Self::Admin => Relation::Admin,
            Self::Editor => Relation::Editor,
            Self::Member => Relation::Member,
            Self::Viewer => Relation::Viewer,
        }
    }
}

/// A simulated scope (channel / domain / tenant).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimScope {
    /// Unique scope UUID.
    pub scope_id: ScopeId,
    /// Kind of scope.
    pub kind: ScopeKind,
    /// Tenant that owns this scope.
    pub tenant_id: Uuid,
    /// User IDs that are members of this scope.
    pub members: Vec<Uuid>,
    /// Parent domain scope (if any).
    pub parent_domain: Option<ScopeId>,
    /// Parent tenant scope (if any).
    pub parent_tenant: Option<ScopeId>,
}

/// A simulated tenant.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimTenant {
    /// Unique tenant UUID.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Industry / domain.
    pub industry: String,
    /// Region (e.g. "APAC", "EMEA", "Americas").
    pub region: String,
    /// Primary language for this tenant.
    pub primary_language: String,
    /// Users in this tenant.
    pub users: Vec<SimUser>,
    /// Scopes in this tenant.
    pub scopes: Vec<SimScope>,
    /// Tenant-level scope id.
    pub tenant_scope: ScopeId,
}

/// The complete simulated world: all tenants, users, scopes, and the
/// permission graph.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct World {
    /// All tenants.
    pub tenants: Vec<SimTenant>,
    /// Lookup: user_id → tenant_id.
    pub user_to_tenant: HashMap<Uuid, Uuid>,
    /// Lookup: scope_id → (tenant_id, scope index).
    pub scope_index: HashMap<ScopeId, (Uuid, usize)>,
    /// Permission tuple store.
    pub tuples: TupleStore,
    /// Namespace registry for permission checks.
    #[serde(skip)]
    pub namespaces: NamespaceRegistry,
    /// Simulated start time.
    pub start_time: DateTime<Utc>,
}

impl World {
    /// Build the permission tuples for all scopes and users.
    pub fn build_permissions(&mut self) {
        for tenant in &self.tenants {
            // Tenant scope: all users get their role.
            let tenant_obj = ObjectRef::new(
                permission_service::ObjectType::Tenant,
                tenant.tenant_scope.as_uuid(),
            );
            for user in &tenant.users {
                let subject = SubjectRef::direct(SubjectType::User, user.id);
                self.tuples
                    .insert(RelationTuple::new(
                        tenant_obj,
                        user.role.to_relation(),
                        subject,
                    ))
                    .ok();
            }

            // Channel / community scopes: members get Editor, owner gets Owner.
            for scope in &tenant.scopes {
                let obj = match scope.kind {
                    ScopeKind::DirectMessage | ScopeKind::GroupMessage | ScopeKind::Community => {
                        ObjectRef::new(permission_service::ObjectType::Channel, scope.scope_id.as_uuid())
                    }
                    ScopeKind::Domain => {
                        ObjectRef::new(permission_service::ObjectType::Domain, scope.scope_id.as_uuid())
                    }
                    ScopeKind::Tenant => continue,
                };
                for (i, &user_id) in scope.members.iter().enumerate() {
                    let subject = SubjectRef::direct(SubjectType::User, user_id);
                    let relation = if i == 0 { Relation::Owner } else { Relation::Editor };
                    self.tuples
                        .insert(RelationTuple::new(obj, relation, subject))
                        .ok();
                }
            }
        }
    }

    /// Look up a tenant by ID.
    pub fn tenant(&self, id: Uuid) -> Option<&SimTenant> {
        self.tenants.iter().find(|t| t.id == id)
    }

    /// Look up a scope by ID.
    pub fn scope(&self, id: ScopeId) -> Option<&SimScope> {
        let (tenant_id, idx) = self.scope_index.get(&id)?;
        let tenant = self.tenants.iter().find(|t| t.id == *tenant_id)?;
        tenant.scopes.get(*idx)
    }
}
