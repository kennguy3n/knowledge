//! In-process integration tests for the loopback API.
//!
//! Each test builds the real axum [`Router`] over a fresh temp
//! SQLCipher store and drives it via `tower::ServiceExt::oneshot`
//! (axum's official testing pattern — no socket, no port races).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use substrate_server::config::ServerConfig;
use substrate_server::state::AppState;
use substrate_server::{build_router, open_runtime};
use tower::ServiceExt as _;

/// A deterministic 64-hex-char (32-byte) master key for tests.
const TEST_MASTER_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Build an [`AppState`] backed by a fresh temp store. The returned
/// `TempDir` guard must be kept alive for the duration of the test.
fn test_state() -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("substrate.db");
    let permissions_path = dir.path().join("permissions.db");
    let config = ServerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("addr"),
        store_path: store_path.to_string_lossy().into_owned(),
        master_key_hex: zeroize::Zeroizing::new(TEST_MASTER_KEY.to_string()),
        permissions_path: permissions_path.to_string_lossy().into_owned(),
        update_check: substrate_server::update_check::UpdateCheckConfig::default(),
        synthesis: None,
    };
    let config = Arc::new(config);
    // `open_runtime` may build and drop a short-lived Tokio runtime
    // during store rehydration; doing that on the `#[tokio::test]`
    // worker thread trips tokio's "cannot drop a runtime within an
    // async context" guard. Opening on a plain thread (no ambient
    // runtime) sidesteps it. The returned handle indexes a global
    // registry, so it is valid back on the async thread.
    let cfg = Arc::clone(&config);
    let handle = std::thread::spawn(move || open_runtime(&cfg))
        .join()
        .expect("open-store thread")
        .expect("open store");
    (
        AppState::new(handle, config).expect("open permission store"),
        dir,
    )
}

/// Send a JSON request through the router and return `(status, body)`.
async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let req = if let Some(b) = body {
        builder = builder.header("content-type", "application/json");
        builder
            .body(Body::from(serde_json::to_vec(&b).expect("serialize body")))
            .expect("build req")
    } else {
        builder.body(Body::empty()).expect("build req")
    };
    let resp = router.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Send a request expecting a text/plain body (used for metrics).
async fn send_text(router: axum::Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("build req");
    let resp = router.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn health_reports_subsystems() {
    let (state, _dir) = test_state();
    let (status, body) = send(build_router(state), "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    // HealthStatus is a record; at minimum it must serialise to an
    // object with subsystem detail.
    assert!(body.is_object(), "health body should be a JSON object");
}

#[tokio::test]
async fn ingest_query_get_evidence_round_trip() {
    let (state, _dir) = test_state();
    let router = build_router(state);
    let scope = uuid::Uuid::new_v4().to_string();

    // Ingest.
    let (status, body) = send(
        router.clone(),
        "POST",
        "/ingest",
        Some(json!({
            "scope_id": scope,
            "body": "the quarterly revenue report is ready",
            "source": "Manual",
            "importance": "Important"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ingest body: {body}");
    let evidence_id = body["id"].as_str().expect("id").to_string();

    // Query finds it.
    let (status, body) = send(
        router.clone(),
        "POST",
        "/query",
        Some(json!({ "scope_id": scope, "query_text": "revenue", "limit": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_array());
    assert!(!body.as_array().unwrap().is_empty(), "query should hit");

    // Fetch the row by id.
    let (status, body) = send(
        router.clone(),
        "GET",
        &format!("/evidence/{evidence_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap(), evidence_id);

    // Forget it.
    let (status, _) = send(
        router,
        "POST",
        "/forget",
        Some(json!({ "id": evidence_id })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn ingest_rejects_non_uuid_scope() {
    let (state, _dir) = test_state();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/ingest",
        Some(json!({
            "scope_id": "not-a-uuid",
            "body": "x",
            "source": "Manual",
            "importance": "Noise"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["kind"], "InvalidId");
}

#[tokio::test]
async fn query_rescues_malformed_fts_with_200() {
    // A search box must never surface an FTS5 syntax error. Malformed
    // MATCH input — a stray/unbalanced `"`, a `NEAR(`, a dangling
    // boolean operator — is rescued by the literal-token fallback and
    // returns 200, NOT a 400 (`InvalidQuery`) that would make ordinary
    // search-box text look like a client error. Regression guard for
    // the gateway bouncing a 400 at a user who simply typed a quote.
    let (state, _dir) = test_state();
    let router = build_router(state);
    let scope = uuid::Uuid::new_v4().to_string();

    // Seed one row so the scope is non-empty (the rescue must not
    // depend on whether the scope happens to have data).
    let (status, _) = send(
        router.clone(),
        "POST",
        "/ingest",
        Some(json!({
            "scope_id": scope,
            "body": "the quarterly revenue report is ready",
            "source": "Manual",
            "importance": "Important"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for bad in ["\"unbalanced", "revenue AND", "NEAR("] {
        let (status, body) = send(
            router.clone(),
            "POST",
            "/query",
            Some(json!({ "scope_id": scope, "query_text": bad, "limit": 10 })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "query {bad:?} should be rescued, body: {body}"
        );
    }

    // An unbalanced quote on a word that is present still *matches* the
    // seeded row, proving the rescue searched literally rather than
    // returning a hollow empty result.
    let (status, body) = send(
        router.clone(),
        "POST",
        "/query",
        Some(json!({ "scope_id": scope, "query_text": "\"revenue", "limit": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.as_array().unwrap().is_empty(),
        "rescued query should match the seeded row"
    );

    // A well-formed query against the same scope still returns 200 —
    // the rescue does not regress the happy path.
    let (status, body) = send(
        router,
        "POST",
        "/query",
        Some(json!({ "scope_id": scope, "query_text": "revenue", "limit": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.as_array().unwrap().is_empty(),
        "valid query should hit"
    );
}

#[tokio::test]
async fn get_unknown_evidence_returns_404() {
    let (state, _dir) = test_state();
    let id = uuid::Uuid::new_v4();
    let (status, body) = send(build_router(state), "GET", &format!("/evidence/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["kind"], "NotFound");
}

#[tokio::test]
async fn list_memories_returns_array() {
    let (state, _dir) = test_state();
    let scope = uuid::Uuid::new_v4().to_string();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/memories",
        Some(json!({ "scope_id": scope })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_array());
}

/// Regression: before the user-memory write path existed there was no
/// route to create a memory, so `POST /memories` (list) always
/// returned `[]`. This drives the full write→read round-trip:
/// `POST /user_memory` creates a `Candidate` row and `POST /memories`
/// reads it back.
#[tokio::test]
async fn add_user_memory_then_list_round_trip() {
    let (state, _dir) = test_state();
    let router = build_router(state);
    let scope = uuid::Uuid::new_v4().to_string();

    // List is empty before any write.
    let (status, body) = send(
        router.clone(),
        "POST",
        "/memories",
        Some(json!({ "scope_id": scope })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("array").len(), 0);

    // Create a user-memory observation.
    let (status, created) = send(
        router.clone(),
        "POST",
        "/user_memory",
        Some(json!({
            "scope_id": scope,
            "observation_type": "preference",
            "content": "prefers async standups",
            "sensitivity": "Useful"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create body: {created}");
    assert_eq!(created["scope_id"].as_str(), Some(scope.as_str()));
    assert_eq!(created["summary"].as_str(), Some("prefers async standups"));
    assert_eq!(created["state"].as_str(), Some("Candidate"));
    let created_id = created["id"].as_str().expect("id").to_string();

    // List now returns the freshly-created row.
    let (status, body) = send(
        router.clone(),
        "POST",
        "/memories",
        Some(json!({ "scope_id": scope })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"].as_str(), Some(created_id.as_str()));
}

/// Fail-closed: a blank `content` is rejected with `400` before any
/// row is created, and the `sensitivity` field defaults to `Useful`
/// when omitted.
#[tokio::test]
async fn add_user_memory_rejects_blank_content_and_defaults_sensitivity() {
    let (state, _dir) = test_state();
    let router = build_router(state);
    let scope = uuid::Uuid::new_v4().to_string();

    // Blank content → 400.
    let (status, _body) = send(
        router.clone(),
        "POST",
        "/user_memory",
        Some(json!({
            "scope_id": scope,
            "observation_type": "note",
            "content": "   "
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Omitting `sensitivity` is accepted (defaults to Useful).
    let (status, created) = send(
        router.clone(),
        "POST",
        "/user_memory",
        Some(json!({
            "scope_id": scope,
            "observation_type": "note",
            "content": "remember the launch date"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create body: {created}");
}

#[tokio::test]
async fn concept_graph_returns_empty_graph_for_fresh_scope() {
    let (state, _dir) = test_state();
    let scope = uuid::Uuid::new_v4().to_string();
    let (status, body) = send(
        build_router(state),
        "GET",
        &format!("/concept_graph/{scope}"),
        None,
    )
    .await;
    // A scope with no memory is a valid, honest empty graph — 200
    // with empty node/edge arrays, never a 404.
    assert_eq!(status, StatusCode::OK);
    assert!(body["nodes"].as_array().expect("nodes array").is_empty());
    assert!(body["edges"].as_array().expect("edges array").is_empty());
}

#[tokio::test]
async fn concept_graph_rejects_non_uuid_scope() {
    let (state, _dir) = test_state();
    let (status, body) = send(
        build_router(state),
        "GET",
        "/concept_graph/not-a-uuid",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["kind"], "InvalidId");
}

#[tokio::test]
async fn reasoning_contradictions_returns_empty_array_for_fresh_scope() {
    let (state, _dir) = test_state();
    let scope = uuid::Uuid::new_v4().to_string();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/reasoning/contradictions",
        Some(json!({ "scope_id": scope })),
    )
    .await;
    // No contradictions in an empty scope is a valid empty list, never
    // a 404.
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().expect("array").is_empty());
}

#[tokio::test]
async fn reasoning_drift_returns_empty_array_for_fresh_scope() {
    let (state, _dir) = test_state();
    let scope = uuid::Uuid::new_v4().to_string();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/reasoning/drift",
        Some(json!({ "scope_id": scope })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().expect("array").is_empty());
}

#[tokio::test]
async fn reasoning_contradictions_reject_non_uuid_scope() {
    let (state, _dir) = test_state();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/reasoning/contradictions",
        Some(json!({ "scope_id": "not-a-uuid" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["kind"], "InvalidId");
}

#[tokio::test]
async fn reasoning_explain_returns_plan_rationale() {
    let (state, _dir) = test_state();
    let scope = uuid::Uuid::new_v4().to_string();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/reasoning/explain",
        Some(json!({ "scope_id": scope, "query": "what was approved by finance" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["class"], "relational");
    let steps = body["steps"].as_array().expect("steps array");
    assert!(!steps.is_empty());
    assert_eq!(steps[0]["mode"], "graph_traversal");
    assert!(body["rationale"]
        .as_str()
        .expect("rationale")
        .contains("relational"));
}

#[tokio::test]
async fn reasoning_explain_rejects_empty_query() {
    let (state, _dir) = test_state();
    let scope = uuid::Uuid::new_v4().to_string();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/reasoning/explain",
        Some(json!({ "scope_id": scope, "query": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["kind"], "InvalidQuery");
}

#[tokio::test]
async fn permission_grant_check_revoke_flow() {
    let (state, _dir) = test_state();
    let router = build_router(state);
    let tenant = uuid::Uuid::new_v4();
    let user = uuid::Uuid::new_v4();
    let tuple = json!({
        "object": { "object_type": "tenant", "object_id": tenant },
        "relation": "owner",
        "subject": { "subject_type": "user", "subject_id": user, "subject_relation": null }
    });

    // Before grant: not allowed.
    let (status, body) = send(
        router.clone(),
        "POST",
        "/permission/check",
        Some(tuple.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["allowed"], false);

    // Grant (fresh → 201).
    let (status, _) = send(
        router.clone(),
        "POST",
        "/permission/grant",
        Some(tuple.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Re-grant is idempotent (→ 200).
    let (status, _) = send(
        router.clone(),
        "POST",
        "/permission/grant",
        Some(tuple.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Now allowed.
    let (status, body) = send(
        router.clone(),
        "POST",
        "/permission/check",
        Some(tuple.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["allowed"], true);

    // Revoke.
    let (status, _) = send(
        router.clone(),
        "POST",
        "/permission/revoke",
        Some(tuple.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Revoking again → 404.
    let (status, _) = send(
        router.clone(),
        "POST",
        "/permission/revoke",
        Some(tuple.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // No longer allowed.
    let (status, body) = send(router, "POST", "/permission/check", Some(tuple)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["allowed"], false);
}

/// Group-based authorization must round-trip through the live API: a
/// `group`-typed membership tuple is accepted, a role granted to that
/// group via a `# member` userset rewrite is resolved, and every member
/// of the group is then granted that role on the tenant. This is the
/// exact path SCIM provisioning drives, and it regressed silently when
/// `ObjectType` lacked a `Group` variant (the grant 422'd on
/// `unknown variant "group"`, so SCIM group sync failed). The check uses
/// the granted relation directly so it isolates the group-userset
/// round-trip from the namespace-inheritance chain (covered by the
/// `permission_service` unit tests).
#[tokio::test]
async fn group_membership_userset_grants_tenant_role() {
    let (state, _dir) = test_state();
    let router = build_router(state);
    let tenant = uuid::Uuid::new_v4();
    let group = uuid::Uuid::new_v4();
    let member = uuid::Uuid::new_v4();
    let outsider = uuid::Uuid::new_v4();

    // SCIM-style membership tuple: `group:<g># member @ user:<m>`.
    let membership = json!({
        "object": { "object_type": "group", "object_id": group },
        "relation": "member",
        "subject": { "subject_type": "user", "subject_id": member, "subject_relation": null }
    });
    let (status, _) = send(
        router.clone(),
        "POST",
        "/permission/grant",
        Some(membership),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "group membership grant must be accepted"
    );

    // Role binding: `tenant:<t># admin @ group:<g># member`.
    let role = json!({
        "object": { "object_type": "tenant", "object_id": tenant },
        "relation": "admin",
        "subject": { "subject_type": "group", "subject_id": group, "subject_relation": "member" }
    });
    let (status, _) = send(router.clone(), "POST", "/permission/grant", Some(role)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "group role binding must be accepted"
    );

    // The member resolves to `admin` on the tenant through the group
    // userset.
    let check_member = json!({
        "object": { "object_type": "tenant", "object_id": tenant },
        "relation": "admin",
        "subject": { "subject_type": "user", "subject_id": member, "subject_relation": null }
    });
    let (status, body) = send(
        router.clone(),
        "POST",
        "/permission/check",
        Some(check_member),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["allowed"], true,
        "group member must resolve to the tenant role"
    );

    // A non-member gets nothing.
    let check_outsider = json!({
        "object": { "object_type": "tenant", "object_id": tenant },
        "relation": "admin",
        "subject": { "subject_type": "user", "subject_id": outsider, "subject_relation": null }
    });
    let (status, body) = send(router, "POST", "/permission/check", Some(check_outsider)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["allowed"], false,
        "a non-member must not inherit the role"
    );
}

#[tokio::test]
async fn hybrid_keypair_has_expected_hex_lengths() {
    let (state, _dir) = test_state();
    let (status, body) = send(build_router(state), "POST", "/crypto/hybrid_keypair", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["algorithm"], "x25519+ml-kem-768");
    // public = x25519 (32) + ml-kem-768 (1184) = 1216 bytes → 2432 hex.
    assert_eq!(
        body["public_key_hex"].as_str().unwrap().len(),
        (32 + 1184) * 2
    );
    // secret = x25519 (32) + ml-kem-768 (2400) = 2432 bytes → 4864 hex.
    assert_eq!(
        body["secret_key_hex"].as_str().unwrap().len(),
        (32 + 2400) * 2
    );
}

#[tokio::test]
async fn signing_keypair_returns_algorithm_and_keys() {
    let (state, _dir) = test_state();
    let (status, body) = send(build_router(state), "POST", "/crypto/signing_keypair", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["algorithm"].is_string());
    assert!(body["public_key"].is_array());
    assert!(body["private_key"].is_array());
}

#[tokio::test]
async fn fetch_content_is_not_implemented() {
    let (state, _dir) = test_state();
    let (status, body) = send(
        build_router(state),
        "POST",
        "/connector/fetch_content",
        Some(json!({ "instance_id": uuid::Uuid::new_v4(), "content_ref": "msg-1" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body["kind"], "Unimplemented");
}

#[tokio::test]
async fn metrics_exposition_contains_counters() {
    let (state, _dir) = test_state();
    let router = build_router(state);

    // Drive one ingest so the counter is non-trivial.
    let scope = uuid::Uuid::new_v4().to_string();
    let _ = send(
        router.clone(),
        "POST",
        "/ingest",
        Some(json!({ "scope_id": scope, "body": "x", "source": "Manual", "importance": "Noise" })),
    )
    .await;

    let (status, text) = send_text(router, "/internal/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("knowledge_ingest_total"));
    assert!(text.contains("# TYPE knowledge_query_total counter"));
}

/// With the default (opt-out) config the update-check endpoint must
/// answer cheaply with `enabled: false` and never touch the network,
/// so probing it is always safe regardless of build features.
#[tokio::test]
async fn update_check_disabled_by_default() {
    let (state, _dir) = test_state();
    let (status, body) = send(build_router(state), "GET", "/internal/update_check", None).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["enabled"], json!(false));
    assert_eq!(body["update_available"], json!(false));
    assert!(body["latest_version"].is_null());
    assert!(body["current_version"].is_string());
}

#[tokio::test]
async fn export_evaluate_empty_profile_approves_nothing() {
    let (state, _dir) = test_state();
    let scope = evidence_store::ScopeId::new_v4();
    let profile = export_plane::PortableConceptProfile::new("p", "desc", "hubspot", scope);
    let (status, body) = send(
        build_router(state),
        "POST",
        "/export/evaluate",
        Some(json!({ "profile": profile })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body["approved"].as_array().unwrap().is_empty());
    assert!(body["rejected"].as_array().unwrap().is_empty());
}

/// A permission grant must survive re-opening the permission store
/// (i.e. a substrate_server restart). Exercises [`PermissionState`]
/// directly so it does not collide with the global evidence-store
/// handle registry by re-opening the same `RuntimeHandle`.
#[test]
fn permission_grants_persist_across_reopen() {
    use permission_service::{
        check_permission, ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef, SubjectType,
    };
    use substrate_server::config::decode_master_key;
    use substrate_server::state::PermissionState;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permissions.db");
    let path = path.to_string_lossy().into_owned();
    let key = decode_master_key(TEST_MASTER_KEY).expect("valid key");

    let tenant = uuid::Uuid::new_v4();
    let user = uuid::Uuid::new_v4();
    let tuple = RelationTuple::new(
        ObjectRef::new(ObjectType::Tenant, tenant),
        Relation::Owner,
        SubjectRef::direct(SubjectType::User, user),
    );

    // First instance: grant, then drop (flushing to SQLCipher).
    {
        let mut perms = PermissionState::open(&path, &key).expect("open #1");
        assert!(perms.store.upsert(tuple).expect("upsert"));
        assert!(check_permission(
            perms.store.store(),
            &perms.namespaces,
            tuple.object,
            tuple.relation,
            tuple.subject,
        ));
    }

    // Second instance: rehydrated from disk — the grant is still there.
    let perms = PermissionState::open(&path, &key).expect("open #2");
    assert!(
        check_permission(
            perms.store.store(),
            &perms.namespaces,
            tuple.object,
            tuple.relation,
            tuple.subject,
        ),
        "grant should survive reopening the permission store"
    );
}

/// The server-level [`PermissionState`] must carry the default relation-
/// implication chains, so a higher role satisfies a check for a lower
/// one (`Admin ⇒ Viewer`). This guards against the registry being
/// constructed empty (`NamespaceRegistry::default()`), which would make
/// `/permission/check` match relations exactly and surprise an `admin`
/// user with a `403` on a `viewer`-gated route.
#[test]
fn permission_state_inherits_default_relation_chain() {
    use permission_service::{
        check_permission, ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef, SubjectType,
    };
    use substrate_server::config::decode_master_key;
    use substrate_server::state::PermissionState;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permissions.db");
    let path = path.to_string_lossy().into_owned();
    let key = decode_master_key(TEST_MASTER_KEY).expect("valid key");

    let tenant = uuid::Uuid::new_v4();
    let user = uuid::Uuid::new_v4();
    let object = ObjectRef::new(ObjectType::Tenant, tenant);
    let subject = SubjectRef::direct(SubjectType::User, user);

    let mut perms = PermissionState::open(&path, &key).expect("open");
    // Grant only the higher `Admin` role…
    assert!(perms
        .store
        .upsert(RelationTuple::new(object, Relation::Admin, subject))
        .expect("upsert admin grant"));

    // …and the user must still satisfy a `Viewer` check via the
    // Owner ⇒ Admin ⇒ … ⇒ Viewer implication chain.
    assert!(
        check_permission(
            perms.store.store(),
            &perms.namespaces,
            object,
            Relation::Viewer,
            subject,
        ),
        "an Admin grant must satisfy a Viewer check through the default \
         relation-implication chain"
    );
}

// ─────────────────────── Server-side synthesis ──────────────────────

#[tokio::test]
async fn server_synthesis_routes_report_engine_unavailable_without_engine() {
    // With no synthesis engine configured (the default test runtime),
    // the domain/tenant routes are still reachable and dispatch into the
    // FFI, which reports the empty engine slot as 503. This proves the
    // route → FFI wiring independently of any live engine.
    let (state, _dir) = test_state();
    let router = build_router(state);
    let scope = uuid::Uuid::new_v4().to_string();

    for path in ["/synthesis/domain", "/synthesis/tenant"] {
        let (status, body) = send(
            router.clone(),
            "POST",
            path,
            Some(json!({ "scope_id": scope })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} should report engine unavailable, body: {body}"
        );
    }
}

#[tokio::test]
async fn server_synthesis_rejects_bad_scope_id() {
    // A malformed scope id is rejected with 400 before any dispatch.
    let (state, _dir) = test_state();
    let router = build_router(state);
    for path in ["/synthesis/domain", "/synthesis/tenant"] {
        let (status, _body) = send(
            router.clone(),
            "POST",
            path,
            Some(json!({ "scope_id": "not-a-uuid" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} bad scope");
    }
}

/// With the managed-endpoint engine installed (via the production
/// `configure_synthesis` wiring), the server-side routes dispatch past
/// the engine slot to the tier-specific gather, which 404s on an
/// unregistered scope *before* any network call to the endpoint. A 503
/// here would mean the engine was not installed — so this test is the
/// end-to-end proof that `KNOWLEDGE_SYNTHESIS_URL` → boot-time
/// `configure_synthesis_engine` → live domain/tenant dispatch is wired.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn configured_synthesis_engine_makes_server_routes_reach_dispatch() {
    use substrate_server::config::SynthesisEngineSettings;

    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("substrate.db");
    let permissions_path = dir.path().join("permissions.db");
    let config = ServerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("addr"),
        store_path: store_path.to_string_lossy().into_owned(),
        master_key_hex: zeroize::Zeroizing::new(TEST_MASTER_KEY.to_string()),
        permissions_path: permissions_path.to_string_lossy().into_owned(),
        update_check: substrate_server::update_check::UpdateCheckConfig::default(),
        synthesis: Some(SynthesisEngineSettings {
            url: "https://synth.invalid/v1/complete".into(),
            api_key_ref: "SYNTH_KEY_REF".into(),
            model_id: "slm-recap-test".into(),
            max_tokens: 0,
            timeout_ms: 0,
            scope_bindings: None,
            single_tenant: true,
            max_requests_per_minute: None,
            rate_capacity: 0,
            rate_refill_per_sec: 0.0,
        }),
    };
    let config = Arc::new(config);
    // Open the store *and* install the synthesis engine on a plain
    // thread: `configure_synthesis_engine` builds a reqwest blocking
    // client whose internal runtime is dropped during construction,
    // which trips tokio's "cannot drop a runtime in an async context"
    // guard if done on the `#[tokio::test]` worker. The handle indexes a
    // global registry, so it stays valid back on the async thread.
    let cfg = Arc::clone(&config);
    let handle = std::thread::spawn(move || {
        let handle = open_runtime(&cfg).expect("open store");
        substrate_server::configure_synthesis(handle, &cfg).expect("install engine");
        handle
    })
    .join()
    .expect("open-store thread");
    let state = AppState::new(handle, config).expect("open permission store");
    let router = build_router(state);
    let scope = uuid::Uuid::new_v4().to_string();

    for (path, kind) in [
        ("/synthesis/domain", "domain_memory"),
        ("/synthesis/tenant", "tenant_memory"),
    ] {
        let (status, body) = send(
            router.clone(),
            "POST",
            path,
            Some(json!({ "scope_id": scope })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{path} should reach dispatch (engine installed), body: {body}"
        );
        // FfiError serialises with `#[serde(tag = "kind", content =
        // "detail")]`, so the variant tag is the outer `kind` and the
        // NotFound struct's own `kind` (the missing object type) is at
        // `detail.kind`.
        assert_eq!(body["kind"].as_str(), Some("NotFound"), "{path} variant");
        assert_eq!(
            body["detail"]["kind"].as_str(),
            Some(kind),
            "{path} should 404 on the tier-specific memory lookup",
        );
    }
}
