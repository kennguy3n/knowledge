//! B2C end-to-end lifecycle integration tests.
//!
//! Each test exercises a full consumer-facing lifecycle —
//! `ingest → extract → retrieve` (and, where the scenario calls for
//! it, `forget`) — straight through the real substrate crates
//! (`evidence_store`, `observation_engine`, `connector_framework`,
//! `connectors`), never a mock of the substrate itself.
//!
//! Scenarios:
//!
//! * `teen_group_chat` — group-chat messages land in a personal
//!   (`User`-typed) scope, observations are extracted, and hybrid
//!   retrieval surfaces the relevant evidence.
//! * `consumer_assistant` — a single-user assistant scope ingests
//!   mixed notes/messages and answers a question via hybrid retrieval.
//! * `creator_personal_connectors` — a creator attaches a personal
//!   connector; connector-sourced documents are ingested, and a second
//!   sync that re-surfaces an already-seen item is deduped by the
//!   connector framework's [`WatermarkCursor`] so no duplicate evidence
//!   row is written.
//! * `gdpr_erasure_lifecycle` — data is ingested into a scope, then
//!   cryptographically forgotten (CEK-wrap purge + FTS purge +
//!   tombstone + DEK destroy); the evidence becomes irrecoverable, the
//!   FTS index returns nothing, and the tombstone survives reopen.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{Duration, Utc};
use uuid::Uuid;

use connector_framework::{
    AttachmentRegistry, AuthKind, Connector, ConnectorConfig, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, HttpMethod, HttpTransport, MockHttpTransport, MockResponse, OAuth2CodeExchange,
    OAuth2Token, Result as ConnectorResult, SyncState,
};
use connectors::GitHubConnector;
use evidence_store::{EvidenceId, HybridRetriever};
use observation_engine::{LexiconExtractor, ObservationExtractor, ObservationType};
use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};

use integration_tests::test_helpers::{
    open_store, padded_body, ImportanceClass, ScopeId, BODY_SIZE,
};

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

/// A GitHub `issues` list response for one page (final page — fewer
/// than 100 results, so the connector stops paginating).
fn issues_page(issues: &[serde_json::Value]) -> MockResponse {
    MockResponse::ok_json(serde_json::to_vec(issues).unwrap())
}

/// Ingest the documents named by a connector sync run into `store`
/// under `scope`, skipping any source-document id already ingested.
/// Returns the number of *new* evidence rows written.
///
/// This mirrors what the substrate runtime does for every
/// `DocumentCreated` / `DocumentUpdated` event: fetch the body and
/// ingest it, citing the connector source. The `seen` set models the
/// store-side idempotency that backs the connector framework's
/// at-most-once delivery guarantee.
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
            "github issue {doc_id} about the migration deadline"
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

// ── teen_group_chat ─────────────────────────────────────────────────

#[test]
fn teen_group_chat() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    // A personal scope (`User`-typed) owned by the chat participant.
    let user_id = Uuid::new_v4();
    let scope = ScopeId::new_v4();
    let subject = SubjectRef::direct(SubjectType::User, user_id);

    let mut tuples = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let obj = ObjectRef::new(ObjectType::User, scope.as_uuid());
    tuples
        .insert(RelationTuple::new(obj, Relation::Owner, subject))
        .unwrap();
    assert!(
        check_permission(&tuples, &ns, obj, Relation::Owner, subject),
        "participant owns their personal scope"
    );

    // 1. Ingest group-chat style messages into the personal scope.
    let messages = [
        "@Alex please bring the speakers to the party on Friday.",
        "We decided to meet at the skate park after school tomorrow.",
        "TODO: charge the camera before the trip.",
        "lol that meme was hilarious",
    ];
    let mut store = open_store(&db_path);
    let extractor = LexiconExtractor::default();

    let mut all_observations = Vec::new();
    for msg in messages {
        let body = padded_body(msg);
        let res = store
            .ingest(scope, &body, Some("chat:group"), ImportanceClass::Useful)
            .expect("ingest chat message");

        // 2. Extract observations and cite the evidence row they came
        //    from — the same wiring the production pipeline performs.
        let mut observations = extractor.extract(msg, scope);
        for o in &mut observations {
            o.source_evidence_ids.push(res.evidence_id);
        }
        all_observations.extend(observations);
    }

    // Observations were extracted, each scoped + cited correctly.
    assert!(
        !all_observations.is_empty(),
        "group chat should yield observations"
    );
    for o in &all_observations {
        assert_eq!(o.scope_id, scope);
        assert_eq!(o.source_evidence_ids.len(), 1);
    }
    // The @mention and the action item are both surfaced.
    assert!(
        all_observations
            .iter()
            .any(|o| o.observation_type == ObservationType::Entity && o.content.contains("Alex")),
        "the @Alex mention is extracted as an entity"
    );
    assert!(
        all_observations
            .iter()
            .any(|o| o.observation_type == ObservationType::Task),
        "the TODO is extracted as a task"
    );

    // 3. Hybrid retrieval surfaces the relevant message.
    let retriever = HybridRetriever::new(&store);
    let hits = retriever
        .search_hybrid(scope, "skate park", 5)
        .expect("hybrid search");
    assert!(!hits.is_empty(), "retrieval returns the skate-park message");
    assert!(
        hits.iter().all(|h| h.score > 0.0),
        "every hybrid hit has a positive fan-in score"
    );
}

// ── consumer_assistant ──────────────────────────────────────────────

#[test]
fn consumer_assistant() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let scope = ScopeId::new_v4();
    let mut store = open_store(&db_path);

    // A single-user assistant scope ingests a mix of notes and
    // messages. Each note is distinguished by a unique keyword so the
    // assistant can later cite the right one.
    let corpus = [
        (
            "note:travel",
            "Flight to Lisbon departs at 7am on June 14th from gate 22.",
        ),
        (
            "note:recipe",
            "The sourdough recipe needs 500g flour and an overnight proof.",
        ),
        (
            "msg:work",
            "Reminder: the quarterly review meeting moved to Thursday.",
        ),
        (
            "note:car",
            "The car insurance renewal is due in March; policy number QX-1199.",
        ),
    ];
    let mut ids = Vec::new();
    for (source, text) in corpus {
        let body = padded_body(text);
        let res = store
            .ingest(scope, &body, Some(source), ImportanceClass::Useful)
            .expect("ingest note");
        ids.push((source, res.evidence_id));
    }

    // The assistant answers "when is the insurance due?" via hybrid
    // retrieval: the car-insurance note must be the top hit.
    let retriever = HybridRetriever::new(&store);
    let answer = retriever
        .search_hybrid(scope, "insurance renewal due", 4)
        .expect("hybrid search");
    assert!(!answer.is_empty(), "assistant finds a relevant note");

    let expected = ids
        .iter()
        .find(|(s, _)| *s == "note:car")
        .map(|(_, id)| *id)
        .unwrap();
    assert_eq!(
        answer[0].evidence_id, expected,
        "the car-insurance note ranks first for an insurance query"
    );

    // A second, lexically-distinct question routes to a different note.
    let recipe = retriever
        .search_hybrid(scope, "sourdough flour proof", 4)
        .expect("hybrid search");
    let recipe_id = ids
        .iter()
        .find(|(s, _)| *s == "note:recipe")
        .map(|(_, id)| *id)
        .unwrap();
    assert_eq!(
        recipe[0].evidence_id, recipe_id,
        "the recipe note ranks first for a baking query"
    );
}

// ── creator_personal_connectors ─────────────────────────────────────

#[test]
fn creator_personal_connectors() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let creator_id = Uuid::new_v4();
    let user_scope = ScopeId::new_v4();
    let subject = SubjectRef::direct(SubjectType::User, creator_id);

    // 1. The creator grants themselves editor on their personal
    //    (`User`-typed) scope and attaches a connector to it.
    let mut tuples = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    tuples
        .insert(RelationTuple::new(
            ObjectRef::new(ObjectType::User, user_scope.as_uuid()),
            Relation::Editor,
            subject,
        ))
        .unwrap();

    let mut registry = AttachmentRegistry::new();
    let connector_inst = ConnectorInstanceId::new_v4();
    let attachment = registry
        .attach(
            connector_inst,
            ConnectorKind::GitHub,
            user_scope,
            ObjectType::User,
            &tuples,
            &ns,
            subject,
        )
        .expect("attach personal connector");
    assert_eq!(attachment.scope_id, user_scope);
    assert_eq!(registry.scope_for(connector_inst).unwrap(), user_scope);

    let base = "https://api.test";
    let repo = "creator/portfolio";
    let now = Utc::now();
    let cfg = ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, user_scope)
        .with_auth_config(serde_json::json!({
            "authorization_code": "code",
            "repository": repo,
            "api_base_url": base,
            "webhook_secret": "test-webhook-secret",
        }));

    // 2. initial_sync over a single page of two issues.
    let transport = MockHttpTransport::new();
    transport.expect(
        HttpMethod::Get,
        format!(
            "{base}/repos/{repo}/issues\
             ?state=all&sort=updated&direction=asc&per_page=100&page=1"
        ),
        issues_page(&[
            serde_json::json!({
                "number": 1, "id": 1, "title": "Issue 1", "state": "open",
                "created_at": now - Duration::hours(2),
                "updated_at": now - Duration::hours(1),
            }),
            serde_json::json!({
                "number": 2, "id": 2, "title": "Issue 2", "state": "open",
                "created_at": now - Duration::hours(2),
                "updated_at": now,
            }),
        ]),
    );
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    let connector =
        GitHubConnector::new(connector_inst, transport, oauth()).with_api_base_url(base);
    let token = connector.authenticate(&cfg).unwrap();
    let initial = connector.initial_sync(&cfg, &token).unwrap();
    assert_eq!(initial.events.len(), 2);

    // 3. Ingest the connector-sourced documents into the creator's
    //    scope, tracking which source ids we have already stored.
    let mut store = open_store(&db_path);
    let mut seen: HashSet<String> = HashSet::new();
    let written = ingest_sync_events(&mut store, user_scope, &initial.events, &mut seen);
    assert_eq!(written, 2, "both connector documents are ingested once");

    // 4. An incremental sync that re-surfaces the boundary issue (#2,
    //    already seen) plus a brand-new one (#3). The framework's
    //    WatermarkCursor drops the already-seen boundary id. The
    //    incremental list query carries a `since=<watermark>` filter we
    //    do not assert on here, so a default response keeps the test
    //    focused on the dedup behaviour rather than URL shape.
    let transport2 = MockHttpTransport::new();
    transport2.with_default_response(issues_page(&[
        // Same updated_at as the cursor watermark + already seen → deduped.
        serde_json::json!({
            "number": 2, "id": 2, "title": "Issue 2", "state": "open",
            "created_at": now - Duration::hours(2),
            "updated_at": now,
        }),
        // Strictly newer → surfaced.
        serde_json::json!({
            "number": 3, "id": 3, "title": "Issue 3", "state": "open",
            "created_at": now,
            "updated_at": now + Duration::minutes(5),
        }),
    ]));
    let transport2: Arc<dyn HttpTransport> = Arc::new(transport2);
    let connector2 =
        GitHubConnector::new(connector_inst, transport2, oauth()).with_api_base_url(base);
    let mut state = SyncState::new(connector_inst);
    state.cursor = initial.next_cursor;
    let incremental = connector2.incremental_sync(&cfg, &token, &state).unwrap();

    // The connector framework already deduped the boundary issue.
    let inc_ids: Vec<&str> = incremental
        .events
        .iter()
        .map(|e| e.document_id().as_str())
        .collect();
    assert_eq!(
        inc_ids,
        vec!["3"],
        "WatermarkCursor drops the seen issue #2"
    );

    let written_inc = ingest_sync_events(&mut store, user_scope, &incremental.events, &mut seen);
    assert_eq!(written_inc, 1, "only the new document #3 is ingested");

    // 5. Exactly three distinct connector documents are now retrievable.
    let hits = store
        .search_fts(user_scope, "migration", 100)
        .expect("search connector evidence");
    assert_eq!(
        hits.len(),
        3,
        "issues 1, 2, 3 each produced exactly one evidence row"
    );
}

// ── gdpr_erasure_lifecycle ──────────────────────────────────────────

#[test]
fn gdpr_erasure_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    let scope = ScopeId::new_v4();
    let mut store = open_store(&db_path);

    // 1. Ingest evidence (bodies above the inline threshold so they
    //    land in the deduplicated body table — the only routing path
    //    where the CEK-wrap purge actually shreds the plaintext).
    let mut ids = Vec::new();
    for i in 0..4 {
        let body = padded_body(&format!("subject access record {i} personal-data erasure"));
        let res = store
            .ingest(
                scope,
                &body,
                Some("gdpr:subject"),
                ImportanceClass::Important,
            )
            .expect("ingest record");
        ids.push(res.evidence_id);
    }

    // Pre-condition: everything is readable and FTS-searchable.
    assert_eq!(
        store.search_fts(scope, "erasure", 100).unwrap().len(),
        4,
        "all records are searchable before erasure"
    );
    for &id in &ids {
        assert_eq!(store.read_body(id).unwrap().len(), BODY_SIZE);
    }

    // 2. Cryptographic forgetting via the canonical sequence.
    store
        .purge_body_key_wraps_for_scope(scope)
        .expect("purge CEK wraps");
    store.purge_fts_for_scope(scope).expect("purge FTS");
    store
        .record_forgotten_scope(scope)
        .expect("record tombstone");
    store.delete_scope_dek(scope).expect("destroy scope DEK");

    // 3a. FTS index returns nothing for the forgotten scope.
    let post: Vec<EvidenceId> = store.search_fts(scope, "erasure", 100).unwrap();
    assert!(post.is_empty(), "FTS yields no hits after erasure");

    // 3b. The evidence bodies are irrecoverable.
    for &id in &ids {
        assert!(
            store.read_body(id).is_err(),
            "body read must fail after DEK destruction"
        );
    }

    // 4. The tombstone is durable across a reopen.
    drop(store);
    let store = open_store(&db_path);
    let tombstones = store.load_forgotten_scopes().expect("load tombstones");
    assert!(
        tombstones.contains(&scope),
        "the forgotten-scope tombstone survives reopen"
    );
    // And the evidence is still irrecoverable after reopen.
    for &id in &ids {
        assert!(
            store.read_body(id).is_err(),
            "evidence stays irrecoverable across reopen"
        );
    }
}
