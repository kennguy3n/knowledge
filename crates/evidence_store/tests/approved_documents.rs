//! Integration tests for the `approved_document_payloads` table
//! (Phase 8 / schema v10; reshaped in Phase 10 Item 6 / schema v12
//! to back the payload bytes with the deduplicated `body_store`
//! table + per-scope CEK wraps in `body_store_key_wraps`).
//!
//! Each test exercises one slice of the contract the FFI surface
//! (`admit_approved_document`, `revoke_approved_document`,
//! `list_approved_documents`, and the tenant-synthesis dispatch
//! materialization step) leans on:
//!
//! * AEAD roundtrip through the body_store + per-scope wrap pair.
//! * Upsert overwrites the previous metadata row.
//! * Point and scope-grain deletes.
//! * Metadata listing returns id / size / hash without paying the
//!   decryption cost.
//! * Tampering with the body or wrap ciphertext fails AEAD
//!   decryption (defense-in-depth #1 + #2).
//! * Cross-scope content dedup: the same plaintext admitted into N
//!   scopes produces ONE body row + N wraps.

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
fn approved_doc_payload_body_cipher_tampering_fails_aead() {
    // v12 defense-in-depth #1: the AEAD on every `body_store` row
    // binds the BLAKE3 content_hash via `body_table_aad`, so an
    // attacker (or buggy migration) that corrupts the ciphertext
    // — without also forging a matching nonce + tag under the
    // randomly-generated CEK — must NOT be able to silently feed
    // garbage into a tenant-synthesis run.
    //
    // Pre-v12 this property was enforced per-row in
    // `approved_document_payloads` via `approved_doc_payload_aad`;
    // post-v12 it moves to the shared body table where the same
    // AAD discipline applies to every Phase-5 body row.
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();

    let payload = fake_payload(0x99, 1024);
    let hash = crypto::content_hash(&payload);
    store
        .save_approved_document_payload(scope, doc, &payload, &hash)
        .expect("save");

    // Flip a single byte of the stored ciphertext. The CEK + the
    // AEAD tag still claim integrity over the original plaintext +
    // AAD, so the tag check must reject the modified row.
    store
        .raw_conn()
        .execute(
            "UPDATE body_store \
             SET body = substr(body, 1, length(body) - 1) || x'00' \
             WHERE content_hash = ?1",
            params![hash.as_slice()],
        )
        .expect("corrupt body ciphertext");

    let err = store
        .load_approved_document_payload(scope, doc)
        .expect_err("tampered body ciphertext must fail AEAD");
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "body-cipher tamper must surface a descriptive error, got empty: {msg}",
    );
}

#[test]
fn approved_doc_payload_wrap_cipher_tampering_fails_aead() {
    // v12 defense-in-depth #2: the AEAD on every
    // `body_store_key_wraps` row binds content_hash via `wrap_cek`,
    // so a wrap relocated to a different content_hash row — or
    // corrupted in place — must NOT silently unwrap a CEK that
    // decrypts the original body. The AAD scope binding adds the
    // belt-and-braces guarantee that a wrap forged by another
    // scope cannot be unwrapped under this scope's DEK.
    let (_dir, store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();

    let payload = fake_payload(0xBB, 512);
    let hash = crypto::content_hash(&payload);
    store
        .save_approved_document_payload(scope_a, doc, &payload, &hash)
        .expect("save under scope_a");
    // Plant a *different* admitted payload under scope_b so the row
    // we copy onto exists. Reuse the same `doc` id; under v12 the
    // metadata row is (scope, doc)-keyed but the body is dedup'd by
    // content_hash so the bodies live in separate `body_store` rows.
    let placeholder = b"placeholder content for scope_b's metadata row" as &[u8];
    let placeholder_hash = crypto::content_hash(placeholder);
    store
        .save_approved_document_payload(scope_b, doc, placeholder, &placeholder_hash)
        .expect("save placeholder under scope_b");

    // Copy scope_a's wrapped CEK over scope_b's wrap. The wrap_cek
    // AAD binds (content_hash, scope_id) so even though scope_b's
    // metadata row now points at scope_a's body content, scope_b's
    // wrap of `placeholder`'s CEK cannot be unwrapped after this
    // tamper.
    let (wrapped_a, nonce_a): (Vec<u8>, Vec<u8>) = store
        .raw_conn()
        .query_row(
            "SELECT wrapped_cek, nonce FROM body_store_key_wraps \
             WHERE content_hash = ?1 AND scope_id = ?2",
            params![hash.as_slice(), scope_a.as_uuid().as_bytes().as_slice(),],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read scope_a wrap");
    store
        .raw_conn()
        .execute(
            "UPDATE body_store_key_wraps \
             SET wrapped_cek = ?1, nonce = ?2 \
             WHERE content_hash = ?3 AND scope_id = ?4",
            params![
                wrapped_a.as_slice(),
                nonce_a.as_slice(),
                placeholder_hash.as_slice(),
                scope_b.as_uuid().as_bytes().as_slice(),
            ],
        )
        .expect("relocate scope_a wrap into scope_b row");

    let err = store
        .load_approved_document_payload(scope_b, doc)
        .expect_err("cross-scope wrap relocation must fail AEAD");
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "wrap-cipher tamper must surface a descriptive error, got empty: {msg}",
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

#[test]
fn approved_doc_payload_dedups_identical_content_across_scopes() {
    // v12 contract: admitting identical content into N tenant scopes
    // costs one `body_store` row + N wraps. Verify that:
    //   1. Both scopes can decrypt the payload independently.
    //   2. Only ONE `body_store` row exists for the shared hash.
    //   3. Exactly N rows exist in `body_store_key_wraps`.
    let (_dir, store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let scope_c = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();
    let payload = fake_payload(0xDD, 4096);
    let hash = crypto::content_hash(&payload);

    for scope in [scope_a, scope_b, scope_c] {
        store
            .save_approved_document_payload(scope, doc, &payload, &hash)
            .unwrap_or_else(|e| panic!("save for {}: {e}", scope.as_uuid()));
    }

    for scope in [scope_a, scope_b, scope_c] {
        let loaded = store
            .load_approved_document_payload(scope, doc)
            .unwrap_or_else(|e| panic!("load for {}: {e}", scope.as_uuid()))
            .expect("row exists");
        assert_eq!(
            loaded,
            payload,
            "payload must roundtrip via dedup for {}",
            scope.as_uuid(),
        );
    }

    let body_rows: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM body_store WHERE content_hash = ?1",
            params![hash.as_slice()],
            |row| row.get(0),
        )
        .expect("count bodies");
    assert_eq!(body_rows, 1, "dedup must collapse to one body row");

    let wrap_rows: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM body_store_key_wraps WHERE content_hash = ?1",
            params![hash.as_slice()],
            |row| row.get(0),
        )
        .expect("count wraps");
    assert_eq!(
        wrap_rows, 3,
        "each scope must own its own wrap, three scopes admitted",
    );

    let ref_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT ref_count FROM body_store WHERE content_hash = ?1",
            params![hash.as_slice()],
            |row| row.get(0),
        )
        .expect("read ref_count");
    assert_eq!(
        ref_count, 3,
        "ref_count tracks total per-scope wrap admissions",
    );
}

#[test]
fn approved_doc_payload_migration_v11_to_v12_round_trips_legacy_payloads() {
    // Plant a real v11-shape row (inline nonce + payload columns,
    // PRAGMA user_version = 11), close the store, and verify the
    // post-bootstrap migration in `Self::open` moves the bytes
    // through the v12 body-store pipeline and drops the legacy
    // columns.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();
    let plaintext = fake_payload(0xEE, 3071);
    let hash = crypto::content_hash(&plaintext);

    {
        let store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).expect("open");
        store
            .write_legacy_approved_doc_payload_for_tests(scope, doc, &plaintext, &hash)
            .expect("plant legacy row");
    }

    // Reopen — migration runs as a post-bootstrap step.
    let store =
        EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).expect("reopen");

    let loaded = store
        .load_approved_document_payload(scope, doc)
        .expect("load after migration")
        .expect("row exists after migration");
    assert_eq!(
        loaded, plaintext,
        "v11 -> v12 migration must roundtrip plaintext",
    );

    // The legacy columns must be gone after the migration ran.
    let has_payload_column: bool = store
        .raw_conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 \
             FROM pragma_table_info('approved_document_payloads') \
             WHERE name = 'payload')",
            [],
            |row| row.get(0),
        )
        .expect("check column");
    assert!(
        !has_payload_column,
        "legacy `payload` column must be dropped after v12 migration",
    );
    let has_nonce_column: bool = store
        .raw_conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 \
             FROM pragma_table_info('approved_document_payloads') \
             WHERE name = 'nonce')",
            [],
            |row| row.get(0),
        )
        .expect("check column");
    assert!(
        !has_nonce_column,
        "legacy `nonce` column must be dropped after v12 migration",
    );

    // body_store + wrap must exist for the migrated row.
    let body_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM body_store WHERE content_hash = ?1",
            params![hash.as_slice()],
            |row| row.get(0),
        )
        .expect("count body rows");
    assert_eq!(
        body_count, 1,
        "migration must admit one body_store row for the migrated content",
    );
    let wrap_count: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM body_store_key_wraps \
             WHERE content_hash = ?1 AND scope_id = ?2",
            params![hash.as_slice(), scope.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count wraps");
    assert_eq!(
        wrap_count, 1,
        "migration must admit a per-scope wrap for the migrated content",
    );
}

#[test]
fn approved_doc_payload_replace_admits_new_body_and_leaves_old_wrap_for_forget() {
    // When a scope admits content C1 and then replaces it with C2,
    // the v12 design intentionally leaves the C1 wrap in place
    // until `purge_body_key_wraps_for_scope` (forget_scope step 3)
    // runs. The C2 admission adds a new wrap; the body_store table
    // ends up with both rows.
    let (_dir, store) = fresh_store();
    let scope = ScopeId::new_v4();
    let doc = uuid::Uuid::new_v4();

    let c1 = fake_payload(0x11, 1024);
    let c2 = fake_payload(0x22, 2048);
    let h1 = crypto::content_hash(&c1);
    let h2 = crypto::content_hash(&c2);

    store
        .save_approved_document_payload(scope, doc, &c1, &h1)
        .expect("save c1");
    store
        .save_approved_document_payload(scope, doc, &c2, &h2)
        .expect("save c2 (replace)");

    // Read-back must surface c2.
    let loaded = store
        .load_approved_document_payload(scope, doc)
        .expect("load")
        .expect("row exists");
    assert_eq!(loaded, c2, "replace must surface the new content");

    // Both body rows present (no eager GC on replace; forget cycle
    // handles it).
    for (hash, label) in [(h1, "c1"), (h2, "c2")] {
        let body_count: i64 = store
            .raw_conn()
            .query_row(
                "SELECT COUNT(*) FROM body_store WHERE content_hash = ?1",
                params![hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("count {label}: {e}"));
        assert_eq!(body_count, 1, "{label} body row must remain after replace");
        let wrap_count: i64 = store
            .raw_conn()
            .query_row(
                "SELECT COUNT(*) FROM body_store_key_wraps \
                 WHERE content_hash = ?1 AND scope_id = ?2",
                params![hash.as_slice(), scope.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("count {label} wrap: {e}"));
        assert_eq!(
            wrap_count, 1,
            "{label} per-scope wrap must remain after replace",
        );
    }
}
