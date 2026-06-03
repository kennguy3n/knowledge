//! Scope-typed connector attachments.
//!
//! Per `docs/technical/design.md` §10.2 (point 4) every connector instance is
//! attached to exactly one substrate scope (channel / domain / user).
//! The attachment binding is the source of truth for:
//!
//! * Which scope an inbound observation should inherit (per
//!   `docs/technical/architecture.md` §5.2).
//! * Whether a scope already has a connector for a particular
//!   `(source_kind)` and should reject a duplicate attachment.
//! * Whether a caller is permitted to attach / detach a connector
//!   on the scope (we require `admin` or `editor` on the scope).
//!
//! The registry is intentionally keyed on `(scope_id, object_type,
//! connector_kind)` so the substrate can enforce *one connector per
//! source per scope per type* — a Notion connector and a Google Drive
//! connector can coexist on the same channel, but two Notion
//! connectors on the same channel cannot.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use evidence_store::ScopeId;
use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, SubjectRef, TupleStore,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ConnectorKind;
use crate::error::{ConnectorError, Result};
use crate::token_vault::ConnectorInstanceId;

/// Stable id for one [`ConnectorAttachment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachmentId(pub Uuid);

impl AttachmentId {
    /// Generate a fresh id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One connector ↔ scope binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorAttachment {
    /// Stable attachment id.
    pub id: AttachmentId,
    /// Connector instance bound to the scope.
    pub connector: ConnectorInstanceId,
    /// Connector source kind — kept on the attachment so the
    /// registry can enforce uniqueness without round-tripping
    /// through the connector instance.
    pub kind: ConnectorKind,
    /// Substrate scope (channel / domain / user) the connector is bound to.
    pub scope_id: ScopeId,
    /// Permission-model object type for this scope (e.g. `Channel`,
    /// `User`, `Domain`).  Stored so that `detach` uses the same
    /// authorization context that `attach` established.
    #[serde(default = "default_object_type")]
    pub object_type: ObjectType,
    /// Wall-clock attachment time.
    pub attached_at: DateTime<Utc>,
}

/// Legacy default: pre-existing attachments were always channel-scoped.
fn default_object_type() -> ObjectType {
    ObjectType::Channel
}

impl ConnectorAttachment {
    /// Construct a fresh attachment.
    pub fn new(
        connector: ConnectorInstanceId,
        kind: ConnectorKind,
        scope_id: ScopeId,
        object_type: ObjectType,
    ) -> Self {
        Self {
            id: AttachmentId::new_v4(),
            connector,
            kind,
            scope_id,
            object_type,
            attached_at: Utc::now(),
        }
    }
}

/// Scope-style object types accepted by attachment operations.
const SCOPE_OBJECT_TYPES: [ObjectType; 3] =
    [ObjectType::Channel, ObjectType::User, ObjectType::Domain];

/// Registry of [`ConnectorAttachment`]s.
///
/// Keyed on `(scope_id, object_type, connector_kind)` to enforce
/// uniqueness; also indexed by `connector_id` for fast detach.
#[derive(Debug, Clone, Default)]
pub struct AttachmentRegistry {
    by_scope_kind: HashMap<(ScopeId, ObjectType, ConnectorKind), ConnectorAttachment>,
    by_connector: HashMap<ConnectorInstanceId, ConnectorAttachment>,
}

impl AttachmentRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of attachments registered.
    pub fn len(&self) -> usize {
        self.by_connector.len()
    }

    /// True iff no attachments are registered.
    pub fn is_empty(&self) -> bool {
        self.by_connector.is_empty()
    }

    /// Look up the attachment for a connector instance, if any.
    pub fn get_by_connector(&self, connector: ConnectorInstanceId) -> Option<&ConnectorAttachment> {
        self.by_connector.get(&connector)
    }

    /// Look up the attachment bound to `(scope, object_type, kind)`,
    /// if any.
    pub fn get_by_scope_kind(
        &self,
        scope_id: ScopeId,
        object_type: ObjectType,
        kind: ConnectorKind,
    ) -> Option<&ConnectorAttachment> {
        self.by_scope_kind.get(&(scope_id, object_type, kind))
    }

    /// All attachments on a given scope.
    pub fn list_for_scope(&self, scope_id: ScopeId) -> Vec<&ConnectorAttachment> {
        self.by_connector
            .values()
            .filter(|a| a.scope_id == scope_id)
            .collect()
    }

    /// Attach a connector to a scope, enforcing both the
    /// permission check and the one-connector-per-source-per-scope
    /// constraint.
    ///
    /// `subject` is the user attempting the operation; the call
    /// requires the subject to hold `editor` on the scope (or any
    /// relation that implies `editor`, such as `admin` or `owner`).
    /// `object_type` must be a scope-style type (`Channel`, `User`,
    /// or `Domain`); other variants are rejected with
    /// [`ConnectorError::UnsupportedObjectType`].
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        &mut self,
        connector: ConnectorInstanceId,
        kind: ConnectorKind,
        scope_id: ScopeId,
        object_type: ObjectType,
        store: &TupleStore,
        namespaces: &NamespaceRegistry,
        subject: SubjectRef,
    ) -> Result<&ConnectorAttachment> {
        if !SCOPE_OBJECT_TYPES.contains(&object_type) {
            return Err(ConnectorError::UnsupportedObjectType);
        }
        require_editor(scope_id, object_type, store, namespaces, subject)?;

        if self.by_connector.contains_key(&connector) {
            return Err(ConnectorError::DuplicateAttachment);
        }
        if self
            .by_scope_kind
            .contains_key(&(scope_id, object_type, kind))
        {
            return Err(ConnectorError::DuplicateAttachment);
        }

        let attachment = ConnectorAttachment::new(connector, kind, scope_id, object_type);
        self.by_scope_kind
            .insert((scope_id, object_type, kind), attachment.clone());
        self.by_connector.insert(connector, attachment);
        Ok(self.by_connector.get(&connector).expect("just inserted"))
    }

    /// Detach a connector. Requires the same permission as
    /// [`Self::attach`] — the `object_type` stored on the
    /// attachment at attach-time is used for the authorization
    /// check so callers cannot bypass it by supplying a different
    /// type.
    pub fn detach(
        &mut self,
        connector: ConnectorInstanceId,
        store: &TupleStore,
        namespaces: &NamespaceRegistry,
        subject: SubjectRef,
    ) -> Result<ConnectorAttachment> {
        let existing = self
            .by_connector
            .get(&connector)
            .ok_or(ConnectorError::AttachmentNotFound)?;
        let scope = existing.scope_id;
        let kind = existing.kind;
        let ot = existing.object_type;
        require_editor(scope, ot, store, namespaces, subject)?;

        self.by_scope_kind.remove(&(scope, ot, kind));
        Ok(self.by_connector.remove(&connector).expect("checked above"))
    }

    /// Resolve the scope a connector instance is attached to (used
    /// by the observation pipeline so connector-derived
    /// observations inherit the attached scope).
    pub fn scope_for(&self, connector: ConnectorInstanceId) -> Result<ScopeId> {
        self.by_connector
            .get(&connector)
            .map(|a| a.scope_id)
            .ok_or(ConnectorError::AttachmentNotFound)
    }
}

fn require_editor(
    scope_id: ScopeId,
    object_type: ObjectType,
    store: &TupleStore,
    namespaces: &NamespaceRegistry,
    subject: SubjectRef,
) -> Result<()> {
    let object = ObjectRef::new(object_type, scope_id.as_uuid());
    // The inheritance chain `Owner ⇒ Admin ⇒ Editor` means a single
    // check for `Editor` also covers `Admin` and `Owner`.
    if check_permission(store, namespaces, object, Relation::Editor, subject) {
        Ok(())
    } else {
        Err(ConnectorError::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use permission_service::{RelationTuple, SubjectType};

    fn fresh() -> (TupleStore, NamespaceRegistry) {
        (TupleStore::new(), NamespaceRegistry::with_defaults())
    }

    fn grant_on(
        store: &mut TupleStore,
        object_type: ObjectType,
        scope: ScopeId,
        relation: Relation,
        user: Uuid,
    ) {
        store
            .insert(RelationTuple::new(
                ObjectRef::new(object_type, scope.as_uuid()),
                relation,
                SubjectRef::direct(SubjectType::User, user),
            ))
            .unwrap();
    }

    fn grant(store: &mut TupleStore, scope: ScopeId, relation: Relation, user: Uuid) {
        grant_on(store, ObjectType::Channel, scope, relation, user);
    }

    // ---- channel-scoped (existing behaviour) ----

    #[test]
    fn attach_succeeds_with_editor_relation() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant(&mut store, scope, Relation::Editor, user);

        let mut reg = AttachmentRegistry::new();
        let connector = ConnectorInstanceId::new_v4();
        let attachment = reg
            .attach(
                connector,
                ConnectorKind::Notion,
                scope,
                ObjectType::Channel,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap();
        assert_eq!(attachment.connector, connector);
        assert_eq!(attachment.scope_id, scope);
        assert_eq!(attachment.kind, ConnectorKind::Notion);
    }

    #[test]
    fn attach_denied_for_member_relation() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant(&mut store, scope, Relation::Member, user);

        let mut reg = AttachmentRegistry::new();
        let err = reg
            .attach(
                ConnectorInstanceId::new_v4(),
                ConnectorKind::Notion,
                scope,
                ObjectType::Channel,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::PermissionDenied));
    }

    #[test]
    fn attach_denied_for_unrelated_user() {
        let (store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        let mut reg = AttachmentRegistry::new();
        let err = reg
            .attach(
                ConnectorInstanceId::new_v4(),
                ConnectorKind::Jira,
                scope,
                ObjectType::Channel,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::PermissionDenied));
    }

    #[test]
    fn admin_relation_can_attach() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant(&mut store, scope, Relation::Admin, user);
        let mut reg = AttachmentRegistry::new();
        reg.attach(
            ConnectorInstanceId::new_v4(),
            ConnectorKind::GoogleDrive,
            scope,
            ObjectType::Channel,
            &store,
            &ns,
            SubjectRef::direct(SubjectType::User, user),
        )
        .unwrap();
    }

    #[test]
    fn duplicate_kind_on_same_scope_rejected() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant(&mut store, scope, Relation::Editor, user);
        let mut reg = AttachmentRegistry::new();
        let subject = SubjectRef::direct(SubjectType::User, user);
        reg.attach(
            ConnectorInstanceId::new_v4(),
            ConnectorKind::Notion,
            scope,
            ObjectType::Channel,
            &store,
            &ns,
            subject,
        )
        .unwrap();
        let err = reg
            .attach(
                ConnectorInstanceId::new_v4(),
                ConnectorKind::Notion,
                scope,
                ObjectType::Channel,
                &store,
                &ns,
                subject,
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::DuplicateAttachment));
    }

    #[test]
    fn different_kinds_on_same_scope_coexist() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant(&mut store, scope, Relation::Editor, user);
        let mut reg = AttachmentRegistry::new();
        let subject = SubjectRef::direct(SubjectType::User, user);
        reg.attach(
            ConnectorInstanceId::new_v4(),
            ConnectorKind::Notion,
            scope,
            ObjectType::Channel,
            &store,
            &ns,
            subject,
        )
        .unwrap();
        reg.attach(
            ConnectorInstanceId::new_v4(),
            ConnectorKind::Jira,
            scope,
            ObjectType::Channel,
            &store,
            &ns,
            subject,
        )
        .unwrap();
        assert_eq!(reg.list_for_scope(scope).len(), 2);
    }

    #[test]
    fn detach_removes_attachment() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant(&mut store, scope, Relation::Admin, user);
        let mut reg = AttachmentRegistry::new();
        let subject = SubjectRef::direct(SubjectType::User, user);
        let connector = ConnectorInstanceId::new_v4();
        reg.attach(
            connector,
            ConnectorKind::Slack,
            scope,
            ObjectType::Channel,
            &store,
            &ns,
            subject,
        )
        .unwrap();
        let removed = reg.detach(connector, &store, &ns, subject).unwrap();
        assert_eq!(removed.connector, connector);
        assert!(reg.is_empty());
        assert!(reg.get_by_connector(connector).is_none());
    }

    #[test]
    fn detach_unknown_connector_errors() {
        let (store, ns) = fresh();
        let mut reg = AttachmentRegistry::new();
        let err = reg
            .detach(
                ConnectorInstanceId::new_v4(),
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, Uuid::new_v4()),
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::AttachmentNotFound));
    }

    #[test]
    fn scope_for_returns_attached_scope() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant(&mut store, scope, Relation::Editor, user);
        let mut reg = AttachmentRegistry::new();
        let connector = ConnectorInstanceId::new_v4();
        reg.attach(
            connector,
            ConnectorKind::OneDrive,
            scope,
            ObjectType::Channel,
            &store,
            &ns,
            SubjectRef::direct(SubjectType::User, user),
        )
        .unwrap();
        assert_eq!(reg.scope_for(connector).unwrap(), scope);
    }

    #[test]
    fn scope_for_missing_errors() {
        let reg = AttachmentRegistry::new();
        let err = reg.scope_for(ConnectorInstanceId::new_v4()).unwrap_err();
        assert!(matches!(err, ConnectorError::AttachmentNotFound));
    }

    // ---- user-scoped connector tests ----

    #[test]
    fn user_scoped_attach_succeeds_with_editor() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant_on(&mut store, ObjectType::User, scope, Relation::Editor, user);

        let mut reg = AttachmentRegistry::new();
        let connector = ConnectorInstanceId::new_v4();
        let attachment = reg
            .attach(
                connector,
                ConnectorKind::GoogleDrive,
                scope,
                ObjectType::User,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap();
        assert_eq!(attachment.scope_id, scope);
    }

    #[test]
    fn user_scoped_attach_denied_for_member() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant_on(&mut store, ObjectType::User, scope, Relation::Member, user);

        let mut reg = AttachmentRegistry::new();
        let err = reg
            .attach(
                ConnectorInstanceId::new_v4(),
                ConnectorKind::Notion,
                scope,
                ObjectType::User,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::PermissionDenied));
    }

    #[test]
    fn user_scoped_detach_succeeds() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant_on(&mut store, ObjectType::User, scope, Relation::Admin, user);

        let mut reg = AttachmentRegistry::new();
        let subject = SubjectRef::direct(SubjectType::User, user);
        let connector = ConnectorInstanceId::new_v4();
        reg.attach(
            connector,
            ConnectorKind::Slack,
            scope,
            ObjectType::User,
            &store,
            &ns,
            subject,
        )
        .unwrap();
        let removed = reg.detach(connector, &store, &ns, subject).unwrap();
        assert_eq!(removed.connector, connector);
        assert!(reg.is_empty());
    }

    // ---- domain-scoped connector tests ----

    #[test]
    fn domain_scoped_attach_succeeds_with_admin() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant_on(&mut store, ObjectType::Domain, scope, Relation::Admin, user);

        let mut reg = AttachmentRegistry::new();
        let connector = ConnectorInstanceId::new_v4();
        let attachment = reg
            .attach(
                connector,
                ConnectorKind::Jira,
                scope,
                ObjectType::Domain,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap();
        assert_eq!(attachment.scope_id, scope);
    }

    #[test]
    fn domain_scoped_attach_denied_for_viewer() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant_on(
            &mut store,
            ObjectType::Domain,
            scope,
            Relation::Viewer,
            user,
        );

        let mut reg = AttachmentRegistry::new();
        let err = reg
            .attach(
                ConnectorInstanceId::new_v4(),
                ConnectorKind::Confluence,
                scope,
                ObjectType::Domain,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::PermissionDenied));
    }

    #[test]
    fn domain_scoped_detach_succeeds() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant_on(
            &mut store,
            ObjectType::Domain,
            scope,
            Relation::Editor,
            user,
        );

        let mut reg = AttachmentRegistry::new();
        let subject = SubjectRef::direct(SubjectType::User, user);
        let connector = ConnectorInstanceId::new_v4();
        reg.attach(
            connector,
            ConnectorKind::GitHub,
            scope,
            ObjectType::Domain,
            &store,
            &ns,
            subject,
        )
        .unwrap();
        let removed = reg.detach(connector, &store, &ns, subject).unwrap();
        assert_eq!(removed.connector, connector);
        assert!(reg.is_empty());
    }

    #[test]
    fn wrong_object_type_denies_even_with_grant() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        // Grant on Channel, but attempt attach as User scope.
        grant_on(
            &mut store,
            ObjectType::Channel,
            scope,
            Relation::Admin,
            user,
        );

        let mut reg = AttachmentRegistry::new();
        let err = reg
            .attach(
                ConnectorInstanceId::new_v4(),
                ConnectorKind::Notion,
                scope,
                ObjectType::User,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::PermissionDenied));
    }

    #[test]
    fn unsupported_object_type_rejected() {
        let (mut store, ns) = fresh();
        let scope = ScopeId::new_v4();
        let user = Uuid::new_v4();
        grant_on(&mut store, ObjectType::Device, scope, Relation::Admin, user);

        let mut reg = AttachmentRegistry::new();
        let err = reg
            .attach(
                ConnectorInstanceId::new_v4(),
                ConnectorKind::Notion,
                scope,
                ObjectType::Device,
                &store,
                &ns,
                SubjectRef::direct(SubjectType::User, user),
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::UnsupportedObjectType));
    }
}
