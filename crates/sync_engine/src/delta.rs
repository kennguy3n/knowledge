//! Wire-format delta serialisation for CRDT sync.
//!
//! A delta is the set of [`SyncOp`] entries authored or merged into
//! a sender's op log **since** the receiver's last-known
//! `(replica_id, seq)` watermark, wrapped in a [`DeltaEnvelope`]
//! carrying:
//!
//! * `compaction_epoch` — the sender's
//!   [`OpLog::compaction_epoch`] at the time of encoding. The
//!   receiver checks this against its own epoch: if the sender is
//!   one or more compactions ahead, a delta is unsafe and the
//!   receiver must bootstrap via [`SyncEngine::snapshot`] instead
//!   (the historical `Remove` ops the sender dropped during
//!   compaction may be ones the receiver still needs).
//! * `since_seq` — the watermark the delta was computed against.
//!   Carried for diagnostics and so the receiver can validate the
//!   delta was actually computed against the watermark it
//!   requested.
//!
//! [`SyncEngine::snapshot`]: crate::SyncEngine::snapshot

use std::hash::Hash;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::error::{Result, SyncError};
use crate::op_log::{OpLog, SyncOp};
use crate::SyncEngine;

/// Wire envelope for a delta: the post-watermark ops plus the
/// metadata the receiver needs to safely apply them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaEnvelope<T>
where
    T: Eq + Hash + Clone,
{
    /// Sender's [`OpLog::compaction_epoch`] at the time of
    /// encoding.
    pub compaction_epoch: u64,
    /// Sequence watermark this delta was computed against — every
    /// `op` in `ops` has `seq > since_seq` (within its own
    /// replica's stream).
    pub since_seq: u64,
    /// Post-watermark ops, in op-log order.
    pub ops: Vec<SyncOp<T>>,
}

/// Encode every op in `log` with `seq > since_seq` (taking the
/// authoring replica into account) into a [`DeltaEnvelope`]
/// byte blob.
///
/// The `since_seq` filter is applied **per replica**: this engine's
/// own ops are filtered by `since_seq`, and merged-in foreign ops
/// are included unconditionally so newly-joined peers see them.
/// This avoids the pathological "I haven't seen any of peer B's
/// ops yet, but my local clock is already at 100" case.
pub fn encode_delta_since<T>(log: &OpLog<T>, since_seq: u64) -> Result<Vec<u8>>
where
    T: Eq + Hash + Clone + Serialize,
{
    let ops: Vec<SyncOp<T>> = log
        .ops
        .iter()
        .filter(|entry| entry.replica_id != log.replica_id || entry.seq > since_seq)
        .cloned()
        .collect();
    let envelope = DeltaEnvelope {
        compaction_epoch: log.compaction_epoch,
        since_seq,
        ops,
    };
    serde_json::to_vec(&envelope)
        .map_err(|_| SyncError::Serialisation("could not serialise delta envelope"))
}

/// Encode **only** the ops authored by `log.replica_id` with
/// `seq > since_seq` into a [`DeltaEnvelope`] byte blob.
///
/// This is the relay-forwarding counterpart to
/// [`encode_delta_since`]. Where `encode_delta_since` also forwards
/// merged-in foreign ops (the right choice for direct
/// peer-to-peer exchange), this function uploads *only this
/// replica's own* ops. It is the primitive a relay-mediated client
/// uses so that every op reaches the relay **exactly once** —
/// authored by its originating replica — rather than being
/// re-uploaded by every peer that later merges it. That bound is
/// what keeps relay storage and bandwidth `O(total ops)` instead
/// of `O(total ops × replicas)`: a log-index watermark that
/// re-forwarded foreign ops would amplify every op by the replica
/// count on each hop.
///
/// The "exactly once" guarantee relies on each replica eventually
/// reaching the relay directly. A replica that only ever gossips
/// peer-to-peer (and never connects to the relay) will not have
/// its ops forwarded by this path; such a replica should sync its
/// own ops to the relay when it next has connectivity.
pub fn encode_own_delta_since<T>(log: &OpLog<T>, since_seq: u64) -> Result<Vec<u8>>
where
    T: Eq + Hash + Clone + Serialize,
{
    let ops: Vec<SyncOp<T>> = log
        .ops
        .iter()
        .filter(|entry| entry.replica_id == log.replica_id && entry.seq > since_seq)
        .cloned()
        .collect();
    let envelope = DeltaEnvelope {
        compaction_epoch: log.compaction_epoch,
        since_seq,
        ops,
    };
    serde_json::to_vec(&envelope)
        .map_err(|_| SyncError::Serialisation("could not serialise delta envelope"))
}

/// Decode a byte blob produced by [`encode_delta_since`] back into
/// a [`DeltaEnvelope`].
pub fn decode_delta<T>(bytes: &[u8]) -> Result<DeltaEnvelope<T>>
where
    T: Eq + Hash + Clone + DeserializeOwned,
{
    serde_json::from_slice(bytes).map_err(|_| SyncError::DeltaDecode)
}

/// Apply a `delta` byte blob to `engine`.
///
/// Returns [`SyncError::CompactionEpochBehind`] if the delta was
/// authored at a higher [`OpLog::compaction_epoch`] than the
/// receiver — the receiver must then bootstrap from a snapshot
/// before further delta sync.
///
/// On success, every newly-absorbed op is reflected in the engine's
/// materialised-state cache so the next [`SyncEngine::state`] call
/// is O(1).
pub fn apply_delta<T>(engine: &mut SyncEngine<T>, delta: &[u8]) -> Result<usize>
where
    T: Eq + Hash + Clone + Serialize + DeserializeOwned,
{
    let envelope: DeltaEnvelope<T> = decode_delta(delta)?;

    if envelope.compaction_epoch > engine.op_log().compaction_epoch {
        return Err(SyncError::CompactionEpochBehind {
            local: engine.op_log().compaction_epoch,
            delta: envelope.compaction_epoch,
        });
    }

    // We cannot use `op_log_mut()` because that would invalidate
    // the engine's materialised-state cache (per
    // `SyncEngine::op_log_mut`'s contract). Instead, we drive the
    // append through a dedicated `merge_delta_envelope` helper on
    // the engine that mirrors `SyncEngine::merge`'s incremental
    // cache-extension path.
    Ok(engine.merge_delta_envelope(envelope))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::SyncEngine;

    #[test]
    fn delta_round_trip_reproduces_state() {
        let mut sender = SyncEngine::<String>::new();
        sender.add("a".into());
        sender.add("b".into());
        sender.remove("a".into());

        let delta = encode_delta_since(sender.op_log(), 0).unwrap();
        let mut receiver = SyncEngine::<String>::new();
        let absorbed = apply_delta(&mut receiver, &delta).unwrap();
        assert!(absorbed > 0);

        let (state, _) = receiver.state().unwrap();
        assert!(!state.contains(&"a".to_string()));
        assert!(state.contains(&"b".to_string()));
    }

    #[test]
    fn delta_since_skips_already_seen_local_ops() {
        let mut e = SyncEngine::<String>::new();
        e.add("a".into());
        e.add("b".into());

        let last_seq = e.op_log().clock;
        e.add("c".into());

        let delta = encode_delta_since(e.op_log(), last_seq).unwrap();
        let env: DeltaEnvelope<String> = decode_delta(&delta).unwrap();
        // Only the third add (`c`) should be in the post-watermark
        // window — and only from the local replica's stream.
        let local_ops: Vec<_> = env
            .ops
            .iter()
            .filter(|o| o.replica_id == e.replica_id())
            .collect();
        assert_eq!(local_ops.len(), 1);
    }

    #[test]
    fn delta_rejects_when_sender_is_ahead_one_compaction() {
        let mut sender = SyncEngine::<String>::new();
        sender.add("a".into());
        sender.compact().unwrap();
        // Sender's epoch is now 1, receiver's is still 0.
        let delta = encode_delta_since(sender.op_log(), 0).unwrap();

        let mut receiver = SyncEngine::<String>::new();
        let err = apply_delta(&mut receiver, &delta).unwrap_err();
        assert!(
            matches!(err, SyncError::CompactionEpochBehind { local: 0, delta: 1 }),
            "expected CompactionEpochBehind {{ local: 0, delta: 1 }}, got {err:?}",
        );
    }
}
