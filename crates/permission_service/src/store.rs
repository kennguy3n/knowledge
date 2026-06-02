//! In-memory tuple store — the query surface for the permission
//! service. The on-disk durability layer wraps this store via
//! [`crate::PersistentTupleStore`], mirroring every mutation to a
//! SQLCipher database while leaving the in-memory `HashSet` as the
//! authoritative query view for `check_permission`.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::{PermissionError, Result};
use crate::tuple::{ObjectRef, Relation, RelationTuple};

/// Append-only-ish in-memory tuple store. Tuples are inserted /
/// removed as a set; idempotent insertion is rejected so callers can
/// detect double-write bugs (use [`Self::upsert`] for the
/// best-effort flavour).
///
/// This is the query surface used by [`crate::check::check_permission`];
/// callers that want their tuples to survive a process restart
/// should wrap this store with [`crate::PersistentTupleStore`].
///
/// Internally, a secondary `(object, relation) -> [tuple]` index is
/// maintained alongside the authoritative `HashSet` so that
/// [`Self::iter_for_object_relation`] is `O(k)` in the size of the
/// matching set rather than `O(n)` in the size of the entire store —
/// this is the hot path in [`crate::check::check_permission`].
#[derive(Debug, Clone, Default)]
pub struct TupleStore {
    tuples: HashSet<RelationTuple>,
    /// Reverse index `(object, relation) -> [tuple]` for `O(1)`
    /// average dispatch from `walk` in [`crate::check`]. Skipped on
    /// the wire and rebuilt from `tuples` on deserialise.
    index: HashMap<(ObjectRef, Relation), Vec<RelationTuple>>,
}

impl TupleStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a tuple. Returns
    /// [`PermissionError::DuplicateTuple`] if it was already present.
    pub fn insert(&mut self, tuple: RelationTuple) -> Result<()> {
        if !self.tuples.insert(tuple) {
            return Err(PermissionError::DuplicateTuple);
        }
        self.index_push(&tuple);
        Ok(())
    }

    /// Insert a tuple, ignoring duplicates. Returns `true` iff the
    /// tuple was newly inserted.
    pub fn upsert(&mut self, tuple: RelationTuple) -> bool {
        let newly_inserted = self.tuples.insert(tuple);
        if newly_inserted {
            self.index_push(&tuple);
        }
        newly_inserted
    }

    /// Remove a tuple. Returns
    /// [`PermissionError::NotFound`] if it was not present.
    pub fn remove(&mut self, tuple: &RelationTuple) -> Result<()> {
        if !self.tuples.remove(tuple) {
            return Err(PermissionError::NotFound);
        }
        self.index_remove(tuple);
        Ok(())
    }

    /// True iff the exact tuple is in the store.
    pub fn contains(&self, tuple: &RelationTuple) -> bool {
        self.tuples.contains(tuple)
    }

    /// Number of tuples in the store.
    pub fn len(&self) -> usize {
        self.tuples.len()
    }

    /// True iff the store is empty.
    pub fn is_empty(&self) -> bool {
        self.tuples.is_empty()
    }

    /// All tuples matching `(object, relation)` via the
    /// secondary index — `O(k)` where `k` is the size of the
    /// matching set.
    pub fn iter_for_object_relation(&self,
        object: ObjectRef,
        relation: Relation,
    ) -> impl Iterator<Item = &RelationTuple> + '_ {
        self.index
            .get(&(object, relation))
            .map_or(&[][..], Vec::as_slice)
            .iter()
    }

    /// All tuples in the store. The iteration order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = &RelationTuple> {
        self.tuples.iter()
    }

    /// Insert `tuple` into the secondary index. Caller must ensure
    /// the tuple was actually newly inserted into `tuples`.
    fn index_push(&mut self, tuple: &RelationTuple) {
        self.index
            .entry((tuple.object, tuple.relation))
            .or_default()
            .push(*tuple);
    }

    /// Remove `tuple` from the secondary index. Caller must ensure
    /// the tuple was actually removed from `tuples`.
    fn index_remove(&mut self, tuple: &RelationTuple) {
        let key = (tuple.object, tuple.relation);
        let drop_bucket = if let Some(bucket) = self.index.get_mut(&key) {
            if let Some(pos) = bucket.iter().position(|t| t == tuple) {
                bucket.swap_remove(pos);
            }
            bucket.is_empty()
        } else {
            false
        };
        if drop_bucket {
            self.index.remove(&key);
        }
    }

    /// Drop and re-populate the secondary index from the `tuples`
    /// `HashSet`. Used after deserialisation (which skips the
    /// index field) or after a manual mutation through a raw
    /// `tuples` view.
    fn rebuild_index(&mut self) {
        self.index.clear();
        for tuple in &self.tuples {
            self.index
                .entry((tuple.object, tuple.relation))
                .or_default()
                .push(*tuple);
        }
    }
}

impl PartialEq for TupleStore {
    fn eq(&self, other: &Self) -> bool {
        // Equality is defined by the authoritative `tuples` set —
        // the secondary index is purely derived state.
        self.tuples == other.tuples
    }
}

impl Eq for TupleStore {}

impl Serialize for TupleStore {
    fn serialize<S: serde::Serializer>(&self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            tuples: &'a HashSet<RelationTuple>,
        }
        Wire {
            tuples: &self.tuples,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TupleStore {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            tuples: HashSet<RelationTuple>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let mut store = TupleStore {
            tuples: wire.tuples,
            index: HashMap::new(),
        };
        store.rebuild_index();
        Ok(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuple::{ObjectType, SubjectRef, SubjectType};
    use uuid::Uuid;

    fn fixture_tuple(rel: Relation, obj_id: Uuid, sub_id: Uuid) -> RelationTuple {
        RelationTuple::new(ObjectRef::new(ObjectType::Channel, obj_id),
            rel,
            SubjectRef::direct(SubjectType::User, sub_id),
        )
    }

    #[test]
    fn index_lookup_returns_only_matching_tuples() {
        let mut store = TupleStore::new();
        let chan_a = Uuid::new_v4();
        let chan_b = Uuid::new_v4();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();

        let t1 = fixture_tuple(Relation::Member, chan_a, user1);
        let t2 = fixture_tuple(Relation::Member, chan_a, user2);
        let t3 = fixture_tuple(Relation::Owner, chan_a, user1);
        let t4 = fixture_tuple(Relation::Member, chan_b, user1);
        store.insert(t1).unwrap();
        store.insert(t2).unwrap();
        store.insert(t3).unwrap();
        store.insert(t4).unwrap();

        let chan_a_ref = ObjectRef::new(ObjectType::Channel, chan_a);
        let members: HashSet<_> = store
            .iter_for_object_relation(chan_a_ref, Relation::Member)
            .copied()
            .collect();
        assert_eq!(members, HashSet::from([t1, t2]));

        let owners: HashSet<_> = store
            .iter_for_object_relation(chan_a_ref, Relation::Owner)
            .copied()
            .collect();
        assert_eq!(owners, HashSet::from([t3]));
    }

    #[test]
    fn index_is_maintained_across_remove() {
        let mut store = TupleStore::new();
        let chan = Uuid::new_v4();
        let user = Uuid::new_v4();
        let t = fixture_tuple(Relation::Member, chan, user);
        store.insert(t).unwrap();
        store.remove(&t).unwrap();

        let chan_ref = ObjectRef::new(ObjectType::Channel, chan);
        assert_eq!(store
                .iter_for_object_relation(chan_ref, Relation::Member)
                .count(),
            0
        );
        // The empty bucket should have been pruned to bound memory.
        assert!(!store.index.contains_key(&(chan_ref, Relation::Member)));
    }

    #[test]
    fn deserialise_rebuilds_index() {
        let mut original = TupleStore::new();
        let chan = Uuid::new_v4();
        let user = Uuid::new_v4();
        let t = fixture_tuple(Relation::Member, chan, user);
        original.insert(t).unwrap();

        let json = serde_json::to_string(&original).unwrap();
        let round: TupleStore = serde_json::from_str(&json).unwrap();
        assert_eq!(round, original);
        let chan_ref = ObjectRef::new(ObjectType::Channel, chan);
        assert_eq!(round
                .iter_for_object_relation(chan_ref, Relation::Member)
                .count(),
            1
        );
    }
}
