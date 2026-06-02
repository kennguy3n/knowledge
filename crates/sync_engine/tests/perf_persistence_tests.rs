//! Integration tests for the performance + persistence enhancements
//! to the sync engine: cached materialised state, log compaction,
//! SQLCipher persistence, delta wire serialisation, and snapshot
//! checkpointing.
//!
//! These tests deliberately exercise the *real* code paths
//! end-to-end — no stubs, no scaffolding — per the project's
//! testing rules (see `CONTRIBUTING.md` + the user-authored
//! "Knowledge: real implementation" rule).

use std::time::Instant;

use sync_engine::delta::{apply_delta, decode_delta, encode_delta_since, DeltaEnvelope};
use sync_engine::persist::PersistentSyncEngine;
use sync_engine::{SyncEngine, SyncError, SyncScopeId};
use tempfile::tempdir;
use uuid::Uuid;

/// Build a deterministic test master key. `MasterKey` is the
/// type alias `[u8; MASTER_KEY_LEN]`.
fn test_master_key() -> crypto::MasterKey {
    let mut k: crypto::MasterKey = [0u8; crypto::MASTER_KEY_LEN];
    for (i, slot) in k.iter_mut().enumerate() {
        // `i` is bounded by `MASTER_KEY_LEN` (32) so masking to a
        // byte never truncates the meaningful bits.
        #[allow(clippy::cast_possible_truncation,
            reason = "deterministic test key seed; i < MASTER_KEY_LEN < 256"
        )]
        let byte = (i & 0xFF) as u8;
        *slot = byte.wrapping_mul(11).wrapping_add(31);
    }
    k
}

#[test]
fn cached_state_is_orders_of_magnitude_faster_than_replay() {
    // Build a 10k-op log: 5000 adds + 5000 removes (so the live
    // set has 0 entries but the op log is sizable).
    let mut engine: SyncEngine<u64> = SyncEngine::new();
    let n: u64 = 10_000;
    for i in 0..n / 2 {
        engine.add(i);
    }
    for i in 0..n / 2 {
        engine.remove(i);
    }
    assert_eq!(engine.op_log().ops.len() as u64, n);

    // Warm up: the first state() call extends/builds the cache.
    let _ = engine.state().unwrap();

    // Measure cache-hit `state()` cost across many calls.
    let iters = 200u32;
    let start = Instant::now();
    for _ in 0..iters {
        let _ = engine.state().unwrap();
    }
    let cached_avg = start.elapsed() / iters;

    // Measure full-replay cost for the same log via the underlying
    // OpLog. This is what `state()` used to cost on every call
    // before caching was introduced.
    let start = Instant::now();
    for _ in 0..iters {
        let _ = engine.op_log().replay().unwrap();
    }
    let replay_avg = start.elapsed() / iters;

    println!("cached_avg={:?}  replay_avg={:?}  ratio={:.2}x",
        cached_avg,
        replay_avg,
        replay_avg.as_secs_f64() / cached_avg.as_secs_f64().max(1e-9),
    );

    // Replay must be measurably more expensive than the cached
    // path for a 10k-op log. We assert a conservative 5x ratio so
    // CI noise doesn't flake; in practice the ratio is far higher
    // because the cache is O(live elements) while replay is
    // O(total ops).
    assert!(cached_avg.as_secs_f64() * 5.0 < replay_avg.as_secs_f64(),
        "cached state() ({cached_avg:?}) should be \u{2265}5\u{00d7} faster than a full replay ({replay_avg:?})"
    );
}

#[test]
fn compaction_preserves_state_and_shortens_log() {
    let mut engine: SyncEngine<String> = SyncEngine::new();
    for i in 0..1000 {
        engine.add(format!("v{i}"));
    }
    for i in 0..500 {
        engine.remove(format!("v{i}"));
    }
    let pre_log_len = engine.op_log().ops.len();
    assert_eq!(pre_log_len, 1500);

    let (pre_state, pre_supers) = engine.state().unwrap();
    let pre_epoch = engine.compaction_epoch();

    let removed = engine.compact().unwrap();
    assert!(removed > 0, "compaction removed {removed} ops");

    let post_log_len = engine.op_log().ops.len();
    assert!(post_log_len < pre_log_len,
        "log should shrink: pre={pre_log_len}, post={post_log_len}"
    );

    let (post_state, post_supers) = engine.state().unwrap();
    assert_eq!(post_state.elements_count(), pre_state.elements_count());
    for value in pre_state.elements() {
        assert!(post_state.contains(value), "{value} disappeared");
    }
    assert_eq!(post_supers.len(), pre_supers.len());
    assert_eq!(engine.compaction_epoch(), pre_epoch + 1);

    // Continued operation after compaction works.
    engine.add("post_compact".to_string());
    let (state, _) = engine.state().unwrap();
    assert!(state.contains(&"post_compact".to_string()));
}

#[test]
fn delta_round_trip_full_history() {
    let mut sender: SyncEngine<String> = SyncEngine::new();
    sender.add("alpha".into());
    sender.add("beta".into());
    sender.add("gamma".into());
    sender.remove("alpha".into());
    sender.supersede("beta".into(), "beta_v2".into());

    let delta = encode_delta_since(sender.op_log(), 0).unwrap();
    let env: DeltaEnvelope<String> = decode_delta(&delta).unwrap();
    assert!(!env.ops.is_empty());
    assert_eq!(env.compaction_epoch, 0);
    assert_eq!(env.since_seq, 0);

    let mut receiver: SyncEngine<String> = SyncEngine::new();
    let absorbed = apply_delta(&mut receiver, &delta).unwrap();
    assert_eq!(absorbed, sender.op_log().ops.len());

    let (sender_state, sender_supers) = sender.state().unwrap();
    let (receiver_state, receiver_supers) = receiver.state().unwrap();
    assert_eq!(sender_state.elements_count(),
        receiver_state.elements_count()
    );
    for v in sender_state.elements() {
        assert!(receiver_state.contains(v));
    }
    assert_eq!(sender_supers, receiver_supers);
}

#[test]
fn delta_round_trip_incremental_after_partial_sync() {
    let mut sender: SyncEngine<String> = SyncEngine::new();
    sender.add("a".into());
    sender.add("b".into());

    // Receiver pulls everything so far.
    let initial = encode_delta_since(sender.op_log(), 0).unwrap();
    let mut receiver: SyncEngine<String> = SyncEngine::new();
    apply_delta(&mut receiver, &initial).unwrap();

    // Receiver remembers the highest seq it has from `sender`.
    let watermark = sender.op_log().clock;

    // Sender keeps going.
    sender.add("c".into());
    sender.remove("a".into());

    // Receiver pulls the post-watermark delta only.
    let incremental = encode_delta_since(sender.op_log(), watermark).unwrap();
    let env: DeltaEnvelope<String> = decode_delta(&incremental).unwrap();
    // The two new ops authored after the watermark.
    let from_sender: Vec<_> = env
        .ops
        .iter()
        .filter(|o| o.replica_id == sender.replica_id())
        .collect();
    assert_eq!(from_sender.len(), 2);

    apply_delta(&mut receiver, &incremental).unwrap();
    let (state, _) = receiver.state().unwrap();
    assert!(state.contains(&"b".to_string()));
    assert!(state.contains(&"c".to_string()));
    assert!(!state.contains(&"a".to_string()));
}

#[test]
fn delta_rejected_when_sender_is_post_compaction_and_receiver_is_not() {
    let mut sender: SyncEngine<String> = SyncEngine::new();
    sender.add("a".into());
    sender.add("b".into());
    sender.compact().unwrap();

    let delta = encode_delta_since(sender.op_log(), 0).unwrap();
    let mut receiver: SyncEngine<String> = SyncEngine::new();
    let err = apply_delta(&mut receiver, &delta).unwrap_err();
    assert!(matches!(err, SyncError::CompactionEpochBehind { local: 0, delta: 1 }),
        "expected CompactionEpochBehind, got {err:?}"
    );

    // After bootstrapping via a snapshot, the receiver's epoch
    // catches up and the delta is now applicable.
    let snap = sender.snapshot().unwrap();
    let mut receiver = SyncEngine::<String>::restore_snapshot(&snap).unwrap();
    assert_eq!(receiver.compaction_epoch(), 1);
    // Delta now matches the local epoch — applying it is a no-op
    // (snapshot already absorbed the live state) but does not
    // error.
    let absorbed = apply_delta(&mut receiver, &delta).unwrap();
    // The snapshot rehydrated as Add-only ops that share the same
    // tag UUIDs as the sender, so the delta's Adds dedupe.
    assert_eq!(absorbed, 0);
}

#[test]
fn bootstrap_from_snapshot_keeps_receiver_replica_id_independent() {
    // A new peer joining the cluster MUST keep its own
    // `replica_id` after bootstrapping from another replica's
    // snapshot — otherwise its local writes would be attributed
    // to the original author and silently corrupt the cluster's
    // `(replica_id, seq)` dedup table.
    let mut author: SyncEngine<String> = SyncEngine::new();
    author.add("hello".into());
    author.add("world".into());
    author.supersede("hello".into(), "hello_v2".into());
    let author_id = author.replica_id();
    let snap = author.snapshot().unwrap();

    let mut receiver = SyncEngine::<String>::bootstrap_from_snapshot(&snap).unwrap();
    let receiver_id = receiver.replica_id();
    assert_ne!(receiver_id, author_id,
        "bootstrap_from_snapshot must NOT inherit the author's replica_id"
    );

    // Receiver sees the author's materialised state.
    let (state, supers) = receiver.state().unwrap();
    assert!(state.contains(&"world".to_string()));
    assert!(!state.contains(&"hello".to_string()));
    assert_eq!(supers, vec![("hello".to_string(), "hello_v2".to_string())]);

    // Receiver's local writes are authored under its own id with a
    // fresh seq stream (starting from 1, since clock starts at 0
    // and `record_add` bumps it before stamping).
    receiver.add("from_receiver".into());
    let last_local = receiver
        .op_log()
        .ops
        .iter()
        .rfind(|o| o.replica_id == receiver_id)
        .expect("receiver authored at least one op");
    assert_eq!(last_local.replica_id, receiver_id);
    assert_eq!(last_local.seq, 1);

    // Subsequent delta sync from the author dedupes against the
    // snapshot ops (which still carry the author's `replica_id`).
    let delta = encode_delta_since(author.op_log(), 0).unwrap();
    let absorbed = apply_delta(&mut receiver, &delta).unwrap();
    assert_eq!(absorbed, 0, "author's pre-snapshot ops must dedupe");
}

#[test]
fn snapshot_restore_round_trip_preserves_state_and_epoch() {
    let mut engine: SyncEngine<String> = SyncEngine::new();
    engine.add("foo".into());
    engine.add("bar".into());
    engine.supersede("foo".into(), "foo_v2".into());
    engine.compact().unwrap();
    let pre_epoch = engine.compaction_epoch();
    let (pre_state, pre_supers) = engine.state().unwrap();

    let bytes = engine.snapshot().unwrap();
    let restored = SyncEngine::<String>::restore_snapshot(&bytes).unwrap();

    let (post_state, post_supers) = restored.state().unwrap();
    assert_eq!(restored.replica_id(), engine.replica_id());
    assert_eq!(restored.compaction_epoch(), pre_epoch);
    assert_eq!(pre_state.elements_count(), post_state.elements_count());
    for v in pre_state.elements() {
        assert!(post_state.contains(v));
    }
    assert_eq!(pre_supers, post_supers);
}

#[test]
fn persistence_write_close_reopen_round_trip() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sync.sqlite");
    let scope = SyncScopeId::new_v4();
    let replica = Uuid::new_v4();
    let mk = test_master_key();

    let expected_live: Vec<String>;
    let expected_supers: Vec<(String, String)>;
    {
        let mut p = PersistentSyncEngine::<String>::open(&db_path, scope, replica, &mk).unwrap();
        for i in 0..50 {
            p.add(format!("k{i}")).unwrap();
        }
        for i in 0..20 {
            p.remove(format!("k{i}")).unwrap();
        }
        p.supersede("k49".into(), "k49_v2".into()).unwrap();

        let (state, supers) = p.engine().state().unwrap();
        expected_live = (20..49).map(|i| format!("k{i}")).collect();
        expected_supers = supers;
        assert_eq!(state.elements_count(), expected_live.len());
    }

    let p2 = PersistentSyncEngine::<String>::open(&db_path, scope, replica, &mk).unwrap();
    let (state, supers) = p2.engine().state().unwrap();
    assert_eq!(state.elements_count(), expected_live.len());
    for v in &expected_live {
        assert!(state.contains(v), "missing {v} after reopen");
    }
    assert_eq!(supers, expected_supers);
}

#[test]
fn persistence_compact_persists_compacted_log() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sync.sqlite");
    let scope = SyncScopeId::new_v4();
    let replica = Uuid::new_v4();
    let mk = test_master_key();

    let post_compact_live;
    let post_compact_log_len;
    {
        let mut p = PersistentSyncEngine::<String>::open(&db_path, scope, replica, &mk).unwrap();
        for i in 0..100 {
            p.add(format!("v{i}")).unwrap();
        }
        for i in 0..50 {
            p.remove(format!("v{i}")).unwrap();
        }
        assert_eq!(p.persisted_len().unwrap(), 150);
        p.compact().unwrap();
        post_compact_log_len = p.persisted_len().unwrap();
        post_compact_live = p
            .engine()
            .state()
            .unwrap()
            .0
            .elements()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        assert!(post_compact_log_len < 150);
        assert_eq!(p.engine().compaction_epoch(), 1);
    }

    let p2 = PersistentSyncEngine::<String>::open(&db_path, scope, replica, &mk).unwrap();
    assert_eq!(p2.persisted_len().unwrap(), post_compact_log_len);
    assert_eq!(p2.engine().compaction_epoch(), 1);
    let (state, _) = p2.engine().state().unwrap();
    let live: std::collections::HashSet<_> = state.elements().cloned().collect();
    assert_eq!(live, post_compact_live);
}

#[test]
fn persistence_two_scopes_share_one_db_independently() {
    // Two different scopes opened against the same file must not
    // see each other's ops — the per-row `scope_id` plus the
    // per-scope AEAD key keep them isolated cryptographically and
    // at the schema level.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sync.sqlite");
    let scope_a = SyncScopeId::new_v4();
    let scope_b = SyncScopeId::new_v4();
    let replica = Uuid::new_v4();
    let mk = test_master_key();

    {
        let mut a = PersistentSyncEngine::<String>::open(&db_path, scope_a, replica, &mk).unwrap();
        a.add("from_a".into()).unwrap();
    }
    {
        let mut b = PersistentSyncEngine::<String>::open(&db_path, scope_b, replica, &mk).unwrap();
        b.add("from_b".into()).unwrap();
    }

    let a2 = PersistentSyncEngine::<String>::open(&db_path, scope_a, replica, &mk).unwrap();
    let b2 = PersistentSyncEngine::<String>::open(&db_path, scope_b, replica, &mk).unwrap();
    let (state_a, _) = a2.engine().state().unwrap();
    let (state_b, _) = b2.engine().state().unwrap();
    assert!(state_a.contains(&"from_a".to_string()));
    assert!(!state_a.contains(&"from_b".to_string()));
    assert!(state_b.contains(&"from_b".to_string()));
    assert!(!state_b.contains(&"from_a".to_string()));
}

#[test]
fn remove_on_unknown_value_is_a_full_no_op_including_persistence() {
    // Defensive `remove()` of a value the local replica has never
    // seen must not append a Remove op to the log and must not
    // persist a row to the on-disk table — a Remove op with empty
    // `observed_tags` is a no-op on every receiver, so emitting it
    // would only inflate the log + disk without carrying any
    // information.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sync.sqlite");
    let scope = SyncScopeId::new_v4();
    let replica = Uuid::new_v4();
    let mk = test_master_key();

    let mut p = PersistentSyncEngine::<String>::open(&db_path, scope, replica, &mk).unwrap();

    // Add one real value so the log + on-disk table are not empty
    // — this guarantees we are testing the "no-op remove leaves
    // length unchanged" property rather than "everything is empty".
    p.add("present".to_string()).unwrap();
    let len_after_add = p.engine().op_log().ops.len();
    let persisted_after_add = p.persisted_len().unwrap();
    assert_eq!(len_after_add, 1);
    assert_eq!(persisted_after_add, 1);

    // Defensive remove of values that were never added.
    for never_added in ["absent_a", "absent_b", "absent_c"] {
        p.remove(never_added.to_string()).unwrap();
    }

    // Log length must be exactly what it was after the single add.
    assert_eq!(p.engine().op_log().ops.len(),
        len_after_add,
        "no-op remove must not append to the in-memory op log",
    );
    // On-disk row count must be exactly what it was after the
    // single add.
    assert_eq!(p.persisted_len().unwrap(),
        persisted_after_add,
        "no-op remove must not write any rows to sync_ops",
    );

    // Removing the present value still works and is reflected in
    // both the log and the persisted table.
    p.remove("present".to_string()).unwrap();
    assert_eq!(p.engine().op_log().ops.len(), len_after_add + 1);
    assert_eq!(p.persisted_len().unwrap(), persisted_after_add + 1);

    // A second remove of the now-absent value is a no-op again.
    p.remove("present".to_string()).unwrap();
    assert_eq!(p.engine().op_log().ops.len(), len_after_add + 1);
    assert_eq!(p.persisted_len().unwrap(), persisted_after_add + 1);
}

#[test]
fn op_log_serialised_form_does_not_carry_seen_index() {
    // The `seen: HashSet<(replica_id, seq)>` dedup index inside
    // OpLog is a redundant projection of `ops` — every entry
    // corresponds to one element of `ops`. Including it in the
    // serialised payload would double the `(replica_id, seq)` byte
    // cost on the wire (snapshots and delta payloads) for zero
    // information gain.
    //
    // This regression test pins two properties:
    //   (1) the JSON-serialised snapshot form does not contain a
    //       `"seen"` key (would otherwise duplicate every
    //       `(replica_id, seq)` pair on the wire)
    //   (2) the rehydrated dedup index is correctly rebuilt from
    //       `ops` on deserialise — re-applying the same delta to
    //       a receiver bootstrapped from the snapshot is a no-op.
    let mut engine: SyncEngine<u64> = SyncEngine::new();
    for i in 0..50_u64 {
        engine.add(i);
    }
    let snapshot_bytes = engine.snapshot().unwrap();
    let text = std::str::from_utf8(&snapshot_bytes).unwrap();
    assert!(!text.contains("\"seen\""),
        "OpLog serialised snapshot form must not include the redundant `seen` dedup index",
    );

    // Encode the delta the snapshot author would send.
    let delta_bytes = encode_delta_since(engine.op_log(), 0).unwrap();

    // Receiver bootstraps from the snapshot — already absorbs
    // every op the author included.
    let mut receiver: SyncEngine<u64> =
        SyncEngine::bootstrap_from_snapshot(&snapshot_bytes).unwrap();
    let before = receiver.op_log().ops.len();
    assert_eq!(before, 50,
        "receiver must observe every op the snapshot carried"
    );

    // Applying the same delta must be a no-op iff the rehydrated
    // `seen` index actually dedupes — which is the property the
    // `#[serde(skip)]` + `From<OpLogOnDisk>` shadow-type pattern
    // provides.
    apply_delta(&mut receiver, &delta_bytes).unwrap();
    apply_delta(&mut receiver, &delta_bytes).unwrap();
    assert_eq!(receiver.op_log().ops.len(),
        before,
        "rehydrated `seen` index must dedupe re-applied delta ops",
    );
}
