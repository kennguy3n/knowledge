//! Blob storage for the relay: an append-only log of opaque
//! [`SealedDelta`]s per `(tenant, topic)`.
//!
//! The store is the heart of the "untrusted relay" guarantee. Its
//! entire vocabulary is *append opaque bytes* and *read opaque bytes
//! past a cursor*. It has no notion of CRDT ops, scopes, replicas, or
//! plaintext — so a correct implementation **cannot** decrypt or
//! resolve anything even if it wanted to.

use std::collections::HashMap;
use std::sync::RwLock;

use sync_engine::transport::{PullPage, SealedDelta, TopicId};

use crate::auth::TenantId;
use crate::error::RelayError;

/// Storage limits that bound a single relay's memory and the blast
/// radius of a hostile client.
#[derive(Debug, Clone, Copy)]
pub struct StoreLimits {
    /// Maximum number of blobs retained per `(tenant, topic)`. A push
    /// that would exceed this is rejected (the relay never silently
    /// drops earlier blobs, which would break cursor monotonicity).
    pub max_blobs_per_topic: usize,
    /// Maximum size of a single sealed delta, in bytes.
    pub max_blob_bytes: usize,
}

impl Default for StoreLimits {
    fn default() -> Self {
        // Generous defaults sized for the in-memory reference store.
        // A production deployment backs the relay with durable
        // storage and tunes these per tenant tier.
        Self {
            max_blobs_per_topic: 1_000_000,
            max_blob_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Append-only, cursor-addressed store of opaque sealed deltas.
///
/// Implementations are **untrusted**: they observe only ciphertext +
/// nonce (inside [`SealedDelta`]), the [`TenantId`] namespace, and the
/// [`TopicId`] routing key. They never see plaintext, the master key,
/// or the scope identity.
pub trait BlobStore: Send + Sync + 'static {
    /// Append `blobs` to `(tenant, topic)` in order and return the
    /// topic's new high-water cursor (offset after the last blob).
    ///
    /// # Errors
    ///
    /// Returns [`RelayError::QuotaExceeded`] / [`RelayError::BlobTooLarge`]
    /// if a configured limit would be violated.
    fn append(
        &self,
        tenant: &TenantId,
        topic: &TopicId,
        blobs: &[SealedDelta],
    ) -> Result<u64, RelayError>;

    /// Read every blob for `(tenant, topic)` with offset `> since`,
    /// plus the new high-water cursor.
    fn read_since(
        &self,
        tenant: &TenantId,
        topic: &TopicId,
        since: u64,
    ) -> Result<PullPage, RelayError>;
}

/// In-memory reference [`BlobStore`].
///
/// Backs the relay binary's default deployment and the integration
/// tests. Storage is a `RwLock<HashMap<(tenant, topic), Vec<blob>>>`:
/// reads (pull) take the shared lock, appends (push) take the
/// exclusive lock. For a single-process relay fronting a user's
/// devices this is more than fast enough; a multi-node production
/// relay would implement [`BlobStore`] over a durable, replicated log
/// without touching the rest of the crate.
#[derive(Debug)]
pub struct InMemoryBlobStore {
    limits: StoreLimits,
    topics: RwLock<HashMap<(TenantId, TopicId), Vec<SealedDelta>>>,
}

impl InMemoryBlobStore {
    /// Build a store with the given limits.
    pub fn new(limits: StoreLimits) -> Self {
        Self {
            limits,
            topics: RwLock::new(HashMap::new()),
        }
    }

    /// Total number of blobs currently held for `(tenant, topic)`.
    /// Test/operability helper.
    pub fn blob_count(&self, tenant: &TenantId, topic: &TopicId) -> usize {
        self.topics
            .read()
            .expect("blob store lock poisoned")
            .get(&(tenant.clone(), *topic))
            .map_or(0, Vec::len)
    }

    /// Snapshot the raw stored blobs for `(tenant, topic)`.
    ///
    /// Exposed so tests can assert the relay holds only opaque
    /// ciphertext — i.e. that no plaintext element bytes ever land in
    /// the store-and-forward buffer.
    pub fn raw_blobs(&self, tenant: &TenantId, topic: &TopicId) -> Vec<SealedDelta> {
        self.topics
            .read()
            .expect("blob store lock poisoned")
            .get(&(tenant.clone(), *topic))
            .cloned()
            .unwrap_or_default()
    }
}

impl Default for InMemoryBlobStore {
    fn default() -> Self {
        Self::new(StoreLimits::default())
    }
}

impl BlobStore for InMemoryBlobStore {
    fn append(
        &self,
        tenant: &TenantId,
        topic: &TopicId,
        blobs: &[SealedDelta],
    ) -> Result<u64, RelayError> {
        for blob in blobs {
            if blob.ciphertext.len() > self.limits.max_blob_bytes {
                return Err(RelayError::BlobTooLarge {
                    size: blob.ciphertext.len(),
                    limit: self.limits.max_blob_bytes,
                });
            }
        }

        let mut topics = self.topics.write().expect("blob store lock poisoned");
        let entry = topics.entry((tenant.clone(), *topic)).or_default();
        if entry.len().saturating_add(blobs.len()) > self.limits.max_blobs_per_topic {
            return Err(RelayError::QuotaExceeded {
                limit: self.limits.max_blobs_per_topic,
            });
        }
        entry.extend_from_slice(blobs);
        Ok(u64::try_from(entry.len()).unwrap_or(u64::MAX))
    }

    fn read_since(
        &self,
        tenant: &TenantId,
        topic: &TopicId,
        since: u64,
    ) -> Result<PullPage, RelayError> {
        let topics = self.topics.read().expect("blob store lock poisoned");
        let all = topics.get(&(tenant.clone(), *topic));
        let len = all.map_or(0, Vec::len);
        let start = usize::try_from(since).unwrap_or(usize::MAX).min(len);
        let blobs = all.map_or_else(Vec::new, |v| v[start..].to_vec());
        Ok(PullPage {
            next_cursor: u64::try_from(len).unwrap_or(u64::MAX),
            blobs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_engine::transport::SealedDelta;

    fn blob(byte: u8, len: usize) -> SealedDelta {
        SealedDelta {
            nonce: [byte; 24],
            ciphertext: vec![byte; len],
        }
    }

    fn topic() -> TopicId {
        TopicId::from_hex(&"ab".repeat(32)).unwrap()
    }

    #[test]
    fn append_and_read_round_trip() {
        let store = InMemoryBlobStore::default();
        let t = TenantId::new("tenant-1");
        let top = topic();

        assert_eq!(
            store.append(&t, &top, &[blob(1, 4), blob(2, 4)]).unwrap(),
            2
        );
        let page = store.read_since(&t, &top, 0).unwrap();
        assert_eq!(page.next_cursor, 2);
        assert_eq!(page.blobs.len(), 2);

        // Reading past the consumed cursor returns only the tail.
        let page = store.read_since(&t, &top, 1).unwrap();
        assert_eq!(page.blobs.len(), 1);
        assert_eq!(page.blobs[0].nonce[0], 2);
    }

    #[test]
    fn tenants_are_isolated() {
        let store = InMemoryBlobStore::default();
        let top = topic();
        let a = TenantId::new("a");
        let b = TenantId::new("b");
        store.append(&a, &top, &[blob(1, 4)]).unwrap();
        // Same topic, different tenant: B sees nothing.
        assert_eq!(store.read_since(&b, &top, 0).unwrap().blobs.len(), 0);
        assert_eq!(store.read_since(&a, &top, 0).unwrap().blobs.len(), 1);
    }

    #[test]
    fn quota_and_size_limits_reject() {
        let store = InMemoryBlobStore::new(StoreLimits {
            max_blobs_per_topic: 2,
            max_blob_bytes: 8,
        });
        let t = TenantId::new("t");
        let top = topic();
        assert!(matches!(
            store.append(&t, &top, &[blob(1, 9)]),
            Err(RelayError::BlobTooLarge { .. })
        ));
        store.append(&t, &top, &[blob(1, 4), blob(2, 4)]).unwrap();
        assert!(matches!(
            store.append(&t, &top, &[blob(3, 4)]),
            Err(RelayError::QuotaExceeded { .. })
        ));
    }
}
