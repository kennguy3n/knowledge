//! Integration tests for the `approved_document_payloads` table
//! (Phase 8 / schema v10).
//!
//! Each test exercises one slice of the contract the FFI surface
//! (`admit_approved_document`, `revoke_approved_document`,
//! `list_approved_documents`, and the tenant-synthesis dispatch
//! materialization step) leans on:
//!
//! * AEAD roundtrip under the per-scope DEK with AAD binding
//!   `(scope_id, document_id)`.
//! * Upsert overwrites the previous ciphertext / hash / size.
//! * Point and scope-grain deletes.
//! * Metadata listing returns id / size / hash without paying the
//!   decryption cost.
//! * Tampering with the row key (relocating a ciphertext to a
//!   different `document_id` or `scope_id`) fails AEAD decryption
//!   instead of silently aliasing onto a different identity.

use evidence_store::{ApprovedDocumentPayloadMeta, EvidenceStore, EvidenceStoreConfig, ScopeId};
use rusqlite::params;
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xC8; 32];

fn fresh_store() -> (tempfile::TempDir, EvidenceStore) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");
    (dir, store)
}

fn fake_payload(seed: u8, len: usize) -> Vec<u8> {
    // Deterministic-but-non-uniform plaintext for AEAD tests.
    // `u8::wrapping_add` over a `(i % 256) -> u8` index keeps the
    // byte sequence stable across test invocations without leaking a
    // truncating `as u8` cast that clippy rejects under
    // `cast_possible_truncation`.
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let masked = u8::try_from(i & 0xff).expect("0..=255 fits in u8");
        out.push(seed.wrapping_add(masked));
    }
    out
}

#[test]
fn approved_doc_payload_roundtrips_under_aead() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();
    let payload = fake_payload(0x11, 4096);
    let hash = crypto::content_hash(&payload);

    store
        .save_approved_document_payload(scope, doc, &payload, &hash)
        .expect("save");

    let loaded = store
        .load_approved_document_payload(scope, doc)
        .expect("load")
        .expect("row exists");
    assert_eq!(loaded, payload, "ciphertext must decrypt to original bytes");
}

#[test]
fn approved_doc_payload_load_returns_none_when_missing() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();
    let got = store
        .load_approved_document_payload(scope, doc)
        .expect("load");
    assert!(got.is_none(), "missing row must be None, not Err");
}

#[test]
fn approved_doc_payload_upsert_overwrites_previous_row() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();

    let first = fake_payload(0x22, 256);
    let first_hash = crypto::content_hash(&first);
    store
        .save_approved_document_payload(scope, doc, &first, &first_hash)
        .expect("first save");

    let second = fake_payload(0x33, 8192);
    let second_hash = crypto::content_hash(&second);
    store
        .save_approved_document_payload(scope, doc, &second, &second_hash)
        .expect("second save");

    let loaded = store
        .load_approved_document_payload(scope, doc)
        .expect("load")
        .expect("row exists");
    assert_eq!(
        loaded, second,
        "re-admission must overwrite the prior payload"
    );

    let meta = store
        .list_approved_document_payload_meta_for_scope(scope)
        .expect("list meta");
    assert_eq!(meta.len(), 1, "upsert must keep the row count at 1");
    assert_eq!(meta[0].size_bytes, second.len() as u64);
    assert_eq!(meta[0].content_hash, second_hash);
}

#[test]
fn approved_doc_payload_delete_single_purges_row_but_not_siblings() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let doc_a = uuid::Uuid::new_v4();
    let doc_b = uuid::Uuid::new_v4();

    let payload_a = fake_payload(0x44, 1024);
    let payload_b = fake_payload(0x55, 1024);
    store
        .save_approved_document_payload(scope, doc_a, &payload_a, &crypto::content_hash(&payload_a))
        .expect("save a");
    store
        .save_approved_document_payload(scope, doc_b, &payload_b, &crypto::content_hash(&payload_b))
        .expect("save b");

    let removed = store
        .delete_approved_document_payload(scope, doc_a)
        .expect("delete a");
    assert_eq!(removed, 1, "delete must report exactly one row removed");

    assert!(
        store
            .load_approved_document_payload(scope, doc_a)
            .expect("load a")
            .is_none(),
        "doc_a row must be gone",
    );
    assert!(
        store
            .load_approved_document_payload(scope, doc_b)
            .expect("load b")
            .is_some(),
        "doc_b row must survive a sibling delete",
    );

    // No-op second delete must succeed and report zero rows.
    let removed = store
        .delete_approved_document_payload(scope, doc_a)
        .expect("delete a again");
    assert_eq!(removed, 0, "re-deleting a missing row must report 0");
}

#[test]
fn delete_approved_document_payloads_for_scope_purges_all_rows_for_that_scope() {
    let (_dir, store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();

    for _ in 0..3 {
        let payload = fake_payload(0x66, 512);
        let hash = crypto::content_hash(&payload);
        store
            .save_approved_document_payload(scope_a, uuid::Uuid::new_v4(), &payload, &hash)
            .expect("save scope_a");
    }
    for _ in 0..2 {
        let payload = fake_payload(0x77, 512);
        let hash = crypto::content_hash(&payload);
        store
            .save_approved_document_payload(scope_b, uuid::Uuid::new_v4(), &payload, &hash)
            .expect("save scope_b");
    }

    let removed = store
        .delete_approved_document_payloads_for_scope(scope_a)
        .expect("delete scope_a");
    assert_eq!(removed, 3, "all 3 scope_a rows must be removed");

    assert!(
        store
            .list_approved_document_payload_meta_for_scope(scope_a)
            .expect("list a")
            .is_empty(),
        "scope_a must have no rows left",
    );
    assert_eq!(
        store
            .list_approved_document_payload_meta_for_scope(scope_b)
            .expect("list b")
            .len(),
        2,
        "scope_b rows must survive scope_a's purge",
    );
}

#[test]
fn list_approved_document_payload_meta_returns_size_and_hash_without_decrypting() {
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();

    let mut expected: Vec<ApprovedDocumentPayloadMeta> = Vec::new();
    for i in 0..4u8 {
        let doc = uuid::Uuid::new_v4();
        let payload = fake_payload(0x80 + i, (i as usize + 1) * 1024);
        let hash = crypto::content_hash(&payload);
        store
            .save_approved_document_payload(scope, doc, &payload, &hash)
            .expect("save");
        expected.push(ApprovedDocumentPayloadMeta {
            document_id: doc,
            content_hash: hash,
            size_bytes: payload.len() as u64,
            updated_at: 0, // not asserted
        });
    }

    let mut got = store
        .list_approved_document_payload_meta_for_scope(scope)
        .expect("list");
    // Order is unspecified — sort both sides by document_id for a
    // stable comparison.
    got.sort_by_key(|m| m.document_id);
    expected.sort_by_key(|m| m.document_id);

    assert_eq!(got.len(), expected.len(), "row count must match");
    for (g, e) in got.iter().zip(expected.iter()) {
        assert_eq!(g.document_id, e.document_id, "document_id");
        assert_eq!(
            g.content_hash, e.content_hash,
            "content_hash (no decrypt needed)"
        );
        assert_eq!(g.size_bytes, e.size_bytes, "size_bytes (no decrypt needed)");
        assert!(g.updated_at > 0, "updated_at must be a real timestamp");
    }
}

#[test]
fn approved_doc_payload_aad_rejects_cross_document_cipher_relocation() {
    // Defense-in-depth: an attacker (or a buggy migration) that
    // copies a ciphertext payload from row (scope, doc_a) into
    // (scope, doc_b) must NOT be able to silently feed doc_a's
    // payload into a tenant-synthesis run keyed by doc_b. The AAD
    // includes the document id, so the AEAD `decrypt_aead` call
    // surfaces a structured `Crypto` failure on the relocated row.
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let doc_a = uuid::Uuid::new_v4();
    let doc_b = uuid::Uuid::new_v4();

    let payload_a = fake_payload(0x99, 1024);
    let payload_b = fake_payload(0xAA, 1024);
    store
        .save_approved_document_payload(scope, doc_a, &payload_a, &crypto::content_hash(&payload_a))
        .expect("save a");
    store
        .save_approved_document_payload(scope, doc_b, &payload_b, &crypto::content_hash(&payload_b))
        .expect("save b");

    // Surgically copy doc_a's ciphertext into doc_b's row. This is
    // exactly the silent-aliasing attack the AAD is supposed to
    // prevent.
    let (nonce_a, payload_a_cipher): (Vec<u8>, Vec<u8>) = store
        .raw_conn()
        .query_row(
            "SELECT nonce, payload FROM approved_document_payloads \
             WHERE scope_id = ?1 AND document_id = ?2",
            params![
                scope.as_uuid().as_bytes().as_slice(),
                doc_a.as_bytes().as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read a");
    store
        .raw_conn()
        .execute(
            "UPDATE approved_document_payloads SET nonce = ?1, payload = ?2 \
             WHERE scope_id = ?3 AND document_id = ?4",
            params![
                nonce_a.as_slice(),
                payload_a_cipher.as_slice(),
                scope.as_uuid().as_bytes().as_slice(),
                doc_b.as_bytes().as_slice(),
            ],
        )
        .expect("relocate cipher");

    let err = store
        .load_approved_document_payload(scope, doc_b)
        .expect_err("relocated cipher must fail AEAD");
    // The error kind itself is not part of the public contract;
    // the load-time check that matters is that we do NOT return
    // `Ok(Some(payload_a))` from a `doc_b` query.
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "AAD mismatch must surface a descriptive error, got empty: {msg}",
    );
}

#[test]
fn approved_doc_payload_aad_rejects_cross_scope_cipher_relocation() {
    // Same defense-in-depth check as the cross-document test, but
    // for the scope axis: a ciphertext from scope_a's row must not
    // decrypt under scope_b's DEK even if the row blob is moved
    // across (different DEK means the AEAD tag check fails before
    // AAD enters the picture, but the AAD scope binding is the
    // belt-and-braces layer that catches a future regression of
    // scope-key derivation collapse).
    let (_dir, store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();

    let payload = fake_payload(0xBB, 512);
    store
        .save_approved_document_payload(scope_a, doc, &payload, &crypto::content_hash(&payload))
        .expect("save a");
    // Plant a second row for scope_b with the same document_id so we
    // have somewhere to relocate the ciphertext into.
    store
        .save_approved_document_payload(
            scope_b,
            doc,
            b"placeholder",
            &crypto::content_hash(b"placeholder"),
        )
        .expect("save b placeholder");

    let (nonce_a, cipher_a): (Vec<u8>, Vec<u8>) = store
        .raw_conn()
        .query_row(
            "SELECT nonce, payload FROM approved_document_payloads \
             WHERE scope_id = ?1 AND document_id = ?2",
            params![
                scope_a.as_uuid().as_bytes().as_slice(),
                doc.as_bytes().as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read a");
    store
        .raw_conn()
        .execute(
            "UPDATE approved_document_payloads SET nonce = ?1, payload = ?2 \
             WHERE scope_id = ?3 AND document_id = ?4",
            params![
                nonce_a.as_slice(),
                cipher_a.as_slice(),
                scope_b.as_uuid().as_bytes().as_slice(),
                doc.as_bytes().as_slice(),
            ],
        )
        .expect("relocate cipher across scopes");

    let err = store
        .load_approved_document_payload(scope_b, doc)
        .expect_err("cross-scope relocated cipher must fail AEAD");
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "cross-scope AAD mismatch must surface a descriptive error",
    );
}

#[test]
fn approved_doc_payload_survives_store_close_and_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();
    let payload = fake_payload(0xCC, 7777);
    let hash = crypto::content_hash(&payload);

    {
        let store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).expect("open");
        store
            .save_approved_document_payload(scope, doc, &payload, &hash)
            .expect("save");
    }
    // Re-open the same SQLCipher file with the same master key.
    let store =
        EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).expect("reopen");
    let loaded = store
        .load_approved_document_payload(scope, doc)
        .expect("load after reopen")
        .expect("row exists after reopen");
    assert_eq!(
        loaded, payload,
        "payload must roundtrip through SQLCipher restart"
    );

    let meta = store
        .list_approved_document_payload_meta_for_scope(scope)
        .expect("list after reopen");
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].content_hash, hash);
}
