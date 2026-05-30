//! Append-only audit log + query API.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::entry::{Actor, AuditActionType, AuditEntry, AuditEntryId};
use crate::error::{AuditError, Result};

/// Filter specification for [`AuditLog::query`].
///
/// All fields are `AND`-ed; leaving a field unset means "no filter on
/// this dimension".
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// Restrict to entries with this scope id.
    pub scope_id: Option<ScopeId>,
    /// Restrict to entries with one of the listed action types. An
    /// empty vector means "no filter".
    pub action_types: Vec<AuditActionType>,
    /// Restrict to entries with timestamp `>= since`.
    pub since: Option<DateTime<Utc>>,
    /// Restrict to entries with timestamp `<= until`.
    pub until: Option<DateTime<Utc>>,
    /// Restrict to entries by this actor.
    pub actor_id: Option<Uuid>,
}

impl AuditQuery {
    /// Construct an unconstrained query.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict to entries with `scope`.
    pub fn with_scope(mut self, scope: ScopeId) -> Self {
        self.scope_id = Some(scope);
        self
    }

    /// Restrict to a single action type.
    pub fn with_action(mut self, action: AuditActionType) -> Self {
        self.action_types.push(action);
        self
    }

    /// Restrict to entries with `since <= ts`.
    pub fn since(mut self, since: DateTime<Utc>) -> Self {
        self.since = Some(since);
        self
    }

    /// Restrict to entries with `ts <= until`.
    pub fn until(mut self, until: DateTime<Utc>) -> Self {
        self.until = Some(until);
        self
    }

    /// Restrict to entries by `actor_id` (matches both
    /// [`Actor::User`] and [`Actor::Agent`]).
    pub fn with_actor(mut self, actor_id: Uuid) -> Self {
        self.actor_id = Some(actor_id);
        self
    }

    fn matches(&self, entry: &AuditEntry) -> bool {
        if let Some(scope) = self.scope_id {
            if entry.scope_id != Some(scope) {
                return false;
            }
        }
        if !self.action_types.is_empty() && !self.action_types.contains(&entry.action_type) {
            return false;
        }
        if let Some(since) = self.since {
            if entry.timestamp < since {
                return false;
            }
        }
        if let Some(until) = self.until {
            if entry.timestamp > until {
                return false;
            }
        }
        if let Some(actor_id) = self.actor_id {
            match entry.actor {
                Actor::User(id) | Actor::Agent(id) => {
                    if id != actor_id {
                        return false;
                    }
                }
                Actor::System => return false,
            }
        }
        true
    }
}

/// Append-only audit log. Entries are stored by value in insertion
/// order; the log assigns a strictly monotonic sequence number on
/// each [`Self::append`].
///
/// The log intentionally exposes no public API for mutating or
/// removing entries — the type system makes the append-only invariant
/// unforgeable.
///
/// To keep [`Self::query`] and [`Self::get`] fast as the log grows
/// (production deployments can hold tens of thousands of entries per
/// scope), the log maintains secondary indexes that map each filter
/// dimension to the positions of matching entries in the `entries`
/// `Vec`. These indexes are derived state — they are rebuilt from
/// `entries` on deserialise and are `#[serde(skip)]` on the wire.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
    next_sequence: u64,
    /// `scope_id -> positions in entries` (entries with no scope
    /// are not indexed here).
    #[serde(skip)]
    scope_index: HashMap<ScopeId, Vec<usize>>,
    /// `action_type -> positions in entries`.
    #[serde(skip)]
    action_index: HashMap<AuditActionType, Vec<usize>>,
    /// `actor uuid -> positions in entries` (entries actored by
    /// [`Actor::System`] are not indexed here, matching the query
    /// semantics in [`AuditQuery::matches`]).
    #[serde(skip)]
    actor_index: HashMap<Uuid, Vec<usize>>,
    /// `entry id -> position` for `O(1)` [`Self::get`].
    #[serde(skip)]
    id_index: HashMap<AuditEntryId, usize>,
}

impl AuditLog {
    /// Construct an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries in the log.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True iff the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append `entry` to the log. The log assigns the entry's
    /// `sequence` field.
    pub fn append(&mut self, mut entry: AuditEntry) -> AuditEntryId {
        entry.sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let id = entry.id;
        let position = self.entries.len();
        self.index_entry(&entry, position);
        self.entries.push(entry);
        id
    }

    /// Replay a previously-appended entry, preserving its existing
    /// `sequence` field. Used by [`crate::PersistentAuditLog`] when
    /// rehydrating the log from disk so the in-memory sequence
    /// numbers match the on-disk ones.
    ///
    /// The entry must arrive in `sequence` order — strictly
    /// increasing and not gapped with respect to the current
    /// `next_sequence`. A replay out of order is an integrity
    /// violation (the row order on disk does not match the
    /// claimed sequence numbers) and is rejected with
    /// [`AuditError::Persistence`]. The log's `next_sequence`
    /// advances to `entry.sequence + 1` so subsequent
    /// [`Self::append`] calls keep the monotonic invariant.
    pub fn replay_persisted(&mut self, entry: AuditEntry) -> Result<()> {
        if entry.sequence != self.next_sequence {
            return Err(AuditError::Persistence(
                "replayed audit entry sequence is not contiguous with next_sequence",
            ));
        }
        // `saturating_add` is the same overflow guard
        // `append` uses, so the replay path cannot regress the
        // counter past `u64::MAX`.
        self.next_sequence = self.next_sequence.saturating_add(1);
        let position = self.entries.len();
        self.index_entry(&entry, position);
        self.entries.push(entry);
        Ok(())
    }

    /// The sequence number the next [`Self::append`] /
    /// [`Self::replay_persisted`] call will assign. Used by
    /// [`crate::PersistentAuditLog`] to stamp an entry *before*
    /// writing it to disk, so a persist failure can leave the
    /// in-memory log untouched.
    pub fn peek_next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Look up an entry by id in `O(1)`. Returns `None` if absent.
    pub fn get(&self, id: AuditEntryId) -> Option<&AuditEntry> {
        self.id_index.get(&id).map(|&pos| &self.entries[pos])
    }

    /// All entries in chronological (insertion / sequence) order.
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// Run a [`AuditQuery`] against the log. Result is in
    /// chronological order.
    ///
    /// When the query constrains scope, actor, or action types, the
    /// log walks the smallest matching index and verifies the
    /// remaining predicates per entry. A query with only time-range
    /// constraints (or no constraints at all) falls back to a linear
    /// scan over `entries`.
    pub fn query<'a>(&'a self, q: &'a AuditQuery) -> Box<dyn Iterator<Item = &'a AuditEntry> + 'a> {
        let candidates = self.candidate_positions(q);
        match candidates {
            Some(positions) => Box::new(positions.into_iter().filter_map(move |pos| {
                let entry = &self.entries[pos];
                if q.matches(entry) {
                    Some(entry)
                } else {
                    None
                }
            })),
            None => Box::new(self.entries.iter().filter(move |e| q.matches(e))),
        }
    }

    /// Compute the smallest set of candidate positions to consider
    /// for `q`. Returns `None` when the query has no indexable
    /// predicates and a full linear scan is the only option.
    ///
    /// When multiple indexable dimensions are set, the candidate
    /// sets are intersected so the per-entry predicate check only
    /// runs on the intersection.
    fn candidate_positions(&self, q: &AuditQuery) -> Option<Vec<usize>> {
        // Gather every available candidate set in `(size, set)`
        // order so we start the intersection from the smallest one.
        let mut sets: Vec<&[usize]> = Vec::new();
        if let Some(scope) = q.scope_id {
            sets.push(self.scope_index.get(&scope).map_or(&[][..], Vec::as_slice));
        }
        if !q.action_types.is_empty() {
            // Gather the union of every requested action's positions
            // (positions are unique per action so the union is a
            // simple concatenation followed by dedupe).
            let mut union: BTreeSet<usize> = BTreeSet::new();
            for action in &q.action_types {
                if let Some(positions) = self.action_index.get(action) {
                    union.extend(positions.iter().copied());
                }
            }
            // Materialise so we own the union for the intersection
            // pass below; this is cheap because the action set is
            // small.
            let owned: Vec<usize> = union.into_iter().collect();
            return Some(self.intersect_with(owned, sets, q.actor_id));
        }
        if let Some(actor_id) = q.actor_id {
            sets.push(
                self.actor_index
                    .get(&actor_id)
                    .map_or(&[][..], Vec::as_slice),
            );
        }
        if sets.is_empty() {
            return None;
        }
        sets.sort_by_key(|s| s.len());
        let mut iter = sets.into_iter();
        let smallest = iter.next()?;
        let mut acc: BTreeSet<usize> = smallest.iter().copied().collect();
        for set in iter {
            let other: BTreeSet<usize> = set.iter().copied().collect();
            acc = acc.intersection(&other).copied().collect();
            if acc.is_empty() {
                break;
            }
        }
        Some(acc.into_iter().collect())
    }

    /// Intersect `seed` (already an ordered set) with `extra` (raw
    /// candidate slices not yet deduped) and optionally with the
    /// actor index for `actor_id`. Returns a sorted vector.
    fn intersect_with(
        &self,
        seed: Vec<usize>,
        extra: Vec<&[usize]>,
        actor_id: Option<Uuid>,
    ) -> Vec<usize> {
        let mut acc: BTreeSet<usize> = seed.into_iter().collect();
        for set in extra {
            let other: BTreeSet<usize> = set.iter().copied().collect();
            acc = acc.intersection(&other).copied().collect();
            if acc.is_empty() {
                return Vec::new();
            }
        }
        if let Some(actor_id) = actor_id {
            if let Some(positions) = self.actor_index.get(&actor_id) {
                let other: BTreeSet<usize> = positions.iter().copied().collect();
                acc = acc.intersection(&other).copied().collect();
            } else {
                return Vec::new();
            }
        }
        acc.into_iter().collect()
    }

    /// Push `entry` at `position` into every relevant index. Caller
    /// must ensure `position == self.entries.len()` (i.e. the
    /// entry has not yet been pushed onto `entries`).
    fn index_entry(&mut self, entry: &AuditEntry, position: usize) {
        if let Some(scope) = entry.scope_id {
            self.scope_index.entry(scope).or_default().push(position);
        }
        self.action_index
            .entry(entry.action_type)
            .or_default()
            .push(position);
        match entry.actor {
            Actor::User(id) | Actor::Agent(id) => {
                self.actor_index.entry(id).or_default().push(position);
            }
            Actor::System => {
                // `System` actors are never matched by `actor_id`
                // queries, so skipping the index keeps it tight.
            }
        }
        self.id_index.insert(entry.id, position);
    }

    /// Rebuild every secondary index from `entries`. Called after
    /// `Deserialize` to repopulate the `#[serde(skip)]` fields.
    pub(crate) fn rebuild_indexes(&mut self) {
        self.scope_index.clear();
        self.action_index.clear();
        self.actor_index.clear();
        self.id_index.clear();
        // Re-borrow the entries through a clone of the indexable
        // metadata to satisfy the borrow checker — we cannot
        // immutably borrow `self.entries` while mutating
        // `self.*_index`.
        let snapshot: Vec<(usize, AuditEntry)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (i, e.clone()))
            .collect();
        for (position, entry) in snapshot {
            self.index_entry(&entry, position);
        }
    }
}

impl<'de> Deserialize<'de> for AuditLog {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            entries: Vec<AuditEntry>,
            next_sequence: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        let mut log = AuditLog {
            entries: wire.entries,
            next_sequence: wire.next_sequence,
            scope_index: HashMap::new(),
            action_index: HashMap::new(),
            actor_index: HashMap::new(),
            id_index: HashMap::new(),
        };
        log.rebuild_indexes();
        Ok(log)
    }
}
