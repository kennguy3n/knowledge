//! Add-Wins Observed-Remove Set (`AddWinsSet<T>`).
//!
//! Per `docs/technical/design.md` §3.2: synthesis objects
//! that are *added* concurrently with a *remove* must remain in the
//! merged set — "add wins". This is implemented as the standard
//! observed-remove set: each `add` allocates a fresh tag (UUID v4),
//! and a `remove` records every tag the local replica has observed
//! at the time. After merge, an element is **present** iff there
//! exists at least one tag for it that has not been tombstoned.
//!
//! The CRDT is conflict-free, commutative, associative, and
//! idempotent under [`Self::merge`].

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Add-wins observed-remove set parameterised over the element type.
///
/// `T` must be hashable and serialisable so the set can be merged
/// across replicas and persisted in op logs.
///
/// Identity is byte-exact (i.e. whatever `Hash` + `Eq` derive on
/// `T`). When callers pick a text-bearing `T` (`String` /
/// `Cow<'_, str>` / a struct with a string field), Unicode
/// normalisation (NFC vs NFD vs NFKC) and the preservation of
/// bidi-control / zero-width / BOM code points are the caller's
/// responsibility — see the "Multilingual contract" section in
/// the crate-level docs (`crates/sync_engine/src/lib.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddWinsSet<T>
where
    T: Eq + Hash + Clone,
{
    /// `element -> set of unique tags`. A tag is present iff the
    /// corresponding `add` has not been tombstoned.
    elements: HashMap<T, HashSet<Uuid>>,
    /// Tombstoned tags — observed `remove`s.
    tombstones: HashSet<Uuid>,
}

impl<T> AddWinsSet<T>
where
    T: Eq + Hash + Clone,
{
    /// Construct an empty set.
    pub fn new() -> Self {
        Self {
            elements: HashMap::new(),
            tombstones: HashSet::new(),
        }
    }

    /// Number of elements currently observed (regardless of tombstone
    /// status). Equivalent to `self.elements().count() +
    /// (number of tombstoned elements)`.
    pub fn observed_len(&self) -> usize {
        self.elements.len()
    }

    /// Add `value` to the set, returning the tag allocated for the
    /// add operation. The tag is what callers must record on the op
    /// log so that concurrent removes can be matched against it.
    pub fn add(&mut self, value: T) -> Uuid {
        let tag = Uuid::new_v4();
        self.elements.entry(value).or_default().insert(tag);
        tag
    }

    /// Add `value` with an externally-allocated `tag`. Used when
    /// replaying an op log so that `tag` is propagated rather than
    /// re-generated.
    pub fn add_with_tag(&mut self, value: T, tag: Uuid) {
        self.elements.entry(value).or_default().insert(tag);
    }

    /// Remove every observed tag for `value` (observed-remove
    /// semantics: only the *currently-observed* tags get
    /// tombstoned). Concurrent adds with fresh tags are unaffected
    /// — this is the "add wins" property.
    ///
    /// In-process callers (e.g. unit tests, the synthesis pipeline
    /// before any cross-replica merge) use this convenience form,
    /// which snapshots whatever tags are currently observed locally
    /// and tombstones exactly those. For replay across an [`OpLog`]
    /// — where the original `Remove` op carries the tags that the
    /// authoring replica had observed — use [`Self::remove_tags`]
    /// instead so that tags added concurrently by another replica
    /// (and merged into the log out of order) are not tombstoned by
    /// mistake.
    ///
    /// [`OpLog`]: crate::op_log::OpLog
    pub fn remove(&mut self, value: &T) {
        if let Some(tags) = self.elements.get(value).cloned() {
            for tag in tags {
                self.tombstones.insert(tag);
            }
        }
    }

    /// Tombstone *exactly* the supplied tags for `value`. Tags not
    /// listed in `tags` are left observable, which preserves the
    /// add-wins property even when ops from concurrent replicas have
    /// been merged into the log in arbitrary order.
    ///
    /// Used by [`OpLog::replay`] to honour the `observed_tags`
    /// snapshot recorded on each `Remove` / `Supersede` op.
    ///
    /// [`OpLog::replay`]: crate::op_log::OpLog::replay
    pub fn remove_tags(&mut self, _value: &T, tags: &[Uuid]) {
        for tag in tags {
            self.tombstones.insert(*tag);
        }
    }

    /// True iff `value` has any non-tombstoned tag.
    pub fn contains(&self, value: &T) -> bool {
        self.elements
            .get(value)
            .is_some_and(|tags| tags.iter().any(|tag| !self.tombstones.contains(tag)))
    }

    /// Iterate over the live elements of the set (those with at
    /// least one non-tombstoned tag). Order is unspecified.
    pub fn elements(&self) -> impl Iterator<Item = &T> {
        self.elements
            .iter()
            .filter(|(_, tags)| tags.iter().any(|tag| !self.tombstones.contains(tag)))
            .map(|(elem, _)| elem)
    }

    /// All tags ever observed for `value` (including tombstoned).
    pub fn tags_for(&self, value: &T) -> Vec<Uuid> {
        self.elements
            .get(value)
            .map(|tags| tags.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Number of distinct values currently live (i.e. with at
    /// least one non-tombstoned tag).
    pub fn elements_count(&self) -> usize {
        self.elements
            .iter()
            .filter(|(_, tags)| tags.iter().any(|tag| !self.tombstones.contains(tag)))
            .count()
    }

    /// Iterate over `(value, live_tags)` pairs — i.e. every value
    /// with at least one non-tombstoned tag, paired with the
    /// snapshot of its surviving tags. Used by [`OpLog::compact`]
    /// to emit a minimal set of `Add` ops covering the live state.
    ///
    /// Order is unspecified, but stable within a single iteration.
    ///
    /// [`OpLog::compact`]: crate::op_log::OpLog::compact
    pub fn entries(&self) -> impl Iterator<Item = (&T, Vec<Uuid>)> {
        self.elements.iter().filter_map(|(value, tags)| {
            let live: Vec<Uuid> = tags
                .iter()
                .copied()
                .filter(|tag| !self.tombstones.contains(tag))
                .collect();
            if live.is_empty() {
                None
            } else {
                Some((value, live))
            }
        })
    }

    /// Set of tombstoned tags (for diagnostics / logging).
    pub fn tombstones(&self) -> &HashSet<Uuid> {
        &self.tombstones
    }

    /// Merge `other` into `self` in place.
    ///
    /// Idempotent, commutative, associative.
    pub fn merge(&mut self, other: &Self) {
        for (value, tags) in &other.elements {
            self.elements.entry(value.clone()).or_default().extend(tags);
        }
        self.tombstones.extend(other.tombstones.iter().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_is_empty() {
        let s: AddWinsSet<&str> = AddWinsSet::new();
        assert!(!s.contains(&"a"));
        assert_eq!(s.elements().count(), 0);
    }

    #[test]
    fn add_then_remove() {
        let mut s = AddWinsSet::new();
        s.add("a");
        assert!(s.contains(&"a"));
        s.remove(&"a");
        assert!(!s.contains(&"a"));
    }

    #[test]
    fn add_wins_over_concurrent_remove() {
        // Replica X removes "a"; replica Y concurrently re-adds "a"
        // (so it has a fresh tag that Y did not yet observe in X's
        // view). After merge, "a" must still be present.
        let mut x = AddWinsSet::new();
        let mut y = AddWinsSet::new();
        x.add("a");

        // Y forks from X.
        let mut y_view = x.clone();
        // X removes the observed-tag for "a".
        x.remove(&"a");
        // Y concurrently adds a fresh tag for "a".
        y.add_with_tag("a", Uuid::new_v4());
        // Merge X into Y; merge Y into the canonical view.
        y_view.merge(&x);
        y_view.merge(&y);
        assert!(y_view.contains(&"a"), "add wins over concurrent remove");
    }

    #[test]
    fn merge_is_idempotent() {
        let mut a = AddWinsSet::new();
        a.add("x");
        a.add("y");
        a.remove(&"x");
        let snapshot = a.clone();
        a.merge(&snapshot);
        a.merge(&snapshot);
        assert!(!a.contains(&"x"));
        assert!(a.contains(&"y"));
    }

    #[test]
    fn merge_is_commutative() {
        let mut a = AddWinsSet::new();
        a.add("a");
        let mut b = AddWinsSet::new();
        b.add("b");
        b.remove(&"a");

        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);

        assert_eq!(ab.contains(&"a"), ba.contains(&"a"));
        assert_eq!(ab.contains(&"b"), ba.contains(&"b"));
    }
}
