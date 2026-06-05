//! Primary-mode replication: tail the local WAL and ship new frames.
//!
//! In WAL journal mode SQLite appends every committed transaction's
//! pages to the `<store>-wal` sidecar before (eventually) checkpointing
//! them back into the main database. The primary replicator polls that
//! sidecar, extracts the frames it has not shipped yet via
//! [`WalShipper`], and publishes them as [`WalSegment`]s onto the
//! [`WalBus`]. Standbys replay those segments (see [`super::standby`]).
//!
//! Polling — rather than hooking SQLite's WAL commit callback — keeps
//! the replicator completely decoupled from the FFI store: it never
//! holds a database lock and a slow or absent transport can never stall
//! a writer. The trade-off is sub-poll-interval latency, which the
//! `cumulative_frames` watermark surfaces as replication lag on the
//! standby.

use std::sync::Arc;

use tokio::sync::watch;

use super::{ReplicationConfig, ReplicationShared, WalBus, WalShipper};

/// Drives WAL extraction + shipping for a primary node.
pub struct PrimaryReplicator<B: WalBus> {
    bus: Arc<B>,
    shared: Arc<ReplicationShared>,
    config: ReplicationConfig,
    shipper: WalShipper,
}

impl<B: WalBus> PrimaryReplicator<B> {
    /// Construct a primary replicator over `bus`, recording progress
    /// into `shared`.
    #[must_use]
    pub fn new(bus: Arc<B>, shared: Arc<ReplicationShared>, config: ReplicationConfig) -> Self {
        Self {
            bus,
            shared,
            config,
            shipper: WalShipper::new(),
        }
    }

    /// Extract and publish any new committed frames present in
    /// `wal_bytes`. Returns the number of frames shipped (`0` when there
    /// is nothing new or only an uncommitted tail).
    ///
    /// On success the primary's `published_frames_total` watermark is
    /// advanced; this is the value standbys read to compute lag.
    ///
    /// # Errors
    ///
    /// Propagates WAL parse errors and transport publish failures so the
    /// caller can decide whether to retry on the next poll.
    pub async fn ship_once(&mut self, wal_bytes: &[u8]) -> super::ReplResult<u64> {
        let Some(segment) = self.shipper.next_segment(wal_bytes)? else {
            return Ok(0);
        };
        self.bus.publish(&segment).await?;
        self.shared
            .set_published_frames_total(self.shipper.cumulative_frames());
        let shipped = segment.frame_count();
        tracing::debug!(
            seq = segment.seq,
            frames = shipped,
            cumulative = segment.cumulative_frames,
            "primary: shipped WAL segment"
        );
        Ok(shipped)
    }

    /// Run the poll/ship loop until `shutdown` flips to `true`.
    ///
    /// Each tick reads the WAL sidecar and ships any new committed
    /// frames. A missing sidecar (no writes yet, or SQLite is between
    /// checkpoints) is treated as "nothing to ship"; transient read or
    /// publish errors are logged and retried on the next tick rather
    /// than tearing down replication.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let wal_path = self.config.wal_path();
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(%wal_path, "primary: starting WAL shipper");
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match tokio::fs::read(&wal_path).await {
                        Ok(bytes) => {
                            if let Err(e) = self.ship_once(&bytes).await {
                                tracing::warn!(error = %e, "primary: shipping failed; will retry");
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // No WAL sidecar yet — nothing to ship.
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, %wal_path, "primary: reading WAL failed");
                        }
                    }
                }
                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        tracing::info!("primary: WAL shipper shutting down");
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
    use crate::replication::{
        ChecksumOrder, Role, WalFrame, WalHeader, WalSegment, WalSubscription,
    };

    fn wal(salt1: u32, salt2: u32, specs: &[(u32, u32, u8)]) -> Vec<u8> {
        let frames = specs
            .iter()
            .map(|&(pn, db, fill)| WalFrame {
                page_number: pn,
                db_size_after_commit: db,
                page_data: vec![fill; 512],
            })
            .collect::<Vec<_>>();
        let header = WalHeader {
            order: ChecksumOrder::Little,
            page_size: 512,
            checkpoint_seq: 0,
            salt1,
            salt2,
            checksum: (0, 0),
        };
        crate::replication::encode_wal(&header, &frames).expect("encode")
    }

    async fn drain(sub: &mut WalSubscription, n: usize) -> Vec<WalSegment> {
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(sub.next().await.expect("segment"));
        }
        out
    }

    #[tokio::test]
    async fn ships_committed_frames_and_advances_watermark() {
        let bus = Arc::new(InMemoryWalBus::new());
        let shared = Arc::new(ReplicationShared::enabled(Role::Primary));
        let config = ReplicationConfig::from_env("/tmp/unused.db", Some("primary")).unwrap();
        let mut primary = PrimaryReplicator::new(Arc::clone(&bus), Arc::clone(&shared), config);

        let mut sub = bus.subscribe().await.unwrap();

        // One committed transaction → one segment of two frames.
        let shipped = primary
            .ship_once(&wal(10, 20, &[(1, 0, 0xA1), (2, 2, 0xA2)]))
            .await
            .unwrap();
        assert_eq!(shipped, 2);
        assert_eq!(shared.snapshot().published_frames_total, 2);

        // Idempotent on an unchanged WAL.
        assert_eq!(
            primary
                .ship_once(&wal(10, 20, &[(1, 0, 0xA1), (2, 2, 0xA2)]))
                .await
                .unwrap(),
            0
        );

        let segs = drain(&mut sub, 1).await;
        assert_eq!(segs[0].frame_count(), 2);
        assert_eq!(segs[0].cumulative_frames, 2);
    }

    #[tokio::test]
    async fn withholds_uncommitted_tail() {
        let bus = Arc::new(InMemoryWalBus::new());
        let shared = Arc::new(ReplicationShared::enabled(Role::Primary));
        let config = ReplicationConfig::from_env("/tmp/unused.db", Some("primary")).unwrap();
        let mut primary = PrimaryReplicator::new(bus, shared, config);
        // A lone non-commit frame ships nothing.
        assert_eq!(
            primary
                .ship_once(&wal(1, 2, &[(1, 0, 0x01)]))
                .await
                .unwrap(),
            0
        );
    }
}
