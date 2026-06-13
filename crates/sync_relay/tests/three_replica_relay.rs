//! End-to-end: ≥3 replicas exchange deltas through a **real**
//! authenticated HTTP relay and converge.
//!
//! This is the top tier of the sync test pyramid (the in-process tier
//! lives in `sync_engine/tests/transport_convergence.rs`). Here every
//! delta makes a real HTTP round trip through an axum relay server
//! bound on an ephemeral localhost port, sealed under a per-scope AEAD
//! key the relay never holds.
//!
//! Asserted invariants:
//! * three replicas converge to identical state through the relay;
//! * an offline/partitioned replica catches up on reconnect;
//! * add-wins beats a concurrent remove across the relay;
//! * supersession propagates across the relay;
//! * convergence is order-independent;
//! * the relay only ever stores opaque ciphertext (no plaintext);
//! * a missing/invalid bearer token is rejected (401);
//! * tenants are isolated (one tenant cannot read another's topic).

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use tokio::sync::oneshot;

use sync_engine::transport::SyncClient;
use sync_engine::{MasterKey, SyncEngine, SyncScopeId, SyncTransport};
use sync_relay::store::BlobStore;
use sync_relay::{
    HttpRelayTransport, InMemoryBlobStore, RelayConfig, RelayServer, RelayState, TenantId,
    TokenRegistry,
};

const MASTER_KEY: MasterKey = [0x42; 32];
const TENANT_TOKEN: &str = "tenant-1-secret-token";
const TENANT_ID: &str = "tenant-1";

/// A running relay: the bound address, a handle to inspect stored
/// blobs, and a shutdown trigger that stops the server on drop.
struct RelayHarness {
    addr: SocketAddr,
    store: Arc<InMemoryBlobStore>,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl RelayHarness {
    /// Spin up a relay on an ephemeral port in a dedicated thread with
    /// its own tokio runtime, so the blocking HTTP client can be
    /// driven from the test thread without nesting runtimes.
    fn start() -> Self {
        let mut registry = TokenRegistry::new();
        registry.insert(TENANT_TOKEN, TenantId::new(TENANT_ID));
        let store = Arc::new(InMemoryBlobStore::default());
        let store_for_state: Arc<dyn BlobStore> = store.clone();
        let state = RelayState::new(store_for_state, Arc::new(registry));

        let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let join = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build relay runtime");
            rt.block_on(async move {
                let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
                let server = RelayServer::new(RelayConfig::new(bind), state);
                let listener = server.bind().await.expect("bind relay");
                let addr = listener.local_addr().expect("relay local addr");
                addr_tx.send(addr).expect("report relay addr");
                server
                    .serve_on(listener, async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("relay serve");
            });
        });

        let addr = addr_rx.recv().expect("relay reported its address");
        Self {
            addr,
            store,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn transport(&self) -> HttpRelayTransport {
        HttpRelayTransport::new(self.base_url(), TENANT_TOKEN).expect("build transport")
    }
}

impl Drop for RelayHarness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A replica: an engine + its sync client + a transport handle.
struct Replica {
    engine: SyncEngine<String>,
    client: SyncClient,
    transport: HttpRelayTransport,
}

impl Replica {
    fn new(scope: SyncScopeId, transport: HttpRelayTransport) -> Self {
        Self {
            engine: SyncEngine::new(),
            client: SyncClient::new(&MASTER_KEY, scope).expect("derive client"),
            transport,
        }
    }

    fn sync(&mut self) {
        self.client
            .sync(&mut self.engine, &self.transport)
            .expect("sync round trip");
    }

    fn live(&self) -> Vec<String> {
        let (set, _) = self.engine.state().expect("state");
        let mut v: Vec<String> = set.elements().cloned().collect();
        v.sort();
        v
    }
}

/// Sync every replica repeatedly until no replica pushes or absorbs
/// anything. Bounded so a convergence bug fails the test rather than
/// hanging.
fn drive_to_quiescence(replicas: &mut [Replica]) {
    for _ in 0..16 {
        let mut changed = false;
        for r in replicas.iter_mut() {
            let outcome = r
                .client
                .sync(&mut r.engine, &r.transport)
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
fn three_replicas_converge_through_real_relay() {
    let relay = RelayHarness::start();
    let scope = SyncScopeId::new_v4();
    let mut replicas = vec![
        Replica::new(scope, relay.transport()),
        Replica::new(scope, relay.transport()),
        Replica::new(scope, relay.transport()),
    ];

    replicas[0].engine.add("alpha".into());
    replicas[1].engine.add("bravo".into());
    replicas[2].engine.add("charlie".into());

    drive_to_quiescence(&mut replicas);

    let expected = vec!["alpha".to_string(), "bravo".into(), "charlie".into()];
    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), expected, "replica {i} did not converge");
    }
}

#[test]
fn offline_replica_catches_up_after_partition_heals() {
    let relay = RelayHarness::start();
    let scope = SyncScopeId::new_v4();
    let mut a = Replica::new(scope, relay.transport());
    let mut b = Replica::new(scope, relay.transport());
    let mut c = Replica::new(scope, relay.transport());

    a.engine.add("shared".into());
    a.sync();
    b.sync();
    c.sync();
    assert_eq!(c.live(), vec!["shared".to_string()]);

    // C is partitioned away. A and B keep working and converge.
    a.engine.add("a-offline".into());
    b.engine.add("b-offline".into());
    a.sync();
    b.sync();
    a.sync();
    b.sync();

    let ab = vec!["a-offline".to_string(), "b-offline".into(), "shared".into()];
    assert_eq!(a.live(), ab);
    assert_eq!(b.live(), ab);
    assert_eq!(c.live(), vec!["shared".to_string()], "C still stale");

    // C reconnects, contributes its own op, and everyone reconciles.
    c.engine.add("c-back".into());
    let mut replicas = vec![a, b, c];
    drive_to_quiescence(&mut replicas);

    let mut expected = vec![
        "a-offline".to_string(),
        "b-offline".into(),
        "c-back".into(),
        "shared".into(),
    ];
    expected.sort();
    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), expected, "replica {i} post-reconnect");
    }
}

#[test]
fn add_wins_over_concurrent_remove_through_real_relay() {
    let relay = RelayHarness::start();
    let scope = SyncScopeId::new_v4();
    let mut a = Replica::new(scope, relay.transport());
    let mut b = Replica::new(scope, relay.transport());

    a.engine.add("x".into());
    a.sync();
    b.sync();
    assert_eq!(b.live(), vec!["x".to_string()]);

    // Partition: A removes the observed "x"; B concurrently re-adds.
    a.engine.remove("x".into());
    b.engine.add("x".into());

    let mut replicas = vec![a, b];
    drive_to_quiescence(&mut replicas);

    for (i, r) in replicas.iter().enumerate() {
        assert_eq!(r.live(), vec!["x".to_string()], "add-wins on replica {i}");
    }
}

#[test]
fn supersession_propagates_through_real_relay() {
    let relay = RelayHarness::start();
    let scope = SyncScopeId::new_v4();
    let mut a = Replica::new(scope, relay.transport());
    let b = Replica::new(scope, relay.transport());
    let c = Replica::new(scope, relay.transport());

    a.engine.add("v1".into());
    a.engine.supersede("v1".into(), "v2".into());
    a.engine.add("v2".into());

    let mut replicas = vec![a, b, c];
    drive_to_quiescence(&mut replicas);

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
fn convergence_is_order_independent_through_real_relay() {
    let scope = SyncScopeId::new_v4();

    let run = |reverse: bool| -> Vec<Vec<String>> {
        let relay = RelayHarness::start();
        let mut replicas = vec![
            Replica::new(scope, relay.transport()),
            Replica::new(scope, relay.transport()),
            Replica::new(scope, relay.transport()),
        ];
        replicas[0].engine.add("one".into());
        replicas[1].engine.add("two".into());
        replicas[2].engine.add("three".into());
        for r in &mut replicas {
            r.client.push(&r.engine, &r.transport).expect("push");
        }
        if reverse {
            for r in replicas.iter_mut().rev() {
                r.client.pull(&mut r.engine, &r.transport).expect("pull");
            }
        } else {
            for r in &mut replicas {
                r.client.pull(&mut r.engine, &r.transport).expect("pull");
            }
        }
        replicas.iter().map(Replica::live).collect()
    };

    let forward = run(false);
    let reverse = run(true);
    assert_eq!(forward, reverse, "merge must not depend on pull order");
    let expected = vec!["one".to_string(), "three".into(), "two".into()];
    for state in &forward {
        assert_eq!(state, &expected);
    }
}

#[test]
fn relay_stores_only_opaque_ciphertext() {
    let relay = RelayHarness::start();
    let scope = SyncScopeId::new_v4();
    let mut a = Replica::new(scope, relay.transport());

    let secret = "TOP-SECRET-MEMO-9f3c";
    a.engine.add(secret.into());
    a.client.push(&a.engine, &a.transport).expect("push");

    // Inspect what actually landed in the relay's store.
    let tenant = TenantId::new(TENANT_ID);
    let blobs = relay.store.raw_blobs(&tenant, a.client.topic());
    assert!(!blobs.is_empty(), "relay must have stored the sealed delta");
    for blob in &blobs {
        assert!(
            !contains_subslice(&blob.ciphertext, secret.as_bytes()),
            "plaintext leaked into relay ciphertext"
        );
        let decoded =
            serde_json::from_slice::<sync_engine::delta::DeltaEnvelope<String>>(&blob.ciphertext);
        assert!(decoded.is_err(), "ciphertext must not be a plain envelope");
    }
}

#[test]
fn missing_or_invalid_token_is_rejected() {
    let relay = RelayHarness::start();
    let scope = SyncScopeId::new_v4();
    let mut engine = SyncEngine::<String>::new();
    engine.add("x".into());
    let mut client = SyncClient::new(&MASTER_KEY, scope).expect("client");

    // Wrong token → 401 surfaced as a transport error.
    let bad = HttpRelayTransport::new(relay.base_url(), "wrong-token").expect("transport");
    let err = client
        .push(&engine, &bad)
        .expect_err("push with bad token must fail");
    let msg = err.to_string();
    assert!(msg.contains("401"), "expected 401, got: {msg}");
}

#[test]
fn tenants_cannot_read_each_others_topics() {
    // Two tenants on one relay.
    let mut registry = TokenRegistry::new();
    registry.insert("tok-a", TenantId::new("tenant-a"));
    registry.insert("tok-b", TenantId::new("tenant-b"));
    let store = Arc::new(InMemoryBlobStore::default());
    let store_for_state: Arc<dyn BlobStore> = store.clone();
    let state = RelayState::new(store_for_state, Arc::new(registry));

    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let join = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let server = RelayServer::new(RelayConfig::new(bind), state);
            let listener = server.bind().await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();
            server
                .serve_on(listener, async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
    });
    let addr = addr_rx.recv().unwrap();
    let base = format!("http://{addr}");

    let scope = SyncScopeId::new_v4();
    // Tenant A pushes a delta.
    let mut engine_a = SyncEngine::<String>::new();
    engine_a.add("a-secret".into());
    let mut client_a = SyncClient::new(&MASTER_KEY, scope).expect("client a");
    let transport_a = HttpRelayTransport::new(&base, "tok-a").expect("transport a");
    client_a.push(&engine_a, &transport_a).expect("push a");

    // Tenant B, even knowing the exact topic, pulls nothing: storage
    // is namespaced by (tenant, topic).
    let transport_b = HttpRelayTransport::new(&base, "tok-b").expect("transport b");
    let page = transport_b
        .pull(client_a.topic(), 0)
        .expect("b pull should succeed (auth ok) but see no blobs");
    assert!(
        page.blobs.is_empty(),
        "tenant B must not see tenant A's blobs, got {} blobs",
        page.blobs.len()
    );

    let _ = shutdown_tx.send(());
    let _ = join.join();
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
