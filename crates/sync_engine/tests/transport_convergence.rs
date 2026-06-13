//! Multi-replica convergence over the [`SyncTransport`] abstraction.
//!
//! These exercise the client push/pull API and the in-process
//! [`InMemoryTransport`] — fast, deterministic, no network — as the
//! lower tier of the sync test pyramid. The `sync_relay` crate's
//! `three_replica_relay` test repeats the same scenarios over a real
//! authenticated HTTP relay.
//!
//! Covered invariants:
//! * ≥3 replicas converge to identical state through the relay;
//! * convergence is order-independent (deterministic merge);
//! * add-wins beats a concurrent remove;
//! * supersession propagates;
//! * an offline/partitioned replica catches up on reconnect;
//! * the transport only ever holds opaque ciphertext.

use std::sync::Arc;

use sync_engine::transport::{InMemoryTransport, SyncClient};
use sync_engine::{MasterKey, SyncEngine, SyncScopeId};

const MASTER_KEY: MasterKey = [0x42; 32];

/// A replica = an engine + the client that syncs it.
struct Replica {
    engine: SyncEngine<String>,
    client: SyncClient,
}

impl Replica {
    fn new(scope: SyncScopeId) -> Self {
        Self {
            engine: SyncEngine::new(),
            client: SyncClient::new(&MASTER_KEY, scope).expect("derive client"),
        }
    }

    fn sync(&mut self, transport: &InMemoryTransport) {
        self.client
            .sync(&mut self.engine, transport)
            .expect("sync round trip");
    }

    fn live(&self) -> Vec<String> {
        let (set, _) = self.engine.state().expect("state");
        let mut v: Vec<String> = set.elements().cloned().collect();
        v.sort();
        v
    }
}

/// Drive a set of replicas to quiescence: repeat push+pull until no
/// replica absorbs or pushes anything new. Bounded so a convergence
/// bug surfaces as a test failure rather than a hang.
fn drive_to_quiescence(replicas: &mut [Replica], transport: &InMemoryTransport) {
    for _ in 0..16 {
        let mut changed = false;
        for r in replicas.iter_mut() {
            let outcome = r
                .client
                .sync(&mut r.engine, transport)
                .expect("sync round trip");
            if outcome.pushed > 0 || outcome.absorbed > 0 {
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
    panic!("replicas did not converge within the round budget");
}

#[test]
fn three_replicas_converge_through_relay() {
    let scope = SyncScopeId::new_v4();
    let transport = InMemoryTransport::new();
    let mut replicas = vec![
        Replica::new(scope),
        Replica::new(scope),
        Replica::new(scope),
    ];

    replicas[0].engine.add("alpha".into());
    replicas[1].engine.add("bravo".into());
    replicas[2].engine.add("charlie".into());

    drive_to_quiescence(&mut replicas, &transport);

    let expected = vec!["alpha".to_string(), "bravo".into(), "charlie".into()];
    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), expected, "replica {i} did not converge");
    }
}

#[test]
fn offline_replica_catches_up_on_reconnect() {
    let scope = SyncScopeId::new_v4();
    let transport = InMemoryTransport::new();
    let mut a = Replica::new(scope);
    let mut b = Replica::new(scope);
    let mut c = Replica::new(scope);

    // All three start in sync with a shared element.
    a.engine.add("shared".into());
    a.sync(&transport);
    b.sync(&transport);
    c.sync(&transport);
    assert_eq!(b.live(), vec!["shared".to_string()]);
    assert_eq!(c.live(), vec!["shared".to_string()]);

    // C goes offline. A and B keep working and syncing.
    a.engine.add("a-while-c-offline".into());
    b.engine.add("b-while-c-offline".into());
    a.sync(&transport);
    b.sync(&transport);
    a.sync(&transport); // pull B's op
    b.sync(&transport); // pull A's op

    // A and B have converged; C is still behind.
    let ab_expected = vec![
        "a-while-c-offline".to_string(),
        "b-while-c-offline".into(),
        "shared".into(),
    ];
    assert_eq!(a.live(), ab_expected);
    assert_eq!(b.live(), ab_expected);
    assert_eq!(
        c.live(),
        vec!["shared".to_string()],
        "C must still be stale"
    );

    // C reconnects and authors its own op, then syncs.
    c.engine.add("c-back-online".into());
    c.sync(&transport);
    // Everyone reconciles.
    let mut replicas = vec![a, b, c];
    drive_to_quiescence(&mut replicas, &transport);

    let final_expected = vec![
        "a-while-c-offline".to_string(),
        "b-while-c-offline".into(),
        "c-back-online".into(),
        "shared".into(),
    ];
    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), final_expected, "replica {i} post-reconnect");
    }
}

#[test]
fn add_wins_over_concurrent_remove_through_relay() {
    let scope = SyncScopeId::new_v4();
    let transport = InMemoryTransport::new();
    let mut a = Replica::new(scope);
    let mut b = Replica::new(scope);

    // Both observe "x".
    a.engine.add("x".into());
    a.sync(&transport);
    b.sync(&transport);
    assert_eq!(b.live(), vec!["x".to_string()]);

    // Partition: A removes the "x" it observed; B concurrently
    // re-adds "x" with a fresh tag. Neither has seen the other's op.
    a.engine.remove("x".into());
    b.engine.add("x".into());

    // Heal: exchange through the relay until quiescent.
    let mut replicas = vec![a, b];
    drive_to_quiescence(&mut replicas, &transport);

    // Add-wins: the concurrently-added "x" survives the remove.
    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), vec!["x".to_string()], "add-wins on replica {i}");
    }
}

#[test]
fn supersession_propagates_through_relay() {
    let scope = SyncScopeId::new_v4();
    let transport = InMemoryTransport::new();
    let mut a = Replica::new(scope);
    let b = Replica::new(scope);
    let c = Replica::new(scope);

    a.engine.add("v1".into());
    a.engine.supersede("v1".into(), "v2".into());
    a.engine.add("v2".into());

    let mut replicas = vec![a, b, c];
    drive_to_quiescence(&mut replicas, &transport);

    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), vec!["v2".to_string()], "replica {i} live set");
        let (_, supers) = r.engine.state().expect("state");
        assert!(
            supers.contains(&("v1".to_string(), "v2".to_string())),
            "supersession record missing on replica {i}"
        );
    }
}

#[test]
fn convergence_is_order_independent() {
    // Two transports fed the SAME ops but pulled in different orders
    // must yield identical final state on every replica.
    let scope = SyncScopeId::new_v4();

    let run = |reverse_pull: bool| -> Vec<Vec<String>> {
        let transport = InMemoryTransport::new();
        let mut replicas = vec![
            Replica::new(scope),
            Replica::new(scope),
            Replica::new(scope),
        ];
        replicas[0].engine.add("one".into());
        replicas[1].engine.add("two".into());
        replicas[2].engine.add("three".into());
        // Push everyone first (deterministic relay content)...
        for r in &mut replicas {
            r.client.push(&r.engine, &transport).expect("push");
        }
        // ...then pull, optionally in reverse replica order.
        if reverse_pull {
            for r in replicas.iter_mut().rev() {
                r.client.pull(&mut r.engine, &transport).expect("pull");
            }
        } else {
            for r in &mut replicas {
                r.client.pull(&mut r.engine, &transport).expect("pull");
            }
        }
        replicas.iter().map(Replica::live).collect()
    };

    let forward = run(false);
    let reverse = run(true);
    assert_eq!(forward, reverse, "merge result must not depend on order");
    let expected = vec!["one".to_string(), "three".into(), "two".into()];
    for state in &forward {
        assert_eq!(state, &expected);
    }
}

#[test]
fn relay_only_ever_holds_opaque_ciphertext() {
    let scope = SyncScopeId::new_v4();
    let transport = Arc::new(InMemoryTransport::new());
    let mut a = Replica::new(scope);

    // A plaintext marker that must never appear on the wire.
    let secret = "TOP-SECRET-MEMO-9f3c";
    a.engine.add(secret.into());
    a.client.push(&a.engine, transport.as_ref()).expect("push");

    let blobs = transport.raw_blobs(a.client.topic());
    assert!(!blobs.is_empty(), "relay must have stored the sealed delta");
    for blob in &blobs {
        // The marker bytes never appear in the ciphertext.
        assert!(
            !contains_subslice(&blob.ciphertext, secret.as_bytes()),
            "plaintext leaked into relay ciphertext"
        );
        // The ciphertext does not deserialise as a DeltaEnvelope —
        // it is sealed, not merely re-encoded.
        let decoded =
            serde_json::from_slice::<sync_engine::delta::DeltaEnvelope<String>>(&blob.ciphertext);
        assert!(
            decoded.is_err(),
            "relay ciphertext must not be a readable DeltaEnvelope"
        );
    }
}

#[test]
fn forged_blob_is_skipped_and_does_not_wedge_sync() {
    use sync_engine::transport::SyncTransport;

    let scope = SyncScopeId::new_v4();
    let transport = InMemoryTransport::new();
    let mut a = Replica::new(scope);
    let mut b = Replica::new(scope);

    a.engine.add("legit".into());
    a.client.push(&a.engine, &transport).expect("push legit");

    // Forge a blob the way a hostile/buggy *untrusted* relay could:
    // take a real sealed blob (so the nonce shape is valid) and flip a
    // ciphertext byte so AEAD authentication fails on open.
    let mut forged = transport
        .raw_blobs(a.client.topic())
        .pop()
        .expect("a real blob to corrupt");
    let last = forged.ciphertext.len() - 1;
    forged.ciphertext[last] ^= 0xff;
    transport
        .push(b.client.topic(), std::slice::from_ref(&forged))
        .expect("inject forged blob");

    // B pulls: the forged blob is skipped (no error, no hang), the
    // genuine op is still absorbed, and the cursor advances past both.
    let report = b
        .client
        .pull_reporting(&mut b.engine, &transport)
        .expect("pull tolerates forged blob");
    assert_eq!(report.skipped, 1, "forged blob must be skipped");
    assert!(report.absorbed >= 1, "genuine op must still be absorbed");
    assert_eq!(b.live(), vec!["legit".to_string()]);

    // Sync keeps making progress afterwards — the cursor did not wedge
    // on the bad blob.
    b.engine.add("after".into());
    let mut replicas = vec![a, b];
    drive_to_quiescence(&mut replicas, &transport);
    let expected = vec!["after".to_string(), "legit".into()];
    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), expected, "replica {i} after forged-blob recovery");
    }
}

#[test]
fn push_count_reflects_live_ops_not_inflated_clock_after_compaction() {
    let scope = SyncScopeId::new_v4();
    let transport = InMemoryTransport::new();
    let mut a = Replica::new(scope);

    // A churny, never-pushed history: adds + removes inflate the
    // op-log clock far beyond the live-set size.
    for i in 0..6 {
        a.engine.add(format!("e{i}"));
    }
    for i in 0..4 {
        a.engine.remove(format!("e{i}"));
    }
    a.engine.compact().expect("compact");

    let clock_range = a.engine.op_log().clock; // (watermark is still 0)
    let pushed = a.client.push(&a.engine, &transport).expect("push");

    // The count must reflect the ops actually encoded (the 2 live
    // adds), not the width of the inflated `(0, clock]` seq range.
    assert_eq!(pushed, 2, "push must count live ops, not the clock range");
    assert!(
        (pushed as u64) < clock_range,
        "clock range ({clock_range}) should exceed the live op count ({pushed})"
    );

    // Exactly one blob reached the relay, and a re-push with nothing
    // new advances no further.
    assert_eq!(
        transport.raw_blobs(a.client.topic()).len(),
        1,
        "exactly one sealed delta uploaded"
    );
    assert_eq!(a.client.push(&a.engine, &transport).expect("re-push"), 0);
}

#[test]
fn push_after_compaction_to_empty_set_sends_no_blob() {
    let scope = SyncScopeId::new_v4();
    let transport = InMemoryTransport::new();
    let mut a = Replica::new(scope);

    // Add then remove the same element, then compact: the live set is
    // empty and the compacted log carries no ops to upload.
    a.engine.add("ephemeral".into());
    a.engine.remove("ephemeral".into());
    a.engine.compact().expect("compact");

    let pushed = a.client.push(&a.engine, &transport).expect("push");
    assert_eq!(pushed, 0, "nothing to upload after compaction to empty");
    assert!(
        transport.raw_blobs(a.client.topic()).is_empty(),
        "no empty envelope should be sealed and pushed"
    );
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
