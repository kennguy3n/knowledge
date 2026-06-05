//! Integration tests for offline master-key rotation
//! ([`EvidenceStore::rotate_master_key`]).
//!
//! Rotation must produce a copy that is byte-for-byte recoverable
//! under the *new* master key and completely opaque under the *old*
//! one, without ever re-encrypting evidence bodies (they are sealed
//! under per-scope DEKs that are independent of the master key). These
//! tests exercise only the crate's public surface and cover:
//!
//! 1. **Round-trip** — every body decrypts identically under the new
//!    key across the inline and body-table storage paths, and across
//!    a legacy HKDF-derived scope and a scope with an explicit stored
//!    DEK.
//! 2. **Old key is dead** — the rotated copy refuses to open under the
//!    previous master key.
//! 3. **Refuses to clobber** — rotation will not overwrite an existing
//!    destination file.
//! 4. **Forgotten scopes** — a cryptographically forgotten scope is
//!    skipped (its DEK is gone) without aborting the rotation, and its
//!    live siblings still round-trip.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::tempdir;

const OLD_KEY: [u8; 32] = [0x11; 32];
const NEW_KEY: [u8; 32] = [0xEE; 32];

/// A body comfortably larger than the inline threshold so it is routed
/// through the deduplicated `body_store` (CEK-wrapped) path rather than
/// stored inline in the evidence row.
fn big_body(seed: u8) -> Vec<u8> {
    let mut v = vec![seed; DEFAULT_INLINE_THRESHOLD_BYTES + 4096];
    // Perturb a few bytes so the content hash is unique per seed.
    v[0] = seed.wrapping_add(1);
    v[1] = seed.wrapping_add(2);
    v
}

#[test]
fn rotation_round_trips_every_body_under_the_new_key() {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("substrate.db");
    let dest = dir.path().join("substrate.rotated.db");

    // Legacy scope: bodies sealed under the HKDF-derived scope key.
    let legacy_scope = ScopeId::new_v4();
    // Explicit-DEK scope: provisioned with a random stored DEK before
    // ingest so its body key is independent of the master key.
    let dek_scope = ScopeId::new_v4();

    let inline_a = b"inline body for the legacy scope".to_vec();
    let inline_b = b"inline body for the explicit-DEK scope".to_vec();
    let big_a = big_body(0x42);
    let big_b = big_body(0x99);

    let mut ids = Vec::new();
    {
        let mut store =
            EvidenceStore::open(&src, &OLD_KEY, EvidenceStoreConfig::default()).expect("open src");

        // Provision an explicit random DEK for `dek_scope` so it does
        // not fall back to the legacy derived key.
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

        let report = store.rotate_master_key(&NEW_KEY, &dest).expect("rotate");
        assert_eq!(report.evidence_rows, 4, "all four evidence rows counted");
        assert_eq!(report.bodies_verified, 4, "all four bodies verified");
        assert!(
            report.scopes_rewrapped >= 2,
            "both scopes re-wrapped, got {}",
            report.scopes_rewrapped
        );
    }

    // The rotated copy must be dead under the old master key.
    assert!(
        EvidenceStore::open(&dest, &OLD_KEY, EvidenceStoreConfig::default()).is_err(),
        "rotated copy must not open under the old master key"
    );

    // ...and fully recoverable under the new master key.
    let rotated =
        EvidenceStore::open(&dest, &NEW_KEY, EvidenceStoreConfig::default()).expect("open rotated");
    for (id, expected) in &ids {
        let got = rotated
            .read_body(*id)
            .expect("read body from rotated store");
        assert_eq!(&got, expected, "body {id} must round-trip under new key");
    }
}

#[test]
fn rotation_refuses_to_overwrite_existing_destination() {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("substrate.db");
    let dest = dir.path().join("already-there.db");
    std::fs::write(&dest, b"do not clobber me").expect("seed dest");

    let store =
        EvidenceStore::open(&src, &OLD_KEY, EvidenceStoreConfig::default()).expect("open src");
    let err = store
        .rotate_master_key(&NEW_KEY, &dest)
        .expect_err("must refuse existing destination");
    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err}"
    );
    // The pre-existing file must be left untouched.
    assert_eq!(
        std::fs::read(&dest).expect("read dest"),
        b"do not clobber me"
    );
}

#[test]
fn rotation_skips_forgotten_scopes_and_preserves_live_ones() {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("substrate.db");
    let dest = dir.path().join("substrate.rotated.db");

    let live_scope = ScopeId::new_v4();
    let doomed_scope = ScopeId::new_v4();
    let live_body = b"this body survives the rotation".to_vec();

    let live_id;
    {
        let mut store =
            EvidenceStore::open(&src, &OLD_KEY, EvidenceStoreConfig::default()).expect("open src");

        // `doomed_scope` gets a random DEK and a body, then is
        // cryptographically forgotten (DEK destroyed + tombstoned).
        store.ensure_scope_dek(doomed_scope).expect("ensure dek");
        store
            .ingest(
                doomed_scope,
                b"secret to be forgotten",
                Some("source:test"),
                ImportanceClass::Important,
            )
            .expect("ingest doomed");

        let res = store
            .ingest(
                live_scope,
                &live_body,
                Some("source:test"),
                ImportanceClass::Important,
            )
            .expect("ingest live");
        live_id = res.evidence_id;

        // Forget the doomed scope: destroy its DEK and write the
        // durable tombstone.
        store.delete_scope_dek(doomed_scope).expect("delete dek");
        store
            .record_forgotten_scope(doomed_scope)
            .expect("record forgotten");

        let report = store.rotate_master_key(&NEW_KEY, &dest).expect("rotate");
        // Both evidence rows are copied (evidence is append-only)...
        assert_eq!(report.evidence_rows, 2);
        // ...but only the live scope's body is decrypted + verified.
        assert_eq!(report.bodies_verified, 1);
    }

    let rotated =
        EvidenceStore::open(&dest, &NEW_KEY, EvidenceStoreConfig::default()).expect("open rotated");
    assert_eq!(
        rotated.read_body(live_id).expect("read live body"),
        live_body
    );
}

/// Rotation must tolerate a *pre-existing* inconsistency where a scope
/// is tombstoned in `forgotten_scopes` but its `scope_deks` row was not
/// deleted (e.g. a crash between the DEK delete and the tombstone write
/// in `forget`). The orphaned row is wrapped under the old key and is
/// excluded from the re-wrap set, so without a defensive purge step 4's
/// re-open would fail trying to unwrap it under the new key. Rotation
/// must instead purge the stale row and complete cleanly, upholding the
/// forgetting guarantee (no DEK for a forgotten scope survives).
#[test]
fn rotation_purges_orphaned_dek_for_forgotten_scope() {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("substrate.db");
    let dest = dir.path().join("substrate.rotated.db");

    let live_scope = ScopeId::new_v4();
    let orphan_scope = ScopeId::new_v4();
    let live_body = b"survivor body".to_vec();

    let live_id;
    {
        let mut store =
            EvidenceStore::open(&src, &OLD_KEY, EvidenceStoreConfig::default()).expect("open src");

        store.ensure_scope_dek(orphan_scope).expect("ensure dek");
        store
            .ingest(
                orphan_scope,
                b"secret in an inconsistent scope",
                Some("source:test"),
                ImportanceClass::Important,
            )
            .expect("ingest orphan");

        let res = store
            .ingest(
                live_scope,
                &live_body,
                Some("source:test"),
                ImportanceClass::Important,
            )
            .expect("ingest live");
        live_id = res.evidence_id;

        // Simulate the crash window: write the tombstone but DO NOT
        // delete the DEK row, leaving an orphaned `scope_deks` entry
        // wrapped under the old master key.
        store
            .record_forgotten_scope(orphan_scope)
            .expect("record forgotten");

        // Rotation must succeed despite the orphaned row.
        store.rotate_master_key(&NEW_KEY, &dest).expect("rotate");
    }

    // The rotated copy must open cleanly under the new key — proof that
    // the orphaned old-key-wrapped DEK row was purged rather than left
    // to fail the re-open's scope-cache hydration.
    let rotated =
        EvidenceStore::open(&dest, &NEW_KEY, EvidenceStoreConfig::default()).expect("open rotated");
    assert_eq!(
        rotated.read_body(live_id).expect("read live body"),
        live_body
    );
}
