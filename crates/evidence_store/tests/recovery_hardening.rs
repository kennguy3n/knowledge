//! Recovery- and forgetting-hardening integration tests for
//! [`EvidenceStore`].
//!
//! These cover two durability/forgetting contracts requested by the
//! substrate's security work (`docs/COMPLIANCE.md`,
//! `docs/SUPPLY_CHAIN.md`), exercising only the crate's **public**
//! surface:
//!
//! 1. **Crash-recovery of an interrupted forget** — a forget that
//!    destroyed the scope DEK and wrote the durable tombstone but
//!    crashed *before* the FTS5 plaintext purge finished. Re-opening
//!    the store and replaying the persisted tombstone (exactly what
//!    the FFI runtime does on `open_store`, see
//!    `crates/ffi/src/lib.rs:701` `forget_scope_state` and
//!    [`EvidenceStore::load_forgotten_scopes`]) must complete the
//!    purge so the plaintext index no longer leaks the forgotten
//!    body.
//! 2. **Ring-buffer FIFO eviction** — filling the noise ring buffer
//!    past its byte cap evicts the oldest entries first, the survivors
//!    stay in insertion order, and evicted rows are physically gone
//!    (unrecoverable across a re-open).

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::tempdir;

/// 32-byte master key shared by every test here (matches the pattern
/// used across the existing `evidence_store` integration suite).
const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// Distinctive single token so the FTS5 `unicode61` tokenizer indexes
/// it verbatim and `MATCH` needs no phrase quoting.
const FORGETTING_PHRASE: &str = "xyzzyrecoverytestphrase";

// ===========================================================================
// 1. Crash-recovery: tombstone replay completes an interrupted forget
// ===========================================================================

#[test]
fn interrupted_forget_is_completed_by_tombstone_replay_on_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope = ScopeId::new_v4();
    let body = format!(
        "Audit note: the {FORGETTING_PHRASE} must be cryptographically \
         forgotten before the device is decommissioned."
    );
    assert!(body.len() <= DEFAULT_INLINE_THRESHOLD_BYTES);

    // --- Session 1: ingest, then crash mid-forget. ---
    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open store");
        let res = store
            .ingest(
                scope,
                body.as_bytes(),
                Some("source:crash-recovery"),
                ImportanceClass::Important,
            )
            .expect("ingest");
        let evidence_id = res.evidence_id;

        // FTS5 indexed the plaintext before any forget runs.
        assert_eq!(
            store
                .search_fts(scope, FORGETTING_PHRASE, 10)
                .expect("search pre-forget"),
            vec![evidence_id],
            "FTS5 must surface the phrase before forgetting"
        );

        // Simulate a forget that crashed *after* the load-bearing
        // step (DEK destruction + durable tombstone) but *before* the
        // best-effort FTS5 purge. This is the exact failure window
        // documented in `forget_scope_state` step 1 vs step 3.
        store.delete_scope_dek(scope).expect("destroy scope DEK");
        store
            .record_forgotten_scope(scope)
            .expect("write durable tombstone");
        // NOTE: purge_fts_for_scope is deliberately NOT called — that
        // is the work the crash interrupted.

        // The FTS5 plaintext index still leaks the body at this point.
        assert_eq!(
            store
                .search_fts(scope, FORGETTING_PHRASE, 10)
                .expect("search after interrupted forget"),
            vec![evidence_id],
            "interrupted forget must leave the FTS5 index un-purged"
        );
    } // store dropped == process crash / restart boundary

    // --- Session 2: re-open and replay the tombstone. ---
    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("re-open store");

    // The durable tombstone survived the crash.
    let forgotten = store.load_forgotten_scopes().expect("load tombstones");
    assert!(
        forgotten.contains(&scope),
        "the forgotten-scope tombstone must persist across re-open"
    );

    // The FTS5 index is still un-purged immediately after re-open:
    // `EvidenceStore::open` does not itself replay tombstones (that is
    // the host runtime's job), so the leak is still observable here.
    assert!(
        !store
            .search_fts(scope, FORGETTING_PHRASE, 10)
            .expect("search on reopen")
            .is_empty(),
        "FTS5 leak must still be present until the tombstone is replayed"
    );

    // Replay: for every persisted tombstone, finish the purge. This
    // mirrors the FFI runtime's open_store reconciliation loop.
    for forgotten_scope in store.load_forgotten_scopes().expect("load tombstones") {
        store
            .purge_fts_for_scope(forgotten_scope)
            .expect("replayed purge");
    }

    // The leak is now closed.
    assert!(
        store
            .search_fts(scope, FORGETTING_PHRASE, 10)
            .expect("search after replay")
            .is_empty(),
        "tombstone replay must purge the FTS5 index"
    );

    // And it stays closed across another re-open (durable, not a
    // flushed cache).
    drop(store);
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("third open");
    let raw_fts_rows: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_fts WHERE evidence_fts MATCH ?1 AND scope_id = ?2",
            rusqlite::params![FORGETTING_PHRASE, scope.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count fts rows");
    assert_eq!(
        raw_fts_rows, 0,
        "purge from the replayed tombstone must be durable on disk"
    );
}

// ===========================================================================
// 2. Ring-buffer FIFO eviction
// ===========================================================================

/// Fixed body length so every ring-buffer entry has an identical
/// on-disk `payload_size` (`ciphertext = body + 16-byte tag`, plus a
/// 24-byte nonce), making the cap arithmetic below exact.
const RING_BODY_LEN: usize = 64;
/// `payload_size` charged per entry: body + Poly1305 tag + nonce.
const RING_PAYLOAD_PER_ENTRY: usize = RING_BODY_LEN + 16 + 24;
/// Cap chosen to hold exactly three entries (3 * 104 = 312 ≤ 320) but
/// force eviction on the fourth (4 * 104 = 416 > 320).
const RING_CAP_BYTES: usize = 3 * RING_PAYLOAD_PER_ENTRY + 8;
/// Number of entries inserted in the eviction test.
const RING_INSERTED: usize = 6;
/// Number expected to survive eviction (the cap holds three).
const RING_SURVIVORS: usize = 3;

/// Build a distinctive, fixed-length body for ring entry `i`.
fn ring_entry_body(i: usize) -> Vec<u8> {
    let mut body = format!("noise-entry-{i:04}");
    while body.len() < RING_BODY_LEN {
        body.push('.');
    }
    body.truncate(RING_BODY_LEN);
    body.into_bytes()
}

fn small_ring_config() -> EvidenceStoreConfig {
    EvidenceStoreConfig {
        inline_threshold_bytes: DEFAULT_INLINE_THRESHOLD_BYTES,
        ring_buffer_max_bytes: RING_CAP_BYTES,
    }
}

#[test]
fn ring_buffer_evicts_oldest_first_and_keeps_insertion_order() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();

    {
        let mut store =
            EvidenceStore::open(&path, &MASTER_KEY, small_ring_config()).expect("open store");

        for i in 0..RING_INSERTED {
            store
                .ring_buffer_insert(scope, &ring_entry_body(i))
                .unwrap_or_else(|e| panic!("ring insert {i}: {e:?}"));
        }

        // Only the last SURVIVORS entries remain, oldest → newest.
        assert_eq!(
            store.ring_buffer_len().expect("len"),
            RING_SURVIVORS,
            "FIFO eviction must cap the entry count"
        );
        assert!(
            store.ring_buffer_current_size().expect("size") <= RING_CAP_BYTES,
            "ring buffer must stay within its byte cap"
        );

        let window = store.ring_buffer_read_window(scope).expect("window");
        let bodies: Vec<Vec<u8>> = window.into_iter().map(|e| e.body).collect();
        let expected: Vec<Vec<u8>> = (RING_INSERTED - RING_SURVIVORS..RING_INSERTED)
            .map(ring_entry_body)
            .collect();
        assert_eq!(
            bodies, expected,
            "survivors must be the newest entries in insertion order"
        );

        // The evicted (oldest) bodies must not appear anywhere in the
        // decrypted window.
        for evicted in 0..RING_INSERTED - RING_SURVIVORS {
            let evicted_body = ring_entry_body(evicted);
            assert!(
                !bodies.contains(&evicted_body),
                "evicted entry {evicted} must not be recoverable from the window"
            );
        }
    }

    // Eviction is durable: re-open and confirm the evicted rows are
    // physically gone, not merely hidden by a cache.
    let mut store =
        EvidenceStore::open(&path, &MASTER_KEY, small_ring_config()).expect("re-open store");
    let raw_rows: i64 = store
        .raw_conn()
        .query_row("SELECT COUNT(*) FROM ring_buffer", [], |r| r.get(0))
        .expect("count ring rows");
    assert_eq!(
        raw_rows,
        i64::try_from(RING_SURVIVORS).expect("survivor count fits in i64"),
        "evicted ring-buffer rows must be deleted on disk"
    );
    let bodies: Vec<Vec<u8>> = store
        .ring_buffer_read_window(scope)
        .expect("window after reopen")
        .into_iter()
        .map(|e| e.body)
        .collect();
    let expected: Vec<Vec<u8>> = (RING_INSERTED - RING_SURVIVORS..RING_INSERTED)
        .map(ring_entry_body)
        .collect();
    assert_eq!(
        bodies, expected,
        "survivor set must be stable across re-open"
    );
}

#[test]
fn ring_buffer_entry_larger_than_cap_is_not_retained() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();

    let mut store =
        EvidenceStore::open(&path, &MASTER_KEY, small_ring_config()).expect("open store");

    // A single body whose encrypted payload exceeds the whole cap
    // cannot be retained: the eviction loop deletes it right back out,
    // leaving the buffer empty rather than over budget.
    let oversized = vec![0x5Au8; RING_CAP_BYTES * 2];
    store
        .ring_buffer_insert(scope, &oversized)
        .expect("insert oversized");

    assert_eq!(
        store.ring_buffer_len().expect("len"),
        0,
        "an entry larger than the cap must not be retained"
    );
    assert!(
        store
            .ring_buffer_read_window(scope)
            .expect("window")
            .is_empty(),
        "oversized entry must not be readable"
    );
}
