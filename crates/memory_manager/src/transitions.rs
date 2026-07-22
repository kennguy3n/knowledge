//! The decay state machine from `docs/technical/architecture.md` §7.
//!
//! Every transition is an explicit method on [`MemoryStateMachine`].
//! Each method validates the current state of the supplied
//! [`MemoryObject`] and returns the new [`MemoryState`] on success or
//! [`MemoryError::InvalidTransition`] on rejection.

use chrono::{DateTime, Utc};

use crate::error::{MemoryError, Result};
use crate::object::MemoryObject;
use crate::state::MemoryState;

/// Pure (no internal state) state-machine driver.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryStateMachine;

impl MemoryStateMachine {
    /// Construct a fresh state-machine driver.
    pub fn new() -> Self {
        Self
    }

    /// `Candidate -> Reinforced` (retrieval / corroboration).
    pub fn reinforce(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(obj, &[MemoryState::Candidate], MemoryState::Reinforced)
    }

    /// `Candidate -> Archived` (low retention score).
    pub fn archive_candidate(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(obj, &[MemoryState::Candidate], MemoryState::Archived)
    }

    /// `Reinforced -> Consolidated` (cross-source corroboration).
    ///
    /// `now` is the wall-clock reference used to stamp
    /// [`MemoryObject::consolidated_at`], which drives the later
    /// promotion to Canonical.
    pub fn consolidate(
        &self,
        obj: &mut MemoryObject,
        now: DateTime<Utc>,
    ) -> Result<MemoryState> {
        Self::expect(obj, &[MemoryState::Reinforced], MemoryState::Consolidated)
            .map(|state| {
                obj.consolidated_at = Some(now);
                state
            })
    }

    /// `Consolidated -> Canonical` (human / policy approval).
    pub fn canonicalize(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(obj, &[MemoryState::Consolidated], MemoryState::Canonical)
    }

    /// `Canonical -> Superseded` (newer canonical claim).
    ///
    /// `now` is the wall-clock reference used to stamp
    /// [`MemoryObject::last_accessed_at`], which the downstream
    /// decay sweep uses as the "supersession time" reference point —
    /// the Superseded TTL counts forward from supersession, not from
    /// the row's last read.
    pub fn supersede(
        &self,
        obj: &mut MemoryObject,
        superseded_by: uuid::Uuid,
        now: DateTime<Utc>,
    ) -> Result<MemoryState> {
        let new_state = Self::expect(obj, &[MemoryState::Canonical], MemoryState::Superseded)?;
        obj.superseded_by = Some(superseded_by);
        obj.last_accessed_at = now;
        Ok(new_state)
    }

    /// `Canonical -> Deleted` (explicit forget / key destruction).
    ///
    /// This is the "explicit forget" transition. The complementary
    /// `Archived -> Deleted` is [`Self::delete_archived`].
    pub fn delete_canonical(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(obj, &[MemoryState::Canonical], MemoryState::Deleted)
    }

    /// `Superseded -> Archived` (TTL elapsed).
    pub fn archive_superseded(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(obj, &[MemoryState::Superseded], MemoryState::Archived)
    }

    /// `Archived -> Deleted` (scope key destroyed).
    pub fn delete_archived(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(obj, &[MemoryState::Archived], MemoryState::Deleted)
    }

    /// Policy-driven archive from any active state.
    ///
    /// Supports event-driven archival (e.g. project/account closure)
    /// which may target Reinforced, Consolidated, or Canonical rows
    /// in addition to Candidate and Superseded.
    pub fn archive(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(
            obj,
            &[
                MemoryState::Candidate,
                MemoryState::Reinforced,
                MemoryState::Consolidated,
                MemoryState::Canonical,
                MemoryState::Superseded,
            ],
            MemoryState::Archived,
        )
    }

    /// Policy-driven delete from any state.
    pub fn delete(&self, obj: &mut MemoryObject) -> Result<MemoryState> {
        Self::expect(
            obj,
            &[
                MemoryState::Candidate,
                MemoryState::Reinforced,
                MemoryState::Consolidated,
                MemoryState::Canonical,
                MemoryState::Superseded,
                MemoryState::Archived,
            ],
            MemoryState::Deleted,
        )
    }

    /// Resurrect an Archived object back to Reinforced.
    /// Clears any supersession pointer so the object can be re-evaluated.
    /// `now` is the wall-clock reference used to refresh
    /// [`MemoryObject::last_accessed_at`].
    pub fn resurrect(
        &self,
        obj: &mut MemoryObject,
        now: DateTime<Utc>,
    ) -> Result<MemoryState> {
        let state = Self::expect(obj, &[MemoryState::Archived], MemoryState::Reinforced)?;
        obj.superseded_by = None;
        obj.last_accessed_at = now;
        Ok(state)
    }

    fn expect(
        obj: &mut MemoryObject,
        allowed_from: &[MemoryState],
        to: MemoryState,
    ) -> Result<MemoryState> {
        if !allowed_from.contains(&obj.state) {
            return Err(MemoryError::InvalidTransition {
                from: obj.state,
                to,
            });
        }
        obj.state = to;
        Ok(to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::SensitivityClass;
    use evidence_store::ScopeId;
    use uuid::Uuid;

    fn fresh() -> MemoryObject {
        MemoryObject::new_candidate(ScopeId::new_v4(), SensitivityClass::Useful)
    }

    #[test]
    fn full_happy_path_walks_state_machine() {
        let sm = MemoryStateMachine::new();
        let mut obj = fresh();
        sm.reinforce(&mut obj).unwrap();
        assert_eq!(obj.state, MemoryState::Reinforced);
        sm.consolidate(&mut obj, chrono::Utc::now()).unwrap();
        assert_eq!(obj.state, MemoryState::Consolidated);
        sm.canonicalize(&mut obj).unwrap();
        assert_eq!(obj.state, MemoryState::Canonical);
        sm.supersede(&mut obj, Uuid::new_v4(), chrono::Utc::now()).unwrap();
        assert_eq!(obj.state, MemoryState::Superseded);
        assert!(obj.superseded_by.is_some());
        sm.archive_superseded(&mut obj).unwrap();
        assert_eq!(obj.state, MemoryState::Archived);
        sm.delete_archived(&mut obj).unwrap();
        assert_eq!(obj.state, MemoryState::Deleted);
    }

    #[test]
    fn rejects_skipping_states() {
        let sm = MemoryStateMachine::new();
        let mut obj = fresh();
        // Candidate -> Consolidated is invalid (must go through
        // Reinforced).
        let err = sm.consolidate(&mut obj, chrono::Utc::now()).unwrap_err();
        assert_eq!(
            err,
            MemoryError::InvalidTransition {
                from: MemoryState::Candidate,
                to: MemoryState::Consolidated
            }
        );
        // Candidate -> Canonical is invalid.
        let err = sm.canonicalize(&mut obj).unwrap_err();
        assert!(matches!(err, MemoryError::InvalidTransition { .. }));
    }
}
