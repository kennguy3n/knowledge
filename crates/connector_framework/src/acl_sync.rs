//! ACL sync — projects source-system permissions into the
//! substrate's `permission_service` relation graph.
//!
//! Per `docs/technical/design.md` §10.2 point 5, every connector is responsible
//! for keeping the substrate's view of *who can read what* in sync
//! with the source. The substrate models this with relation tuples
//! over the source-mirrored object (typically `Concept`, but in
//! this module we project to whichever object type the caller
//! configures via [`PermissionMapping::object_type`]).
//!
//! The mapping is bidirectional in spirit:
//!
//! * On grant — the source reports that user `u` has `read` /
//!   `write` / `admin` on document `d`. [`AclSyncEngine::sync`]
//!   upserts `(object, relation, user)` into the
//!   [`permission_service::TupleStore`].
//! * On revoke — the source reports that `u` no longer has access
//!   to `d`. [`AclSyncEngine::sync`] removes the corresponding
//!   tuple.
//!
//! [`PermissionMapping`] resolves source-side identifiers
//! (`SourceUserId`, `SourceDocumentId`) to substrate-side UUIDs.
//! Resolution misses (an unknown source user) are surfaced via
//! [`AclSyncReport::unknown_users`] rather than failing the entire
//! batch.

use std::collections::HashMap;

use permission_service::{
    ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef, SubjectType, TupleStore,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;
use crate::event::{SourceDocumentId, SourceUserId};
use crate::token_vault::ConnectorInstanceId;

/// Permission level as reported by the source system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourcePermissionLevel {
    /// Read-only access on the source.
    Read,
    /// Read / write access on the source.
    Write,
    /// Administrative access on the source (manage permissions etc.).
    Admin,
}

impl SourcePermissionLevel {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

/// One source-side permission grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePermission {
    /// Source-side user.
    pub source_user_id: SourceUserId,
    /// Source-side document.
    pub source_document_id: SourceDocumentId,
    /// Permission level granted to the user on the document.
    pub level: SourcePermissionLevel,
}

impl SourcePermission {
    /// Construct a new source permission.
    pub fn new(
        user: SourceUserId,
        document: SourceDocumentId,
        level: SourcePermissionLevel,
    ) -> Self {
        Self {
            source_user_id: user,
            source_document_id: document,
            level,
        }
    }
}

/// Mapping from source-side identifiers to substrate-side
/// `(ObjectRef, SubjectRef)` pairs.
///
/// The mapping also encodes the *level → relation* projection used
/// when upserting tuples (e.g. `Read → Viewer`, `Write → Editor`,
/// `Admin → Admin`). Callers can override the projection by
/// constructing their own `level_to_relation` map.
#[derive(Debug, Clone)]
pub struct PermissionMapping {
    /// Object type that source documents project to (typically
    /// [`ObjectType::Concept`] for connector-derived rows).
    pub object_type: ObjectType,
    /// Subject type that source users project to (typically
    /// [`SubjectType::User`]).
    pub subject_type: SubjectType,
    /// Source user → substrate user UUID.
    user_resolver: HashMap<SourceUserId, Uuid>,
    /// Source document → substrate object UUID.
    document_resolver: HashMap<SourceDocumentId, Uuid>,
    /// Level → relation projection.
    level_to_relation: HashMap<SourcePermissionLevel, Relation>,
}

impl PermissionMapping {
    /// Construct a default mapping that projects:
    ///
    /// * `Read → Viewer`
    /// * `Write → Editor`
    /// * `Admin → Admin`
    ///
    /// against the supplied `object_type` / `subject_type`.
    pub fn new(object_type: ObjectType, subject_type: SubjectType) -> Self {
        let mut level_to_relation = HashMap::new();
        level_to_relation.insert(SourcePermissionLevel::Read, Relation::Viewer);
        level_to_relation.insert(SourcePermissionLevel::Write, Relation::Editor);
        level_to_relation.insert(SourcePermissionLevel::Admin, Relation::Admin);
        Self {
            object_type,
            subject_type,
            user_resolver: HashMap::new(),
            document_resolver: HashMap::new(),
            level_to_relation,
        }
    }

    /// Override the level → relation projection for one level.
    pub fn with_relation_for(mut self, level: SourcePermissionLevel, relation: Relation) -> Self {
        self.level_to_relation.insert(level, relation);
        self
    }

    /// Register a `source_user_id → substrate user uuid` mapping.
    pub fn map_user(&mut self, source: SourceUserId, substrate: Uuid) {
        self.user_resolver.insert(source, substrate);
    }

    /// Register a `source_document_id → substrate object uuid`
    /// mapping.
    pub fn map_document(&mut self, source: SourceDocumentId, substrate: Uuid) {
        self.document_resolver.insert(source, substrate);
    }

    /// Resolve a source user to a substrate UUID.
    pub fn resolve_user(&self, source: &SourceUserId) -> Option<Uuid> {
        self.user_resolver.get(source).copied()
    }

    /// Resolve a source document to a substrate UUID.
    pub fn resolve_document(&self, source: &SourceDocumentId) -> Option<Uuid> {
        self.document_resolver.get(source).copied()
    }

    /// Map a source level to a substrate relation.
    pub fn relation_for(&self, level: SourcePermissionLevel) -> Relation {
        self.level_to_relation
            .get(&level)
            .copied()
            .unwrap_or(Relation::Viewer)
    }
}

/// One source-side revocation — the user no longer has *any*
/// permission on the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRevocation {
    /// Source-side user whose access was revoked.
    pub source_user_id: SourceUserId,
    /// Source-side document.
    pub source_document_id: SourceDocumentId,
}

/// One delta to project — either a grant (with a level) or a
/// revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDelta {
    /// Upsert this grant.
    Grant(SourcePermission),
    /// Remove any existing grant for `(user, document)`.
    Revoke(SourceRevocation),
}

/// Summary of one ACL sync run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AclSyncReport {
    /// Number of new tuples inserted.
    pub inserted: usize,
    /// Number of existing tuples whose relation was updated
    /// (level changed). Implemented as remove-then-insert so the
    /// underlying store never holds two tuples for the same
    /// `(object, user)` simultaneously.
    pub updated: usize,
    /// Number of tuples removed (revocations + level changes).
    pub removed: usize,
    /// Source users that did not resolve via [`PermissionMapping`].
    pub unknown_users: Vec<SourceUserId>,
    /// Source documents that did not resolve via
    /// [`PermissionMapping`].
    pub unknown_documents: Vec<SourceDocumentId>,
}

/// Engine that drives ACL sync against a [`TupleStore`].
#[derive(Debug)]
pub struct AclSyncEngine<'a> {
    /// Underlying tuple store.
    pub store: &'a mut TupleStore,
    /// Mapping for resolving source ids and projecting levels.
    pub mapping: &'a PermissionMapping,
}

impl<'a> AclSyncEngine<'a> {
    /// Construct a fresh engine.
    pub fn new(store: &'a mut TupleStore, mapping: &'a PermissionMapping) -> Self {
        Self { store, mapping }
    }

    /// Project `deltas` (grants and revocations) onto the
    /// underlying store. Returns a [`AclSyncReport`] summarising
    /// what changed and which source ids could not be resolved.
    ///
    /// The `connector` argument is currently informational — it is
    /// not stored in the resulting tuples but is reserved for
    /// future provenance / audit metadata.
    pub fn sync(
        &mut self,
        _connector: ConnectorInstanceId,
        deltas: &[PermissionDelta],
    ) -> Result<AclSyncReport> {
        let mut report = AclSyncReport::default();
        for delta in deltas {
            match delta {
                PermissionDelta::Grant(g) => self.apply_grant(g, &mut report),
                PermissionDelta::Revoke(r) => self.apply_revoke(r, &mut report),
            }
        }
        Ok(report)
    }

    fn apply_grant(&mut self, grant: &SourcePermission, report: &mut AclSyncReport) {
        let Some(object_id) = self.mapping.resolve_document(&grant.source_document_id) else {
            report
                .unknown_documents
                .push(grant.source_document_id.clone());
            return;
        };
        let Some(subject_id) = self.mapping.resolve_user(&grant.source_user_id) else {
            report.unknown_users.push(grant.source_user_id.clone());
            return;
        };
        let object = ObjectRef::new(self.mapping.object_type, object_id);
        let subject = SubjectRef::direct(self.mapping.subject_type, subject_id);
        let relation = self.mapping.relation_for(grant.level);

        // Idempotent upsert: if the same tuple already exists,
        // there is nothing to do. If a tuple exists for the same
        // `(object, user)` but with a different relation, we
        // remove-then-insert and count one "update".
        let target = RelationTuple::new(object, relation, subject);
        if self.store.contains(&target) {
            return;
        }
        let mut existing_for_pair: Vec<RelationTuple> = Vec::new();
        for tuple in self.store.iter() {
            if tuple.object == object
                && tuple.subject.subject_type == subject.subject_type
                && tuple.subject.subject_id == subject.subject_id
                && tuple.subject.subject_relation.is_none()
            {
                existing_for_pair.push(*tuple);
            }
        }
        if existing_for_pair.is_empty() {
            self.store
                .insert(target)
                .expect("contains() returned false above");
            report.inserted += 1;
        } else {
            for tuple in &existing_for_pair {
                self.store.remove(tuple).expect("tuple was iter'd above");
                report.removed += 1;
            }
            self.store
                .insert(target)
                .expect("contains() returned false above");
            report.updated += 1;
        }
    }

    fn apply_revoke(&mut self, revoke: &SourceRevocation, report: &mut AclSyncReport) {
        let Some(object_id) = self.mapping.resolve_document(&revoke.source_document_id) else {
            report
                .unknown_documents
                .push(revoke.source_document_id.clone());
            return;
        };
        let Some(subject_id) = self.mapping.resolve_user(&revoke.source_user_id) else {
            report.unknown_users.push(revoke.source_user_id.clone());
            return;
        };
        let object = ObjectRef::new(self.mapping.object_type, object_id);

        let to_remove: Vec<RelationTuple> = self
            .store
            .iter()
            .filter(|t| {
                t.object == object
                    && t.subject.subject_type == self.mapping.subject_type
                    && t.subject.subject_id == subject_id
                    && t.subject.subject_relation.is_none()
            })
            .copied()
            .collect();
        for tuple in to_remove {
            self.store.remove(&tuple).expect("tuple was iter'd above");
            report.removed += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_mapping() -> PermissionMapping {
        PermissionMapping::new(ObjectType::Concept, SubjectType::User)
    }

    fn make_grant(
        user: &str,
        doc: &str,
        level: SourcePermissionLevel,
    ) -> (SourcePermission, Uuid, Uuid) {
        (
            SourcePermission::new(SourceUserId::new(user), SourceDocumentId::new(doc), level),
            Uuid::new_v4(),
            Uuid::new_v4(),
        )
    }

    #[test]
    fn level_projects_to_default_relations() {
        let m = fresh_mapping();
        assert_eq!(
            m.relation_for(SourcePermissionLevel::Read),
            Relation::Viewer
        );
        assert_eq!(
            m.relation_for(SourcePermissionLevel::Write),
            Relation::Editor
        );
        assert_eq!(
            m.relation_for(SourcePermissionLevel::Admin),
            Relation::Admin
        );
    }

    #[test]
    fn custom_level_relation_overrides_default() {
        let m = fresh_mapping().with_relation_for(SourcePermissionLevel::Write, Relation::Member);
        assert_eq!(
            m.relation_for(SourcePermissionLevel::Write),
            Relation::Member
        );
    }

    #[test]
    fn grant_inserts_tuple_when_resolvable() {
        let mut store = TupleStore::new();
        let mut mapping = fresh_mapping();
        let (grant, doc_uuid, user_uuid) = make_grant("u-1", "d-1", SourcePermissionLevel::Write);
        mapping.map_document(grant.source_document_id.clone(), doc_uuid);
        mapping.map_user(grant.source_user_id.clone(), user_uuid);
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let report = engine
            .sync(
                ConnectorInstanceId::new_v4(),
                &[PermissionDelta::Grant(grant)],
            )
            .unwrap();
        assert_eq!(report.inserted, 1);
        assert_eq!(report.removed, 0);
        assert_eq!(report.updated, 0);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn duplicate_grant_is_idempotent() {
        let mut store = TupleStore::new();
        let mut mapping = fresh_mapping();
        let (grant, doc_uuid, user_uuid) = make_grant("u-2", "d-2", SourcePermissionLevel::Read);
        mapping.map_document(grant.source_document_id.clone(), doc_uuid);
        mapping.map_user(grant.source_user_id.clone(), user_uuid);
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let cid = ConnectorInstanceId::new_v4();
        let r1 = engine
            .sync(cid, &[PermissionDelta::Grant(grant.clone())])
            .unwrap();
        let r2 = engine.sync(cid, &[PermissionDelta::Grant(grant)]).unwrap();
        assert_eq!(r1.inserted, 1);
        assert_eq!(r2.inserted, 0);
        assert_eq!(r2.removed, 0);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn level_change_updates_tuple() {
        let mut store = TupleStore::new();
        let mut mapping = fresh_mapping();
        let user = SourceUserId::new("u-3");
        let doc = SourceDocumentId::new("d-3");
        let user_uuid = Uuid::new_v4();
        let doc_uuid = Uuid::new_v4();
        mapping.map_user(user.clone(), user_uuid);
        mapping.map_document(doc.clone(), doc_uuid);
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let cid = ConnectorInstanceId::new_v4();
        engine
            .sync(
                cid,
                &[PermissionDelta::Grant(SourcePermission::new(
                    user.clone(),
                    doc.clone(),
                    SourcePermissionLevel::Read,
                ))],
            )
            .unwrap();
        let r = engine
            .sync(
                cid,
                &[PermissionDelta::Grant(SourcePermission::new(
                    user,
                    doc,
                    SourcePermissionLevel::Admin,
                ))],
            )
            .unwrap();
        assert_eq!(r.updated, 1);
        assert_eq!(r.removed, 1);
        // Old tuple replaced; only the new one remains.
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn revoke_removes_tuple() {
        let mut store = TupleStore::new();
        let mut mapping = fresh_mapping();
        let user = SourceUserId::new("u-4");
        let doc = SourceDocumentId::new("d-4");
        mapping.map_user(user.clone(), Uuid::new_v4());
        mapping.map_document(doc.clone(), Uuid::new_v4());
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let cid = ConnectorInstanceId::new_v4();
        engine
            .sync(
                cid,
                &[PermissionDelta::Grant(SourcePermission::new(
                    user.clone(),
                    doc.clone(),
                    SourcePermissionLevel::Write,
                ))],
            )
            .unwrap();
        let r = engine
            .sync(
                cid,
                &[PermissionDelta::Revoke(SourceRevocation {
                    source_user_id: user,
                    source_document_id: doc,
                })],
            )
            .unwrap();
        assert_eq!(r.removed, 1);
        assert!(store.is_empty());
    }

    #[test]
    fn unknown_user_is_reported_not_failed() {
        let mut store = TupleStore::new();
        let mut mapping = fresh_mapping();
        // Map only the document; leave the user unmapped.
        mapping.map_document(SourceDocumentId::new("d-5"), Uuid::new_v4());
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let r = engine
            .sync(
                ConnectorInstanceId::new_v4(),
                &[PermissionDelta::Grant(SourcePermission::new(
                    SourceUserId::new("ghost"),
                    SourceDocumentId::new("d-5"),
                    SourcePermissionLevel::Read,
                ))],
            )
            .unwrap();
        assert_eq!(r.unknown_users, vec![SourceUserId::new("ghost")]);
        assert!(store.is_empty());
    }

    #[test]
    fn unknown_document_is_reported_not_failed() {
        let mut store = TupleStore::new();
        let mut mapping = fresh_mapping();
        mapping.map_user(SourceUserId::new("u-known"), Uuid::new_v4());
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let r = engine
            .sync(
                ConnectorInstanceId::new_v4(),
                &[PermissionDelta::Grant(SourcePermission::new(
                    SourceUserId::new("u-known"),
                    SourceDocumentId::new("d-ghost"),
                    SourcePermissionLevel::Read,
                ))],
            )
            .unwrap();
        assert_eq!(r.unknown_documents, vec![SourceDocumentId::new("d-ghost")]);
        assert!(store.is_empty());
    }
}
