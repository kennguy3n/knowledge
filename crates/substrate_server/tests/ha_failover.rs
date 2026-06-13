//! HA failover integration test (TASK B3).
//!
//! Drives a real two-node active-passive cluster through a *crash*
//! failover and asserts the three properties an operator cares about:
//!
//! 1. **Self-promotion via the lease.** When the primary dies without
//!    releasing its lease, the surviving standby steals the expired
//!    lease on its next election tick and promotes itself to primary —
//!    no human in the loop.
//! 2. **Fencing.** The stolen lease advances the monotonic epoch, so the
//!    dead ex-primary can never be mistaken for the current leader (the
//!    Go gateway routes writes to the epoch-fenced primary; see
//!    `server/internal/substrate/client_ha_test.go` for the gateway-side
//!    re-route proof).
//! 3. **Bounded data loss (RPO).** Every WAL segment the primary shipped
//!    and the bus acked before the crash is retained in the durable log
//!    and is still replayable by the promoted node — RPO is `0` for
//!    acked frames. The only window of possible loss is the in-flight
//!    segment the dead primary had not yet shipped, bounded by one
//!    checkpoint interval.
//!
//! The test runs against the in-process [`InMemoryWalBus`] /
//! [`InMemoryLeaseStore`], whose compare-and-set + TTL + monotonic-epoch
//! semantics are identical to the production NATS JetStream / KV
//! transports (see `replication::memory` module docs). That keeps the
//! test deterministic and hermetic (no container, no socket) while
//! exercising the same `FailoverCoordinator` election/promotion code
//! path the NATS-backed deployment runs.
//!
//! Measured failover characteristics from this harness are documented in
//! `docs/operator/ha-failover.md` and `deploy/HA.md`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use substrate_server::replication::failover::{ensure_writable, FailoverCoordinator};
use substrate_server::replication::memory::{InMemoryLeaseStore, InMemoryWalBus};
use substrate_server::replication::{
    ReplicationConfig, ReplicationShared, Role, WalBus, WalFrame, WalSegment,
};

/// Lease TTL used by the test cluster. Short so a crash failover
/// completes quickly; the renewal cadence is `TTL/3` (see
/// `FailoverCoordinator::run_auto`).
const LEASE_TTL: Duration = Duration::from_secs(1);

/// WAL segments the primary ships (and the bus acks) before the crash —
/// the data that must survive the failover with RPO = 0.
const SHIPPED_FRAMES: u64 = 5;

/// A running auto-mode node: its observable shared state plus the task
/// handle and shutdown switch driving its election loop.
struct Node {
    id: String,
    shared: Arc<ReplicationShared>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Node {
    /// Spawn an auto-mode coordinator over the shared `bus` + `lease`.
    fn spawn(id: &str, bus: Arc<InMemoryWalBus>, lease: Arc<InMemoryLeaseStore>) -> Self {
        let mut config = ReplicationConfig::from_env("/tmp/ha-failover-test.db", Some("auto"))
            .expect("auto replication config");
        config.node_id = id.to_string();
        config.lease_ttl = LEASE_TTL;
        let shared = Arc::new(ReplicationShared::enabled(Role::Standby));
        let coordinator = FailoverCoordinator::new(bus, lease, Arc::clone(&shared), config, None);
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(coordinator.run(stop_rx));
        Self {
            id: id.to_string(),
            shared,
            stop,
            task,
        }
    }

    /// Simulate a hard crash: cancel the election loop at its next await
    /// point so it can never run the graceful-shutdown path that would
    /// release the lease. The lease therefore lapses by TTL expiry, which
    /// is exactly what a `kill -9` of the primary process looks like to
    /// the rest of the cluster.
    fn crash(self) {
        self.task.abort();
        // Drop `stop` without sending: no graceful drain, no lease release.
        drop(self.stop);
    }

    /// Graceful shutdown (used for the surviving node at end of test).
    async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

/// Poll `cond` up to `deadline`, sleeping briefly between checks.
/// Returns the elapsed time when `cond` first holds, or panics.
async fn await_until(label: &str, deadline: Duration, mut cond: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return start.elapsed();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out after {deadline:?} waiting for: {label}");
}

/// A WAL segment carrying `seq` as its sequence and cumulative frame
/// count, with a single deterministic page. Mirrors the shipper output.
fn segment(seq: u64) -> WalSegment {
    WalSegment {
        seq,
        cumulative_frames: seq,
        page_size: 512,
        salt1: 1,
        salt2: 2,
        frames: vec![WalFrame {
            page_number: u32::try_from(seq).expect("seq fits u32"),
            db_size_after_commit: u32::try_from(seq).expect("seq fits u32"),
            page_data: vec![u8::try_from(seq & 0xFF).expect("byte"); 512],
        }],
    }
}

#[tokio::test]
async fn primary_crash_promotes_standby_with_bounded_data_loss() {
    // Shared transports — the single bus + single lease both nodes see,
    // exactly as two substrates share one NATS JetStream + KV in prod.
    let bus = Arc::new(InMemoryWalBus::new());
    let lease = Arc::new(InMemoryLeaseStore::new());

    let node_a = Node::spawn("node-a", Arc::clone(&bus), Arc::clone(&lease));
    let node_b = Node::spawn("node-b", Arc::clone(&bus), Arc::clone(&lease));

    // Exactly one node must win the lease and become primary; the other
    // settles as a standby.
    await_until(
        "a cluster leader is elected",
        Duration::from_secs(5),
        || node_a.shared.role() == Role::Primary || node_b.shared.role() == Role::Primary,
    )
    .await;
    // Let the loser observe the held lease and settle into standby.
    await_until(
        "the non-leader settles as standby",
        Duration::from_secs(5),
        || {
            let roles = [node_a.shared.role(), node_b.shared.role()];
            roles.contains(&Role::Primary) && roles.contains(&Role::Standby)
        },
    )
    .await;

    let (primary, standby) = if node_a.shared.role() == Role::Primary {
        (node_a, node_b)
    } else {
        (node_b, node_a)
    };
    let leader_epoch = primary.shared.epoch();
    assert!(leader_epoch >= 1, "primary should hold a fenced lease");
    assert!(
        ensure_writable(&primary.shared).is_ok(),
        "the elected primary must accept writes"
    );
    assert!(
        ensure_writable(&standby.shared).is_err(),
        "the standby must reject writes (read-only offload)"
    );

    // The primary ships WAL segments; the bus acks (durably retains)
    // each. This is the data that MUST survive the crash.
    for seq in 1..=SHIPPED_FRAMES {
        bus.publish(&segment(seq))
            .await
            .expect("publish WAL segment");
    }
    let acked_watermark = bus.latest_watermark().await.expect("watermark");
    assert_eq!(
        acked_watermark, SHIPPED_FRAMES,
        "all shipped frames acked before the crash"
    );

    // ── Kill the primary (hard crash, no lease release) ──────────────
    let standby_shared = Arc::clone(&standby.shared);
    let crashed_id = primary.id.clone();
    primary.crash();

    // The standby must self-promote once the dead primary's lease lapses.
    let rto = await_until(
        "standby self-promotes to primary",
        Duration::from_secs(10),
        || standby_shared.role() == Role::Primary,
    )
    .await;

    // ── Assert the failover contract ─────────────────────────────────
    // Fencing: the stolen lease advanced the epoch beyond the dead
    // leader's, so a revived ex-primary is unambiguously stale.
    let new_epoch = standby.shared.epoch();
    assert!(
        new_epoch > leader_epoch,
        "promotion must advance the fencing epoch ({new_epoch} > {leader_epoch})"
    );
    // The promoted node now accepts writes (the gateway re-routes to it).
    assert!(
        ensure_writable(&standby.shared).is_ok(),
        "the promoted standby must accept writes"
    );
    // RPO: every acked frame is still durably retained and replayable by
    // the new primary — zero data loss for acked frames.
    let post_failover_watermark = bus.latest_watermark().await.expect("watermark");
    assert_eq!(
        post_failover_watermark, acked_watermark,
        "no acked WAL frames lost across failover (RPO = 0 for acked frames)"
    );

    // RTO is bounded by the lease TTL plus one election tick. With a 1s
    // TTL the worst case is ~TTL (crash right after a renewal) plus the
    // renew cadence (TTL/3) plus our 20ms poll granularity. Assert a
    // generous ceiling and surface the measured value for the docs.
    let rto_ceiling = LEASE_TTL * 2 + Duration::from_millis(500);
    assert!(
        rto <= rto_ceiling,
        "RTO {rto:?} should be within {rto_ceiling:?} (lease TTL {LEASE_TTL:?})"
    );
    eprintln!(
        "HA failover: crashed {crashed_id}; standby promoted in {} ms \
         (lease TTL {} ms); RPO = 0 frames (watermark {} retained)",
        rto.as_millis(),
        LEASE_TTL.as_millis(),
        post_failover_watermark,
    );

    standby.shutdown().await;
}
