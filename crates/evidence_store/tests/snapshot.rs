//! Integration tests for consistent backup snapshots
//! ([`EvidenceStore::snapshot_to`]).
//!
//! A snapshot must produce a standalone copy that is byte-for-byte
//! recoverable under the *same* master key (it is a backup, not a
//! rekey), stays opaque under a different key, never mutates or
//! disturbs the live source store, and refuses to clobber an existing
//! file. These tests exercise only the crate's public surface.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::tempdir;

const KEY: [u8; 32] = [0x33; 32];
const OTHER_KEY: [u8; 32] = [0xC4; 32];

/// A body comfortably larger than the inline threshold so it is routed
/// through the deduplicated `body_store` (CEK-wrapped) path rather than
/// stored inline in the evidence row — exercises both storage paths.
fn big_body(seed: u8) -> Vec<u8> {
    let mut v = vec![seed; DEFAULT_INLINE_THRESHOLD_BYTES + 4096];
    v[0] = seed.wrapping_add(1);
    v[1] = seed.wrapping_add(2);
    v
}

#[test]
fn snapshot_round_trips_every_body_under_the_same_key() {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("substrate-evidence.db");
    let dest = dir.path().join("substrate-evidence.snapshot.db");

    let legacy_scope = ScopeId::new_v4();
    let dek_scope = ScopeId::new_v4();

    let inline_a = b"inline body for the legacy scope".to_vec();
    let inline_b = b"inline body for the explicit-DEK scope".to_vec();
    let big_a = big_body(0x42);
    let big_b = big_body(0x99);

    let mut ids = Vec::new();
    {
        let mut store =
            EvidenceStore::open(&src, &KEY, EvidenceStoreConfig::default()).expect("open src");
        store.ensure_scope_dek(dek_scope).expect("ensure dek");

        for (scope, body) in [
            (legacy_scope, &inline_a),
            (dek_scope, &inline_b),
            (legacy_scope, &big_a),
            (dek_scope, &big_b),
        ] {
            let res = store
                .ingest(scope, body, Some("source:test"), ImportanceClass::Important)
                .expect("ingest");
            ids.push((res.evidence_id, body.clone()));
        }

        store.snapshot_to(&dest).expect("snapshot");

        // The live source store is untouched by the snapshot: it still
        // holds every row and stays usable for further writes.
        assert_eq!(store.evidence_count().expect("count"), 4);
        let res = store
            .ingest(
                legacy_scope,
                b"post-snapshot write",
                Some("source:test"),
                ImportanceClass::Important,
            )
            .expect("ingest after snapshot");
        assert_eq!(store.evidence_count().expect("count"), 5);
        // The snapshot is a point-in-time copy: the post-snapshot row
        // must NOT have leaked into it.
        let _ = res;
    }

    // The snapshot opens under the SAME key (a backup, not a rekey) and
    // every body decrypts identically to what was ingested.
    {
        let snap = EvidenceStore::open(&dest, &KEY, EvidenceStoreConfig::default())
            .expect("open snapshot");
        assert_eq!(
            snap.evidence_count().expect("count"),
            4,
            "snapshot is a point-in-time copy taken before the 5th write"
        );
        for (id, body) in &ids {
            assert_eq!(&snap.read_body(*id).expect("read body"), body);
        }
    }

    // The snapshot stays opaque under a different master key.
    assert!(
        EvidenceStore::open(&dest, &OTHER_KEY, EvidenceStoreConfig::default()).is_err(),
        "snapshot must not open under a different master key"
    );
}

#[test]
fn snapshot_refuses_to_clobber_existing_destination() {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("substrate-evidence.db");
    let dest = dir.path().join("already-here.db");
    std::fs::write(&dest, b"pre-existing").expect("seed dest");

    let store = EvidenceStore::open(&src, &KEY, EvidenceStoreConfig::default()).expect("open src");
    store
        .snapshot_to(&dest)
        .expect_err("must refuse to overwrite an existing destination");
}
