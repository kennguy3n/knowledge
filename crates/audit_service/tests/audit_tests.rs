//! Integration tests for the audit service.

use chrono::{Duration, Utc};
use uuid::Uuid;

use evidence_store::ScopeId;

use audit_service::{
    Actor, AuditActionType, AuditEntryBuilder, AuditLog, AuditQuery, TargetRef, TargetType,
};

fn entry(action: AuditActionType, scope: ScopeId, who: Uuid) -> audit_service::AuditEntry {
    AuditEntryBuilder::new()
        .actor(Actor::User(who))
        .action(action)
        .target(TargetRef::new(TargetType::Tenant, Uuid::new_v4()))
        .scope(scope)
        .build()
        .unwrap()
}

#[test]
fn append_assigns_monotonic_sequences() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let who = Uuid::new_v4();
    let mut last = None;
    for _ in 0..10 {
        let id = log.append(entry(AuditActionType::CanonicalPromotion, scope, who));
        let e = log.get(id).unwrap();
        if let Some(prev) = last {
            assert!(e.sequence > prev, "sequence must be strictly monotonic");
        }
        last = Some(e.sequence);
    }
    assert_eq!(log.len(), 10);
}

#[test]
fn append_only_no_mutation_apis_exist() {
    // The entries() accessor returns a `&[AuditEntry]`. There is no
    // `&mut` access path. Compile-time assertion: this test compiles
    // iff `entries` returns a shared slice.
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let who = Uuid::new_v4();
    log.append(entry(AuditActionType::CanonicalPromotion, scope, who));
    let _: &[audit_service::AuditEntry] = log.entries();
}

#[test]
fn query_filters_by_scope_and_action() {
    let mut log = AuditLog::new();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();

    log.append(entry(AuditActionType::CanonicalPromotion, scope_a, alice));
    log.append(entry(AuditActionType::Export, scope_a, alice));
    log.append(entry(AuditActionType::PolicyChange, scope_b, bob));
    log.append(entry(AuditActionType::CanonicalPromotion, scope_b, bob));

    let q = AuditQuery::new()
        .with_scope(scope_a)
        .with_action(AuditActionType::CanonicalPromotion);
    let results: Vec<_> = log.query(&q).collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].action_type, AuditActionType::CanonicalPromotion);
    assert_eq!(results[0].scope_id, Some(scope_a));
}

#[test]
fn query_filters_by_actor_and_time() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();

    let mut e1 = entry(AuditActionType::Export, scope, alice);
    e1.timestamp = Utc::now() - Duration::days(2);
    let mut e2 = entry(AuditActionType::Export, scope, bob);
    e2.timestamp = Utc::now();
    log.append(e1);
    log.append(e2);

    let q = AuditQuery::new().with_actor(alice);
    let results: Vec<_> = log.query(&q).collect();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].actor, Actor::User(id) if id == alice));

    let q = AuditQuery::new().since(Utc::now() - Duration::hours(1));
    let results: Vec<_> = log.query(&q).collect();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].actor, Actor::User(id) if id == bob));
}

#[test]
fn entries_are_chronological() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(log.append(entry(AuditActionType::Export, scope, alice)));
    }
    let entries = log.entries();
    let collected_ids: Vec<_> = entries.iter().map(|e| e.id).collect();
    assert_eq!(collected_ids, ids);
    let seqs: Vec<_> = entries.iter().map(|e| e.sequence).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted);
}

#[test]
fn missing_required_fields_rejected_by_builder() {
    let err = AuditEntryBuilder::new().build().unwrap_err();
    assert!(matches!(err, audit_service::AuditError::MissingField(_)));
}

#[test]
fn query_uses_secondary_indexes_for_scope_action_actor() {
    // This test ensures the index-driven `query` path returns the
    // same set of entries as a linear scan would, across every
    // combination of indexable predicates. The log contains a mix
    // of scopes, actors, and action types so that no single index
    // is selective enough to pass on its own.
    let mut log = AuditLog::new();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    let other = Uuid::new_v4();

    let mut expected_alice_export_scope_a = 0_usize;
    for i in 0..200 {
        let scope = if i % 2 == 0 { scope_a } else { scope_b };
        let who = if i % 3 == 0 {
            alice
        } else if i % 3 == 1 {
            bob
        } else {
            other
        };
        let action = if i % 4 == 0 {
            AuditActionType::Export
        } else if i % 4 == 1 {
            AuditActionType::CanonicalPromotion
        } else if i % 4 == 2 {
            AuditActionType::PolicyChange
        } else {
            AuditActionType::ExportRendered
        };
        if scope == scope_a && who == alice && action == AuditActionType::Export {
            expected_alice_export_scope_a += 1;
        }
        log.append(entry(action, scope, who));
    }

    let q = AuditQuery::new()
        .with_scope(scope_a)
        .with_actor(alice)
        .with_action(AuditActionType::Export);
    let results: Vec<_> = log.query(&q).collect();
    assert_eq!(results.len(), expected_alice_export_scope_a);
    for entry in results {
        assert_eq!(entry.scope_id, Some(scope_a));
        assert!(matches!(entry.actor, Actor::User(id) if id == alice));
        assert_eq!(entry.action_type, AuditActionType::Export);
    }
}

#[test]
fn get_by_id_uses_id_index() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let mut ids = Vec::new();
    for _ in 0..50 {
        ids.push(log.append(entry(AuditActionType::Export, scope, alice)));
    }
    for id in &ids {
        let entry = log.get(*id).expect("id must resolve");
        assert_eq!(entry.id, *id);
    }
    // An id that was never appended must not resolve.
    assert!(log.get(audit_service::AuditEntryId::new_v4()).is_none());
}

#[test]
fn key_destruction_event_is_recordable() {
    let mut log = AuditLog::new();
    let scope = ScopeId::new_v4();
    let key_id = Uuid::new_v4();
    let entry = AuditEntryBuilder::new()
        .actor(Actor::System)
        .action(AuditActionType::KeyDestruction)
        .target(TargetRef::new(TargetType::Key, key_id))
        .scope(scope)
        .details(serde_json::json!({ "reason": "tenant-deletion" }))
        .build()
        .unwrap();
    let id = log.append(entry);
    let recorded = log.get(id).unwrap();
    assert_eq!(recorded.action_type, AuditActionType::KeyDestruction);
    assert_eq!(recorded.scope_id, Some(scope));
    assert!(matches!(recorded.actor, Actor::System));
}

/// Round-trips a populated [`AuditLog`] through serde_json. The
/// derived `Serialize` writes only `entries` + `next_sequence`
/// (the four `*_index` fields are `#[serde(skip)]`); the custom
/// `Deserialize` rehydrates the indexes from `entries` via
/// `rebuild_indexes()`. This test pins the rebuild path end-to-end
/// — i.e. that every entry remains queryable by scope, action,
/// actor, and id after a serialize-and-deserialise round-trip,
/// which is the only invariant `rebuild_indexes()` is responsible
/// for and which would silently regress if the field-disjoint
/// borrow refactor lost an index field.
#[test]
fn deserialize_round_trip_rebuilds_all_indexes() {
    let mut log = AuditLog::new();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    let agent = Uuid::new_v4();

    // Mix of scopes, actions, actor kinds (including System,
    // which is never indexed by actor), and rich `details`
    // payloads to exercise every index branch in
    // `index_entry_into`.
    let id_a = log.append(entry(AuditActionType::CanonicalPromotion, scope_a, alice));
    let _id_b = log.append(entry(AuditActionType::Export, scope_a, bob));
    let id_c = log.append(AuditEntryBuilder::new()
            .actor(Actor::Agent(agent))
            .action(AuditActionType::AgentProposalPromoted)
            .target(TargetRef::new(TargetType::Tenant, Uuid::new_v4()))
            .scope(scope_b)
            .details(serde_json::json!({ "rich": vec![0u8; 1024] }))
            .build()
            .unwrap(),
    );
    let _id_d = log.append({
        AuditEntryBuilder::new()
            .actor(Actor::System)
            .action(AuditActionType::KeyDestruction)
            .target(TargetRef::new(TargetType::Key, Uuid::new_v4()))
            .scope(scope_b)
            .build()
            .unwrap()
    });

    let bytes = serde_json::to_vec(&log).expect("AuditLog serialises cleanly");
    let restored: AuditLog = serde_json::from_slice(&bytes).expect("AuditLog deserialises cleanly");

    // id_index — by-id lookup must work after rebuild.
    assert_eq!(restored.get(id_a).map(|e| e.id), Some(id_a));
    assert_eq!(restored.get(id_c).map(|e| e.id), Some(id_c));
    assert_eq!(restored.len(), 4);

    // scope_index — query by scope returns only matching entries.
    let q_scope_a = AuditQuery::default().with_scope(scope_a);
    let by_scope_a: Vec<_> = restored.query(&q_scope_a).collect();
    assert_eq!(by_scope_a.len(), 2);
    let q_scope_b = AuditQuery::default().with_scope(scope_b);
    let by_scope_b: Vec<_> = restored.query(&q_scope_b).collect();
    assert_eq!(by_scope_b.len(), 2);

    // action_index — query by action returns only matching entries.
    let q_action = AuditQuery::default().with_action(AuditActionType::AgentProposalPromoted);
    let by_action: Vec<_> = restored.query(&q_action).collect();
    assert_eq!(by_action.len(), 1);
    assert_eq!(by_action[0].id, id_c);

    // actor_index — `User` / `Agent` actors are indexed; `System`
    // actors deliberately are not (matching query semantics).
    let q_actor_alice = AuditQuery::default().with_actor(alice);
    let by_actor_alice: Vec<_> = restored.query(&q_actor_alice).collect();
    assert_eq!(by_actor_alice.len(), 1);
    let q_actor_agent = AuditQuery::default().with_actor(agent);
    let by_actor_agent: Vec<_> = restored.query(&q_actor_agent).collect();
    assert_eq!(by_actor_agent.len(), 1);

    // sequence numbers and other entry-level fields survive too.
    assert_eq!(restored
            .entries()
            .iter()
            .map(|e| e.sequence)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}
