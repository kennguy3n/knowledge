//! [`SynthesisWindow`] — per-scope synthesis window abstraction.
//!
//! Per `docs/technical/architecture.md` §2.1 and `docs/technical/design.md` §6.4, "heavy
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
use crate::hierarchy::WindowScopeTier;

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
    /// Tier the window was opened against, captured at open time so
    /// callers can report the synthesis tier without needing the
    /// `Complete` synthesis object to be present.
    ///
    /// `None` for windows opened via the low-level
    /// [`SynthesisWindowManager::open_window`] path (legacy / channel-
    /// scope use), `Some(tier)` for windows opened via
    /// [`crate::HierarchyEnforcedWindowManager::open_tiered_window`].
    ///
    /// `#[serde(default)]` so blobs persisted before this field was
    /// introduced rehydrate cleanly — old windows surface as
    /// `tier: None` and the FFI layer falls back to inferring the
    /// tier from the associated synthesis object, matching the
    /// pre-existing behaviour.
    #[serde(default)]
    pub tier: Option<WindowScopeTier>,
    /// Wall-clock instant the window was opened.
    ///
    /// Captured by [`SynthesisWindow::new`] at construction time and
    /// used by [`SynthesisWindowManager::sweep_stuck_pending`] (and
    /// the FFI `open_store` recovery sweep that wraps it) to detect
    /// `Pending` windows that have outlived the host's expected
    /// dispatch latency — typically because the host crashed mid-
    /// dispatch between the earlier `flush_synthesis_windows` and
    /// the earlier `apply_dispatch_outcome` commit, leaving the
    /// window stranded in `Pending` on disk with no in-flight
    /// worker. Distinct from [`window_start`] / [`window_end`],
    /// which describe the synthesis *interval* (often backfilled
    /// relative to wall clock).
    ///
    /// `#[serde(default)]` so blobs persisted before this field was
    /// introduced rehydrate cleanly — `None` is interpreted by
    /// `sweep_stuck_pending` as "unknown age, conservatively leave
    /// alone" so legacy windows are never swept on age grounds.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
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
            tier: None,
            created_at: Some(Utc::now()),
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
///
/// `Serialize` / `Deserialize` so the FFI substrate can flush the
/// entire manager into the encrypted evidence store under the
/// `synthesis_windows` memory-blob kind and rehydrate it on
/// `open_store`.
///
/// # Serialization invariant
///
/// `serde_json` requires `HashMap` keys to serialise as strings.
/// Both [`WindowId`] (this crate, see lines above) and
/// [`ScopeId`] (`evidence_store::ids::ScopeId`) are
/// `#[serde(transparent)]` newtypes over [`uuid::Uuid`], so they
/// serialise as the hyphenated-UUID string form that JSON object
/// keys accept. If a future refactor removes the `transparent`
/// annotation from either id type, `serde_json` would fall back to
/// the array-of-pairs encoding which does NOT round-trip through
/// the deserialiser configured here — the substrate would silently
/// fail to rehydrate the manager and every restart would discard
/// the prior window history.
///
/// Callers extending this struct with additional `HashMap`-keyed
/// fields must ensure the new key type either has
/// `#[serde(transparent)]` over a string-serialisable type or
/// provides an explicit `#[serde(with = ...)]` adapter that
/// stringifies the key.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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

    /// Mutable variant of [`Self::get`].
    ///
    /// Intended for tests and recovery paths that need to adjust
    /// fields on a tracked window without going through the
    /// [`Self::mark_in_progress`] / [`Self::mark_failed`] state
    /// machine — e.g. the FFI crate's `open_store` stuck-Pending
    /// recovery test backdates [`SynthesisWindow::created_at`] to
    /// drive [`Self::sweep_stuck_pending`] without a live clock
    /// shift, and the same hook lets crash-recovery code path
    /// reconcile fields (e.g. clear stale `last_synth_at` cursors)
    /// without re-creating the window.
    pub fn get_mut(&mut self, id: WindowId) -> Option<&mut SynthesisWindow> {
        self.windows.get_mut(&id)
    }

    /// All windows for `scope_id`, in insertion order.
    pub fn windows_for(&self, scope_id: ScopeId) -> Vec<&SynthesisWindow> {
        let Some(ids) = self.by_scope.get(&scope_id) else {
            return Vec::new();
        };
        ids.iter().filter_map(|id| self.windows.get(id)).collect()
    }

    /// Every scope id with at least one tracked window. Used by the
    /// FFI substrate during `open_store` rehydration to drop windows
    /// whose scope has been cryptographically forgotten (the
    /// substrate owns the tombstone set, not the pipeline, so the
    /// purge is driven externally).
    ///
    /// Iteration order is unspecified — callers that need a stable
    /// order should sort the result themselves.
    pub fn tracked_scopes(&self) -> Vec<ScopeId> {
        self.by_scope.keys().copied().collect()
    }

    /// Set the persisted [`WindowScopeTier`] tag on an existing
    /// window. Called by
    /// [`crate::HierarchyEnforcedWindowManager::open_tiered_window`]
    /// immediately after [`Self::open_window`] so the freshly opened
    /// window carries the tier in its persisted shape.
    ///
    /// # Errors
    ///
    /// * [`PipelineError::WindowNotFound`] if no window with `id`
    ///   exists. The tiered-open path should never trigger this
    ///   because `open_window` either errors out or inserts the
    ///   window; the explicit error variant is for completeness when
    ///   the helper is invoked manually.
    pub fn set_tier(&mut self, id: WindowId, tier: WindowScopeTier) -> Result<()> {
        let w = self
            .windows
            .get_mut(&id)
            .ok_or(PipelineError::WindowNotFound(id.0))?;
        w.tier = Some(tier);
        Ok(())
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

    /// Re-open a `Complete` window so a `replay_synthesis` call can
    /// walk it back through `Pending → InProgress → Complete` with
    /// a fresh synthesis output, without minting a new window id.
    ///
    /// Only `Complete` is accepted: replaying a `Pending` /
    /// `InProgress` window would race the original dispatch, and
    /// replaying a `Failed` window is already covered by
    /// [`Self::mark_in_progress`] (which accepts `Failed` as a
    /// starting state).
    ///
    /// # Errors
    ///
    /// * [`PipelineError::WindowNotFound`] if no such window.
    /// * [`PipelineError::InvalidWindowTransition`] if the window is
    ///   not currently `Complete`.
    pub fn mark_replay_pending(&mut self, id: WindowId) -> Result<()> {
        let w = self.find_mut(id)?;
        match w.status {
            WindowStatus::Complete => {
                w.status = WindowStatus::Pending;
                Ok(())
            }
            _ => Err(PipelineError::InvalidWindowTransition),
        }
    }

    /// Transition every `Pending` window whose [`SynthesisWindow::created_at`]
    /// is older than `now - threshold` straight to `Failed`, bypassing
    /// the usual `Pending → InProgress → Failed` chain that
    /// [`Self::mark_in_progress`] / [`Self::mark_failed`] enforce.
    ///
    /// Returns the swept window ids so callers can flush / log /
    /// increment counters per recovered window. Windows whose
    /// `created_at` is `None` (blobs persisted before that field
    /// was added) are *not* swept — we cannot prove they are stuck
    /// without an opening timestamp, so the conservative choice is
    /// to leave them alone until the host explicitly fails or
    /// retries them.
    ///
    /// Used by the FFI `open_store` recovery sweep to clean up the
    /// state described in the docstring on [`SynthesisWindow::created_at`]:
    /// `Pending` windows whose earlier flush landed but whose
    /// earlier commit never did, either because the host crashed
    /// mid-dispatch or because the synthesis-apply transaction
    /// failed and the in-process recovery (`apply_dispatch_outcome`'s
    /// `fail_window_on_live_manager` on commit failure) also failed
    /// to flush. The direct `Pending → Failed` transition exists
    /// specifically for this sweep — it is not part of the normal
    /// dispatcher lifecycle and must not be used from
    /// `fail_window_on_live_manager` (which keeps the
    /// `Pending → InProgress → Failed` chain so a live operator can
    /// correlate refusals in the warn log).
    pub fn sweep_stuck_pending(
        &mut self,
        now: DateTime<Utc>,
        threshold: Duration,
    ) -> Vec<WindowId> {
        let mut swept = Vec::new();
        for window in self.windows.values_mut() {
            if window.status != WindowStatus::Pending {
                continue;
            }
            let Some(created) = window.created_at else {
                continue;
            };
            if now - created > threshold {
                window.status = WindowStatus::Failed;
                swept.push(window.id);
            }
        }
        swept
    }

    /// Remove every window registered for `scope_id` — both from
    /// the per-id `windows` map and from the per-scope `by_scope`
    /// index.
    ///
    /// Used by the FFI substrate's cryptographic-forgetting path
    /// (`forget_scope` / `forget_scope_state`) so synthesis state
    /// for the forgotten scope is unreachable in-memory as soon as
    /// the on-disk row is deleted. Idempotent: returns silently
    /// when the scope has no registered windows.
    pub fn remove_windows_for_scope(&mut self, scope_id: ScopeId) {
        if let Some(ids) = self.by_scope.remove(&scope_id) {
            for id in ids {
                self.windows.remove(&id);
            }
        }
    }

    /// Remove the supplied `window_ids` for `scope_id` (typically
    /// the result of a retention-cap prune).
    ///
    /// `window_ids` that do not appear in the scope's
    /// `by_scope` list are silently ignored — the caller may pass
    /// the output of a separate filter without re-validating each
    /// id. The remaining ids in the per-scope list preserve their
    /// original order so callers that depend on insertion order
    /// (e.g. `windows_for`) keep observing the same sequence.
    pub fn remove_windows<I>(&mut self, scope_id: ScopeId, window_ids: I)
    where
        I: IntoIterator<Item = WindowId>,
    {
        let removed: std::collections::HashSet<WindowId> = window_ids.into_iter().collect();
        if removed.is_empty() {
            return;
        }
        if let Some(ids) = self.by_scope.get_mut(&scope_id) {
            ids.retain(|id| !removed.contains(id));
            if ids.is_empty() {
                self.by_scope.remove(&scope_id);
            }
        }
        for id in &removed {
            self.windows.remove(id);
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
    fn sweep_stuck_pending_transitions_old_pending_to_failed() {
        let mut mgr = SynthesisWindowManager::new();
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let id = mgr
            .open_window(scope, now - Duration::hours(1), now)
            .unwrap();
        // Backdate `created_at` past the threshold.
        mgr.find_mut(id).unwrap().created_at = Some(now - Duration::hours(2));
        let swept = mgr.sweep_stuck_pending(now, Duration::hours(1));
        assert_eq!(swept, vec![id]);
        assert_eq!(mgr.get(id).unwrap().status, WindowStatus::Failed);
    }

    #[test]
    fn sweep_stuck_pending_leaves_fresh_pending_alone() {
        let mut mgr = SynthesisWindowManager::new();
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let id = mgr
            .open_window(scope, now - Duration::hours(1), now)
            .unwrap();
        // `created_at` is auto-stamped to ~now — comfortably under
        // the one-hour threshold.
        let swept = mgr.sweep_stuck_pending(now, Duration::hours(1));
        assert!(swept.is_empty());
        assert_eq!(mgr.get(id).unwrap().status, WindowStatus::Pending);
    }

    #[test]
    fn sweep_stuck_pending_leaves_in_progress_alone_even_when_old() {
        // `InProgress` is owned by a live dispatcher — sweeping it
        // out from under one would discard real synthesis output.
        // The sweep must touch only `Pending` windows even if they
        // are technically older than the threshold.
        let mut mgr = SynthesisWindowManager::new();
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let id = mgr
            .open_window(scope, now - Duration::hours(1), now)
            .unwrap();
        mgr.mark_in_progress(id).unwrap();
        // Backdate to before the threshold — sweep must still skip.
        mgr.find_mut(id).unwrap().created_at = Some(now - Duration::hours(2));
        let swept = mgr.sweep_stuck_pending(now, Duration::hours(1));
        assert!(swept.is_empty());
        assert_eq!(mgr.get(id).unwrap().status, WindowStatus::InProgress);
    }

    #[test]
    fn sweep_stuck_pending_leaves_legacy_no_created_at_alone() {
        // Blobs persisted before the `created_at` field was added
        // (`#[serde(default)]`-induced `None`) cannot be aged. The
        // sweep must conservatively leave them in `Pending` so the
        // host can decide when to retry or forget them.
        let mut mgr = SynthesisWindowManager::new();
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let id = mgr
            .open_window(scope, now - Duration::hours(1), now)
            .unwrap();
        mgr.find_mut(id).unwrap().created_at = None;
        let swept = mgr.sweep_stuck_pending(now, Duration::hours(1));
        assert!(swept.is_empty());
        assert_eq!(mgr.get(id).unwrap().status, WindowStatus::Pending);
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

    /// Regression test for the invariant documented on the
    /// `SynthesisWindowManager` derive: both `WindowId` and `ScopeId`
    /// must serialise to strings (via their `#[serde(transparent)]`
    /// Uuid wrappers) so the manager's `HashMap` fields round-trip
    /// through `serde_json`. If a future refactor strips
    /// `#[serde(transparent)]` from either id type, this test fails
    /// loudly instead of letting the FFI substrate silently lose its
    /// window history at every `open_store`.
    #[test]
    fn manager_round_trips_through_serde_json() {
        let mut mgr = SynthesisWindowManager::new();
        let scope_a = ScopeId::new_v4();
        let scope_b = ScopeId::new_v4();
        let now = Utc::now();

        let a1 = mgr
            .open_window(scope_a, now - Duration::hours(2), now - Duration::hours(1))
            .unwrap();
        mgr.mark_in_progress(a1).unwrap();
        mgr.mark_complete(a1).unwrap();

        let a2 = mgr
            .open_window(scope_a, now - Duration::hours(1), now)
            .unwrap();
        mgr.mark_in_progress(a2).unwrap();
        mgr.mark_failed(a2).unwrap();

        let b1 = mgr
            .open_window(scope_b, now - Duration::hours(1), now)
            .unwrap();

        let bytes = serde_json::to_vec(&mgr).expect("serialise");
        // Sanity-check the wire format: ScopeId / WindowId must
        // appear as plain UUID-string JSON keys (otherwise
        // `serde_json` fell back to the array-of-pairs encoding
        // and the manager is no longer rehydratable).
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(
            text.contains(&format!("\"{}\"", a1.as_uuid())),
            "WindowId key must serialise as a hyphenated-UUID string",
        );
        assert!(
            text.contains(&format!("\"{}\"", scope_a.as_uuid())),
            "ScopeId key must serialise as a hyphenated-UUID string",
        );

        let restored: SynthesisWindowManager = serde_json::from_slice(&bytes).expect("deserialise");
        assert_eq!(restored.windows_for(scope_a).len(), 2);
        assert_eq!(restored.windows_for(scope_b).len(), 1);
        assert_eq!(restored.get(a1).unwrap().status, WindowStatus::Complete);
        assert_eq!(restored.get(a2).unwrap().status, WindowStatus::Failed);
        assert_eq!(restored.get(b1).unwrap().status, WindowStatus::Pending);
        assert_eq!(restored.len(), 3);
    }

    /// Tier stamping via [`HierarchyEnforcedWindowManager::open_tiered_window`]
    /// must survive a serde round-trip so the FFI layer can report
    /// the tier from rehydrated windows without re-deriving it from
    /// a (possibly absent) [`SynthesisObject`].
    #[test]
    fn open_tiered_window_persists_tier_through_serde() {
        use crate::hierarchy::{HierarchyEnforcedWindowManager, WindowScopeTier};

        let mut mgr = SynthesisWindowManager::new();
        let scope_domain = ScopeId::new_v4();
        let scope_tenant = ScopeId::new_v4();
        let now = Utc::now();

        let dom_handle = mgr
            .open_tiered_window(
                scope_domain,
                WindowScopeTier::Domain,
                now - Duration::hours(1),
                now,
            )
            .unwrap();
        let ten_handle = mgr
            .open_tiered_window(
                scope_tenant,
                WindowScopeTier::Tenant,
                now - Duration::hours(1),
                now,
            )
            .unwrap();
        // Mark the tenant window in_progress so we exercise the
        // non-Complete path where the tier could not previously be
        // inferred from a synthesis object.
        mgr.mark_in_progress(ten_handle.window_id).unwrap();

        let bytes = serde_json::to_vec(&mgr).expect("serialise");
        let restored: SynthesisWindowManager = serde_json::from_slice(&bytes).expect("deserialise");
        assert_eq!(
            restored.get(dom_handle.window_id).unwrap().tier,
            Some(WindowScopeTier::Domain),
        );
        assert_eq!(
            restored.get(ten_handle.window_id).unwrap().tier,
            Some(WindowScopeTier::Tenant),
        );
        assert_eq!(
            restored.get(ten_handle.window_id).unwrap().status,
            WindowStatus::InProgress,
        );
    }

    /// Backwards-compat: blobs serialised before the `tier` field
    /// was introduced must rehydrate cleanly with `tier: None`
    /// (driven by `#[serde(default)]` on `SynthesisWindow::tier`).
    /// If a future refactor accidentally requires the field, this
    /// test catches the regression rather than letting the FFI
    /// substrate fail to open every pre-existing database.
    #[test]
    fn synthesis_window_without_tier_field_rehydrates_as_none() {
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let window = SynthesisWindow::new(scope, now - Duration::hours(1), now).unwrap();
        // Hand-craft a JSON blob omitting the `tier` field — mimics
        // what an older substrate would have written to disk.
        let legacy_json = serde_json::json!({
            "id": window.id.as_uuid().to_string(),
            "scope_id": scope.as_uuid().to_string(),
            "window_start": window.window_start,
            "window_end": window.window_end,
            "status": "pending",
        });
        let restored: SynthesisWindow =
            serde_json::from_value(legacy_json).expect("legacy blob must rehydrate");
        assert_eq!(restored.tier, None);
        assert_eq!(restored.status, WindowStatus::Pending);
    }
}
