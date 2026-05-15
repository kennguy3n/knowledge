//! Phase 5 — Synthesis Pipeline.
//!
//! Drives the full channel → domain → tenant synthesis chain
//! described in `docs/DESIGN.md` §6 and `docs/internal/PHASES.md` Phase 2/3.
//!
//! * **Channel tier** — opens a [`SynthesisWindowManager`] window per
//!   channel scope, runs the [`NoOpSynthesizer`] to emit a
//!   [`SynthesisObjectType::ChannelRecap`], and round-trips it through
//!   [`publish_synthesis_object`] / [`consume_synthesis_object`] so
//!   the AEAD AAD binding is exercised end-to-end.
//! * **Domain tier** — registers each channel scope on a
//!   [`DomainMemoryObject`], assembles a [`DomainSynthesisInput`]
//!   from the channel outputs, and synthesises a
//!   [`SynthesisObjectType::DomainSummary`] via
//!   [`ManagedEndpointSynthesizer::synthesize_domain`].
//! * **Tenant tier** — registers the domain scope and an admitted
//!   [`ApprovedDocumentRef`] on a [`TenantMemoryObject`], assembles a
//!   [`TenantSynthesisInput`] from the domain output + approved doc,
//!   and synthesises a [`SynthesisObjectType::TenantSummary`].
//! * **Hierarchy enforcement** — explicitly attempts the three
//!   forbidden transitions (raw [`ChannelMemoryObject`] -> domain,
//!   channel object -> tenant, mismatched scope handle) and asserts
//!   each returns [`PipelineError::HierarchyViolation`] /
//!   [`EngineError::Hierarchy`].

use std::time::Instant;

use chrono::{Duration, Utc};
use memory_manager::{
    ApprovedDocumentRef, ChannelMemoryObject, DomainMemoryObject, TenantMemoryObject,
};
use synthesis_engine::{ManagedEndpointSynthesizer, SynthesisEngine};
use synthesis_pipeline::{
    consume_synthesis_object, open_domain_window, open_tenant_window, publish_synthesis_object,
    ApprovedDocument, ChannelOutput, DomainOutput, DomainSynthesisInput, NoOpSynthesizer,
    PipelineError, SynthesisInputs, SynthesisObject, SynthesisObjectType, SynthesisPipeline,
    SynthesisWindow, SynthesisWindowManager, TenantSynthesisInput, TieredWindowHandle,
    WindowScopeTier, WindowStatus,
};

use crate::assertions::AssertionLog;
use crate::dataset::{Dataset, ScopeTier};
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "phase05_synthesis";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Phase 5: Synthesis Pipeline");

    let now = Utc::now();
    let mut windows = SynthesisWindowManager::new();

    // Reuse the master key directly as the per-scope AEAD key for the
    // demo. In production each scope would have its own DEK derived
    // from the master key; here we only exercise the publish/consume
    // contract, so a single key is sufficient.
    let scope_key: [u8; 32] = state.master_key;

    // ------- Channel tier -------------------------------------------
    let channel_scopes = [&dataset.channel_scope, &dataset.channel_alt_scope];
    let mut channel_recap_objects: Vec<SynthesisObject> = Vec::new();
    let mut channel_pub_consume_failures: u64 = 0;
    let channel_started = Instant::now();

    for chan in channel_scopes {
        let window_id = windows
            .open_window(chan.id, now - Duration::hours(1), now)
            .expect("open channel window");
        windows
            .mark_in_progress(window_id)
            .expect("channel window -> in_progress");

        // Build a recap seed from every Phase-1 row that landed in
        // this channel scope. Real recap content -- the synthesizer
        // (NoOp) just copies it through.
        let recap_seed: String = state
            .ingested_rows
            .iter()
            .filter(|r| r.scope_id == chan.id)
            .map(|r| r.body.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        let inputs = SynthesisInputs::from_recap(if recap_seed.is_empty() {
            format!("(no rows in {})", chan.label)
        } else {
            recap_seed
        });

        let synthesizer = NoOpSynthesizer::new();
        let window = windows.get(window_id).expect("just-opened window").clone();
        let object = synthesizer
            .synthesize(&window, &inputs)
            .expect("channel synthesizer must emit recap");

        // AEAD round-trip via publish/consume.
        let envelope =
            publish_synthesis_object(&object, &scope_key).expect("publish channel recap");
        let decrypted =
            consume_synthesis_object(&envelope, &scope_key).expect("consume channel recap");
        if decrypted != object {
            channel_pub_consume_failures = channel_pub_consume_failures.saturating_add(1);
        }

        windows
            .mark_complete(window_id)
            .expect("channel window -> complete");
        channel_recap_objects.push(object);
    }
    let channel_elapsed = channel_started.elapsed();
    let channel_object_count = channel_recap_objects.len() as u64;

    // ------- Domain tier --------------------------------------------
    let mut domain_memory = DomainMemoryObject::new(dataset.domain_scope.id);
    for chan in channel_scopes {
        domain_memory.attach_channel_scope(chan.id);
    }

    let domain_handle = open_domain_window(&mut windows, &domain_memory, Duration::hours(1))
        .expect("open domain window");

    let channel_outputs: Vec<ChannelOutput> = channel_recap_objects
        .iter()
        .cloned()
        .map(|o| ChannelOutput::from_channel_object(o).expect("channel-recap -> channel output"))
        .collect();

    let domain_input = DomainSynthesisInput::new(&domain_memory, channel_outputs.clone())
        .expect("admit channel outputs into domain input");

    let engine = ManagedEndpointSynthesizer::new();
    let domain_started = Instant::now();
    let domain_result = engine
        .synthesize_domain(&mut windows, domain_handle, domain_input.clone())
        .expect("ManagedEndpointSynthesizer::synthesize_domain");
    let domain_elapsed = domain_started.elapsed();

    // Verify the domain output round-trips through the AEAD envelope.
    let domain_envelope = publish_synthesis_object(&domain_result.object, &scope_key)
        .expect("publish domain summary");
    let domain_decrypted =
        consume_synthesis_object(&domain_envelope, &scope_key).expect("consume domain summary");

    let domain_window_status = windows
        .get(domain_handle.window_id)
        .map_or(WindowStatus::Pending, |w| w.status);

    // ------- Tenant tier --------------------------------------------
    let mut tenant_memory = TenantMemoryObject::new(dataset.tenant_scope.id);
    tenant_memory.attach_domain_scope(dataset.domain_scope.id);
    let approved_ref =
        ApprovedDocumentRef::new("Tenant Policy v3.2 (data residency)", "compliance-officer");
    tenant_memory.admit_approved_document(approved_ref.clone());
    let approved_doc = ApprovedDocument::new(
        approved_ref.clone(),
        b"customer data MUST stay in EU regions; no exceptions".to_vec(),
    );

    let tenant_handle = open_tenant_window(&mut windows, &tenant_memory, Duration::hours(2))
        .expect("open tenant window");

    let domain_outputs = vec![
        DomainOutput::from_domain_object(domain_result.object.clone())
            .expect("domain summary -> domain output"),
    ];
    let tenant_input = TenantSynthesisInput::new(
        &tenant_memory,
        domain_outputs.clone(),
        vec![approved_doc.clone()],
    )
    .expect("admit domain output + approved doc into tenant input");

    let tenant_started = Instant::now();
    let tenant_result = engine
        .synthesize_tenant(&mut windows, tenant_handle, tenant_input.clone())
        .expect("ManagedEndpointSynthesizer::synthesize_tenant");
    let tenant_elapsed = tenant_started.elapsed();

    let tenant_envelope = publish_synthesis_object(&tenant_result.object, &scope_key)
        .expect("publish tenant summary");
    let tenant_decrypted =
        consume_synthesis_object(&tenant_envelope, &scope_key).expect("consume tenant summary");

    let tenant_window_status = windows
        .get(tenant_handle.window_id)
        .map_or(WindowStatus::Pending, |w| w.status);

    // ------- Hierarchy enforcement (negative tests) ----------------
    // 1. Raw ChannelMemoryObject cannot become a domain input.
    let raw_channel = ChannelMemoryObject::new(dataset.channel_scope.id);
    let raw_channel_rejected = matches!(
        DomainSynthesisInput::reject_raw_channel_memory(&raw_channel),
        Err(PipelineError::HierarchyViolation(_))
    );

    // 2. A channel-recap synthesis object cannot be admitted directly
    //    as a tenant input.
    let stray_channel = channel_recap_objects[0].clone();
    let stray_channel_rejected = matches!(
        TenantSynthesisInput::reject_channel_object(&stray_channel),
        Err(PipelineError::HierarchyViolation(_))
    );

    // 3. A channel-tier window cannot be promoted to domain by
    //    handing it a `DomainSynthesisInput`.
    let bogus_channel_window = windows
        .open_window(dataset.channel_scope.id, now - Duration::minutes(30), now)
        .expect("open noisy channel window for negative test");
    let smuggle_handle = TieredWindowHandle {
        window_id: bogus_channel_window,
        scope_id: dataset.channel_scope.id,
        tier: WindowScopeTier::Channel,
    };
    let smuggle_input = DomainSynthesisInput::new(&domain_memory, channel_outputs.clone())
        .expect("rebuild domain input");
    let mismatched_engine = ManagedEndpointSynthesizer::new();
    let smuggle_result =
        mismatched_engine.synthesize_domain(&mut windows, smuggle_handle, smuggle_input);
    let smuggle_rejected = matches!(
        smuggle_result,
        Err(synthesis_engine::EngineError::Hierarchy(_))
    );

    // 4. Build a fake handle whose `scope_id` claims a different
    //    scope than the underlying window. The validate methods are
    //    supposed to refuse this even with the correct tier tag.
    let off_scope_window = windows
        .open_window(dataset.user_scope.id, now - Duration::minutes(15), now)
        .expect("user-scope window for off-scope smuggling");
    let off_scope_handle = TieredWindowHandle {
        window_id: off_scope_window,
        scope_id: dataset.domain_scope.id, // claim domain scope
        tier: WindowScopeTier::Domain,
    };
    let off_scope_input =
        DomainSynthesisInput::new(&domain_memory, channel_outputs.clone()).unwrap();
    let off_scope_result =
        mismatched_engine.synthesize_domain(&mut windows, off_scope_handle, off_scope_input);
    let off_scope_rejected = matches!(
        off_scope_result,
        Err(synthesis_engine::EngineError::Hierarchy(_))
    );

    // ------- Window-status sanity check ----------------------------
    let mut total_pending = 0_usize;
    let mut total_in_progress = 0_usize;
    let mut total_complete = 0_usize;
    let mut total_failed = 0_usize;
    for chan in channel_scopes {
        for w in windows.windows_for(chan.id) {
            match w.status {
                WindowStatus::Pending => total_pending += 1,
                WindowStatus::InProgress => total_in_progress += 1,
                WindowStatus::Complete => total_complete += 1,
                WindowStatus::Failed => total_failed += 1,
            }
        }
    }
    for w in windows.windows_for(dataset.domain_scope.id) {
        match w.status {
            WindowStatus::Pending => total_pending += 1,
            WindowStatus::InProgress => total_in_progress += 1,
            WindowStatus::Complete => total_complete += 1,
            WindowStatus::Failed => total_failed += 1,
        }
    }
    for w in windows.windows_for(dataset.tenant_scope.id) {
        match w.status {
            WindowStatus::Pending => total_pending += 1,
            WindowStatus::InProgress => total_in_progress += 1,
            WindowStatus::Complete => total_complete += 1,
            WindowStatus::Failed => total_failed += 1,
        }
    }

    // ------- SynthesisWindow constructor sanity --------------------
    // Reject zero-duration windows (per `PipelineError::InvalidWindow`).
    let zero_window_rejected = matches!(
        SynthesisWindow::new(dataset.user_scope.id, now, now),
        Err(PipelineError::InvalidWindow)
    );

    // ------- Assertions --------------------------------------------
    log.check(
        PHASE,
        "every channel scope produced one ChannelRecap synthesis object",
        channel_object_count == channel_scopes.len() as u64
            && channel_recap_objects
                .iter()
                .all(|o| o.object_type == SynthesisObjectType::ChannelRecap),
    );
    log.check(
        PHASE,
        "channel publish/consume AEAD round-trip succeeded for every recap",
        channel_pub_consume_failures == 0,
    );
    log.check(
        PHASE,
        "domain synthesizer emitted a DomainSummary object",
        domain_result.object.object_type == SynthesisObjectType::DomainSummary,
    );
    log.check(
        PHASE,
        "domain summary AEAD round-trip preserved the object",
        domain_decrypted == domain_result.object,
    );
    log.check(
        PHASE,
        "tenant synthesizer emitted a TenantSummary object",
        tenant_result.object.object_type == SynthesisObjectType::TenantSummary,
    );
    log.check(
        PHASE,
        "tenant summary AEAD round-trip preserved the object",
        tenant_decrypted == tenant_result.object,
    );
    log.check(
        PHASE,
        "domain window finished in Complete state",
        domain_window_status == WindowStatus::Complete,
    );
    log.check(
        PHASE,
        "tenant window finished in Complete state",
        tenant_window_status == WindowStatus::Complete,
    );
    log.check(
        PHASE,
        "raw ChannelMemoryObject is rejected as a domain input",
        raw_channel_rejected,
    );
    log.check(
        PHASE,
        "channel-recap object is rejected as a tenant input",
        stray_channel_rejected,
    );
    log.check(
        PHASE,
        "channel-tier window cannot consume a DomainSynthesisInput",
        smuggle_rejected,
    );
    log.check(
        PHASE,
        "off-scope handle is rejected even with the matching tier tag",
        off_scope_rejected,
    );
    log.check(
        PHASE,
        "SynthesisWindow rejects zero-duration intervals",
        zero_window_rejected,
    );

    // ------- State carry forward + reporting -----------------------
    state.channel_output_count = channel_object_count;
    state.domain_output_count = 1;
    state.tenant_output_count = 1;

    let domain_audit = audit_service::AuditEntryBuilder::new()
        .actor(audit_service::Actor::System)
        .action(audit_service::AuditActionType::CanonicalPromotion)
        .target(audit_service::TargetRef::new(
            audit_service::TargetType::Summary,
            domain_result.object.id.0,
        ))
        .scope(dataset.domain_scope.id)
        .details(serde_json::json!({
            "tier": "domain",
            "channel_inputs": channel_object_count,
            "payload_bytes": domain_result.object.payload.len(),
        }))
        .build()
        .expect("domain synthesis audit entry");
    state.audit_log.append(domain_audit);

    let tenant_audit = audit_service::AuditEntryBuilder::new()
        .actor(audit_service::Actor::System)
        .action(audit_service::AuditActionType::CanonicalPromotion)
        .target(audit_service::TargetRef::new(
            audit_service::TargetType::Summary,
            tenant_result.object.id.0,
        ))
        .scope(dataset.tenant_scope.id)
        .details(serde_json::json!({
            "tier": "tenant",
            "domain_inputs": domain_outputs.len(),
            "approved_documents": tenant_input.approved_documents.len(),
            "payload_bytes": tenant_result.object.payload.len(),
        }))
        .build()
        .expect("tenant synthesis audit entry");
    state.audit_log.append(tenant_audit);

    phase.timing = started.elapsed();
    phase.stat("channel_recaps", channel_object_count.to_string());
    phase.stat(
        "channel_pub_consume_failures",
        channel_pub_consume_failures.to_string(),
    );
    phase.stat(
        "domain_summary_payload_bytes",
        domain_result.object.payload.len().to_string(),
    );
    phase.stat(
        "tenant_summary_payload_bytes",
        tenant_result.object.payload.len().to_string(),
    );
    phase.stat(
        "scope_tiers_exercised",
        "user, channel, domain, tenant".to_string(),
    );
    phase.stat(
        "scope_total_messages",
        state
            .ingested_rows
            .iter()
            .filter(|r| matches!(r.scope_tier, ScopeTier::Channel))
            .count()
            .to_string(),
    );
    phase.stat("windows_complete", total_complete.to_string());
    phase.stat("windows_pending", total_pending.to_string());
    phase.stat("windows_in_progress", total_in_progress.to_string());
    phase.stat("windows_failed", total_failed.to_string());
    phase.note(
        "Channel (NoOpSynthesizer) -> Domain (ManagedEndpointSynthesizer) -> \
         Tenant (ManagedEndpointSynthesizer) with AEAD publish/consume + \
         four hierarchy-enforcement negative tests.",
    );

    report.count("channel_recap_objects", channel_object_count);
    report.count("domain_summary_objects", 1);
    report.count("tenant_summary_objects", 1);
    report.count("synthesis_hierarchy_rejections", 4);
    report.add_phase(phase);
    report.add_benchmark(
        "synthesis_channel_tier",
        channel_object_count,
        channel_elapsed,
    );
    report.add_benchmark("synthesis_domain_tier", 1, domain_elapsed);
    report.add_benchmark("synthesis_tenant_tier", 1, tenant_elapsed);
}
