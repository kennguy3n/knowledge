//! B2B end-to-end lifecycle integration tests.
//!
//! Each test exercises an enterprise-facing lifecycle straight through
//! the real substrate crates (`evidence_store`, `connector_framework`,
//! `connectors`, `agent_contract`, `audit_service`,
//! `permission_service`), never a mock of the substrate itself.
//!
//! Scenarios:
//!
//! * `enterprise_knowledge_base` — multiple enterprise sources ingest
//!   into one shared scope, and hybrid retrieval surfaces evidence
//!   regardless of which source produced it.
//! * `multi_tenant_isolation` — two tenant scopes; retrieval, evidence
//!   ownership, permission grants, and cryptographic forgetting are all
//!   strictly isolated (no cross-tenant leakage).
//! * `connector_dedup` — the same upstream item arriving twice (a
//!   boundary record re-surfaced at the [`WatermarkCursor`] watermark)
//!   is ingested exactly once.
//! * `agent_proposal_lifecycle` — an agent proposal flows through the
//!   agent contract approval path (submit → review → promote →
//!   canonical) with matching audit-log entries.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{Duration, Utc};
use uuid::Uuid;

use agent_contract::{
    AgentIdentity, AgentProposal, AutoPromotionPolicy, CanonicalArtifact, ObservationProposal,
    ProposalDecision, ProposalKind, ProposalState, ProposalStore,
};
use audit_service::{
    log_proposal_promoted, log_proposal_submitted, Actor, AuditActionType, AuditLog, AuditQuery,
};
use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorEvent, ConnectorInstanceId, ConnectorKind,
    HttpMethod, HttpTransport, MockHttpTransport, MockResponse, OAuth2CodeExchange, OAuth2Token,
    Result as ConnectorResult, SyncState,
};
use connectors::GitHubConnector;
use crypto::EvidenceRef;
use evidence_store::{EvidenceId, HybridRetriever};
use memory_manager::SensitivityClass;
use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

use integration_tests::test_helpers::{open_store, padded_body, ImportanceClass, ScopeId};

// ── shared connector helpers (mirrors connector_sync_cycle.rs) ──────

struct FixedOAuth;
impl OAuth2CodeExchange for FixedOAuth {
    fn exchange_code(
        &self,
        _config: &ConnectorConfig,
        _code: &str,
    ) -> ConnectorResult<OAuth2Token> {
        Ok(OAuth2Token::new(
            "test-access",
            "test-refresh",
            Utc::now() + Duration::hours(1),
            "test-scope",
        ))
    }
}

fn oauth() -> Arc<dyn OAuth2CodeExchange> {
    Arc::new(FixedOAuth)
}

fn issues_page(issues: &[serde_json::Value]) -> MockResponse {
    MockResponse::ok_json(serde_json::to_vec(issues).unwrap())
}

/// Ingest the documents named by a connector sync run, skipping any
/// source-document id already present. Returns the count of new rows.
fn ingest_sync_events(
    store: &mut evidence_store::EvidenceStore,
    scope: ScopeId,
    events: &[ConnectorEvent],
    seen: &mut HashSet<String>,
) -> usize {
    let mut written = 0;
    for ev in events {
        if matches!(ev, ConnectorEvent::DocumentDeleted { .. }) {
            continue;
        }
        let doc_id = ev.document_id().as_str().to_string();
        if !seen.insert(doc_id.clone()) {
            continue;
        }
        let body = padded_body(&format!(
            "github issue {doc_id} tracking the rollout deadline"
        ));
        store
            .ingest(
                scope,
                &body,
                Some(&format!("github:{doc_id}")),
                ImportanceClass::Useful,
            )
            .expect("ingest connector document");
        written += 1;
    }
    written
}

// ── enterprise_knowledge_base ───────────────────────────────────────

#[test]
fn enterprise_knowledge_base() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    // One shared enterprise scope (a `Channel`/team workspace).
    let scope = ScopeId::new_v4();
    let mut store = open_store(&db_path);

    // Multi-source ingest: docs, tickets, chat and wiki all land in the
    // same shared knowledge base, each citing its upstream source.
    let sources = [
        ("confluence:eng-handbook", "The on-call rotation policy requires a primary and a secondary responder."),
        ("jira:OPS-204", "Incident OPS-204: the payments service exceeded its latency budget during the migration."),
        ("slack:#ops", "Heads up team: the migration window is scheduled for Saturday night."),
        ("notion:runbook", "Runbook: to roll back the migration, restore the previous deployment manifest."),
    ];
    let mut by_source = Vec::new();
    for (source, text) in sources {
        let body = padded_body(text);
        let res = store
            .ingest(scope, &body, Some(source), ImportanceClass::Important)
            .expect("ingest enterprise source");
        by_source.push((source, res.evidence_id));
    }

    // Hybrid retrieval fans across every source, not just one.
    let retriever = HybridRetriever::new(&store);
    let hits = retriever
        .search_hybrid(scope, "migration", 10)
        .expect("hybrid search");
    let hit_ids: HashSet<EvidenceId> = hits.iter().map(|h| h.evidence_id).collect();
    assert!(
        hit_ids.len() >= 3,
        "the migration query spans multiple sources, got {} hits",
        hit_ids.len()
    );

    // A source-specific query resolves to the right upstream document.
    let rollback = retriever
        .search_hybrid(scope, "roll back restore deployment manifest", 5)
        .expect("hybrid search");
    let runbook_id = by_source
        .iter()
        .find(|(s, _)| *s == "notion:runbook")
        .map(|(_, id)| *id)
        .unwrap();
    assert_eq!(
        rollback[0].evidence_id, runbook_id,
        "the runbook ranks first for a rollback query"
    );
}

// ── multi_tenant_isolation ──────────────────────────────────────────

#[test]
fn multi_tenant_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    // Two tenants, each with its own scope, owner, and data.
    let tenant_a = ScopeId::new_v4();
    let tenant_b = ScopeId::new_v4();
    let admin_a = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let admin_b = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    let mut tuples = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let obj_a = ObjectRef::new(ObjectType::Domain, tenant_a.as_uuid());
    let obj_b = ObjectRef::new(ObjectType::Domain, tenant_b.as_uuid());
    tuples
        .insert(RelationTuple::new(obj_a, Relation::Owner, admin_a))
        .unwrap();
    tuples
        .insert(RelationTuple::new(obj_b, Relation::Owner, admin_b))
        .unwrap();

    // Permission isolation: neither admin has any relation on the other
    // tenant's scope.
    assert!(check_permission(
        &tuples,
        &ns,
        obj_a,
        Relation::Owner,
        admin_a
    ));
    assert!(check_permission(
        &tuples,
        &ns,
        obj_b,
        Relation::Owner,
        admin_b
    ));
    assert!(!check_permission(
        &tuples,
        &ns,
        obj_b,
        Relation::Viewer,
        admin_a
    ));
    assert!(!check_permission(
        &tuples,
        &ns,
        obj_a,
        Relation::Viewer,
        admin_b
    ));

    // Ingest tenant-private data with a shared keyword so a naive query
    // would leak across tenants if scoping were broken.
    let mut store = open_store(&db_path);
    let a_body = padded_body("tenant Acme confidential roadmap budget forecast");
    let a = store
        .ingest(tenant_a, &a_body, Some("crm"), ImportanceClass::Important)
        .unwrap();
    let b_body = padded_body("tenant Globex confidential roadmap budget forecast");
    let b = store
        .ingest(tenant_b, &b_body, Some("crm"), ImportanceClass::Important)
        .unwrap();

    // Retrieval isolation: each tenant sees only its own row even for
    // the shared keyword.
    let a_hits = store.search_fts(tenant_a, "roadmap", 100).unwrap();
    assert_eq!(a_hits, vec![a.evidence_id], "tenant A sees only its row");
    let b_hits = store.search_fts(tenant_b, "roadmap", 100).unwrap();
    assert_eq!(b_hits, vec![b.evidence_id], "tenant B sees only its row");
    // The tenant-name token is unique to its own scope.
    assert!(store.search_fts(tenant_b, "Acme", 100).unwrap().is_empty());
    assert!(store
        .search_fts(tenant_a, "Globex", 100)
        .unwrap()
        .is_empty());

    // Key isolation: forgetting tenant A destroys only A's keys.
    store.purge_body_key_wraps_for_scope(tenant_a).unwrap();
    store.purge_fts_for_scope(tenant_a).unwrap();
    store.record_forgotten_scope(tenant_a).unwrap();
    store.delete_scope_dek(tenant_a).unwrap();

    assert!(
        store.read_body(a.evidence_id).is_err(),
        "tenant A evidence is irrecoverable after its own erasure"
    );
    assert_eq!(
        store.read_body(b.evidence_id).unwrap(),
        b_body,
        "tenant B evidence is wholly unaffected by tenant A's erasure"
    );
    assert!(
        store.search_fts(tenant_b, "roadmap", 100).unwrap().len() == 1,
        "tenant B retrieval still works after tenant A is forgotten"
    );

    // Only tenant A is tombstoned.
    let tombstones = store.load_forgotten_scopes().unwrap();
    assert!(tombstones.contains(&tenant_a));
    assert!(!tombstones.contains(&tenant_b));
}

// ── connector_dedup ─────────────────────────────────────────────────

#[test]
fn connector_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let scope = ScopeId::new_v4();
    let inst = ConnectorInstanceId::new_v4();
    let base = "https://api.test";
    let repo = "globex/platform";
    let now = Utc::now();
    let cfg = ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, scope)
        .with_auth_config(serde_json::json!({
            "authorization_code": "code",
            "repository": repo,
            "api_base_url": base,
            "webhook_secret": "test-webhook-secret",
        }));

    // First sync surfaces a single issue at instant `t`.
    let transport = MockHttpTransport::new();
    transport.expect(
        HttpMethod::Get,
        format!(
            "{base}/repos/{repo}/issues\
             ?state=all&sort=updated&direction=asc&per_page=100&page=1"
        ),
        issues_page(&[serde_json::json!({
            "number": 7, "id": 7, "title": "Flaky deploy", "state": "open",
            "created_at": now - Duration::hours(1),
            "updated_at": now,
        })]),
    );
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    let connector = GitHubConnector::new(inst, transport, oauth()).with_api_base_url(base);
    let token = connector.authenticate(&cfg).unwrap();
    let first = connector.initial_sync(&cfg, &token).unwrap();
    assert_eq!(first.events.len(), 1);
    assert!(first.next_cursor.is_some());

    let mut store = open_store(&db_path);
    let mut seen: HashSet<String> = HashSet::new();
    let written_first = ingest_sync_events(&mut store, scope, &first.events, &mut seen);
    assert_eq!(written_first, 1, "the issue is ingested on first sight");

    // Second sync re-surfaces the *same* issue #7 at the same
    // `updated_at` watermark (a provider replaying the boundary record).
    // The WatermarkCursor, seeded with #7 as a boundary id, drops it.
    // The incremental list query carries a `since=<watermark>` filter we
    // do not assert on here, so a default response keeps the test
    // focused on the dedup behaviour rather than URL shape.
    let transport2 = MockHttpTransport::new();
    transport2.with_default_response(issues_page(&[serde_json::json!({
        "number": 7, "id": 7, "title": "Flaky deploy", "state": "open",
        "created_at": now - Duration::hours(1),
        "updated_at": now,
    })]));
    let transport2: Arc<dyn HttpTransport> = Arc::new(transport2);
    let connector2 = GitHubConnector::new(inst, transport2, oauth()).with_api_base_url(base);
    let mut state = SyncState::new(inst);
    state.cursor = first.next_cursor;
    let second = connector2.incremental_sync(&cfg, &token, &state).unwrap();
    assert!(
        second.events.is_empty(),
        "the re-surfaced boundary issue is deduped by the WatermarkCursor"
    );

    // Framework-level dedup is proven above. Now exercise the store-side
    // idempotency guard directly by replaying the *original* event (the
    // item already ingested on first sight) as a non-empty batch: an
    // already-seen upstream id must still write no second row.
    let replayed = ingest_sync_events(&mut store, scope, &first.events, &mut seen);
    assert_eq!(
        replayed, 0,
        "re-ingesting an already-seen upstream item writes no duplicate row"
    );

    // Exactly one evidence row exists for the upstream item.
    let hits = store.search_fts(scope, "deadline", 100).unwrap();
    assert_eq!(hits.len(), 1, "the upstream item is ingested exactly once");
}

// ── agent_proposal_lifecycle ────────────────────────────────────────

#[test]
fn agent_proposal_lifecycle() {
    let scope = ScopeId::new_v4();
    let agent_id = Uuid::new_v4();
    let admin_id = Uuid::new_v4();

    let identity = AgentIdentity::new(agent_id, "enterprise-agent", "qwen3.5-2b", "v1");
    let evidence_ref = EvidenceRef::from_uuid(Uuid::new_v4());
    let payload = ObservationProposal::new(
        "The Q3 board approved the EU data-residency rollout",
        "decision",
    );

    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        scope,
        payload,
        vec![evidence_ref],
        0.9,
        SensitivityClass::Important,
        identity,
    );
    let proposal_id = proposal.id;

    // 1. Submit — recorded but not canonical.
    let mut store = ProposalStore::new();
    let id = store.submit_observation(proposal).unwrap();
    assert_eq!(id, proposal_id);
    assert_eq!(store.get(id).unwrap().state, ProposalState::Proposed);

    let mut audit = AuditLog::new();
    log_proposal_submitted(&mut audit, id, agent_id, scope).unwrap();

    // 2. Review under the deny-by-default policy → human review.
    let policy = AutoPromotionPolicy::default();
    let decision = store.review(id, &policy).unwrap();
    assert_eq!(decision, ProposalDecision::NeedsHumanReview);
    assert_eq!(store.get(id).unwrap().state, ProposalState::UnderReview);

    // 3. A human admin promotes it.
    store.promote(id).unwrap();
    assert_eq!(store.get(id).unwrap().state, ProposalState::Promoted);
    log_proposal_promoted(&mut audit, id, Actor::User(admin_id), scope).unwrap();

    // 4. The promoted proposal yields a canonical artifact.
    let artifact = store.promote_to_canonical(id).unwrap();
    assert!(
        matches!(artifact, CanonicalArtifact::Observation(_)),
        "promotion produces a canonical observation"
    );

    // 5. The audit trail records both lifecycle transitions in order.
    let q = AuditQuery::new().with_scope(scope);
    let entries: Vec<_> = audit.query(&q).collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].action_type,
        AuditActionType::AgentProposalSubmitted
    );
    assert_eq!(
        entries[1].action_type,
        AuditActionType::AgentProposalPromoted
    );
}
