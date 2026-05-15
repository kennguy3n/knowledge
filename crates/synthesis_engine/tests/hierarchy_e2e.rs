//! Phase 3 end-to-end test for the channel → domain → tenant
//! synthesis chain.
//!
//! Per `docs/internal/PHASES.md` Phase 3 exit criteria, the channel → domain →
//! tenant synthesis chain must be exercised end-to-end. This test
//! wires together every Phase 3 crate that participates in that
//! chain:
//!
//! * `memory_manager` — channel / domain / tenant memory objects.
//! * `synthesis_pipeline` — `SynthesisWindowManager`, hierarchy
//!   enforcement (`DomainSynthesisInput`, `TenantSynthesisInput`),
//!   `NoOpSynthesizer` for the channel-recap stage.
//! * `synthesis_engine` — `ManagedEndpointSynthesizer` for the
//!   server-side domain / tenant stages.
//! * `concept_graph` — persisted concept graph receiving the
//!   tenant-level summary as a canonical concept.
//! * `permission_service` — Zanzibar-style permission check at every
//!   scope boundary.
//! * `audit_service` — append-only audit log of canonical promotion
//!   and member provisioning events.
//! * `crypto` — provenance bundles signed at each tier.

use chrono::{Duration, Utc};
use uuid::Uuid;

use audit_service::{
    Actor, AuditActionType, AuditEntryBuilder, AuditLog, AuditQuery, TargetRef, TargetType,
};
use concept_graph::{ConceptNode, PersistentConceptGraph};
use crypto::{
    AgentKind, EvidenceRef, MasterKey, ProvenanceAgent, ProvenanceBundle, ProvenanceSigner,
    SynthesisActivity, TestSigner, MASTER_KEY_LEN, TEST_SIGNER_KEY_LEN,
};
use evidence_store::ScopeId;
use memory_manager::{
    ApprovedDocumentRef, ChannelMemoryObject, Decision, DomainMemoryObject, TenantMemoryObject,
    Workstream,
};
use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
    SubjectRef, SubjectType, TupleStore,
};
use synthesis_engine::{ManagedEndpointSynthesizer, SynthesisEngine};
use synthesis_pipeline::{
    open_domain_window, open_tenant_window, ApprovedDocument, ChannelOutput, DomainOutput,
    DomainSynthesisInput, NoOpSynthesizer, PipelineError, SynthesisInputs, SynthesisObject,
    SynthesisObjectType, SynthesisPipeline, SynthesisWindowManager, TenantSynthesisInput,
    WindowScopeTier,
};

fn fixture_master_key() -> MasterKey {
    let mut k = [0u8; MASTER_KEY_LEN];
    for (i, b) in k.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(17);
    }
    k
}

fn fixture_signer_key() -> [u8; TEST_SIGNER_KEY_LEN] {
    let mut k = [0u8; TEST_SIGNER_KEY_LEN];
    for (i, b) in k.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(3);
    }
    k
}

/// Signed provenance bundle for one synthesis run, used at each
/// scope tier.
fn signed_bundle(
    signer: &TestSigner,
    entity_id: Uuid,
    tier: &str,
    derivations: Vec<EvidenceRef>,
) -> crypto::SignedBundle {
    let bundle = ProvenanceBundle::new(
        entity_id,
        SynthesisActivity::new(
            format!("synth-engine:{tier}"),
            "bonsai-1.7b@phase3-stub",
            format!("synth.{tier}.v1"),
            Uuid::new_v4(),
        ),
        ProvenanceAgent {
            kind: AgentKind::Software,
            identity: format!("synthesizer:{tier}"),
        },
        derivations,
    );
    signer.sign(bundle).expect("sign")
}

#[test]
fn full_channel_to_tenant_chain_with_provenance_and_permissions() {
    // ---------- Tenants / scopes ----------
    let tenant_scope = ScopeId::new_v4();
    let domain_scope = ScopeId::new_v4();
    let channel_scope = ScopeId::new_v4();

    // ---------- Permission tuples ----------
    let mut tuples = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();

    let tenant_obj = ObjectRef::new(ObjectType::Tenant, tenant_scope.as_uuid());
    let domain_obj = ObjectRef::new(ObjectType::Domain, domain_scope.as_uuid());
    let channel_obj = ObjectRef::new(ObjectType::Channel, channel_scope.as_uuid());

    let admin = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let domain_member = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let channel_member = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    let outsider = SubjectRef::direct(SubjectType::User, Uuid::new_v4());

    // Admin owns the tenant.
    tuples
        .insert(RelationTuple::new(tenant_obj, Relation::Owner, admin))
        .unwrap();
    // Domain inherits admin from tenant via userset rewrite.
    tuples
        .insert(RelationTuple::new(
            domain_obj,
            Relation::Admin,
            SubjectRef::via(SubjectType::Tenant, tenant_scope.as_uuid(), Relation::Admin),
        ))
        .unwrap();
    // Channel inherits admin from domain.
    tuples
        .insert(RelationTuple::new(
            channel_obj,
            Relation::Admin,
            SubjectRef::via(SubjectType::Domain, domain_scope.as_uuid(), Relation::Admin),
        ))
        .unwrap();
    // Domain has a direct member.
    tuples
        .insert(RelationTuple::new(
            domain_obj,
            Relation::Member,
            domain_member,
        ))
        .unwrap();
    // Channel has a direct member.
    tuples
        .insert(RelationTuple::new(
            channel_obj,
            Relation::Member,
            channel_member,
        ))
        .unwrap();

    // Sanity checks: admin reaches the channel through the chain;
    // outsider does not.
    assert!(check_permission(
        &tuples,
        &ns,
        channel_obj,
        Relation::Admin,
        admin
    ));
    assert!(check_permission(
        &tuples,
        &ns,
        channel_obj,
        Relation::Member,
        channel_member
    ));
    assert!(!check_permission(
        &tuples,
        &ns,
        channel_obj,
        Relation::Member,
        outsider
    ));
    assert!(!check_permission(
        &tuples,
        &ns,
        tenant_obj,
        Relation::Member,
        outsider
    ));

    // ---------- Memory objects ----------
    let mut channel_mem = ChannelMemoryObject::new(channel_scope);
    channel_mem.add_decision(Decision::new(channel_scope, "ship hierarchy enforcement"));

    let mut domain_mem = DomainMemoryObject::new(domain_scope);
    domain_mem.attach_channel_scope(channel_scope);
    domain_mem.add_workstream(Workstream::new(domain_scope, "phase-3 launch"));

    let mut tenant_mem = TenantMemoryObject::new(tenant_scope);
    tenant_mem.attach_domain_scope(domain_scope);
    let policy_doc = ApprovedDocumentRef::new("Tenant Policy v3", "compliance-officer");
    tenant_mem.admit_approved_document(policy_doc.clone());

    // ---------- Audit log ----------
    let mut audit = AuditLog::new();

    // ---------- Provenance + window manager + engine ----------
    let signer = TestSigner::new(fixture_signer_key());
    let mut windows = SynthesisWindowManager::new();
    let engine = ManagedEndpointSynthesizer::new();

    // ---------- Stage 1: channel synthesis ----------
    // Permission gate: only the elected channel synthesizer should be
    // allowed to run a channel-tier window. For this test, we treat
    // an "admin" subject as the elected synthesizer.
    assert!(check_permission(
        &tuples,
        &ns,
        channel_obj,
        Relation::Admin,
        admin
    ));

    let now = Utc::now();
    let channel_window =
        synthesis_pipeline::SynthesisWindow::new(channel_scope, now - Duration::hours(1), now)
            .unwrap();
    let channel_window_id = channel_window.id;
    // Track the window in the manager so the engine can mark it
    // in_progress / complete later if the substrate wires it through.
    windows
        .open_window(
            channel_scope,
            channel_window.window_start,
            channel_window.window_end,
        )
        .unwrap();

    let channel_synth = NoOpSynthesizer::new();
    let channel_inputs =
        SynthesisInputs::from_recap("ship hierarchy enforcement; raise risks early");
    let channel_object: SynthesisObject = channel_synth
        .synthesize(&channel_window, &channel_inputs)
        .unwrap();
    assert_eq!(
        channel_object.object_type,
        SynthesisObjectType::ChannelRecap
    );
    assert_eq!(channel_object.scope_id, channel_scope);

    let channel_signed = signed_bundle(
        &signer,
        channel_object.id.as_uuid(),
        "channel",
        vec![EvidenceRef::from_uuid(channel_window_id.as_uuid())],
    );
    assert!(signer.verify(&channel_signed).unwrap());

    // ---------- Stage 2: domain synthesis ----------
    // Type-system gate 1: cannot construct a ChannelOutput from a
    // domain-summary object.
    let bogus_domain_obj = SynthesisObject::new(
        domain_scope,
        synthesis_pipeline::WindowId::new_v4(),
        SynthesisObjectType::DomainSummary,
        b"not a recap".to_vec(),
        Uuid::nil(),
    );
    assert!(matches!(
        ChannelOutput::from_channel_object(bogus_domain_obj.clone()),
        Err(PipelineError::HierarchyViolation(_))
    ));

    // Type-system gate 2: domain synthesis cannot take a raw
    // ChannelMemoryObject as input.
    assert!(matches!(
        DomainSynthesisInput::reject_raw_channel_memory(&channel_mem),
        Err(PipelineError::HierarchyViolation(_))
    ));

    // Construct the legal domain input from the channel synthesis
    // output.
    let channel_output = ChannelOutput::from_channel_object(channel_object.clone()).unwrap();
    let domain_input =
        DomainSynthesisInput::new(&domain_mem, vec![channel_output.clone()]).unwrap();

    let domain_handle = open_domain_window(&mut windows, &domain_mem, Duration::hours(1)).unwrap();
    assert_eq!(domain_handle.tier, WindowScopeTier::Domain);

    let domain_result = engine
        .synthesize_domain(&mut windows, domain_handle, domain_input)
        .unwrap();
    assert_eq!(
        domain_result.object.object_type,
        SynthesisObjectType::DomainSummary
    );
    assert_eq!(domain_result.object.scope_id, domain_scope);
    let domain_window_status = windows.get(domain_handle.window_id).unwrap().status;
    assert_eq!(
        domain_window_status,
        synthesis_pipeline::WindowStatus::Complete
    );

    let domain_signed = signed_bundle(
        &signer,
        domain_result.object.id.as_uuid(),
        "domain",
        vec![EvidenceRef::from_uuid(channel_object.id.as_uuid())],
    );
    assert!(signer.verify(&domain_signed).unwrap());

    // ---------- Stage 3: tenant synthesis ----------
    // Type-system gate 3: cannot construct a TenantSynthesisInput
    // from a channel-tier object.
    assert!(matches!(
        TenantSynthesisInput::reject_channel_object(&channel_object),
        Err(PipelineError::HierarchyViolation(_))
    ));

    // Type-system gate 4: tenant input rejects domain outputs whose
    // scope is not registered on the tenant.
    let stray_domain_scope = ScopeId::new_v4();
    let stray_domain_obj = synthesis_pipeline::build_domain_summary_object(
        stray_domain_scope,
        synthesis_pipeline::WindowId::new_v4(),
        b"stray".to_vec(),
        Uuid::nil(),
    );
    let stray_domain_output = DomainOutput::from_domain_object(stray_domain_obj).unwrap();
    let stray_input = TenantSynthesisInput::new(&tenant_mem, vec![stray_domain_output], vec![]);
    assert!(matches!(
        stray_input,
        Err(PipelineError::HierarchyViolation(_))
    ));

    // Construct the legal tenant input from the domain synthesis
    // output and the admitted approved document.
    let domain_output = DomainOutput::from_domain_object(domain_result.object.clone()).unwrap();
    let approved_doc =
        ApprovedDocument::new(policy_doc.clone(), b"PII stays in jurisdiction.".to_vec());
    let tenant_input = TenantSynthesisInput::new(
        &tenant_mem,
        vec![domain_output.clone()],
        vec![approved_doc.clone()],
    )
    .unwrap();

    let tenant_handle = open_tenant_window(&mut windows, &tenant_mem, Duration::hours(1)).unwrap();
    assert_eq!(tenant_handle.tier, WindowScopeTier::Tenant);

    let tenant_result = engine
        .synthesize_tenant(&mut windows, tenant_handle, tenant_input)
        .unwrap();
    assert_eq!(
        tenant_result.object.object_type,
        SynthesisObjectType::TenantSummary
    );
    assert_eq!(tenant_result.object.scope_id, tenant_scope);

    let tenant_signed = signed_bundle(
        &signer,
        tenant_result.object.id.as_uuid(),
        "tenant",
        vec![EvidenceRef::from_uuid(domain_result.object.id.as_uuid())],
    );
    assert!(signer.verify(&tenant_signed).unwrap());

    // Sanity: the stub synthesizer's payload encodes the hierarchy
    // tier prefixes deterministically. This is what proves the
    // tenant-tier output really did derive from the domain-tier
    // output (which itself derived from the channel recap).
    let tenant_payload = String::from_utf8(tenant_result.object.payload.clone()).unwrap();
    assert!(tenant_payload.starts_with("tenant:"));
    assert!(tenant_payload.contains("domain:"));
    assert!(tenant_payload.contains("doc:"));

    // ---------- Stage 4: persist tenant-level summary as concept ----------
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("concepts.db");
    let mut tenant_concept =
        ConceptNode::new_candidate("Tenant Recap", &tenant_payload, tenant_scope);
    tenant_concept.mark_canonical();

    let concept_id = {
        let mut graph = PersistentConceptGraph::open(&db_path, &fixture_master_key()).unwrap();
        graph.add_node(tenant_concept).unwrap()
    };

    // Reload from disk and confirm the concept survived an encrypted
    // round-trip scoped to the tenant.
    {
        let mut graph = PersistentConceptGraph::open(&db_path, &fixture_master_key()).unwrap();
        let (nodes, _edges) = graph.load_scope(tenant_scope).unwrap();
        assert_eq!(nodes, 1);
        assert!(graph.graph().get_node(concept_id).is_some());
    }

    // ---------- Stage 5: append audit entries ----------
    let admin_actor = Actor::User(admin.subject_id);
    let synth_actor = Actor::Agent(Uuid::new_v4());

    audit.append(
        AuditEntryBuilder::new()
            .actor(admin_actor)
            .action(AuditActionType::CanonicalPromotion)
            .target(TargetRef::new(TargetType::Concept, concept_id.as_uuid()))
            .scope(tenant_scope)
            .details(serde_json::json!({"label": "Tenant Recap"}))
            .build()
            .unwrap(),
    );
    audit.append(
        AuditEntryBuilder::new()
            .actor(synth_actor)
            .action(AuditActionType::CanonicalPromotion)
            .target(TargetRef::new(
                TargetType::Summary,
                tenant_result.object.id.as_uuid(),
            ))
            .scope(tenant_scope)
            .details(serde_json::json!({"tier": "tenant"}))
            .build()
            .unwrap(),
    );
    audit.append(
        AuditEntryBuilder::new()
            .actor(admin_actor)
            .action(AuditActionType::MemberProvisioned)
            .target(TargetRef::new(TargetType::User, domain_member.subject_id))
            .scope(domain_scope)
            .details(serde_json::json!({"role": "member"}))
            .build()
            .unwrap(),
    );

    // Audit log invariants: append-only, chronological, scope-filterable.
    assert_eq!(audit.len(), 3);
    let domain_q = AuditQuery::new().with_scope(domain_scope);
    let domain_entries: Vec<_> = audit.query(&domain_q).collect();
    assert_eq!(domain_entries.len(), 1);
    assert_eq!(
        domain_entries[0].action_type,
        AuditActionType::MemberProvisioned
    );
    let tenant_q = AuditQuery::new().with_scope(tenant_scope);
    let tenant_entries: Vec<_> = audit.query(&tenant_q).collect();
    assert_eq!(tenant_entries.len(), 2);
    let timestamps: Vec<_> = audit.entries().iter().map(|e| e.timestamp).collect();
    assert!(timestamps.windows(2).all(|w| w[0] <= w[1]));
    let sequences: Vec<_> = audit.entries().iter().map(|e| e.sequence).collect();
    assert_eq!(sequences, vec![0, 1, 2]);

    // ---------- Stage 6: cross-tier permission rejection ----------
    // Outsider must not be able to read the tenant summary.
    assert!(!check_permission(
        &tuples,
        &ns,
        tenant_obj,
        Relation::Viewer,
        outsider
    ));
    // Channel member should not, by itself, have admin on the tenant.
    assert!(!check_permission(
        &tuples,
        &ns,
        tenant_obj,
        Relation::Admin,
        channel_member
    ));
    // Admin reaches every tier through the userset-rewrite chain.
    assert!(check_permission(
        &tuples,
        &ns,
        tenant_obj,
        Relation::Admin,
        admin
    ));
    assert!(check_permission(
        &tuples,
        &ns,
        domain_obj,
        Relation::Admin,
        admin
    ));
    assert!(check_permission(
        &tuples,
        &ns,
        channel_obj,
        Relation::Admin,
        admin
    ));
}

#[test]
fn raw_evidence_cannot_reach_tenant_window() {
    // The hierarchy module exposes no constructor that turns a raw
    // observation into a tenant input. The test here is a structural
    // assertion that the only path to building a `TenantSynthesisInput`
    // goes through `DomainOutput`s, and the only path to building
    // those goes through a `SynthesisObjectType::DomainSummary`
    // object — which in turn can only legally come from a domain
    // synthesis run (whose own input was a `DomainSynthesisInput`,
    // built from `ChannelOutput`s).
    let tenant = TenantMemoryObject::new(ScopeId::new_v4());

    // Raw channel-recap object — must NOT be admissible at tenant
    // tier. `from_domain_object` rejects non-domain types at the
    // type level.
    let raw_channel_object = SynthesisObject::new(
        ScopeId::new_v4(),
        synthesis_pipeline::WindowId::new_v4(),
        SynthesisObjectType::ChannelRecap,
        b"recap".to_vec(),
        Uuid::nil(),
    );
    assert!(matches!(
        DomainOutput::from_domain_object(raw_channel_object.clone()),
        Err(PipelineError::HierarchyViolation(_))
    ));

    assert!(matches!(
        TenantSynthesisInput::reject_channel_object(&raw_channel_object),
        Err(PipelineError::HierarchyViolation(_))
    ));

    // Empty-domain-output bundle is fine (no domain outputs to
    // admit yet) — but trying to admit unapproved docs is not.
    let unapproved = ApprovedDocument::new(
        ApprovedDocumentRef::new("not approved", "stranger"),
        b"...".to_vec(),
    );
    let err = TenantSynthesisInput::new(&tenant, vec![], vec![unapproved]).unwrap_err();
    assert!(matches!(err, PipelineError::HierarchyViolation(_)));
}
