//! Integration tests for the offline master-key rotation orchestration
//! (`substrate_server::key_rotation::rotate`).
//!
//! These drive the *deployment* choreography end-to-end: seed a real
//! evidence store and permission store under an old key, rotate both to
//! a new key, and assert that (a) the live files open only under the
//! new key, (b) the timestamped backups still open under the old key,
//! and (c) the row/tuple payloads survive intact.

use std::path::Path;

use evidence_store::{EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId};
use permission_service::tuple::{ObjectRef, ObjectType, Relation, SubjectRef, SubjectType};
use permission_service::{PersistentTupleStore, RelationTuple};
use substrate_server::key_rotation::{rotate, RotationError, RotationPaths};
use tempfile::tempdir;
use uuid::Uuid;

const OLD_KEY: [u8; 32] = [0x11; 32];
const NEW_KEY: [u8; 32] = {
    let mut k = [0xEE; 32];
    k[31] = 0xE0;
    k
};

/// Lower-case hex encoding of a 32-byte key, matching the 64-char
/// format `config::decode_master_key` expects. Generated from the byte
/// array so the literal and the bytes can never drift out of sync.
fn hex(key: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    key.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn fresh_tuple() -> RelationTuple {
    RelationTuple::new(
        ObjectRef::new(ObjectType::Tenant, Uuid::new_v4()),
        Relation::Owner,
        SubjectRef::direct(SubjectType::User, Uuid::new_v4()),
    )
}

fn seed_stores(
    store_path: &Path,
    perms_path: &Path,
) -> (ScopeId, uuid::Uuid, Vec<u8>, [RelationTuple; 2]) {
    let scope = ScopeId::new_v4();
    let body = b"sensitive evidence body that must survive rotation".to_vec();

    let mut store =
        EvidenceStore::open(store_path, &OLD_KEY, EvidenceStoreConfig::default()).expect("open ev");
    let res = store
        .ingest(
            scope,
            &body,
            Some("source:test"),
            ImportanceClass::Important,
        )
        .expect("ingest");
    let ev_id = res.evidence_id;
    drop(store);

    let t1 = fresh_tuple();
    let t2 = fresh_tuple();
    let mut perms = PersistentTupleStore::open(perms_path, &OLD_KEY).expect("open perms");
    perms.insert(t1).expect("insert t1");
    perms.insert(t2).expect("insert t2");
    drop(perms);

    (scope, ev_id.0, body, [t1, t2])
}

#[test]
fn rotate_swaps_both_stores_and_keeps_old_key_backups() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("substrate.db");
    let perms_path = dir.path().join("permissions.db");

    let (_scope, ev_uuid, body, tuples) = seed_stores(&store_path, &perms_path);

    let paths = RotationPaths {
        store_path: store_path.clone(),
        permissions_path: perms_path.clone(),
    };
    let outcome = rotate(&paths, &hex(&OLD_KEY), &hex(&NEW_KEY)).expect("rotate");

    assert_eq!(outcome.evidence.evidence_rows, 1);
    assert_eq!(outcome.evidence.bodies_verified, 1);
    assert_eq!(outcome.permission_tuples, 2);

    // Live files must be opaque under the OLD key now.
    assert!(EvidenceStore::open(&store_path, &OLD_KEY, EvidenceStoreConfig::default()).is_err());
    assert!(PersistentTupleStore::open(&perms_path, &OLD_KEY).is_err());

    // Live files open under the NEW key with intact data.
    let rotated_ev =
        EvidenceStore::open(&store_path, &NEW_KEY, EvidenceStoreConfig::default()).expect("ev new");
    assert_eq!(
        rotated_ev
            .read_body(evidence_store::EvidenceId(ev_uuid))
            .expect("read body"),
        body
    );
    let rotated_perms = PersistentTupleStore::open(&perms_path, &NEW_KEY).expect("perms new");
    assert_eq!(rotated_perms.store().len(), 2);
    for t in &tuples {
        assert!(rotated_perms.store().contains(t));
    }

    // The backups still open under the OLD key (rollback material).
    assert!(outcome.evidence_backup.exists());
    assert!(outcome.permissions_backup.exists());
    let backup_ev = EvidenceStore::open(
        &outcome.evidence_backup,
        &OLD_KEY,
        EvidenceStoreConfig::default(),
    )
    .expect("backup ev opens under old key");
    assert_eq!(
        backup_ev
            .read_body(evidence_store::EvidenceId(ev_uuid))
            .expect("read backup body"),
        body
    );
    let backup_perms =
        PersistentTupleStore::open(&outcome.permissions_backup, &OLD_KEY).expect("backup perms");
    assert_eq!(backup_perms.store().len(), 2);
}

#[test]
fn rotate_rejects_identical_keys() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("substrate.db");
    let perms_path = dir.path().join("permissions.db");
    seed_stores(&store_path, &perms_path);

    let paths = RotationPaths {
        store_path,
        permissions_path: perms_path,
    };
    let err =
        rotate(&paths, &hex(&OLD_KEY), &hex(&OLD_KEY)).expect_err("must reject identical keys");
    assert!(matches!(err, RotationError::KeysIdentical));
}

#[test]
fn rotate_rejects_bad_new_key() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("substrate.db");
    let perms_path = dir.path().join("permissions.db");
    seed_stores(&store_path, &perms_path);

    let paths = RotationPaths {
        store_path: store_path.clone(),
        permissions_path: perms_path,
    };
    let err = rotate(&paths, &hex(&OLD_KEY), "tooshort").expect_err("must reject bad key");
    assert!(matches!(err, RotationError::BadMasterKey { which: "new" }));
    // The live store must be untouched.
    assert!(EvidenceStore::open(&store_path, &OLD_KEY, EvidenceStoreConfig::default()).is_ok());
}

#[test]
fn rotate_fails_cleanly_when_evidence_store_missing() {
    let dir = tempdir().expect("tempdir");
    let paths = RotationPaths {
        store_path: dir.path().join("does-not-exist.db"),
        permissions_path: dir.path().join("permissions.db"),
    };
    let err =
        rotate(&paths, &hex(&OLD_KEY), &hex(&NEW_KEY)).expect_err("must fail when store missing");
    assert!(matches!(err, RotationError::EvidenceStoreMissing(_)));
}
