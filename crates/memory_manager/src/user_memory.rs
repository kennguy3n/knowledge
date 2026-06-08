//! User Memory Object — CRUD over a user's personal memory scope.
//!
//! Provides user memory object CRUD (read / pin / unpin / forget)
//! over the FFI surface.
//!
//! This module owns the in-process Rust API; the FFI bindings (UniFFI
//! / JNI / N-API) wrap it without re-implementing the lifecycle
//! rules. The CRUD operations interact with the decay state machine
//! (`docs/technical/architecture.md` §7) so that pinning a `Candidate` promotes it
//! to `Reinforced` and forgetting a `Canonical` deletes it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::decay::{decay_sweep, DecaySweepReport};
use crate::error::{MemoryError, Result};
use crate::object::MemoryObject;
use crate::retention::compute_retention_score;
use crate::state::MemoryState;
use crate::transitions::MemoryStateMachine;

/// Filter for [`UserMemoryObject::list`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFilter {
    /// Restrict to objects in any of these states. Empty = match any.
    pub states: Vec<MemoryState>,
    /// Restrict to objects in this scope.
    pub scope_id: Option<ScopeId>,
    /// Restrict to objects whose `metadata.observation_type` field
    /// equals this string. Useful in combination with the
    /// observation engine.
    pub observation_type: Option<String>,
}

impl MemoryFilter {
    /// Build a filter that matches any object.
    pub fn any() -> Self {
        Self::default()
    }

    /// Restrict to objects in `state`.
    pub fn with_state(mut self, state: MemoryState) -> Self {
        self.states.push(state);
        self
    }

    /// Restrict to objects in `scope`.
    pub fn with_scope(mut self, scope: ScopeId) -> Self {
        self.scope_id = Some(scope);
        self
    }

    /// Restrict to objects with `observation_type`.
    pub fn with_observation_type(mut self, t: impl Into<String>) -> Self {
        self.observation_type = Some(t.into());
        self
    }

    fn matches(&self, obj: &MemoryObject) -> bool {
        if !self.states.is_empty() && !self.states.contains(&obj.state) {
            return false;
        }
        if let Some(scope) = self.scope_id {
            if obj.scope_id != scope {
                return false;
            }
        }
        if let Some(t) = &self.observation_type {
            let actual = obj
                .metadata
                .get("observation_type")
                .and_then(serde_json::Value::as_str);
            if actual != Some(t.as_str()) {
                return false;
            }
        }
        true
    }
}

/// In-process User Memory Object.
///
/// This is a vec-backed CRUD layer; persistence to the encrypted
/// evidence store is the caller's responsibility (the memory plane
/// is currently in-memory; persistence + sync are not yet wired).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMemoryObject {
    /// Identifier of the owning user.
    pub user_id: Uuid,
    /// Default personal scope for newly added observations.
    pub scope_id: ScopeId,
    /// Owned memory objects.
    pub objects: Vec<MemoryObject>,
}

impl UserMemoryObject {
    /// Build a fresh user memory object.
    pub fn new(user_id: Uuid, scope_id: ScopeId) -> Self {
        Self {
            user_id,
            scope_id,
            objects: Vec::new(),
        }
    }

    /// Look up an object by id.
    pub fn read(&self, id: &Uuid) -> Option<&MemoryObject> {
        self.objects.iter().find(|o| o.id == *id)
    }

    /// Add an object directly (used by tests / lower layers).
    pub fn insert(&mut self, obj: MemoryObject) {
        self.objects.push(obj);
    }

    /// Pin an object — increments the pin counter, refreshes the
    /// retention score, and (if the object is currently a candidate)
    /// promotes it to [`MemoryState::Reinforced`].
    pub fn pin(&mut self, id: &Uuid) -> Result<()> {
        let now = Utc::now();
        let obj = self.find_mut(id)?;
        obj.pin_count = obj.pin_count.saturating_add(1);
        obj.last_accessed_at = now;
        if obj.state == MemoryState::Candidate {
            MemoryStateMachine::new().reinforce(obj)?;
        }
        obj.retention_score = compute_retention_score(obj, now).total;
        Ok(())
    }

    /// Decrement the pin counter.
    pub fn unpin(&mut self, id: &Uuid) -> Result<()> {
        let now = Utc::now();
        let obj = self.find_mut(id)?;
        obj.pin_count = obj.pin_count.saturating_sub(1);
        obj.last_accessed_at = now;
        obj.retention_score = compute_retention_score(obj, now).total;
        Ok(())
    }

    /// Explicit forget. Per `docs/technical/architecture.md` §7:
    ///
    /// * `Canonical -> Deleted` is the documented "explicit forget"
    ///   transition.
    /// * For any other state, this method drops the object directly
    ///   so the caller does not have to walk the state machine
    ///   manually (the row is no longer reachable; cryptographic
    ///   forgetting via DEK destruction handles the on-disk side).
    pub fn forget(&mut self, id: &Uuid) -> Result<()> {
        let pos = self
            .objects
            .iter()
            .position(|o| o.id == *id)
            .ok_or(MemoryError::NotFound(*id))?;
        if self.objects[pos].state == MemoryState::Canonical {
            MemoryStateMachine::new().delete_canonical(&mut self.objects[pos])?;
        } else {
            self.objects.remove(pos);
        }
        Ok(())
    }

    /// List references to all objects matching `filter`.
    pub fn list(&self, filter: &MemoryFilter) -> Vec<&MemoryObject> {
        self.objects.iter().filter(|o| filter.matches(o)).collect()
    }

    /// Add a brand-new candidate observation. Returns the new id.
    pub fn add_observation(
        &mut self,
        observation_type: impl Into<String>,
        content: impl Into<String>,
        sensitivity_class: crate::object::SensitivityClass,
    ) -> Uuid {
        let mut obj = MemoryObject::new_candidate(self.scope_id, sensitivity_class);
        obj.metadata = serde_json::json!({
            "observation_type": observation_type.into(),
            "content": content.into(),
        });
        let id = obj.id;
        self.objects.push(obj);
        id
    }

    /// Run a decay sweep over the owned objects.
    pub fn decay_sweep(&mut self, now: DateTime<Utc>) -> DecaySweepReport {
        decay_sweep(&mut self.objects, now)
    }

    fn find_mut(&mut self, id: &Uuid) -> Result<&mut MemoryObject> {
        self.objects
            .iter_mut()
            .find(|o| o.id == *id)
            .ok_or(MemoryError::NotFound(*id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::SensitivityClass;

    fn umo() -> UserMemoryObject {
        UserMemoryObject::new(Uuid::new_v4(), ScopeId::new_v4())
    }

    #[test]
    fn add_and_read_observation() {
        let mut u = umo();
        let id = u.add_observation("task", "ship the launch", SensitivityClass::Important);
        let obj = u.read(&id).unwrap();
        assert_eq!(obj.state, MemoryState::Candidate);
        assert_eq!(obj.sensitivity_class, SensitivityClass::Important);
    }

    #[test]
    fn pin_promotes_candidate_and_unpin_decrements() {
        let mut u = umo();
        let id = u.add_observation("fact", "owner: Sara", SensitivityClass::Useful);
        u.pin(&id).unwrap();
        let obj = u.read(&id).unwrap();
        assert_eq!(obj.state, MemoryState::Reinforced);
        assert_eq!(obj.pin_count, 1);
        u.unpin(&id).unwrap();
        let obj = u.read(&id).unwrap();
        assert_eq!(obj.pin_count, 0);
    }

    #[test]
    fn forget_canonical_marks_deleted_others_drop() {
        let mut u = umo();
        let id_canon = u.add_observation("decision", "ratified", SensitivityClass::Critical);
        // Drive the state machine up to Canonical.
        let sm = MemoryStateMachine::new();
        sm.reinforce(u.find_mut(&id_canon).unwrap()).unwrap();
        sm.consolidate(u.find_mut(&id_canon).unwrap()).unwrap();
        sm.canonicalize(u.find_mut(&id_canon).unwrap()).unwrap();
        u.forget(&id_canon).unwrap();
        assert_eq!(u.read(&id_canon).unwrap().state, MemoryState::Deleted);

        let id_cand = u.add_observation("task", "todo", SensitivityClass::Useful);
        u.forget(&id_cand).unwrap();
        assert!(u.read(&id_cand).is_none());
    }

    #[test]
    fn list_filters_by_state_and_type() {
        let mut u = umo();
        let _ = u.add_observation("task", "a", SensitivityClass::Useful);
        let _ = u.add_observation("fact", "b", SensitivityClass::Useful);
        let tasks = u.list(&MemoryFilter::any().with_observation_type("task"));
        assert_eq!(tasks.len(), 1);
        let candidates = u.list(&MemoryFilter::any().with_state(MemoryState::Candidate));
        assert_eq!(candidates.len(), 2);
    }
}
