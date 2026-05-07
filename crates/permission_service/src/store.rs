//! In-memory tuple store.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{PermissionError, Result};
use crate::tuple::{ObjectRef, Relation, RelationTuple};

/// Append-only-ish in-memory tuple store. Tuples are inserted /
/// removed as a set; idempotent insertion is rejected so callers can
/// detect double-write bugs (use [`Self::upsert`] for the
/// best-effort flavour).
///
/// Persistence is intentionally deferred — Phase 3 ships the
/// in-memory variant and the on-disk SQLCipher-backed variant lands
/// in a later phase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TupleStore {
    tuples: HashSet<RelationTuple>,
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
        Ok(())
    }

    /// Insert a tuple, ignoring duplicates. Returns `true` iff the
    /// tuple was newly inserted.
    pub fn upsert(&mut self, tuple: RelationTuple) -> bool {
        self.tuples.insert(tuple)
    }

    /// Remove a tuple. Returns
    /// [`PermissionError::NotFound`] if it was not present.
    pub fn remove(&mut self, tuple: &RelationTuple) -> Result<()> {
        if !self.tuples.remove(tuple) {
            return Err(PermissionError::NotFound);
        }
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

    /// All tuples matching `(object, relation)`.
    pub fn iter_for_object_relation(
        &self,
        object: ObjectRef,
        relation: Relation,
    ) -> impl Iterator<Item = &RelationTuple> + '_ {
        self.tuples
            .iter()
            .filter(move |t| t.object == object && t.relation == relation)
    }

    /// All tuples in the store. The iteration order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = &RelationTuple> {
        self.tuples.iter()
    }
}
