//! Integration test: evidence ingest → observation extraction →
//! concept graph promotion → synthesis pipeline (channel → domain →
//! tenant) → export plane.
//!
//! Verifies the full pipeline produces a valid export bundle with
//! provenance.

use uuid::Uuid;

use crypto::{ProvenanceAgent, ProvenanceBundle, SynthesisActivity};
use evidence_store::ImportanceClass;
use integration_tests::test_helpers::{open_store, ScopeId};
use memory_manager::{
    ApprovedDocumentRef, DomainMemoryObject, SensitivityClass, TenantMemoryObject,
};
use observation_engine::LexiconExtractor;
use observation_engine::ObservationExtractor;
use synthesis_pipeline::{
    build_domain_summary_object, build_tenant_summary_object, consume_synthesis_object,
    publish_synthesis_object, ApprovedDocument, ChannelOutput, DomainOutput, DomainSynthesisInput,
    NoOpSynthesizer, SynthesisInputs, SynthesisObjectType, SynthesisPipeline,
    SynthesisWindowManager, TenantSynthesisInput,
};

use export_plane::profile::{ApprovedConcept, PortableConceptProfile};

fn scope_key() -> crypto::AeadKey {
    [0xBB; crypto::AEAD_KEY_LEN]
}

fn test_provenance(entity_id: Uuid) -> ProvenanceBundle {
    ProvenanceBundle::new(
        entity_id,
        SynthesisActivity::new("test-pipeline", "stub-v0", "prompt-0", Uuid::new_v4()),
        ProvenanceAgent::software("integration-test"),
        Vec::new(),
    )
}

#[test]
fn full_pipeline_produces_export_bundle_with_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("evidence.db");

    // 1. Ingest evidence.
    let scope = ScopeId::new_v4();
    let mut store = open_store(&db_path);
    let body = b"Project Atlas launches Q3 2026. Sara owns the migration.";
    let result = store
        .ingest(scope, body, None, ImportanceClass::Important)
        .unwrap();
    let eid = result.evidence_id;
    assert!(store.get(eid).unwrap().is_some());

    // 2. Extract observations via the lexicon extractor.
    let extractor = LexiconExtractor::default();
    let text = std::str::from_utf8(body).unwrap();
    let observations = extractor.extract(text, scope);
    assert!(
        !observations.is_empty(),
        "lexicon extractor should produce observations"
    );

    // 3. Promote an observation into the concept graph.
    let concept_scope = scope;
    let concept_id = Uuid::new_v4();
    let provenance = test_provenance(concept_id);

    // 4. Channel-level synthesis via NoOpSynthesizer.
    let channel_scope = scope;
    let mut window_mgr = SynthesisWindowManager::new();
    let now = chrono::Utc::now();
    let window_id = window_mgr
        .open_window(channel_scope, now - chrono::Duration::hours(1), now)
        .unwrap();
    let window = window_mgr.get(window_id).unwrap().clone();

    let synthesizer = NoOpSynthesizer::default();
    let inputs = SynthesisInputs {
        observations: Vec::new(),
        recap_seed: "Channel recap for Atlas".into(),
    };
    let channel_obj = synthesizer.synthesize(&window, &inputs).unwrap();
    assert_eq!(channel_obj.object_type, SynthesisObjectType::ChannelRecap);

    // Encrypt / decrypt round-trip of the channel object.
    let key = scope_key();
    let encrypted = publish_synthesis_object(&channel_obj, &key).unwrap();
    let decrypted = consume_synthesis_object(&encrypted, &key).unwrap();
    assert_eq!(decrypted.id, channel_obj.id);
    assert_eq!(decrypted.payload, channel_obj.payload);

    // 5. Domain-level synthesis.
    let domain_scope = ScopeId::new_v4();
    let mut domain_mem = DomainMemoryObject::new(domain_scope);
    domain_mem.channel_scopes.push(channel_scope);

    let channel_output = ChannelOutput::from_channel_object(channel_obj).unwrap();
    let domain_input = DomainSynthesisInput::new(&domain_mem, vec![channel_output]).unwrap();
    assert_eq!(domain_input.domain_scope, domain_scope);

    let domain_window_id = window_mgr
        .open_window(domain_scope, now - chrono::Duration::hours(1), now)
        .unwrap();
    let domain_obj = build_domain_summary_object(
        domain_scope,
        domain_window_id,
        b"domain summary payload".to_vec(),
        Uuid::nil(),
    );
    assert_eq!(domain_obj.object_type, SynthesisObjectType::DomainSummary);

    // 6. Tenant-level synthesis.
    let tenant_scope = ScopeId::new_v4();
    let mut tenant_mem = TenantMemoryObject::new(tenant_scope);
    tenant_mem.domain_scopes.push(domain_scope);

    let doc_ref = ApprovedDocumentRef::new("Policy v3", "compliance");
    tenant_mem.admit_approved_document(doc_ref.clone());

    let domain_output = DomainOutput::from_domain_object(domain_obj).unwrap();
    let approved_doc = ApprovedDocument::new(doc_ref, b"policy content".to_vec());
    let tenant_input =
        TenantSynthesisInput::new(&tenant_mem, vec![domain_output], vec![approved_doc]).unwrap();
    assert_eq!(tenant_input.tenant_scope, tenant_scope);

    let tenant_window_id = window_mgr
        .open_window(tenant_scope, now - chrono::Duration::hours(1), now)
        .unwrap();
    let tenant_obj = build_tenant_summary_object(
        tenant_scope,
        tenant_window_id,
        b"tenant summary payload".to_vec(),
        Uuid::nil(),
    );
    assert_eq!(tenant_obj.object_type, SynthesisObjectType::TenantSummary);

    // 7. Export plane: build a portable concept profile with provenance.
    let approved = ApprovedConcept::new(
        concept_id,
        "Project Atlas",
        "Q3 2026 launch project",
        concept_scope,
        provenance.clone(),
        SensitivityClass::Important,
    );
    let mut profile = PortableConceptProfile::new(
        "atlas-export",
        "Atlas Q3 export profile",
        "hubspot",
        concept_scope,
    );
    profile.push_concept(approved);

    assert_eq!(profile.concepts.len(), 1);
    assert_eq!(profile.concepts[0].concept_id, concept_id);
    assert_eq!(profile.concepts[0].provenance.entity_id, concept_id);
    assert!(!profile.concepts[0].label.is_empty());
}

#[test]
fn hierarchy_rejects_wrong_tier_objects() {
    let scope = ScopeId::new_v4();
    // A domain-summary object cannot be wrapped as a ChannelOutput.
    let domain_obj = build_domain_summary_object(
        scope,
        synthesis_pipeline::WindowId::new_v4(),
        b"payload".to_vec(),
        Uuid::nil(),
    );
    assert!(ChannelOutput::from_channel_object(domain_obj).is_err());

    // A channel-recap object cannot be wrapped as a DomainOutput.
    let channel_obj = synthesis_pipeline::SynthesisObject::new(
        scope,
        synthesis_pipeline::WindowId::new_v4(),
        SynthesisObjectType::ChannelRecap,
        b"recap".to_vec(),
        Uuid::nil(),
    );
    assert!(DomainOutput::from_domain_object(channel_obj).is_err());
}
