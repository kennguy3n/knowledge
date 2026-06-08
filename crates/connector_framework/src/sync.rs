//! Sync state — per-connector cursor / mode / last-sync tracking.

use std::collections::BTreeSet;

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

/// Timestamp-watermark cursor that is robust to multiple records
/// sharing the same `updated_at` instant.
///
/// A naive incremental cursor that stores only the high-water
/// `updated_at` and then skips every record with `updated_at <= cursor`
/// on the next run will **permanently drop** any record that shares the
/// exact boundary instant but was not part of the previous page — e.g.
/// a record written in the same second just after the previous run's
/// snapshot, or one split across a page boundary. Because the cursor has
/// already advanced past that second, such records are never re-examined.
///
/// [`WatermarkCursor`] fixes this at the root by remembering, alongside
/// the watermark instant, the set of source ids observed *at* that
/// instant. The next run re-queries inclusively (`modified_since =
/// watermark`) and drops only the ids it has actually already emitted,
/// so a brand-new record sharing the boundary second is still surfaced
/// while already-seen records are not re-emitted.
///
/// The wire format is backward compatible with the legacy bare-timestamp
/// cursor:
/// * a legacy `2024-01-02T03:04:05+00:00` parses as that watermark with
///   an empty boundary-id set;
/// * the enriched form appends the boundary ids after a `|`, e.g.
///   `2024-01-02T03:04:05+00:00|a,b,c`.
///
/// Ids are escaped so that `%`, `,` and `|` round-trip safely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatermarkCursor {
    watermark: Option<DateTime<Utc>>,
    boundary_ids: BTreeSet<String>,
}

impl WatermarkCursor {
    /// An empty cursor (no watermark yet). Use this to accumulate the
    /// watermark of a fresh full sync via [`WatermarkCursor::observe`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a persisted cursor string. Accepts both the legacy
    /// bare-timestamp form and the enriched `timestamp|id,id,…` form;
    /// any unparseable input yields an empty cursor (start from the
    /// beginning).
    #[must_use]
    pub fn parse(cursor: Option<&str>) -> Self {
        let Some(raw) = cursor else {
            return Self::default();
        };
        let (ts_part, ids_part) = match raw.split_once('|') {
            Some((ts, ids)) => (ts, Some(ids)),
            None => (raw, None),
        };
        let watermark = DateTime::parse_from_rfc3339(ts_part.trim())
            .ok()
            .map(|dt| dt.with_timezone(&Utc));
        let boundary_ids = match (watermark.is_some(), ids_part) {
            (true, Some(ids)) => ids
                .split(',')
                .filter(|s| !s.is_empty())
                .map(decode_id)
                .collect(),
            _ => BTreeSet::new(),
        };
        Self {
            watermark,
            boundary_ids,
        }
    }

    /// The inclusive `modified_since` / `updated_since` value to send to
    /// the provider, or `None` for a from-the-beginning pull.
    #[must_use]
    pub fn query_since(&self) -> Option<String> {
        self.watermark.map(|t| t.to_rfc3339())
    }

    /// The high-water instant, if any.
    #[must_use]
    pub fn watermark(&self) -> Option<DateTime<Utc>> {
        self.watermark
    }

    /// Whether a record with the given `updated_at` and `id` should be
    /// emitted on an incremental run, given this cursor is the prior
    /// state. Records strictly newer than the watermark always emit;
    /// records exactly at the watermark emit only if their id was not
    /// already seen; records older than the watermark never emit.
    #[must_use]
    pub fn should_emit(&self, updated: DateTime<Utc>, id: &str) -> bool {
        match self.watermark {
            None => true,
            Some(w) if updated > w => true,
            Some(w) if updated == w => !self.boundary_ids.contains(id),
            Some(_) => false,
        }
    }

    /// Fold a processed record into the cursor, advancing the watermark
    /// and tracking the ids observed at the (new) watermark instant.
    pub fn observe(&mut self, updated: DateTime<Utc>, id: &str) {
        match self.watermark {
            Some(w) if updated < w => {}
            Some(w) if updated == w => {
                self.boundary_ids.insert(id.to_string());
            }
            _ => {
                self.watermark = Some(updated);
                self.boundary_ids.clear();
                self.boundary_ids.insert(id.to_string());
            }
        }
    }

    /// Serialize back to a persistable cursor string, or `None` when no
    /// watermark has been observed. Emits the legacy bare-timestamp form
    /// when there are no boundary ids so unrelated tooling stays happy.
    #[must_use]
    pub fn to_cursor_string(&self) -> Option<String> {
        let watermark = self.watermark?;
        let base = watermark.to_rfc3339();
        if self.boundary_ids.is_empty() {
            return Some(base);
        }
        let ids = self
            .boundary_ids
            .iter()
            .map(|id| encode_id(id))
            .collect::<Vec<_>>()
            .join(",");
        Some(format!("{base}|{ids}"))
    }
}

fn encode_id(id: &str) -> String {
    id.replace('%', "%25")
        .replace(',', "%2C")
        .replace('|', "%7C")
}

fn decode_id(s: &str) -> String {
    s.replace("%7C", "|")
        .replace("%2C", ",")
        .replace("%25", "%")
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

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn watermark_cursor_parses_legacy_bare_timestamp() {
        let c = WatermarkCursor::parse(Some("2024-03-01T00:00:00+00:00"));
        assert_eq!(c.watermark(), Some(ts("2024-03-01T00:00:00Z")));
        // No boundary ids known from a legacy cursor, so a record at the
        // exact boundary is treated as new (never silently dropped).
        assert!(c.should_emit(ts("2024-03-01T00:00:00Z"), "anything"));
    }

    #[test]
    fn watermark_cursor_round_trips_boundary_ids() {
        let c = WatermarkCursor::parse(Some("2024-03-01T00:00:00+00:00|o-10,o-13"));
        assert_eq!(c.watermark(), Some(ts("2024-03-01T00:00:00Z")));
        // Already-seen ids at the boundary are deduped...
        assert!(!c.should_emit(ts("2024-03-01T00:00:00Z"), "o-10"));
        assert!(!c.should_emit(ts("2024-03-01T00:00:00Z"), "o-13"));
        // ...but a new id sharing the boundary second is still emitted.
        assert!(c.should_emit(ts("2024-03-01T00:00:00Z"), "o-99"));
        // ...and anything strictly newer always emits.
        assert!(c.should_emit(ts("2024-06-01T00:00:00Z"), "o-10"));
        assert_eq!(
            c.to_cursor_string().as_deref(),
            Some("2024-03-01T00:00:00+00:00|o-10,o-13")
        );
    }

    #[test]
    fn watermark_cursor_does_not_drop_new_boundary_record() {
        // Prior run ended at o-10 @ T.
        let prior = WatermarkCursor::parse(Some("2024-03-01T00:00:00+00:00|o-10"));
        let mut next = prior.clone();
        // This run re-sees o-10 (drop), a brand-new o-13 @ T (keep), and a
        // newer o-11 (keep).
        let boundary = ts("2024-03-01T00:00:00Z");
        let newer = ts("2024-06-01T00:00:00Z");
        assert!(!prior.should_emit(boundary, "o-10"));
        assert!(prior.should_emit(boundary, "o-13"));
        next.observe(boundary, "o-13");
        assert!(prior.should_emit(newer, "o-11"));
        next.observe(newer, "o-11");
        // Cursor advanced to the newer instant, boundary set reset to o-11.
        assert_eq!(
            next.to_cursor_string().as_deref(),
            Some("2024-06-01T00:00:00+00:00|o-11")
        );
    }

    #[test]
    fn watermark_cursor_full_sync_accumulates() {
        let mut c = WatermarkCursor::empty();
        c.observe(ts("2024-01-01T00:00:00Z"), "o-1");
        c.observe(ts("2024-01-03T00:00:00Z"), "o-3");
        c.observe(ts("2024-01-02T00:00:00Z"), "o-2");
        assert_eq!(
            c.to_cursor_string().as_deref(),
            Some("2024-01-03T00:00:00+00:00|o-3")
        );
    }

    #[test]
    fn watermark_cursor_escapes_special_ids() {
        let mut c = WatermarkCursor::empty();
        c.observe(ts("2024-01-01T00:00:00Z"), "a,b|c%d");
        let s = c.to_cursor_string().unwrap();
        let round = WatermarkCursor::parse(Some(&s));
        assert!(!round.should_emit(ts("2024-01-01T00:00:00Z"), "a,b|c%d"));
    }

    #[test]
    fn watermark_cursor_empty_has_no_string() {
        assert_eq!(WatermarkCursor::empty().to_cursor_string(), None);
        assert!(WatermarkCursor::parse(None).should_emit(Utc::now(), "x"));
    }
}
