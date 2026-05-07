//! The seven memory states from `ARCHITECTURE.md` §7 / `PROPOSAL.md`
//! §4.1.

use serde::{Deserialize, Serialize};

/// The seven states a memory object can occupy as it moves through
/// the decay state machine.
///
/// Per `ARCHITECTURE.md` §7 / `PROPOSAL.md` §4.1:
///
/// ```text
/// [*] -> Candidate
/// Candidate    -> Reinforced  (retrieval / corroboration)
/// Candidate    -> Archived    (low retention score)
/// Reinforced   -> Consolidated (cross-source corroboration)
/// Consolidated -> Canonical    (human / policy approval)
/// Canonical    -> Superseded   (newer canonical claim)
/// Canonical    -> Deleted      (explicit forget / key destruction)
/// Superseded   -> Archived     (TTL elapsed)
/// Archived     -> Deleted      (scope key destroyed)
/// ```
///
/// Every other transition is rejected by [`MemoryStateMachine`] —
/// see [`crate::transitions::MemoryStateMachine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemoryState {
    /// Initial state on creation. Most observations land here and
    /// most never leave (they decay to Archived).
    Candidate,
    /// Retrieval or corroboration has bumped the retention score
    /// above the candidate threshold.
    Reinforced,
    /// At least one independent source has corroborated the same
    /// observation.
    Consolidated,
    /// A human or a policy has explicitly approved the object as
    /// canonical knowledge.
    Canonical,
    /// A newer canonical claim has superseded this one. The object
    /// is preserved (with a `superseded_by` pointer) for audit and
    /// contradiction tracking.
    Superseded,
    /// The retention score / TTL has expired. The object is no
    /// longer reachable from retrieval but the row still exists.
    Archived,
    /// The scope DEK has been destroyed; the object is unrecoverable
    /// (`PROPOSAL.md` §4.4 — cryptographic forgetting).
    Deleted,
}

impl MemoryState {
    /// Stable string tag used for serialisation / debugging.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Reinforced => "reinforced",
            Self::Consolidated => "consolidated",
            Self::Canonical => "canonical",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
        }
    }

    /// Inverse of [`Self::as_str`]. See also the [`std::str::FromStr`]
    /// impl, which exposes the same logic via the standard trait.
    pub fn parse_tag(s: &str) -> Option<Self> {
        match s {
            "candidate" => Some(Self::Candidate),
            "reinforced" => Some(Self::Reinforced),
            "consolidated" => Some(Self::Consolidated),
            "canonical" => Some(Self::Canonical),
            "superseded" => Some(Self::Superseded),
            "archived" => Some(Self::Archived),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

impl std::str::FromStr for MemoryState {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_tag(s).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_string_tag() {
        for state in [
            MemoryState::Candidate,
            MemoryState::Reinforced,
            MemoryState::Consolidated,
            MemoryState::Canonical,
            MemoryState::Superseded,
            MemoryState::Archived,
            MemoryState::Deleted,
        ] {
            assert_eq!(MemoryState::parse_tag(state.as_str()), Some(state));
        }
    }
}
