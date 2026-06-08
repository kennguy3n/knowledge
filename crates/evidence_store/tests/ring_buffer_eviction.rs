//! Ring-buffer eviction tests.
//!
//! Verifies FIFO eviction when the noise ring buffer exceeds its
//! configured capacity, and that evicted rows are unrecoverable.

use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xA5; 32];

fn open_store_with_cap(path: &std::path::Path, cap: usize) -> EvidenceStore {
    let config = EvidenceStoreConfig {
        ring_buffer_max_bytes: cap,
        ..EvidenceStoreConfig::default()
    };
    EvidenceStore::open(path, &MASTER_KEY, config).expect("open store")
}

#[test]
fn ring_buffer_fifo_eviction() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("ring_eviction.db");

    // Use a tiny cap so eviction kicks in quickly.
    // Each encrypted entry is roughly body_len + 16 (Poly1305 tag) + 24 (nonce) bytes.
    // A 100-byte body → ~140 bytes of payload.
    let cap = 300; // room for ~2 entries
    let mut store = open_store_with_cap(&db_path, cap);
    let scope = ScopeId::new_v4();

    // Insert three entries. The third should trigger eviction of the first.
    let body_a = vec![0xAAu8; 100];
    let body_b = vec![0xBBu8; 100];
    let body_c = vec![0xCCu8; 100];

    store.ring_buffer_insert(scope, &body_a).expect("insert a");
    store.ring_buffer_insert(scope, &body_b).expect("insert b");
    store.ring_buffer_insert(scope, &body_c).expect("insert c");

    // Read back the ring buffer window.
    let entries = store.ring_buffer_read_window(scope).expect("read window");

    // Due to FIFO eviction, the oldest entry (body_a) should have
    // been evicted. We should see at most 2 entries.
    assert!(
        entries.len() <= 2,
        "ring buffer should have evicted oldest entries to stay under cap, \
         but has {} entries",
        entries.len()
    );

    // Verify the surviving entries are the newer ones.
    // The read window returns entries ordered oldest → newest.
    if entries.len() == 2 {
        assert_eq!(entries[0].body, body_b, "second entry should survive");
        assert_eq!(entries[1].body, body_c, "third entry should survive");
    } else if entries.len() == 1 {
        // Very small cap might evict both older entries.
        assert_eq!(entries[0].body, body_c, "newest entry should survive");
    }
}

#[test]
fn evicted_rows_are_unrecoverable() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("ring_unrecoverable.db");

    // Cap that fits exactly one entry.
    let cap = 200;
    let mut store = open_store_with_cap(&db_path, cap);
    let scope = ScopeId::new_v4();

    let body_first = vec![0x11u8; 80];
    let body_second = vec![0x22u8; 80];

    store
        .ring_buffer_insert(scope, &body_first)
        .expect("insert first");
    store
        .ring_buffer_insert(scope, &body_second)
        .expect("insert second");

    let entries = store.ring_buffer_read_window(scope).expect("read window");

    // The first entry should have been evicted.
    let bodies: Vec<&[u8]> = entries.iter().map(|e| e.body.as_slice()).collect();
    assert!(
        !bodies.contains(&body_first.as_slice()),
        "evicted entry must not be recoverable from ring buffer"
    );

    // Verify the raw SQL also shows the row is gone.
    let conn = store.raw_conn();
    let total_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ring_buffer WHERE scope_id = ?1",
            rusqlite::params![scope.as_uuid().as_bytes().as_slice()],
            |r| r.get(0),
        )
        .expect("count");
    // Should be 1 (only the second entry survives).
    assert!(
        total_rows <= 1,
        "evicted rows must be physically deleted, but found {total_rows} rows"
    );
}

#[test]
fn ring_buffer_empty_scope_returns_empty() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("ring_empty.db");
    let mut store = open_store_with_cap(&db_path, 1024);
    let scope = ScopeId::new_v4();

    let entries = store
        .ring_buffer_read_window(scope)
        .expect("read empty window");
    assert!(entries.is_empty(), "empty scope should return no entries");
}
