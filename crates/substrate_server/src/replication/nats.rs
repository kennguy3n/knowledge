//! Production replication transports backed by NATS JetStream + KV.
//!
//! Only compiled under the `replication-nats` feature so the default
//! and cross-compile builds never link async-nats / rustls (see the
//! crate's `Cargo.toml`). Both transports are thin adapters over the
//! generic [`WalBus`] / [`LeaseStore`] traits:
//!
//! * [`NatsWalBus`] publishes encoded [`WalSegment`]s to a JetStream
//!   stream (`substrate-wal` by default) and serves subscribers with an
//!   **ordered** consumer that replays from the first retained segment,
//!   so a freshly attached standby catches up before tailing live
//!   traffic. The primary's frame watermark is read back via
//!   `get_last_raw_message_by_subject`.
//! * [`NatsLeaseStore`] implements a TTL lease over a NATS KV bucket
//!   using compare-and-set (`create` / `update` with the entry
//!   revision) so exactly one node holds leadership at a time, with a
//!   monotonic fencing epoch carried in the value.

use std::time::Duration;

use async_nats::jetstream::consumer::pull::OrderedConfig;
use async_nats::jetstream::consumer::DeliverPolicy;
use async_nats::jetstream::kv::{CreateErrorKind, Store, UpdateErrorKind};
use async_nats::jetstream::stream::{LastRawMessageErrorKind, Stream};
use async_nats::jetstream::Context;
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::{
    lease_expiry_ms, Lease, LeaseStore, ReplError, ReplResult, ReplicationConfig, WalBus,
    WalSegment, WalSubscription,
};

/// Map any displayable transport error into [`ReplError::Transport`].
fn transport<E: std::fmt::Display>(ctx: &str, e: E) -> ReplError {
    ReplError::Transport(format!("{ctx}: {e}"))
}

/// Establish a JetStream context and ensure the WAL stream + KV bucket
/// exist, returning both transports ready for use.
///
/// # Errors
///
/// Returns [`ReplError::Transport`] if the NATS connection, stream, or
/// KV bucket cannot be established.
pub async fn connect(config: &ReplicationConfig) -> ReplResult<(NatsWalBus, NatsLeaseStore)> {
    let url = config
        .nats_url
        .as_deref()
        .ok_or_else(|| ReplError::Transport("no NATS URL configured".to_string()))?;
    let client = async_nats::connect(url)
        .await
        .map_err(|e| transport("connecting to NATS", e))?;
    let context = async_nats::jetstream::new(client);

    let bus = NatsWalBus::ensure(context.clone(), config).await?;
    let lease = NatsLeaseStore::ensure(&context, config).await?;
    Ok((bus, lease))
}

// ────────────────────────────── WAL bus ─────────────────────────────

/// JetStream-backed [`WalBus`].
pub struct NatsWalBus {
    context: Context,
    stream: Stream,
    subject: String,
}

impl NatsWalBus {
    /// Create or attach the WAL JetStream stream.
    async fn ensure(context: Context, config: &ReplicationConfig) -> ReplResult<Self> {
        let stream = context
            .get_or_create_stream(async_nats::jetstream::stream::Config {
                name: config.stream.clone(),
                subjects: vec![config.subject.clone()],
                // File storage so shipped frames survive a NATS restart;
                // a standby attaching later can still replay history.
                storage: async_nats::jetstream::stream::StorageType::File,
                ..Default::default()
            })
            .await
            .map_err(|e| transport("creating WAL stream", e))?;
        Ok(Self {
            context,
            stream,
            subject: config.subject.clone(),
        })
    }
}

#[async_trait::async_trait]
impl WalBus for NatsWalBus {
    async fn publish(&self, segment: &WalSegment) -> ReplResult<()> {
        let payload = Bytes::from(segment.encode());
        let ack = self
            .context
            .publish(self.subject.clone(), payload)
            .await
            .map_err(|e| transport("publishing WAL segment", e))?;
        // Wait for the server's store ack so the primary's watermark
        // only advances once the frames are durably retained.
        ack.await
            .map_err(|e| transport("awaiting publish ack", e))?;
        Ok(())
    }

    async fn subscribe(&self) -> ReplResult<WalSubscription> {
        let consumer = self
            .stream
            .create_consumer(OrderedConfig {
                deliver_policy: DeliverPolicy::All,
                filter_subject: self.subject.clone(),
                ..Default::default()
            })
            .await
            .map_err(|e| transport("creating WAL consumer", e))?;

        let (tx, rx) = tokio::sync::mpsc::channel(256);
        tokio::spawn(async move {
            let mut messages = match consumer.messages().await {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(error = %e, "nats: opening WAL message stream failed");
                    return;
                }
            };
            while let Some(item) = messages.next().await {
                let msg = match item {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "nats: WAL stream delivery error");
                        continue;
                    }
                };
                match WalSegment::decode(&msg.payload) {
                    Ok(seg) => {
                        if tx.send(seg).await.is_err() {
                            return; // subscriber dropped
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "nats: undecodable WAL segment; skipping"),
                }
            }
        });
        Ok(WalSubscription::new(rx))
    }

    async fn latest_watermark(&self) -> ReplResult<u64> {
        match self
            .stream
            .get_last_raw_message_by_subject(&self.subject)
            .await
        {
            Ok(msg) => {
                let seg = WalSegment::decode(&msg.payload)
                    .map_err(|e| transport("decoding watermark segment", e))?;
                Ok(seg.cumulative_frames)
            }
            Err(e) if e.kind() == LastRawMessageErrorKind::NoMessageFound => Ok(0),
            Err(e) => Err(transport("reading WAL watermark", e)),
        }
    }
}

// ───────────────────────────── Lease store ──────────────────────────

/// The leadership record persisted as the KV value.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRecord {
    holder: String,
    epoch: u64,
    expires_at_ms: i64,
}

impl From<LeaseRecord> for Lease {
    fn from(r: LeaseRecord) -> Self {
        Lease {
            holder: r.holder,
            epoch: r.epoch,
            expires_at_ms: r.expires_at_ms,
        }
    }
}

/// The single KV key under which leadership is arbitrated.
const LEASE_KEY: &str = "leader";

/// NATS KV-backed [`LeaseStore`].
pub struct NatsLeaseStore {
    kv: Store,
}

impl NatsLeaseStore {
    /// Create or attach the leader-election KV bucket.
    async fn ensure(context: &Context, config: &ReplicationConfig) -> ReplResult<Self> {
        if let Ok(kv) = context.get_key_value(&config.kv_bucket).await {
            return Ok(Self { kv });
        }
        let kv = context
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: config.kv_bucket.clone(),
                // Only the latest leadership record matters; CAS uses the
                // entry revision for fencing, not value history.
                history: 1,
                storage: async_nats::jetstream::stream::StorageType::File,
                ..Default::default()
            })
            .await
            .map_err(|e| transport("creating leader KV bucket", e))?;
        Ok(Self { kv })
    }

    fn encode(record: &LeaseRecord) -> ReplResult<Bytes> {
        serde_json::to_vec(record)
            .map(Bytes::from)
            .map_err(|e| transport("encoding lease record", e))
    }

    fn decode(bytes: &[u8]) -> ReplResult<LeaseRecord> {
        serde_json::from_slice(bytes).map_err(|e| transport("decoding lease record", e))
    }
}

#[async_trait::async_trait]
impl LeaseStore for NatsLeaseStore {
    async fn acquire(&self, node_id: &str, ttl: Duration) -> ReplResult<Lease> {
        let now = Utc::now().timestamp_millis();
        let expires_at_ms = lease_expiry_ms(now, ttl);

        let entry = self
            .kv
            .entry(LEASE_KEY)
            .await
            .map_err(|e| transport("reading lease entry", e))?;

        match entry {
            None => {
                // Vacant: try to claim with a create (fails if a peer
                // raced us to it).
                let record = LeaseRecord {
                    holder: node_id.to_string(),
                    epoch: 1,
                    expires_at_ms,
                };
                match self.kv.create(LEASE_KEY, Self::encode(&record)?).await {
                    Ok(_) => Ok(record.into()),
                    Err(e) if e.kind() == CreateErrorKind::AlreadyExists => {
                        // Lost the create race; report whoever won.
                        self.current()
                            .await?
                            .ok_or_else(|| transport("lease", "vanished after create race"))
                    }
                    Err(e) => Err(transport("creating lease", e)),
                }
            }
            Some(entry) => {
                let current = Self::decode(&entry.value)?;
                let revision = entry.revision;
                let held_by_us = current.holder == node_id;
                let expired = current.expires_at_ms <= now;

                if !held_by_us && !expired {
                    // Someone else holds a valid lease.
                    return Ok(current.into());
                }

                // Renew (ours) or steal (expired): CAS on the revision.
                let epoch = if held_by_us {
                    current.epoch
                } else {
                    current.epoch + 1
                };
                let record = LeaseRecord {
                    holder: node_id.to_string(),
                    epoch,
                    expires_at_ms,
                };
                match self
                    .kv
                    .update(LEASE_KEY, Self::encode(&record)?, revision)
                    .await
                {
                    Ok(_) => Ok(record.into()),
                    Err(e) if e.kind() == UpdateErrorKind::WrongLastRevision => {
                        // A peer mutated the lease first; report the winner.
                        self.current()
                            .await?
                            .ok_or_else(|| transport("lease", "vanished after CAS race"))
                    }
                    Err(e) => Err(transport("updating lease", e)),
                }
            }
        }
    }

    async fn release(&self, node_id: &str) -> ReplResult<()> {
        let entry = self
            .kv
            .entry(LEASE_KEY)
            .await
            .map_err(|e| transport("reading lease entry", e))?;
        let Some(entry) = entry else {
            return Ok(());
        };
        let current = Self::decode(&entry.value)?;
        if current.holder != node_id {
            return Ok(());
        }
        // Mark immediately expired (keep the epoch) so peers can steal
        // it without waiting out the TTL.
        let record = LeaseRecord {
            holder: current.holder,
            epoch: current.epoch,
            expires_at_ms: 0,
        };
        match self
            .kv
            .update(LEASE_KEY, Self::encode(&record)?, entry.revision)
            .await
        {
            Ok(_) => Ok(()),
            // A concurrent renew/steal means we no longer hold it anyway.
            Err(e) if e.kind() == UpdateErrorKind::WrongLastRevision => Ok(()),
            Err(e) => Err(transport("releasing lease", e)),
        }
    }

    async fn current(&self) -> ReplResult<Option<Lease>> {
        let now = Utc::now().timestamp_millis();
        let entry = self
            .kv
            .entry(LEASE_KEY)
            .await
            .map_err(|e| transport("reading lease entry", e))?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        let record = Self::decode(&entry.value)?;
        if record.expires_at_ms <= now {
            Ok(None)
        } else {
            Ok(Some(record.into()))
        }
    }
}
