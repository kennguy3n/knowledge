//! Standby-mode replication: replay shipped frames read-only.
//!
//! A standby subscribes to the [`WalBus`], replays the [`WalSegment`]s
//! it receives into its local SQLCipher database file, and tracks how
//! far behind the primary it is. Because every WAL frame is a *full
//! page image*, applying frames in commit order is exactly what a
//! checkpoint does: each frame's page is written at its on-disk offset
//! `(page_number - 1) * page_size`, and a commit frame's
//! post-commit database size truncates/extends the file to the right
//! length. SQLCipher page images are opaque ciphertext, so writing them
//! verbatim preserves encryption — the standby's copy decrypts with the
//! same master key as the primary.
//!
//! ## Lag
//!
//! Each segment carries the primary's cumulative shipped-frame count
//! (`cumulative_frames`). After applying a segment the standby's
//! applied total equals that segment's cumulative count; the live lag
//! is `bus.latest_watermark() - applied_total`, refreshed both on apply
//! and on a periodic timer so an idle-but-behind standby still reports
//! truthfully.

use std::io::SeekFrom;
use std::sync::Arc;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;

use super::{ReplError, ReplResult, ReplicationConfig, ReplicationShared, WalBus, WalSegment};

/// Drives WAL replay for a standby node.
pub struct StandbyReplicator {
    shared: Arc<ReplicationShared>,
    db_path: String,
    last_salts: Option<(u32, u32)>,
    last_seq: u64,
    applied_frames_total: u64,
}

impl StandbyReplicator {
    /// Construct a standby replicator that materialises frames into the
    /// SQLCipher file at `config.store_path`.
    #[must_use]
    pub fn new(shared: Arc<ReplicationShared>, config: &ReplicationConfig) -> Self {
        Self {
            shared,
            db_path: config.store_path.clone(),
            last_salts: None,
            last_seq: 0,
            applied_frames_total: 0,
        }
    }

    /// Total frames applied so far.
    #[must_use]
    pub fn applied_frames_total(&self) -> u64 {
        self.applied_frames_total
    }

    /// Apply one segment to the local database file.
    ///
    /// Segments already seen (`seq <= last_seq` within the current WAL
    /// generation) are skipped so the bus's at-least-once replay is
    /// idempotent. A change in the source WAL salts marks a new
    /// generation and resets the per-generation sequence cursor.
    ///
    /// # Errors
    ///
    /// Returns [`ReplError::Transport`] if the database file cannot be
    /// opened or written.
    pub async fn apply_segment(&mut self, segment: &WalSegment) -> ReplResult<u64> {
        let salts = (segment.salt1, segment.salt2);
        if self.last_salts != Some(salts) {
            self.last_salts = Some(salts);
            self.last_seq = 0;
        }
        if segment.seq <= self.last_seq {
            // Duplicate / already-applied segment from an at-least-once
            // replay: ignore but keep the watermark fresh.
            self.shared.record_applied(self.applied_frames_total);
            return Ok(0);
        }

        let page_size = segment.page_size as u64;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(&self.db_path)
            .await
            .map_err(|e| {
                ReplError::Transport(format!("opening standby db {}: {e}", self.db_path))
            })?;

        let mut db_pages: Option<u32> = None;
        for frame in &segment.frames {
            let offset = (u64::from(frame.page_number) - 1) * page_size;
            file.seek(SeekFrom::Start(offset))
                .await
                .map_err(|e| ReplError::Transport(format!("seek: {e}")))?;
            file.write_all(&frame.page_data)
                .await
                .map_err(|e| ReplError::Transport(format!("write page: {e}")))?;
            if frame.is_commit() {
                db_pages = Some(frame.db_size_after_commit);
            }
        }
        // A commit frame fixes the logical database size; truncate or
        // extend the file so a checkpoint-equivalent state results.
        if let Some(pages) = db_pages {
            file.set_len(u64::from(pages) * page_size)
                .await
                .map_err(|e| ReplError::Transport(format!("set_len: {e}")))?;
        }
        file.flush()
            .await
            .map_err(|e| ReplError::Transport(format!("flush: {e}")))?;

        self.last_seq = segment.seq;
        self.applied_frames_total = segment.cumulative_frames;
        self.shared.record_applied(self.applied_frames_total);
        tracing::debug!(
            seq = segment.seq,
            applied_total = self.applied_frames_total,
            "standby: applied WAL segment"
        );
        Ok(segment.frame_count())
    }

    /// Recompute and publish the current lag from the bus watermark.
    async fn refresh_lag<B: WalBus>(&self, bus: &B) {
        match bus.latest_watermark().await {
            Ok(watermark) => {
                let lag = watermark.saturating_sub(self.applied_frames_total);
                self.shared.set_lag_frames(lag);
            }
            Err(e) => tracing::warn!(error = %e, "standby: reading watermark failed"),
        }
    }

    /// Subscribe and replay segments until `shutdown` flips or the bus
    /// closes. Lag is refreshed on every applied segment and on a 1s
    /// timer so an idle standby still reports an accurate gap.
    pub async fn run<B: WalBus>(mut self, bus: Arc<B>, mut shutdown: watch::Receiver<bool>) {
        let mut sub = match bus.subscribe().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "standby: failed to subscribe to WAL bus");
                return;
            }
        };
        let mut lag_ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        lag_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(db = %self.db_path, "standby: starting WAL replay");
        loop {
            tokio::select! {
                maybe_seg = sub.next() => {
                    let Some(seg) = maybe_seg else {
                        tracing::info!("standby: WAL bus closed; replay loop exiting");
                        return;
                    };
                    if let Err(e) = self.apply_segment(&seg).await {
                        tracing::warn!(error = %e, seq = seg.seq, "standby: apply failed");
                    }
                    self.refresh_lag(bus.as_ref()).await;
                }
                _ = lag_ticker.tick() => {
                    self.refresh_lag(bus.as_ref()).await;
                }
                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        tracing::info!("standby: WAL replay shutting down");
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::memory::InMemoryWalBus;
    use crate::replication::{Role, WalFrame};

    fn segment(seq: u64, cumulative: u64, salts: (u32, u32), frames: Vec<WalFrame>) -> WalSegment {
        WalSegment {
            seq,
            cumulative_frames: cumulative,
            page_size: 512,
            salt1: salts.0,
            salt2: salts.1,
            frames,
        }
    }

    fn page(pn: u32, db: u32, fill: u8) -> WalFrame {
        WalFrame {
            page_number: pn,
            db_size_after_commit: db,
            page_data: vec![fill; 512],
        }
    }

    #[tokio::test]
    async fn reconstructs_database_file_from_frames() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("standby.db").to_string_lossy().into_owned();
        let shared = Arc::new(ReplicationShared::enabled(Role::Standby));
        let config = ReplicationConfig::from_env(&db_path, Some("standby")).unwrap();
        let mut standby = StandbyReplicator::new(Arc::clone(&shared), &config);

        // Commit pages 1 and 2 (db grows to 2 pages).
        let applied = standby
            .apply_segment(&segment(
                1,
                2,
                (10, 20),
                vec![page(1, 0, 0xA1), page(2, 2, 0xA2)],
            ))
            .await
            .unwrap();
        assert_eq!(applied, 2);

        let bytes = std::fs::read(&db_path).unwrap();
        assert_eq!(bytes.len(), 2 * 512, "file truncated to committed db size");
        assert_eq!(&bytes[0..512], &[0xA1; 512][..]);
        assert_eq!(&bytes[512..1024], &[0xA2; 512][..]);
        assert_eq!(shared.snapshot().applied_frames_total, 2);
        assert!(shared.snapshot().last_applied_at.is_some());
    }

    #[tokio::test]
    async fn skips_already_applied_segments() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("standby.db").to_string_lossy().into_owned();
        let shared = Arc::new(ReplicationShared::enabled(Role::Standby));
        let config = ReplicationConfig::from_env(&db_path, Some("standby")).unwrap();
        let mut standby = StandbyReplicator::new(shared, &config);

        let seg = segment(1, 1, (1, 2), vec![page(1, 1, 0x01)]);
        assert_eq!(standby.apply_segment(&seg).await.unwrap(), 1);
        // Replaying the same seq is a no-op.
        assert_eq!(standby.apply_segment(&seg).await.unwrap(), 0);
        assert_eq!(standby.applied_frames_total(), 1);
    }

    #[tokio::test]
    async fn lag_tracks_primary_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("standby.db").to_string_lossy().into_owned();
        let shared = Arc::new(ReplicationShared::enabled(Role::Standby));
        let config = ReplicationConfig::from_env(&db_path, Some("standby")).unwrap();
        let mut standby = StandbyReplicator::new(Arc::clone(&shared), &config);

        let bus = InMemoryWalBus::new();
        // Primary is at watermark 5; standby has applied nothing yet.
        bus.publish(&segment(1, 5, (1, 2), vec![page(1, 1, 0x01)]))
            .await
            .unwrap();
        standby.refresh_lag(&bus).await;
        assert_eq!(shared.snapshot().lag_frames, 5);

        // After applying up to cumulative=5, lag closes to 0.
        standby
            .apply_segment(&segment(1, 5, (1, 2), vec![page(1, 1, 0x01)]))
            .await
            .unwrap();
        standby.refresh_lag(&bus).await;
        assert_eq!(shared.snapshot().lag_frames, 0);
    }
}
