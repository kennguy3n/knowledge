//! Working memory — a bounded, TTL-evicting context window.
//!
//! Per `PHASES.md` Phase 1: "Working memory — current context window
//! management with TTL eviction".
//!
//! The working memory is intentionally an in-RAM, single-process data
//! structure. Synthesis pipelines push entries as they extract context
//! relevant to the current turn; surfaces query [`Self::get_context`]
//! to obtain the still-fresh entries in insertion order.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use evidence_store::ScopeId;

/// One entry in the working-memory window.
///
/// Working memory is intentionally process-local; entries are not
/// serialised. Persistence belongs in the evidence / memory plane.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkingMemoryEntry {
    /// Unique id (UUID v4).
    pub id: Uuid,
    /// Free-form content (typically a brief observation summary).
    pub content: String,
    /// Scope this entry belongs to.
    pub scope_id: ScopeId,
    /// Wall-clock insertion time.
    pub inserted_at: DateTime<Utc>,
    /// TTL — entries older than `inserted_at + ttl` are evicted on
    /// the next sweep.
    pub ttl: Duration,
    /// Optional relevance score (`0.0 ..= 1.0`) used to break ties
    /// when the bounded capacity is reached. Entries with the lowest
    /// score are evicted first when over capacity.
    pub relevance_score: f64,
}

impl WorkingMemoryEntry {
    /// Construct a fresh entry.
    pub fn new(
        scope_id: ScopeId,
        content: impl Into<String>,
        ttl: Duration,
        relevance_score: f64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            scope_id,
            content: content.into(),
            inserted_at: Utc::now(),
            ttl,
            relevance_score,
        }
    }

    /// True iff the entry is still alive at `now`.
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        now < self.inserted_at + self.ttl
    }
}

/// A bounded, TTL-evicting context window.
///
/// Capacity is a hard cap: when [`Self::push`] is called and the
/// window is full, expired entries are evicted first; if the window
/// is still full after that, the entry with the lowest relevance
/// score (then oldest) is dropped.
#[derive(Debug, Clone)]
pub struct WorkingMemory {
    entries: Vec<WorkingMemoryEntry>,
    max_entries: usize,
    default_ttl: Duration,
}

impl WorkingMemory {
    /// Construct a fresh working memory with the given capacity and
    /// default TTL.
    pub fn new(max_entries: usize, default_ttl: Duration) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries: max_entries.max(1),
            default_ttl,
        }
    }

    /// The default TTL applied to entries pushed via
    /// [`Self::push_with_default_ttl`].
    pub fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    /// The hard capacity cap.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Push a fully-formed entry, evicting expired entries first and
    /// then the lowest-relevance / oldest entry if still over cap.
    pub fn push(&mut self, entry: WorkingMemoryEntry) {
        let now = Utc::now();
        self.evict_expired_at(now);
        while self.entries.len() >= self.max_entries {
            self.evict_one();
        }
        self.entries.push(entry);
    }

    /// Convenience: build a [`WorkingMemoryEntry`] with the default
    /// TTL and the supplied relevance score, then push it.
    pub fn push_with_default_ttl(
        &mut self,
        scope_id: ScopeId,
        content: impl Into<String>,
        relevance_score: f64,
    ) -> Uuid {
        let entry = WorkingMemoryEntry::new(scope_id, content, self.default_ttl, relevance_score);
        let id = entry.id;
        self.push(entry);
        id
    }

    /// Return references to all live (non-expired) entries in
    /// insertion order.
    pub fn get_context(&self) -> Vec<&WorkingMemoryEntry> {
        let now = Utc::now();
        self.entries.iter().filter(|e| e.is_live(now)).collect()
    }

    /// Drop every expired entry from the window.
    pub fn evict_expired(&mut self) -> usize {
        let now = Utc::now();
        self.evict_expired_at(now)
    }

    /// Return the total number of entries (including expired ones
    /// that have not yet been swept).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True iff the window has no entries (live or expired).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn evict_expired_at(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.is_live(now));
        before - self.entries.len()
    }

    fn evict_one(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        // Evict the entry with the lowest relevance score; tie-break
        // by oldest insertion time.
        let mut idx = 0;
        for i in 1..self.entries.len() {
            let cur = &self.entries[idx];
            let cand = &self.entries[i];
            if cand.relevance_score < cur.relevance_score
                || (cand.relevance_score == cur.relevance_score
                    && cand.inserted_at < cur.inserted_at)
            {
                idx = i;
            }
        }
        self.entries.remove(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_in_insertion_order() {
        let mut wm = WorkingMemory::new(8, Duration::seconds(60));
        let scope = ScopeId::new_v4();
        wm.push_with_default_ttl(scope, "first", 0.5);
        wm.push_with_default_ttl(scope, "second", 0.5);
        let ctx = wm.get_context();
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].content, "first");
        assert_eq!(ctx[1].content, "second");
    }

    #[test]
    fn capacity_cap_is_enforced_via_relevance_eviction() {
        let mut wm = WorkingMemory::new(3, Duration::seconds(60));
        let scope = ScopeId::new_v4();
        wm.push_with_default_ttl(scope, "low", 0.1);
        wm.push_with_default_ttl(scope, "mid", 0.5);
        wm.push_with_default_ttl(scope, "high", 0.9);
        wm.push_with_default_ttl(scope, "extra", 0.7);
        let ctx = wm.get_context();
        // The "low" relevance entry was evicted to make room for "extra".
        let contents: Vec<_> = ctx.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(contents, vec!["mid", "high", "extra"]);
    }
}
