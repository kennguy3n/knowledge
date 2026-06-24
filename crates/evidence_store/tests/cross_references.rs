//! Integration tests for the cross-reference graph.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, StoragePath,
};
use tempfile::tempdir;

const MASTER_KEY: [u8; 32] = [0xA5; 32];

fn fresh_store() -> (tempfile::TempDir, EvidenceStore) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store");
    (dir, store)
}

#[test]
fn add_and_find_cross_references() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Ingest two emails in the same Gmail thread.
    let r1 = store
        .ingest_with_references(
            scope,
            b"Email reply about PostgreSQL decision.",
            Some("gmail:msg:M001"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "t-abc-123")],
        )
        .unwrap();
    let r2 = store
        .ingest_with_references(
            scope,
            b"Follow-up email confirming PostgreSQL.",
            Some("gmail:msg:M002"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "t-abc-123")],
        )
        .unwrap();

    // r1 should find r2 as a cross-reference.
    let related = store.find_cross_references(r1.evidence_id).unwrap();
    assert!(related.contains(&r2.evidence_id));

    // r2 should find r1 as a cross-reference.
    let related = store.find_cross_references(r2.evidence_id).unwrap();
    assert!(related.contains(&r1.evidence_id));
}

#[test]
fn cross_references_link_across_sources() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    // Slack message and Email share the same conversation_id.
    let r_slack = store
        .ingest_with_references(
            scope,
            b"We decided to use PostgreSQL in the Slack channel.",
            Some("slack:msg:C001"),
            ImportanceClass::Important,
            None,
            &[("conversation_id", "conv-xyz-789")],
        )
        .unwrap();
    let r_email = store
        .ingest_with_references(
            scope,
            b"Re: Database selection - PostgreSQL confirmed.",
            Some("gmail:msg:M001"),
            ImportanceClass::Important,
            None,
            &[("conversation_id", "conv-xyz-789")],
        )
        .unwrap();
    let r_github = store
        .ingest_with_references(
            scope,
            b"Database decision: PostgreSQL. See slack thread for context.",
            Some("github:issue:789"),
            ImportanceClass::Important,
            None,
            &[("conversation_id", "conv-xyz-789")],
        )
        .unwrap();

    // All three should be cross-referenced.
    let related_slack = store.find_cross_references(r_slack.evidence_id).unwrap();
    assert_eq!(related_slack.len(), 2);
    assert!(related_slack.contains(&r_email.evidence_id));
    assert!(related_slack.contains(&r_github.evidence_id));

    let related_email = store.find_cross_references(r_email.evidence_id).unwrap();
    assert_eq!(related_email.len(), 2);
    assert!(related_email.contains(&r_slack.evidence_id));
    assert!(related_email.contains(&r_github.evidence_id));
}

#[test]
fn find_by_reference_returns_all_evidence() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    store
        .ingest_with_references(
            scope,
            b"First message in thread.",
            Some("slack:msg:A"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "thr-001")],
        )
        .unwrap();
    store
        .ingest_with_references(
            scope,
            b"Second message in thread.",
            Some("slack:msg:B"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "thr-001")],
        )
        .unwrap();
    store
        .ingest_with_references(
            scope,
            b"Third message in thread.",
            Some("slack:msg:C"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "thr-001")],
        )
        .unwrap();

    let all = store.find_by_reference("thread_id", "thr-001").unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn no_cross_references_returns_empty() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let r = store
        .ingest(
            scope,
            b"Standalone message with no references.",
            Some("slack:msg:X"),
            ImportanceClass::Important,
        )
        .unwrap();

    let related = store.find_cross_references(r.evidence_id).unwrap();
    assert!(related.is_empty());
}

#[test]
fn get_references_for_evidence() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let r = store
        .ingest_with_references(
            scope,
            b"Email with threading metadata.",
            Some("gmail:msg:M001"),
            ImportanceClass::Important,
            None,
            &[
                ("thread_id", "t-abc-123"),
                ("conversation_id", "conv-xyz-789"),
            ],
        )
        .unwrap();

    let refs = store.get_references_for_evidence(r.evidence_id).unwrap();
    assert_eq!(refs.len(), 2);
    assert!(refs.contains(&("thread_id".to_string(), "t-abc-123".to_string())));
    assert!(refs.contains(&("conversation_id".to_string(), "conv-xyz-789".to_string())));
}

#[test]
fn delete_cross_references_for_scope() {
    let (_dir, mut store) = fresh_store();
    let scope1 = ScopeId::new_v4();
    let scope2 = ScopeId::new_v4();

    store
        .ingest_with_references(
            scope1,
            b"Scope 1 message.",
            Some("slack:msg:A"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "thr-shared")],
        )
        .unwrap();
    store
        .ingest_with_references(
            scope2,
            b"Scope 2 message.",
            Some("slack:msg:B"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "thr-shared")],
        )
        .unwrap();

    // Verify both have cross-references.
    let all = store.find_by_reference("thread_id", "thr-shared").unwrap();
    assert_eq!(all.len(), 2);

    // Delete scope1's cross-references.
    store.delete_cross_references_for_scope(scope1).unwrap();

    // Only scope2's reference should remain.
    let all = store.find_by_reference("thread_id", "thr-shared").unwrap();
    assert_eq!(all.len(), 1);
}

#[test]
fn noise_class_does_not_get_cross_references() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let r = store
        .ingest_with_references(
            scope,
            b"+1",
            Some("slack:msg:A"),
            ImportanceClass::Noise,
            None,
            &[("thread_id", "thr-001")],
        )
        .unwrap();

    // Noise goes to ring buffer — no evidence row, no cross-reference.
    assert_eq!(r.storage_path, StoragePath::RingBuffer);
    let refs = store.get_references_for_evidence(r.evidence_id).unwrap();
    assert!(refs.is_empty());
}

#[test]
fn multiple_ref_keys_for_same_evidence() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let r = store
        .ingest_with_references(
            scope,
            b"Email that is part of a thread and references an issue.",
            Some("gmail:msg:M001"),
            ImportanceClass::Important,
            None,
            &[
                ("thread_id", "t-abc-123"),
                ("issue_number", "42"),
                ("conversation_id", "conv-xyz-789"),
            ],
        )
        .unwrap();

    let refs = store.get_references_for_evidence(r.evidence_id).unwrap();
    assert_eq!(refs.len(), 3);

    // Should be able to find this evidence by any of its references.
    let by_thread = store.find_by_reference("thread_id", "t-abc-123").unwrap();
    assert_eq!(by_thread.len(), 1);
    assert_eq!(by_thread[0], r.evidence_id);

    let by_issue = store.find_by_reference("issue_number", "42").unwrap();
    assert_eq!(by_issue.len(), 1);
    assert_eq!(by_issue[0], r.evidence_id);

    let by_conv = store.find_by_reference("conversation_id", "conv-xyz-789").unwrap();
    assert_eq!(by_conv.len(), 1);
    assert_eq!(by_conv[0], r.evidence_id);
}

#[test]
fn cross_reference_deduplication() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();

    let r = store
        .ingest_with_references(
            scope,
            b"Message with a thread reference.",
            Some("slack:msg:A"),
            ImportanceClass::Important,
            None,
            &[("thread_id", "thr-001")],
        )
        .unwrap();

    // Adding the same cross-reference again should be a no-op (INSERT OR IGNORE).
    store
        .add_cross_reference(r.evidence_id, scope, "thread_id", "thr-001")
        .unwrap();

    let refs = store.get_references_for_evidence(r.evidence_id).unwrap();
    assert_eq!(refs.len(), 1);
}
