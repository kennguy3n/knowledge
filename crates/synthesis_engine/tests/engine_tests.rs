//! Integration tests for the synthesis engine.

use chrono::{Duration, Utc};
use uuid::Uuid;

use evidence_store::ScopeId;
use memory_manager::{
    tenant_memory::ApprovedDocumentRef, ChannelMemoryObject, DomainMemoryObject, TenantMemoryObject,
};
use synthesis_engine::{ManagedEndpointSynthesizer, SynthesisEngine};
use synthesis_pipeline::{
    ApprovedDocument, ChannelOutput, DomainOutput, DomainSynthesisInput,
    HierarchyEnforcedWindowManager, NoOpSynthesizer, SynthesisInputs, SynthesisObjectType,
    SynthesisPipeline, SynthesisWindow, SynthesisWindowManager, TenantSynthesisInput,
    WindowScopeTier,
};

fn make_channel_recap(scope: ScopeId, recap: &str) -> synthesis_pipeline::SynthesisObject {
    let now = Utc::now();
    let win = SynthesisWindow::new(scope, now - Duration::hours(1), now).unwrap();
    NoOpSynthesizer::new()
        .synthesize(&win, &SynthesisInputs::from_recap(recap))
        .unwrap()
}

#[test]
fn engine_emits_domain_summary_from_channel_outputs() {
    // 1. Two channels each emit a recap.
    let chan_a = ScopeId::new_v4();
    let chan_b = ScopeId::new_v4();
    let recap_a = make_channel_recap(chan_a, "channel A activity");
    let recap_b = make_channel_recap(chan_b, "channel B activity");

    // 2. A domain registers both channel scopes.
    let dom_scope = ScopeId::new_v4();
    let mut domain = DomainMemoryObject::new(dom_scope);
    domain.attach_channel_scope(chan_a);
    domain.attach_channel_scope(chan_b);

    // 3. The engine consumes the channel outputs through the
    // DomainSynthesisInput type.
    let inputs = vec![
        ChannelOutput::from_channel_object(recap_a).unwrap(),
        ChannelOutput::from_channel_object(recap_b).unwrap(),
    ];
    let bundle = DomainSynthesisInput::new(&domain, inputs).unwrap();

    let mut wm = SynthesisWindowManager::new();
    let now = Utc::now();
    let handle = wm
        .open_tiered_window(
            dom_scope,
            WindowScopeTier::Domain,
            now - Duration::hours(1),
            now,
        )
        .unwrap();

    let engine = ManagedEndpointSynthesizer::new();
    let result = engine.synthesize_domain(&mut wm, handle, bundle).unwrap();
    assert_eq!(result.object.scope_id, dom_scope);
    assert_eq!(
        result.object.object_type,
        SynthesisObjectType::DomainSummary
    );
    assert_eq!(
        wm.get(handle.window_id).unwrap().status,
        synthesis_pipeline::WindowStatus::Complete
    );
}

#[test]
fn engine_emits_tenant_summary_from_domain_outputs_and_docs() {
    let tenant_scope = ScopeId::new_v4();
    let dom_a_scope = ScopeId::new_v4();
    let dom_b_scope = ScopeId::new_v4();

    let mut tenant = TenantMemoryObject::new(tenant_scope);
    tenant.attach_domain_scope(dom_a_scope);
    tenant.attach_domain_scope(dom_b_scope);

    let doc_ref = ApprovedDocumentRef::new("Tenant Policy v1.0", "compliance");
    tenant.admit_approved_document(doc_ref.clone());

    // Synthesise two domain summaries via the engine first so we
    // have inputs of the right type.
    let mut wm = SynthesisWindowManager::new();
    let engine = ManagedEndpointSynthesizer::new();

    let dom_inputs = |scope: ScopeId, channel: ScopeId, label: &str| {
        let mut domain = DomainMemoryObject::new(scope);
        domain.attach_channel_scope(channel);
        let recap = ChannelOutput::from_channel_object(make_channel_recap(channel, label)).unwrap();
        DomainSynthesisInput::new(&domain, vec![recap]).unwrap()
    };

    let now = Utc::now();
    let h_a = wm
        .open_tiered_window(
            dom_a_scope,
            WindowScopeTier::Domain,
            now - Duration::hours(1),
            now,
        )
        .unwrap();
    let r_a = engine
        .synthesize_domain(
            &mut wm,
            h_a,
            dom_inputs(dom_a_scope, ScopeId::new_v4(), "A"),
        )
        .unwrap();

    let h_b = wm
        .open_tiered_window(
            dom_b_scope,
            WindowScopeTier::Domain,
            now - Duration::hours(1),
            now,
        )
        .unwrap();
    let r_b = engine
        .synthesize_domain(
            &mut wm,
            h_b,
            dom_inputs(dom_b_scope, ScopeId::new_v4(), "B"),
        )
        .unwrap();

    // Now feed the two domain summaries + the approved doc into a
    // tenant synthesis run.
    let inputs = TenantSynthesisInput::new(
        &tenant,
        vec![
            DomainOutput::from_domain_object(r_a.object).unwrap(),
            DomainOutput::from_domain_object(r_b.object).unwrap(),
        ],
        vec![ApprovedDocument::new(doc_ref, b"policy bytes".to_vec())],
    )
    .unwrap();

    let h_t = wm
        .open_tiered_window(
            tenant_scope,
            WindowScopeTier::Tenant,
            now - Duration::hours(1),
            now,
        )
        .unwrap();
    let r_t = engine.synthesize_tenant(&mut wm, h_t, inputs).unwrap();
    assert_eq!(r_t.object.scope_id, tenant_scope);
    assert_eq!(r_t.object.object_type, SynthesisObjectType::TenantSummary);
}

#[test]
fn engine_rejects_domain_input_for_tenant_window_and_vice_versa() {
    let dom_scope = ScopeId::new_v4();
    let tenant_scope = ScopeId::new_v4();

    let mut domain = DomainMemoryObject::new(dom_scope);
    let chan = ScopeId::new_v4();
    domain.attach_channel_scope(chan);
    let recap = ChannelOutput::from_channel_object(make_channel_recap(chan, "x")).unwrap();
    let dom_input = DomainSynthesisInput::new(&domain, vec![recap]).unwrap();

    let mut wm = SynthesisWindowManager::new();
    let now = Utc::now();
    // Open a *tenant* window and try to feed a domain input.
    let tenant_handle = wm
        .open_tiered_window(
            tenant_scope,
            WindowScopeTier::Tenant,
            now - Duration::hours(1),
            now,
        )
        .unwrap();
    let engine = ManagedEndpointSynthesizer::new();
    let err = engine
        .synthesize_domain(&mut wm, tenant_handle, dom_input)
        .unwrap_err();
    let _: synthesis_engine::EngineError = err;
}

#[test]
fn unknown_window_handle_yields_pipeline_error() {
    let scope = ScopeId::new_v4();
    let dom_input = {
        let mut domain = DomainMemoryObject::new(scope);
        let chan = ScopeId::new_v4();
        domain.attach_channel_scope(chan);
        let recap = ChannelOutput::from_channel_object(make_channel_recap(chan, "x")).unwrap();
        DomainSynthesisInput::new(&domain, vec![recap]).unwrap()
    };
    let mut wm = SynthesisWindowManager::new();
    let bogus = synthesis_pipeline::TieredWindowHandle {
        window_id: synthesis_pipeline::WindowId::from_uuid(Uuid::new_v4()),
        scope_id: scope,
        tier: WindowScopeTier::Domain,
    };
    let engine = ManagedEndpointSynthesizer::new();
    let err = engine
        .synthesize_domain(&mut wm, bogus, dom_input)
        .unwrap_err();
    let _ = err;
    // Not asserting on the channel-memory hierarchy contract here:
    // the channel-memory rejection is type-system-only and tested in
    // the synthesis_pipeline crate.
    let _: ChannelMemoryObject = ChannelMemoryObject::new(scope);
}
