//! Append-only audit log + query API.

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
    next_sequence: u64,
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
        self.entries.push(entry);
        Ok(())
    }

    /// Look up an entry by id. Returns `None` if absent.
    pub fn get(&self, id: AuditEntryId) -> Option<&AuditEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// All entries in chronological (insertion / sequence) order.
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// Run a [`AuditQuery`] against the log. Result is in
    /// chronological order.
    pub fn query<'a>(&'a self, q: &'a AuditQuery) -> impl Iterator<Item = &'a AuditEntry> + 'a {
        self.entries.iter().filter(move |e| q.matches(e))
    }
}
