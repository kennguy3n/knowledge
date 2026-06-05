//! Leader election and role orchestration.
//!
//! In [`ReplicationMode::Auto`] every substrate competes for a single
//! [`LeaseStore`] lease. The winner runs as primary (shipping WAL); the
//! losers run as standbys (replaying it). The lease has a TTL and is
//! renewed on a timer at roughly a third of that TTL, so if the primary
//! dies a standby finds the lease expired within one TTL, steals it
//! (advancing the fencing epoch), and **promotes** itself — swapping its
//! standby replay loop for a primary shipping loop. Static
//! [`ReplicationMode::Primary`] / [`ReplicationMode::Standby`] skip the
//! election and just run the corresponding loop.
//!
//! The coordinator owns the lifecycle of the role-specific background
//! task: on every role change it signals the running task to stop, waits
//! for it to drain, then spawns the task for the new role. This keeps
//! "exactly one primary loop at a time" an invariant of the code rather
//! than a hope.

use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::primary::PrimaryReplicator;
use super::standby::StandbyReplicator;
use super::{
    LeaseStore, ReplError, ReplResult, ReplicationConfig, ReplicationMode, ReplicationShared, Role,
    WalBus,
};

/// A running role-specific background task plus the switch that stops it.
struct RoleTask {
    role: Role,
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl RoleTask {
    /// Signal the task to stop and await its completion.
    async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.handle.await;
    }
}

/// Orchestrates leader election and primary/standby task lifecycle.
pub struct FailoverCoordinator<B: WalBus, L: LeaseStore> {
    bus: Arc<B>,
    lease: Arc<L>,
    shared: Arc<ReplicationShared>,
    config: ReplicationConfig,
}

impl<B: WalBus + 'static, L: LeaseStore + 'static> FailoverCoordinator<B, L> {
    /// Construct a coordinator over `bus` and `lease`.
    #[must_use]
    pub fn new(
        bus: Arc<B>,
        lease: Arc<L>,
        shared: Arc<ReplicationShared>,
        config: ReplicationConfig,
    ) -> Self {
        Self {
            bus,
            lease,
            shared,
            config,
        }
    }

    /// Run one election round: acquire/renew the lease and return the
    /// role this node should now serve. Also publishes the observed
    /// fencing epoch into the shared state.
    ///
    /// # Errors
    ///
    /// Propagates [`LeaseStore::acquire`] transport failures.
    pub async fn elect_once(&self) -> ReplResult<Role> {
        let lease = self
            .lease
            .acquire(&self.config.node_id, self.config.lease_ttl)
            .await?;
        self.shared.set_epoch(lease.epoch);
        if lease.holder == self.config.node_id {
            Ok(Role::Primary)
        } else {
            Ok(Role::Standby)
        }
    }

    /// Spawn the background task for `role`, returning its handle. A
    /// [`Role::Disabled`] role spawns nothing.
    fn spawn_role(&self, role: Role) -> Option<RoleTask> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let handle = match role {
            Role::Primary => {
                let primary = PrimaryReplicator::new(
                    Arc::clone(&self.bus),
                    Arc::clone(&self.shared),
                    self.config.clone(),
                );
                tokio::spawn(primary.run(stop_rx))
            }
            Role::Standby => {
                let standby = StandbyReplicator::new(Arc::clone(&self.shared), &self.config);
                let bus = Arc::clone(&self.bus);
                tokio::spawn(standby.run(bus, stop_rx))
            }
            Role::Disabled => return None,
        };
        Some(RoleTask {
            role,
            stop: stop_tx,
            handle,
        })
    }

    /// Drive replication until `shutdown` flips to `true`.
    ///
    /// Static modes spawn their single loop and idle until shutdown.
    /// `Auto` mode runs the election/renewal loop, switching the
    /// role-specific task whenever leadership changes hands.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        match self.config.mode {
            ReplicationMode::Disabled => {
                tracing::debug!("failover: replication disabled; coordinator idle");
            }
            ReplicationMode::Primary => {
                self.shared.set_role(Role::Primary);
                self.run_static(Role::Primary, shutdown).await;
            }
            ReplicationMode::Standby => {
                self.shared.set_role(Role::Standby);
                self.run_static(Role::Standby, shutdown).await;
            }
            ReplicationMode::Auto => self.run_auto(&mut shutdown).await,
        }
    }

    /// Run a single fixed-role loop until shutdown.
    async fn run_static(self, role: Role, mut shutdown: watch::Receiver<bool>) {
        let task = self.spawn_role(role);
        let _ = shutdown.changed().await;
        if let Some(task) = task {
            task.shutdown().await;
        }
    }

    /// Run the auto-failover election loop.
    async fn run_auto(self, shutdown: &mut watch::Receiver<bool>) {
        // Start pessimistically as a standby; promotion happens only on
        // winning the lease below.
        self.shared.set_role(Role::Standby);
        let mut current = self.spawn_role(Role::Standby);
        let mut current_role = Role::Standby;

        // Renew well inside the TTL so a healthy primary never lapses.
        let renew_every = (self.config.lease_ttl / 3).max(std::time::Duration::from_millis(100));
        let mut ticker = tokio::time::interval(renew_every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            node = %self.config.node_id,
            ttl_ms = u64::try_from(self.config.lease_ttl.as_millis()).unwrap_or(u64::MAX),
            "failover: entering auto election loop as standby"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let want = match self.elect_once().await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "failover: election failed; retaining current role");
                            continue;
                        }
                    };
                    if want != current_role {
                        tracing::info!(
                            from = ?current_role, to = ?want, epoch = self.shared.epoch(),
                            "failover: role transition"
                        );
                        if let Some(task) = current.take() {
                            task.shutdown().await;
                        }
                        self.shared.set_role(want);
                        current = self.spawn_role(want);
                        current_role = want;
                    }
                }
                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }

        if let Some(task) = current.take() {
            let was_primary = matches!(task.role, Role::Primary);
            // Stop our role task *before* dropping leadership: a primary
            // first drains and ships its final frames and goes inactive,
            // and only then is the lease released. A peer still promotes
            // well inside the TTL (it merely waits out our fast task
            // shutdown), but two live primaries never overlap — releasing
            // first would briefly let a peer promote while this primary is
            // still shipping.
            task.shutdown().await;
            if was_primary {
                let _ = self.lease.release(&self.config.node_id).await;
            }
        }
    }
}

/// Convenience guard returned by [`ensure_writable`].
///
/// Rejects writes routed to a node that is not currently the primary,
/// so a stale ex-primary (or a misrouted request) cannot mutate a
/// read-only standby.
///
/// # Errors
///
/// Returns [`ReplError::Transport`] describing the current role when the
/// node is not writable.
pub fn ensure_writable(shared: &ReplicationShared) -> ReplResult<()> {
    if shared.is_writable() {
        Ok(())
    } else {
        Err(ReplError::Transport(format!(
            "node is {:?}, not the primary; writes are rejected",
            shared.role()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::memory::{InMemoryLeaseStore, InMemoryWalBus};

    fn coordinator(
        node_id: &str,
        lease: Arc<InMemoryLeaseStore>,
    ) -> FailoverCoordinator<InMemoryWalBus, InMemoryLeaseStore> {
        let mut config =
            ReplicationConfig::from_env("/tmp/failover-test.db", Some("auto")).unwrap();
        config.node_id = node_id.to_string();
        config.lease_ttl = std::time::Duration::from_secs(30);
        FailoverCoordinator::new(
            Arc::new(InMemoryWalBus::new()),
            lease,
            Arc::new(ReplicationShared::enabled(Role::Standby)),
            config,
        )
    }

    #[tokio::test]
    async fn sole_contender_elects_primary() {
        let lease = Arc::new(InMemoryLeaseStore::new());
        let node = coordinator("node-a", Arc::clone(&lease));
        assert_eq!(node.elect_once().await.unwrap(), Role::Primary);
        assert_eq!(node.shared.epoch(), 1);
    }

    #[tokio::test]
    async fn second_node_is_standby_while_leader_holds() {
        let lease = Arc::new(InMemoryLeaseStore::new());
        let a = coordinator("node-a", Arc::clone(&lease));
        let b = coordinator("node-b", Arc::clone(&lease));
        assert_eq!(a.elect_once().await.unwrap(), Role::Primary);
        assert_eq!(b.elect_once().await.unwrap(), Role::Standby);
    }

    #[tokio::test]
    async fn ensure_writable_rejects_standby() {
        let shared = ReplicationShared::enabled(Role::Standby);
        assert!(ensure_writable(&shared).is_err());
        shared.set_role(Role::Primary);
        assert!(ensure_writable(&shared).is_ok());
    }

    #[tokio::test]
    async fn auto_promotes_sole_node_to_primary() {
        let lease = Arc::new(InMemoryLeaseStore::new());
        let node = coordinator("node-a", Arc::clone(&lease));
        let shared = Arc::clone(&node.shared);
        let (stop_tx, stop_rx) = watch::channel(false);
        let handle = tokio::spawn(node.run(stop_rx));

        // Within a couple of renew intervals the sole node should win.
        let mut promoted = false;
        for _ in 0..50 {
            if shared.role() == Role::Primary {
                promoted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(promoted, "sole auto node should self-promote to primary");

        stop_tx.send(true).unwrap();
        let _ = handle.await;
    }
}
