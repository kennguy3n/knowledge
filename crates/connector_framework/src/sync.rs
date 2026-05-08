//! Sync state — per-connector cursor / mode / last-sync tracking.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::token_vault::ConnectorInstanceId;

/// Direction of a sync run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// First-time / re-bootstrap pull. Walks the entire source
    /// surface the connector is authorised for.
    Full,
    /// Steady-state pull keyed off [`SyncState::cursor`].
    Incremental,
}

impl SyncMode {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Incremental => "incremental",
        }
    }
}

/// Lifecycle of a sync run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// Never run.
    NeverRun,
    /// A sync is currently in flight.
    InProgress,
    /// Last sync completed successfully.
    Succeeded,
    /// Last sync failed; the connector should be retried with
    /// back-off.
    Failed,
}

/// Per-connector-instance sync state.
///
/// `cursor` is opaque to the substrate — Google Drive uses a
/// `pageToken`, Notion uses a timestamp, Jira uses a sequence id;
/// connectors round-trip whatever string the source returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// Connector this state belongs to.
    pub connector: ConnectorInstanceId,
    /// Current sync mode.
    pub mode: SyncMode,
    /// Provider-specific cursor (page token, timestamp, …). `None`
    /// means "start from the beginning".
    pub cursor: Option<String>,
    /// Wall-clock time of the last *successful* sync.
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Lifecycle status of the most recent sync run.
    pub status: SyncStatus,
    /// Free-form error message captured when `status == Failed`.
    pub last_error: Option<String>,
}

impl SyncState {
    /// Construct a fresh state for `connector`, starting in
    /// [`SyncMode::Full`] with no cursor.
    pub fn new(connector: ConnectorInstanceId) -> Self {
        Self {
            connector,
            mode: SyncMode::Full,
            cursor: None,
            last_synced_at: None,
            status: SyncStatus::NeverRun,
            last_error: None,
        }
    }

    /// Mark the connector as currently syncing.
    pub fn mark_in_progress(&mut self) {
        self.status = SyncStatus::InProgress;
        self.last_error = None;
    }

    /// Record a successful sync run, advancing `cursor` and
    /// `last_synced_at` and switching the connector into
    /// [`SyncMode::Incremental`] (a successful run, by definition,
    /// produces a usable cursor for follow-up incremental pulls).
    pub fn mark_succeeded(&mut self, new_cursor: Option<String>, at: DateTime<Utc>) {
        self.cursor = new_cursor;
        self.last_synced_at = Some(at);
        self.status = SyncStatus::Succeeded;
        self.mode = SyncMode::Incremental;
        self.last_error = None;
    }

    /// Record a failed sync run.
    pub fn mark_failed(&mut self, err: impl Into<String>) {
        self.status = SyncStatus::Failed;
        self.last_error = Some(err.into());
    }

    /// True iff the connector has a valid cursor for incremental
    /// pulls.
    pub fn can_run_incremental(&self) -> bool {
        self.mode == SyncMode::Incremental && self.cursor.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_starts_in_full_mode() {
        let st = SyncState::new(ConnectorInstanceId::new_v4());
        assert_eq!(st.mode, SyncMode::Full);
        assert_eq!(st.status, SyncStatus::NeverRun);
        assert!(st.cursor.is_none());
        assert!(!st.can_run_incremental());
    }

    #[test]
    fn successful_run_switches_to_incremental_with_cursor() {
        let mut st = SyncState::new(ConnectorInstanceId::new_v4());
        st.mark_in_progress();
        assert_eq!(st.status, SyncStatus::InProgress);
        let now = Utc::now();
        st.mark_succeeded(Some("page-2".into()), now);
        assert_eq!(st.mode, SyncMode::Incremental);
        assert_eq!(st.status, SyncStatus::Succeeded);
        assert_eq!(st.cursor.as_deref(), Some("page-2"));
        assert_eq!(st.last_synced_at, Some(now));
        assert!(st.can_run_incremental());
    }

    #[test]
    fn failed_run_records_error_without_advancing_cursor() {
        let mut st = SyncState::new(ConnectorInstanceId::new_v4());
        st.mark_in_progress();
        st.mark_failed("rate limited");
        assert_eq!(st.status, SyncStatus::Failed);
        assert_eq!(st.last_error.as_deref(), Some("rate limited"));
        assert!(st.cursor.is_none());
        // Mode stays Full because we never advanced.
        assert_eq!(st.mode, SyncMode::Full);
    }

    #[test]
    fn successful_run_clears_previous_error() {
        let mut st = SyncState::new(ConnectorInstanceId::new_v4());
        st.mark_failed("first try failed");
        st.mark_in_progress();
        st.mark_succeeded(Some("p1".into()), Utc::now());
        assert!(st.last_error.is_none());
    }
}
