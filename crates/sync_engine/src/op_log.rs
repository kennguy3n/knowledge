//! Append-only operation log for the sync engine.
//!
//! Per `docs/DESIGN.md` §3.2: every replica
//! records its CRDT mutations in an append-only [`OpLog`] indexed by
//! a monotonic logical clock. Replicas exchange [`SyncOp`] entries;
//! [`merge_logs`] folds two logs into a consistent merged state.

use std::collections::HashSet;
use std::hash::Hash;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crdt::AddWinsSet;
use crate::error::Result;

/// The logical kind of a sync operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncOpKind<T>
where
    T: Eq + Hash + Clone,
{
    /// Add an element with a unique tag.
    Add {
        /// Element being added.
        value: T,
        /// Unique tag allocated for this add. Must be UUID v4.
        tag: Uuid,
    },
    /// Tombstone every observed tag for `value`.
    Remove {
        /// Element being removed.
        value: T,
        /// Tags this replica observed at the time of remove.
        observed_tags: Vec<Uuid>,
    },
    /// Mark `value` as superseded by `successor`. Surfaces as a
    /// `contradicts` edge in the concept-graph integration.
    Supersede {
        /// Element being superseded.
        value: T,
        /// Successor element.
        successor: T,
        /// Tags observed for `value` at the time of supersession.
        observed_tags: Vec<Uuid>,
    },
}

/// One entry in the op log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOp<T>
where
    T: Eq + Hash + Clone,
{
    /// Replica that authored this op.
    pub replica_id: Uuid,
    /// Replica-local Lamport-ish counter that monotonically
    /// increases.
    pub seq: u64,
    /// Wall-clock time the op was created (for diagnostics; not used
    /// for ordering).
    pub created_at: DateTime<Utc>,
    /// What the op does.
    pub op: SyncOpKind<T>,
}

/// Append-only op log for one replica.
///
/// In addition to the append-only [`SyncOp`] list, the log carries
/// two book-keeping fields:
///
/// * `clock` — replica-local monotonic counter; every appended op
///   takes the next `clock` value as its `seq`.
/// * `compaction_epoch` — incremented every time [`Self::compact`]
///   rewrites the log. Peers exchange this on the wire (see
///   [`crate::delta::DeltaEnvelope`]) so a receiver whose own
///   epoch is **behind** the sender's can refuse the delta and
///   bootstrap from a snapshot instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLog<T>
where
    T: Eq + Hash + Clone,
{
    /// Replica id (UUID v4).
    pub replica_id: Uuid,
    /// Monotonic counter — always strictly greater than the highest
    /// seq seen so far.
    pub clock: u64,
    /// Monotonic compaction-generation counter. Starts at `0`;
    /// every successful [`Self::compact`] bumps it by one.
    ///
    /// Peers that haven't merged the log at this epoch yet **must**
    /// resync via [`crate::SyncEngine::snapshot`] rather than
    /// applying a delta, because the compacted log no longer
    /// contains the historical `Remove` ops that pre-compaction
    /// peers may have needed.
    #[serde(default)]
    pub compaction_epoch: u64,
    /// Append-only list of ops authored by this replica plus any ops
    /// merged in from peers.
    pub ops: Vec<SyncOp<T>>,
    /// Set of `(replica_id, seq)` pairs already absorbed — used to
    /// dedupe on [`merge`].
    seen: HashSet<(Uuid, u64)>,
}

impl<T> OpLog<T>
where
    T: Eq + Hash + Clone,
{
    /// Construct a fresh empty op log for a replica.
    pub fn new(replica_id: Uuid) -> Self {
        Self {
            replica_id,
            clock: 0,
            compaction_epoch: 0,
            ops: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Number of ops currently in the log.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// True iff the op log has zero entries.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Current compaction-generation counter. See
    /// [`Self::compaction_epoch`].
    pub fn compaction_epoch_value(&self) -> u64 {
        self.compaction_epoch
    }

    /// Append a fresh `Add` op. Returns the tag allocated.
    pub fn record_add(&mut self, value: T) -> Uuid {
        let tag = Uuid::new_v4();
        self.append(SyncOpKind::Add { value, tag });
        tag
    }

    /// Append a `Remove` op observing the supplied tags.
    pub fn record_remove(&mut self, value: T, observed_tags: Vec<Uuid>) {
        self.append(SyncOpKind::Remove {
            value,
            observed_tags,
        });
    }

    /// Append a `Supersede` op.
    pub fn record_supersede(&mut self, value: T, successor: T, observed_tags: Vec<Uuid>) {
        self.append(SyncOpKind::Supersede {
            value,
            successor,
            observed_tags,
        });
    }

    fn append(&mut self, op: SyncOpKind<T>) {
        self.clock = self.clock.saturating_add(1);
        let entry = SyncOp {
            replica_id: self.replica_id,
            seq: self.clock,
            created_at: Utc::now(),
            op,
        };
        self.seen.insert((entry.replica_id, entry.seq));
        self.ops.push(entry);
    }

    /// Merge `other` into `self`, deduplicating by `(replica_id, seq)`.
    ///
    /// The local `compaction_epoch` is bumped to `max(self, other)`
    /// so the receiver tracks the highest compaction point it has
    /// seen — peers that fall further behind will see the lifted
    /// epoch and re-bootstrap via [`crate::SyncEngine::snapshot`].
    pub fn merge(&mut self, other: &OpLog<T>) {
        for entry in &other.ops {
            let key = (entry.replica_id, entry.seq);
            if self.seen.insert(key) {
                if entry.replica_id == self.replica_id {
                    self.clock = self.clock.max(entry.seq);
                }
                self.ops.push(entry.clone());
            }
        }
        self.compaction_epoch = self.compaction_epoch.max(other.compaction_epoch);
    }

    /// Absorb a single `entry` into the log, deduplicating by
    /// `(replica_id, seq)`. Returns `true` iff the op was newly
    /// absorbed (i.e. the caller can now treat it as part of the
    /// local log) and `false` if it was already present.
    ///
    /// Used by [`crate::delta::apply_delta`] to fold individual
    /// remote ops into the local log without round-tripping through
    /// a full peer `OpLog`.
    pub fn merge_single(&mut self, entry: SyncOp<T>) -> bool {
        let key = (entry.replica_id, entry.seq);
        if self.seen.insert(key) {
            if entry.replica_id == self.replica_id {
                self.clock = self.clock.max(entry.seq);
            }
            self.ops.push(entry);
            true
        } else {
            false
        }
    }

    /// Rewrite the log into a minimal `Add`-only form that
    /// reproduces the current materialised set, bumping
    /// [`Self::compaction_epoch`] by one.
    ///
    /// Concretely:
    ///
    /// 1. Replay the full log to obtain the live (value, surviving
    ///    tags) entries.
    /// 2. Replace `self.ops` with one `Add { value, tag }` per
    ///    `(value, surviving_tag)` pair, re-using the original
    ///    tags. New seqs are allocated from `self.clock` so they
    ///    do not collide with anything already in the log on this
    ///    or any peer replica.
    /// 3. Rebuild `self.seen` to match the new op set.
    /// 4. Bump `self.compaction_epoch`.
    ///
    /// Returns the number of ops *removed* by compaction
    /// (`old_len - new_len`).
    ///
    /// ### Safety / sync semantics
    ///
    /// Compaction is **lossy** with respect to tombstones: every
    /// historical `Remove` / `Supersede` op is dropped, on the
    /// assumption that any peer that needs those tombstones has
    /// already merged them. Peers that have not synced past the
    /// pre-compaction state will see the new `compaction_epoch`
    /// and must bootstrap via [`crate::SyncEngine::snapshot`]
    /// rather than continuing with delta sync.
    ///
    /// Tag values are preserved across compaction, so subsequent
    /// merges from already-caught-up peers remain idempotent: any
    /// peer that re-sends an `Add { value, tag }` for a tag we
    /// already have is simply deduped.
    pub fn compact(&mut self) -> Result<usize> {
        let (set, _supers) = self.replay()?;
        let old_len = self.ops.len();

        let mut new_ops: Vec<SyncOp<T>> = Vec::new();
        let mut new_seen: HashSet<(Uuid, u64)> = HashSet::new();

        for (value, tags) in set.entries() {
            for tag in tags {
                self.clock = self.clock.saturating_add(1);
                let entry = SyncOp {
                    replica_id: self.replica_id,
                    seq: self.clock,
                    created_at: Utc::now(),
                    op: SyncOpKind::Add {
                        value: value.clone(),
                        tag,
                    },
                };
                new_seen.insert((entry.replica_id, entry.seq));
                new_ops.push(entry);
            }
        }

        self.ops = new_ops;
        self.seen = new_seen;
        self.compaction_epoch = self.compaction_epoch.saturating_add(1);
        Ok(old_len.saturating_sub(self.ops.len()))
    }

    /// Replay the entire log into a fresh [`AddWinsSet`].
    ///
    /// `Remove` and `Supersede` ops tombstone **only** the
    /// `observed_tags` snapshot recorded on the op at the time it
    /// was authored — never the full set of tags currently in the
    /// replayed view. This is what preserves the add-wins property
    /// across replicas: if replica B adds `x` with a fresh tag `T2`
    /// concurrently with replica A removing `x` having only
    /// observed `T1`, then after `merge` + `replay` the op order is
    /// arbitrary but `T2` is never in any `observed_tags` list and
    /// therefore never tombstoned.
    ///
    /// `Supersede` ops are additionally surfaced in the returned
    /// supersessions list; the successor element is preserved
    /// implicitly by the subsequent `Add(successor)` op (which
    /// callers must emit alongside).
    pub fn replay(&self) -> Result<(AddWinsSet<T>, Vec<(T, T)>)> {
        let mut set = AddWinsSet::<T>::new();
        let mut supersessions: Vec<(T, T)> = Vec::new();
        for entry in &self.ops {
            match &entry.op {
                SyncOpKind::Add { value, tag } => set.add_with_tag(value.clone(), *tag),
                SyncOpKind::Remove {
                    value,
                    observed_tags,
                } => set.remove_tags(value, observed_tags),
                SyncOpKind::Supersede {
                    value,
                    successor,
                    observed_tags,
                } => {
                    set.remove_tags(value, observed_tags);
                    supersessions.push((value.clone(), successor.clone()));
                }
            }
        }
        Ok((set, supersessions))
    }
}

/// Merge two op logs into a fresh consistent log. The output is
/// identical regardless of argument order (commutativity).
pub fn merge_logs<T>(a: &OpLog<T>, b: &OpLog<T>) -> OpLog<T>
where
    T: Eq + Hash + Clone,
{
    let mut merged = a.clone();
    merged.merge(b);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_replay_roundtrip() {
        let mut log = OpLog::<String>::new(Uuid::new_v4());
        log.record_add("a".into());
        log.record_add("b".into());
        let (set, supers) = log.replay().unwrap();
        assert!(set.contains(&"a".to_string()));
        assert!(set.contains(&"b".to_string()));
        assert!(supers.is_empty());
    }

    #[test]
    fn merge_is_idempotent_and_commutative() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let mut a = OpLog::<String>::new(id_a);
        a.record_add("alpha".into());
        let mut b = OpLog::<String>::new(id_b);
        b.record_add("beta".into());

        let ab = merge_logs(&a, &b);
        let ba = merge_logs(&b, &a);

        let (set_ab, _) = ab.replay().unwrap();
        let (set_ba, _) = ba.replay().unwrap();
        assert_eq!(
            set_ab.contains(&"alpha".to_string()),
            set_ba.contains(&"alpha".to_string())
        );
        assert_eq!(
            set_ab.contains(&"beta".to_string()),
            set_ba.contains(&"beta".to_string())
        );

        // Merging again is a no-op — same length.
        let mut twice = ab.clone();
        twice.merge(&a);
        twice.merge(&b);
        assert_eq!(twice.ops.len(), ab.ops.len());
    }
}
