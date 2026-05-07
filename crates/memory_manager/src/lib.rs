//! `memory_manager` — decay state machine, retention scoring, working
//! memory, user memory CRUD, and the privacy-strip invariant.
//!
//! This crate implements the `memory_manager` module called out in
//! `ARCHITECTURE.md` §2.1 and the Phase 1 deliverables in
//! `PHASES.md`. It is the on-device authority for the lifecycle of
//! every [`MemoryObject`]: how it transitions through the decay state
//! machine, how its retention score is computed and updated, how it
//! is pinned / unpinned / forgotten, and how it is bounded into a
//! [`WorkingMemory`] context window with TTL eviction.
//!
//! The crate is also the home of [`PrivacyStrip`] and
//! [`SynthesisOutput`] — the type-system-enforced invariant from
//! `PROPOSAL.md` §6 / Phase 1 that "every synthesis output carries a
//! privacy strip describing its compute location, model, and egress".
//! It is impossible to construct a [`SynthesisOutput`] without first
//! constructing a [`PrivacyStrip`].
//!
//! Cross-references:
//!
//! * Memory model & decay: `PROPOSAL.md` §4.
//! * Decay state machine: `ARCHITECTURE.md` §7.
//! * Phase 1 deliverables: `PHASES.md` Phase 1.

#![deny(missing_docs)]

pub mod decay;
pub mod error;
pub mod object;
pub mod privacy_strip;
pub mod retention;
pub mod state;
pub mod transitions;
pub mod user_memory;
pub mod working_memory;

pub use decay::{decay_sweep, DecaySweepReport};
pub use error::{MemoryError, Result};
pub use object::{MemoryObject, SensitivityClass};
pub use privacy_strip::{ComputeLocation, PrivacyStrip, PrivacyStripBuilder, SynthesisOutput};
pub use retention::{compute_retention_score, RetentionScore, RetentionWeights};
pub use state::MemoryState;
pub use transitions::MemoryStateMachine;
pub use user_memory::{MemoryFilter, UserMemoryObject};
pub use working_memory::{WorkingMemory, WorkingMemoryEntry};
