//! [`SynthesisWindow`] — per-scope synthesis window abstraction.
//!
//! Per `ARCHITECTURE.md` §2.1 and `docs/DESIGN.md` §6.4, "heavy
//! synthesis runs once per scope window, not once per device". The
//! window manager tracks open / in-progress / complete / failed
//! windows for each scope so the elected synthesizer (or the
//! managed AI endpoint, or the TEE worker) can pick the next window
//! to run.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::error::{PipelineError, Result};

/// Identifier for a [`SynthesisWindow`] (UUID v4 newtype).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub Uuid);

impl WindowId {
    /// Generate a fresh random window id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a raw [`Uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Lifecycle of a synthesis window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowStatus {
    /// Window is open and waiting for a synthesizer to claim it.
    Pending,
    /// A synthesizer has claimed the window and is processing it.
    InProgress,
    /// The synthesizer published a synthesis object back into the
    /// scope; the window is closed.
    Complete,
    /// The synthesizer failed to produce an object; another
    /// synthesizer (or a retry) needs to reclaim the window.
    Failed,
}

impl WindowStatus {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    /// Whether this status is terminal (no further transitions).
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// One synthesis window — a half-open `[window_start, window_end)`
/// interval in a specific scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynthesisWindow {
    /// Unique id (UUID v4).
    pub id: WindowId,
    /// Scope this window belongs to.
    pub scope_id: ScopeId,
    /// Window start (inclusive).
    pub window_start: DateTime<Utc>,
    /// Window end (exclusive).
    pub window_end: DateTime<Utc>,
    /// Lifecycle state.
    pub status: WindowStatus,
}

impl SynthesisWindow {
    /// Construct a fresh `Pending` window for `scope_id`.
    ///
    /// # Errors
    ///
    /// [`PipelineError::InvalidWindow`] if `window_end` is not strictly
    /// after `window_start`.
    pub fn new(
        scope_id: ScopeId,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<Self> {
        if window_end <= window_start {
            return Err(PipelineError::InvalidWindow);
        }
        Ok(Self {
            id: WindowId::new_v4(),
            scope_id,
            window_start,
            window_end,
            status: WindowStatus::Pending,
        })
    }

    /// Convenience: build a window covering the most recent `duration`
    /// up to `now`.
    pub fn rolling(scope_id: ScopeId, now: DateTime<Utc>, duration: Duration) -> Result<Self> {
        let start = now - duration;
        Self::new(scope_id, start, now)
    }

    /// Duration of the window.
    pub fn duration(&self) -> Duration {
        self.window_end - self.window_start
    }
}

/// Tracks per-scope synthesis windows and their lifecycle.
#[derive(Debug, Default, Clone)]
pub struct SynthesisWindowManager {
    windows: HashMap<WindowId, SynthesisWindow>,
    by_scope: HashMap<ScopeId, Vec<WindowId>>,
}

impl SynthesisWindowManager {
    /// Construct a fresh empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked windows (any status).
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// True iff no windows are tracked.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Open a fresh `Pending` window in `scope_id`.
    pub fn open_window(
        &mut self,
        scope_id: ScopeId,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<WindowId> {
        let window = SynthesisWindow::new(scope_id, window_start, window_end)?;
        let id = window.id;
        self.windows.insert(id, window);
        self.by_scope.entry(scope_id).or_default().push(id);
        Ok(id)
    }

    /// Look up a window by id.
    pub fn get(&self, id: WindowId) -> Option<&SynthesisWindow> {
        self.windows.get(&id)
    }

    /// All windows for `scope_id`, in insertion order.
    pub fn windows_for(&self, scope_id: ScopeId) -> Vec<&SynthesisWindow> {
        let Some(ids) = self.by_scope.get(&scope_id) else {
            return Vec::new();
        };
        ids.iter().filter_map(|id| self.windows.get(id)).collect()
    }

    /// Mark a `Pending` window as `InProgress`.
    ///
    /// # Errors
    ///
    /// * [`PipelineError::WindowNotFound`] if no such window.
    /// * [`PipelineError::InvalidWindowTransition`] if the window is
    ///   not currently `Pending` (or `Failed`, which can be retried).
    pub fn mark_in_progress(&mut self, id: WindowId) -> Result<()> {
        let w = self.find_mut(id)?;
        match w.status {
            WindowStatus::Pending | WindowStatus::Failed => {
                w.status = WindowStatus::InProgress;
                Ok(())
            }
            _ => Err(PipelineError::InvalidWindowTransition),
        }
    }

    /// Mark an `InProgress` window as `Complete`.
    pub fn mark_complete(&mut self, id: WindowId) -> Result<()> {
        let w = self.find_mut(id)?;
        match w.status {
            WindowStatus::InProgress => {
                w.status = WindowStatus::Complete;
                Ok(())
            }
            _ => Err(PipelineError::InvalidWindowTransition),
        }
    }

    /// Mark an `InProgress` window as `Failed` so it can be retried.
    pub fn mark_failed(&mut self, id: WindowId) -> Result<()> {
        let w = self.find_mut(id)?;
        match w.status {
            WindowStatus::InProgress => {
                w.status = WindowStatus::Failed;
                Ok(())
            }
            _ => Err(PipelineError::InvalidWindowTransition),
        }
    }

    fn find_mut(&mut self, id: WindowId) -> Result<&mut SynthesisWindow> {
        self.windows
            .get_mut(&id)
            .ok_or(PipelineError::WindowNotFound(id.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_rejects_zero_duration() {
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let err = SynthesisWindow::new(scope, now, now).unwrap_err();
        assert!(matches!(err, PipelineError::InvalidWindow));
    }

    #[test]
    fn manager_tracks_windows_per_scope() {
        let mut mgr = SynthesisWindowManager::new();
        let scope_a = ScopeId::new_v4();
        let scope_b = ScopeId::new_v4();
        let now = Utc::now();
        let _a1 = mgr
            .open_window(scope_a, now - Duration::hours(1), now)
            .unwrap();
        let _a2 = mgr
            .open_window(scope_a, now - Duration::hours(2), now - Duration::hours(1))
            .unwrap();
        let _b1 = mgr
            .open_window(scope_b, now - Duration::hours(1), now)
            .unwrap();
        assert_eq!(mgr.windows_for(scope_a).len(), 2);
        assert_eq!(mgr.windows_for(scope_b).len(), 1);
    }

    #[test]
    fn lifecycle_transitions_are_enforced() {
        let mut mgr = SynthesisWindowManager::new();
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let id = mgr
            .open_window(scope, now - Duration::hours(1), now)
            .unwrap();
        // Cannot mark complete while pending.
        let err = mgr.mark_complete(id).unwrap_err();
        assert!(matches!(err, PipelineError::InvalidWindowTransition));
        mgr.mark_in_progress(id).unwrap();
        mgr.mark_complete(id).unwrap();
        // Cannot transition out of Complete.
        let err = mgr.mark_in_progress(id).unwrap_err();
        assert!(matches!(err, PipelineError::InvalidWindowTransition));
    }

    #[test]
    fn failed_windows_can_be_retried() {
        let mut mgr = SynthesisWindowManager::new();
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let id = mgr
            .open_window(scope, now - Duration::hours(1), now)
            .unwrap();
        mgr.mark_in_progress(id).unwrap();
        mgr.mark_failed(id).unwrap();
        mgr.mark_in_progress(id).unwrap();
        assert_eq!(mgr.get(id).unwrap().status, WindowStatus::InProgress);
    }
}
