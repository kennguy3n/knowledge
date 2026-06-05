//! In-process implementations of the replication transports.
//!
//! These back the unit/integration tests and single-host dev setups
//! where primary and standby run in the same process (or where
//! replication is exercised without standing up NATS). They are always
//! compiled — the production NATS-backed transports in
//! [`super::nats`] are the feature-gated alternative, not a replacement
//! for these.
//!
//! * [`InMemoryWalBus`] is an ordered, fully-retained log: subscribers
//!   replay every segment from the start and then tail live appends,
//!   exactly matching the durable-replay contract the NATS JetStream
//!   bus provides.
//! * [`InMemoryLeaseStore`] is a TTL lease with compare-and-set
//!   semantics and a monotonic epoch, so leader-election tests observe
//!   the same fencing behaviour as the KV-backed store.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Notify;

use super::{lease_expiry_ms, Lease, LeaseStore, ReplResult, WalBus, WalSegment, WalSubscription};

/// Shared, ordered, fully-retained log of published segments.
#[derive(Default)]
struct BusInner {
    log: Mutex<Vec<WalSegment>>,
    notify: Notify,
}

/// In-process [`WalBus`] backed by a growable in-memory log.
#[derive(Clone, Default)]
pub struct InMemoryWalBus {
    inner: Arc<BusInner>,
}

impl InMemoryWalBus {
    /// A fresh, empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of segments currently retained (test helper).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.log.lock().expect("bus log poisoned").len()
    }

    /// Whether the log is empty (test helper).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl WalBus for InMemoryWalBus {
    async fn publish(&self, segment: &WalSegment) -> ReplResult<()> {
        {
            let mut log = self.inner.log.lock().expect("bus log poisoned");
            log.push(segment.clone());
        }
        // Wake every parked subscriber so they pull the new tail.
        self.inner.notify.notify_waiters();
        Ok(())
    }

    async fn subscribe(&self) -> ReplResult<WalSubscription> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut cursor = 0usize;
            loop {
                // Register for wakeups *before* sampling the log so a
                // publish that races this loop cannot be missed.
                let waiter = inner.notify.notified();
                let pending: Vec<WalSegment> = {
                    let log = inner.log.lock().expect("bus log poisoned");
                    if cursor < log.len() {
                        log[cursor..].to_vec()
                    } else {
                        Vec::new()
                    }
                };
                if pending.is_empty() {
                    waiter.await;
                    continue;
                }
                for seg in pending {
                    cursor += 1;
                    if tx.send(seg).await.is_err() {
                        return; // subscriber dropped
                    }
                }
            }
        });
        Ok(WalSubscription::new(rx))
    }

    async fn latest_watermark(&self) -> ReplResult<u64> {
        let log = self.inner.log.lock().expect("bus log poisoned");
        Ok(log.last().map_or(0, |s| s.cumulative_frames))
    }
}

/// Lease bookkeeping behind a single mutex.
struct LeaseInner {
    lease: Option<Lease>,
    /// Highest epoch ever issued, retained across expiry so a stolen
    /// lease always advances the fencing token.
    last_epoch: u64,
}

/// In-process [`LeaseStore`] with TTL + compare-and-set semantics.
#[derive(Clone)]
pub struct InMemoryLeaseStore {
    inner: Arc<Mutex<LeaseInner>>,
}

impl Default for InMemoryLeaseStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryLeaseStore {
    /// A fresh lease store with no current holder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LeaseInner {
                lease: None,
                last_epoch: 0,
            })),
        }
    }
}

#[async_trait]
impl LeaseStore for InMemoryLeaseStore {
    async fn acquire(&self, node_id: &str, ttl: Duration) -> ReplResult<Lease> {
        let now = Utc::now().timestamp_millis();
        let expires_at_ms = lease_expiry_ms(now, ttl);
        let mut g = self.inner.lock().expect("lease poisoned");
        match &g.lease {
            // Renew our own still-valid lease (epoch unchanged).
            Some(l) if l.holder == node_id && l.expires_at_ms > now => {
                let renewed = Lease {
                    holder: node_id.to_string(),
                    epoch: l.epoch,
                    expires_at_ms,
                };
                g.lease = Some(renewed.clone());
                Ok(renewed)
            }
            // Someone else holds a valid lease: report it, not acquired.
            Some(l) if l.expires_at_ms > now => Ok(l.clone()),
            // Vacant or expired: take it, bumping the fencing epoch.
            _ => {
                let epoch = g.last_epoch + 1;
                g.last_epoch = epoch;
                let fresh = Lease {
                    holder: node_id.to_string(),
                    epoch,
                    expires_at_ms,
                };
                g.lease = Some(fresh.clone());
                Ok(fresh)
            }
        }
    }

    async fn release(&self, node_id: &str) -> ReplResult<()> {
        let mut g = self.inner.lock().expect("lease poisoned");
        if g.lease.as_ref().is_some_and(|l| l.holder == node_id) {
            g.lease = None;
        }
        Ok(())
    }

    async fn current(&self) -> ReplResult<Option<Lease>> {
        let now = Utc::now().timestamp_millis();
        let g = self.inner.lock().expect("lease poisoned");
        Ok(g.lease.clone().filter(|l| l.expires_at_ms > now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::{WalFrame, WalSegment};

    fn seg(seq: u64, cumulative: u64) -> WalSegment {
        WalSegment {
            seq,
            cumulative_frames: cumulative,
            page_size: 512,
            salt1: 1,
            salt2: 2,
            frames: vec![WalFrame {
                page_number: u32::try_from(seq).unwrap(),
                db_size_after_commit: u32::try_from(seq).unwrap(),
                page_data: vec![u8::try_from(seq & 0xFF).unwrap(); 512],
            }],
        }
    }

    #[tokio::test]
    async fn bus_replays_backlog_then_tails_live() {
        let bus = InMemoryWalBus::new();
        bus.publish(&seg(1, 1)).await.unwrap();
        bus.publish(&seg(2, 2)).await.unwrap();

        let mut sub = bus.subscribe().await.unwrap();
        // Backlog replays in order.
        assert_eq!(sub.next().await.unwrap().seq, 1);
        assert_eq!(sub.next().await.unwrap().seq, 2);

        // Live append is delivered to the existing subscriber.
        bus.publish(&seg(3, 3)).await.unwrap();
        assert_eq!(sub.next().await.unwrap().seq, 3);

        assert_eq!(bus.latest_watermark().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn lease_single_writer_and_renew() {
        let store = InMemoryLeaseStore::new();
        let ttl = Duration::from_secs(30);

        let a = store.acquire("node-a", ttl).await.unwrap();
        assert_eq!(a.holder, "node-a");
        assert_eq!(a.epoch, 1);

        // Contender loses while the lease is valid.
        let b = store.acquire("node-b", ttl).await.unwrap();
        assert_eq!(b.holder, "node-a");

        // Holder renews without bumping the epoch.
        let a2 = store.acquire("node-a", ttl).await.unwrap();
        assert_eq!(a2.holder, "node-a");
        assert_eq!(a2.epoch, 1);

        assert_eq!(store.current().await.unwrap().unwrap().holder, "node-a");
    }

    #[tokio::test]
    async fn lease_steal_after_expiry_bumps_epoch() {
        let store = InMemoryLeaseStore::new();
        // Acquire with a zero TTL so it is immediately expired.
        let a = store
            .acquire("node-a", Duration::from_millis(0))
            .await
            .unwrap();
        assert_eq!(a.epoch, 1);
        assert!(store.current().await.unwrap().is_none());

        let b = store
            .acquire("node-b", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(b.holder, "node-b");
        assert_eq!(
            b.epoch, 2,
            "stealing an expired lease must advance the epoch"
        );
    }

    #[tokio::test]
    async fn lease_release_frees_holder() {
        let store = InMemoryLeaseStore::new();
        store
            .acquire("node-a", Duration::from_secs(30))
            .await
            .unwrap();
        store.release("node-a").await.unwrap();
        assert!(store.current().await.unwrap().is_none());
        let b = store
            .acquire("node-b", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(b.holder, "node-b");
    }
}
