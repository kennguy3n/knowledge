//! Reachability check — the permission decision procedure.
//!
//! Given an [`ObjectRef`], a [`Relation`], and a [`SubjectRef`], the
//! check returns `true` iff *some* path through the relation graph,
//! folding in namespace inheritance, leads from the object/relation
//! pair to the subject.
//!
//! Algorithm (BFS over the relation graph):
//!
//! 1. Expand `wanted` through the namespace into the set of
//!    relations that *imply* it (the "covering relations" — anyone
//!    holding any of these relations on the object also holds
//!    `wanted`).
//! 2. For every covering relation, look at the tuples
//!    `(object, covering, ?)`:
//!    * If the tuple's subject is the target subject directly,
//!      return `true`.
//!    * If the tuple's subject has a `subject_relation` (userset
//!      rewrite), recurse into the subject as the new object with
//!      that relation as the new wanted, and the original target
//!      subject preserved.
//! 3. Bound the recursion with a visited-set on
//!    `(object, relation, subject_relation_chain_depth)` so cycles
//!    terminate.
//!
//! The visited-set is keyed on `(ObjectRef, Relation)`; for the
//! Phase 3 in-memory store this is more than enough to avoid
//! pathological loops.

use std::collections::HashSet;

use crate::namespace::NamespaceRegistry;
use crate::store::TupleStore;
use crate::tuple::{ObjectRef, Relation, SubjectRef};

/// Output of [`check_permission`] / [`PermissionCheck::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionDecision {
    /// `true` iff the subject can exercise the relation on the object.
    pub allowed: bool,
}

/// Convenience facade bundling a [`TupleStore`] and a
/// [`NamespaceRegistry`].
#[derive(Debug, Clone)]
pub struct PermissionCheck<'a> {
    /// Tuple store to read from.
    pub store: &'a TupleStore,
    /// Namespace registry for relation inheritance.
    pub namespaces: &'a NamespaceRegistry,
}

impl<'a> PermissionCheck<'a> {
    /// Construct a fresh check facade.
    pub fn new(store: &'a TupleStore, namespaces: &'a NamespaceRegistry) -> Self {
        Self { store, namespaces }
    }

    /// Evaluate the permission decision.
    pub fn evaluate(
        &self,
        object: ObjectRef,
        relation: Relation,
        subject: SubjectRef,
    ) -> PermissionDecision {
        let mut visited = HashSet::new();
        let allowed = walk(
            self.store,
            self.namespaces,
            object,
            relation,
            subject,
            &mut visited,
        );
        PermissionDecision { allowed }
    }
}

/// Top-level convenience — equivalent to
/// `PermissionCheck::new(store, namespaces).evaluate(...).allowed`.
pub fn check_permission(
    store: &TupleStore,
    namespaces: &NamespaceRegistry,
    object: ObjectRef,
    relation: Relation,
    subject: SubjectRef,
) -> bool {
    PermissionCheck::new(store, namespaces)
        .evaluate(object, relation, subject)
        .allowed
}

fn walk(
    store: &TupleStore,
    namespaces: &NamespaceRegistry,
    object: ObjectRef,
    wanted: Relation,
    target: SubjectRef,
    visited: &mut HashSet<(ObjectRef, Relation)>,
) -> bool {
    // Expand `wanted` into "any relation that implies `wanted`". For
    // the substrate's default chain (Owner ⇒ Admin ⇒ Editor ⇒ Member
    // ⇒ Viewer), asking for `Viewer` should also accept holders of
    // `Member`, `Editor`, `Admin`, and `Owner`. We compute the
    // *covering* set — the inverse of the closure: every relation
    // whose closure contains `wanted`.
    let covering = covering_relations(namespaces, object.object_type, wanted);

    for cover in covering {
        if !visited.insert((object, cover)) {
            continue;
        }
        for tuple in store.iter_for_object_relation(object, cover) {
            // Direct hit: tuple subject == target (with both having
            // no subject_relation rewrite).
            if tuple.subject.subject_type == target.subject_type
                && tuple.subject.subject_id == target.subject_id
                && tuple.subject.subject_relation.is_none()
                && target.subject_relation.is_none()
            {
                return true;
            }
            // Userset rewrite: tuple subject is `(t, id) # rel`.
            // Recurse: ask the question `does target hold rel on
            // (t, id)?`.
            if let Some(rel) = tuple.subject.subject_relation {
                let next_object =
                    ObjectRef::new(tuple.subject.subject_type, tuple.subject.subject_id);
                if walk(store, namespaces, next_object, rel, target, visited) {
                    return true;
                }
            }
        }
    }
    false
}

/// Compute the set of relations `r` such that `r` *implies*
/// `wanted` under `object_type`'s namespace. With the default
/// inheritance chain `Owner ⇒ Admin ⇒ Editor ⇒ Member ⇒ Viewer`,
/// `covering_relations(_, _, Viewer)` returns all five relations.
fn covering_relations(
    namespaces: &NamespaceRegistry,
    object_type: crate::tuple::ObjectType,
    wanted: Relation,
) -> Vec<Relation> {
    let all = [
        Relation::Owner,
        Relation::Admin,
        Relation::Editor,
        Relation::Member,
        Relation::Viewer,
        Relation::Synthesizer,
        Relation::Proposer,
    ];
    all.into_iter()
        .filter(|r| *r == wanted || namespaces.implies(object_type, *r, wanted))
        .collect()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::tuple::{ObjectType, RelationTuple, SubjectType};

    use super::*;

    fn fresh() -> (TupleStore, NamespaceRegistry) {
        (TupleStore::new(), NamespaceRegistry::with_defaults())
    }

    #[test]
    fn direct_relation_check_succeeds() {
        let (mut store, ns) = fresh();
        let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
        let user = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
        store
            .insert(RelationTuple::new(tenant, Relation::Member, user))
            .unwrap();
        assert!(check_permission(
            &store,
            &ns,
            tenant,
            Relation::Member,
            user
        ));
    }

    #[test]
    fn missing_relation_returns_false() {
        let (store, ns) = fresh();
        let tenant = ObjectRef::new(ObjectType::Tenant, Uuid::new_v4());
        let user = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
        assert!(!check_permission(
            &store,
            &ns,
            tenant,
            Relation::Viewer,
            user
        ));
    }
}
