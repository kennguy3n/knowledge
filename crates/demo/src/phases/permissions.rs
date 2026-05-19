//! Stage 6 — Permission Service.
//!
//! Builds a Zanzibar-style relation graph for the
//! tenant → domain → channel hierarchy used in earlier stages and
//! exercises [`check_permission`] against it. Per `docs/DESIGN.md` §7.1
//! and `ARCHITECTURE.md` §6, every access decision in the substrate
//! is a reachability query over a graph of relation tuples; this
//! stage plants the graph and runs the queries.
//!
//! The graph wired up here is:
//!
//! * **Tenant**: one *owner* (alice), one *admin* (bob), one
//!   *member* (carol).
//! * **Domain**: editor relation rewritten through `tenant#admin`,
//!   so bob inherits Editor on the domain even though he was only
//!   bound directly as a tenant admin.
//! * **Channel**: viewer relation rewritten through `domain#member`,
//!   plus a direct *editor* binding for dave so the namespace
//!   chain Editor→Member→Viewer is exercised on a real subject.
//! * **Outsider** (eve): no tuples whatsoever.
//!
//! Reachability checks then verify:
//!
//! * Owner reaches every tier and every relation.
//! * Admin reaches editor / member / viewer everywhere through the
//!   namespace chain.
//! * Member sees viewer / member but not editor / admin / owner.
//! * Userset rewrites resolve correctly across two hops
//!   (channel#viewer ⇐ domain#member ⇐ tenant#member).
//! * Outsiders are rejected for every relation on every tier.
//! * Duplicate inserts return [`PermissionError::DuplicateTuple`].
//! * Unknown removals return [`PermissionError::NotFound`].

use std::time::Instant;

use permission_service::{
    check_permission, NamespaceConfig, NamespaceRegistry, ObjectRef, ObjectType, PermissionError,
    Relation, RelationTuple, SubjectRef, SubjectType, TupleStore,
};
use uuid::Uuid;

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "permissions";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Stage 6: Permission Service");

    // -------- Namespaces --------------------------------------------
    // Use the substrate's default chain (Owner ⇒ Admin ⇒ Editor ⇒
    // Member ⇒ Viewer) and add an explicit `Agent` namespace so the
    // managed-endpoint synthesizer can be granted Synthesizer rights
    // on the tenant in later phases.
    let mut namespaces = NamespaceRegistry::with_defaults();
    let agent_namespace = NamespaceConfig::new(ObjectType::Agent)
        .imply(Relation::Owner, &[Relation::Admin])
        .imply(Relation::Admin, &[Relation::Editor]);
    namespaces
        .register(agent_namespace)
        .expect("agent namespace must register cleanly");

    // -------- Subjects ----------------------------------------------
    // Deterministic UUIDs so the demo's audit trail is reproducible
    // across runs.
    let alice = Uuid::from_u128(0x4711_0000_0000_0000_0000_0000_0000_0001);
    let bob = Uuid::from_u128(0x4711_0000_0000_0000_0000_0000_0000_0002);
    let carol = Uuid::from_u128(0x4711_0000_0000_0000_0000_0000_0000_0003);
    let dave = Uuid::from_u128(0x4711_0000_0000_0000_0000_0000_0000_0004);
    let eve = Uuid::from_u128(0x4711_0000_0000_0000_0000_0000_0000_0005);
    let synthesis_agent = Uuid::from_u128(0x4711_0000_0000_0000_0000_0000_0000_0099);

    // Reuse the dataset scope ids so reachability checks are tied to
    // the same hierarchy that Phases 1–5 ingested into.
    let tenant_obj = ObjectRef::new(ObjectType::Tenant, dataset.tenant_scope.id.0);
    let domain_obj = ObjectRef::new(ObjectType::Domain, dataset.domain_scope.id.0);
    let channel_obj = ObjectRef::new(ObjectType::Channel, dataset.channel_scope.id.0);
    let channel_alt_obj = ObjectRef::new(ObjectType::Channel, dataset.channel_alt_scope.id.0);
    let agent_obj = ObjectRef::new(ObjectType::Agent, synthesis_agent);

    let alice_subject = SubjectRef::direct(SubjectType::User, alice);
    let bob_subject = SubjectRef::direct(SubjectType::User, bob);
    let carol_subject = SubjectRef::direct(SubjectType::User, carol);
    let dave_subject = SubjectRef::direct(SubjectType::User, dave);
    let eve_subject = SubjectRef::direct(SubjectType::User, eve);
    let agent_subject = SubjectRef::direct(SubjectType::Agent, synthesis_agent);

    // -------- Tuple store -------------------------------------------
    let mut store = TupleStore::new();

    // Direct tenant bindings.
    store
        .insert(RelationTuple::new(
            tenant_obj,
            Relation::Owner,
            alice_subject,
        ))
        .expect("owner tuple inserted cleanly");
    store
        .insert(RelationTuple::new(tenant_obj, Relation::Admin, bob_subject))
        .expect("admin tuple inserted cleanly");
    store
        .insert(RelationTuple::new(
            tenant_obj,
            Relation::Member,
            carol_subject,
        ))
        .expect("member tuple inserted cleanly");
    store
        .insert(RelationTuple::new(
            agent_obj,
            Relation::Owner,
            agent_subject,
        ))
        .expect("agent owner tuple inserted cleanly");

    // Userset-rewrite tuples wiring the hierarchy. `Domain#editor` is
    // anything that is `Tenant#admin`. `Channel#viewer` is anything
    // that is `Domain#member`. With the default inheritance chain
    // this means a tenant Owner ⇒ Admin and therefore Domain Editor
    // ⇒ Member ⇒ Viewer, all the way down to channel viewer.
    let domain_editor_via_tenant_admin = SubjectRef::via(
        SubjectType::Tenant,
        dataset.tenant_scope.id.0,
        Relation::Admin,
    );
    store
        .insert(RelationTuple::new(
            domain_obj,
            Relation::Editor,
            domain_editor_via_tenant_admin,
        ))
        .expect("domain editor->tenant admin tuple inserted");
    let domain_member_via_tenant_member = SubjectRef::via(
        SubjectType::Tenant,
        dataset.tenant_scope.id.0,
        Relation::Member,
    );
    store
        .insert(RelationTuple::new(
            domain_obj,
            Relation::Member,
            domain_member_via_tenant_member,
        ))
        .expect("domain member->tenant member tuple inserted");
    let channel_viewer_via_domain_member = SubjectRef::via(
        SubjectType::Domain,
        dataset.domain_scope.id.0,
        Relation::Member,
    );
    store
        .insert(RelationTuple::new(
            channel_obj,
            Relation::Viewer,
            channel_viewer_via_domain_member,
        ))
        .expect("channel viewer->domain member tuple inserted");

    // Direct editor binding on the alternate channel for dave so
    // namespace inheritance Editor ⇒ Member ⇒ Viewer can be tested
    // on a real subject without any rewrites.
    store
        .insert(RelationTuple::new(
            channel_alt_obj,
            Relation::Editor,
            dave_subject,
        ))
        .expect("dave editor tuple inserted cleanly");

    // Userset rewrite wiring `channel_alt#member <- domain#member` so
    // anyone holding (or implying) `domain#member` automatically holds
    // Member on the alternate channel. Combined with the existing
    // `domain#member <- tenant#member` rewrite this gives a real
    // Owner→Admin→Editor→Member chain landing on the alt channel
    // (alice is tenant Owner ⇒ tenant Member by namespace inheritance,
    // therefore domain Member by rewrite, therefore channel-alt Member
    // by this rewrite).
    let channel_alt_member_via_domain_member = SubjectRef::via(
        SubjectType::Domain,
        dataset.domain_scope.id.0,
        Relation::Member,
    );
    store
        .insert(RelationTuple::new(
            channel_alt_obj,
            Relation::Member,
            channel_alt_member_via_domain_member,
        ))
        .expect("channel-alt member->domain member tuple inserted");

    // -------- Negative API tests ------------------------------------
    // 1. Duplicate insert must error.
    let duplicate_err = store
        .insert(RelationTuple::new(
            tenant_obj,
            Relation::Owner,
            alice_subject,
        ))
        .expect_err("duplicate tuple must error");
    let duplicate_rejected = matches!(duplicate_err, PermissionError::DuplicateTuple);

    // 2. Removing a tuple that was never inserted must error.
    let phantom = RelationTuple::new(channel_obj, Relation::Editor, eve_subject);
    let phantom_remove_err = store
        .remove(&phantom)
        .expect_err("removing absent tuple must error");
    let phantom_rejected = matches!(phantom_remove_err, PermissionError::NotFound);

    // 3. `upsert` is idempotent — a second call returns `false`.
    let upsert_inserted = store.upsert(RelationTuple::new(
        tenant_obj,
        Relation::Synthesizer,
        agent_subject,
    ));
    let upsert_idempotent = !store.upsert(RelationTuple::new(
        tenant_obj,
        Relation::Synthesizer,
        agent_subject,
    ));

    // -------- Reachability checks -----------------------------------
    let benchmark_started = Instant::now();
    let mut total_checks: u64 = 0;
    let mut allowed_checks: u64 = 0;
    let mut denied_checks: u64 = 0;
    let record =
        |allowed: bool, total: &mut u64, allowed_count: &mut u64, denied_count: &mut u64| {
            *total += 1;
            if allowed {
                *allowed_count += 1;
            } else {
                *denied_count += 1;
            }
        };

    // Alice (owner) reaches every relation on every tier (and the
    // agent because she owns it via Owner->Admin->Editor).
    let alice_tenant_owner = check_permission(
        &store,
        &namespaces,
        tenant_obj,
        Relation::Owner,
        alice_subject,
    );
    record(
        alice_tenant_owner,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let alice_channel_viewer = check_permission(
        &store,
        &namespaces,
        channel_obj,
        Relation::Viewer,
        alice_subject,
    );
    record(
        alice_channel_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let alice_channel_alt_member = check_permission(
        &store,
        &namespaces,
        channel_alt_obj,
        Relation::Member,
        alice_subject,
    );
    record(
        alice_channel_alt_member,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );

    // Bob (admin) reaches Editor on the domain via the userset
    // rewrite, then Member / Viewer on the channel via the chain.
    let bob_domain_editor = check_permission(
        &store,
        &namespaces,
        domain_obj,
        Relation::Editor,
        bob_subject,
    );
    record(
        bob_domain_editor,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let bob_channel_viewer = check_permission(
        &store,
        &namespaces,
        channel_obj,
        Relation::Viewer,
        bob_subject,
    );
    record(
        bob_channel_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let bob_tenant_owner = check_permission(
        &store,
        &namespaces,
        tenant_obj,
        Relation::Owner,
        bob_subject,
    );
    // Bob is admin not owner -> denied.
    record(
        bob_tenant_owner,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );

    // Carol (member) reaches Viewer / Member on the channel via the
    // double rewrite, but NOT Editor / Admin / Owner anywhere.
    let carol_channel_viewer = check_permission(
        &store,
        &namespaces,
        channel_obj,
        Relation::Viewer,
        carol_subject,
    );
    record(
        carol_channel_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let carol_channel_member = check_permission(
        &store,
        &namespaces,
        channel_obj,
        Relation::Member,
        carol_subject,
    );
    // Carol -> tenant#member -> domain#member rewrite. The channel
    // tuple is for Viewer though — Member is *not* implied from
    // Viewer, so this should be denied.
    record(
        carol_channel_member,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let carol_channel_editor = check_permission(
        &store,
        &namespaces,
        channel_obj,
        Relation::Editor,
        carol_subject,
    );
    record(
        carol_channel_editor,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let carol_tenant_admin = check_permission(
        &store,
        &namespaces,
        tenant_obj,
        Relation::Admin,
        carol_subject,
    );
    record(
        carol_tenant_admin,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );

    // Dave (channel-alt editor) inherits Member / Viewer through the
    // namespace chain.
    let dave_channel_alt_member = check_permission(
        &store,
        &namespaces,
        channel_alt_obj,
        Relation::Member,
        dave_subject,
    );
    record(
        dave_channel_alt_member,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let dave_channel_alt_viewer = check_permission(
        &store,
        &namespaces,
        channel_alt_obj,
        Relation::Viewer,
        dave_subject,
    );
    record(
        dave_channel_alt_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let dave_channel_alt_admin = check_permission(
        &store,
        &namespaces,
        channel_alt_obj,
        Relation::Admin,
        dave_subject,
    );
    record(
        dave_channel_alt_admin,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    // dave was given editor on alt channel only — main channel must
    // deny him.
    let dave_channel_main_viewer = check_permission(
        &store,
        &namespaces,
        channel_obj,
        Relation::Viewer,
        dave_subject,
    );
    record(
        dave_channel_main_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );

    // Eve (outsider) is denied everywhere.
    let eve_tenant_viewer = check_permission(
        &store,
        &namespaces,
        tenant_obj,
        Relation::Viewer,
        eve_subject,
    );
    record(
        eve_tenant_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let eve_domain_viewer = check_permission(
        &store,
        &namespaces,
        domain_obj,
        Relation::Viewer,
        eve_subject,
    );
    record(
        eve_domain_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    let eve_channel_viewer = check_permission(
        &store,
        &namespaces,
        channel_obj,
        Relation::Viewer,
        eve_subject,
    );
    record(
        eve_channel_viewer,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );

    // Synthesizer agent has Synthesizer on tenant via upsert.
    let agent_tenant_synth = check_permission(
        &store,
        &namespaces,
        tenant_obj,
        Relation::Synthesizer,
        agent_subject,
    );
    record(
        agent_tenant_synth,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );
    // Agent has Owner -> Editor on the agent object via the namespace
    // we registered.
    let agent_self_editor = check_permission(
        &store,
        &namespaces,
        agent_obj,
        Relation::Editor,
        agent_subject,
    );
    record(
        agent_self_editor,
        &mut total_checks,
        &mut allowed_checks,
        &mut denied_checks,
    );

    let benchmark_elapsed = benchmark_started.elapsed();

    // -------- Tuple removal round-trip ------------------------------
    // Insert + remove a sacrificial tuple to verify the contract is
    // bidirectional.
    let scratch = RelationTuple::new(channel_alt_obj, Relation::Viewer, eve_subject);
    store
        .insert(scratch)
        .expect("scratch tuple inserted cleanly");
    let eve_alt_viewer_before_remove = check_permission(
        &store,
        &namespaces,
        channel_alt_obj,
        Relation::Viewer,
        eve_subject,
    );
    store
        .remove(&scratch)
        .expect("scratch tuple removed cleanly");
    let eve_alt_viewer_after_remove = check_permission(
        &store,
        &namespaces,
        channel_alt_obj,
        Relation::Viewer,
        eve_subject,
    );

    // -------- Audit trail -------------------------------------------
    state.audit_log.append(
        audit_service::AuditEntryBuilder::new()
            .actor(audit_service::Actor::User(alice))
            .action(audit_service::AuditActionType::MemberProvisioned)
            .target(audit_service::TargetRef::new(
                audit_service::TargetType::Tenant,
                tenant_obj.object_id,
            ))
            .scope(dataset.tenant_scope.id)
            .details(serde_json::json!({
                "tuples_seeded": store.len(),
                "tier": "tenant",
            }))
            .build()
            .expect("permission audit entry"),
    );
    state.audit_log.append(
        audit_service::AuditEntryBuilder::new()
            .actor(audit_service::Actor::System)
            .action(audit_service::AuditActionType::PolicyChange)
            .target(audit_service::TargetRef::new(
                audit_service::TargetType::Domain,
                domain_obj.object_id,
            ))
            .scope(dataset.domain_scope.id)
            .details(serde_json::json!({
                "rewrite": "domain#editor <- tenant#admin",
            }))
            .build()
            .expect("policy change audit entry"),
    );

    // -------- Assertions --------------------------------------------
    log.check(
        PHASE,
        "alice (owner) reaches Owner on the tenant",
        alice_tenant_owner,
    );
    log.check(
        PHASE,
        "alice reaches Viewer on the channel via the chain",
        alice_channel_viewer,
    );
    log.check(
        PHASE,
        "alice reaches Member on the alternate channel via Owner->...->Member",
        alice_channel_alt_member,
    );
    log.check(
        PHASE,
        "bob (admin) reaches Editor on the domain via tenant#admin rewrite",
        bob_domain_editor,
    );
    log.check(
        PHASE,
        "bob reaches Viewer on the channel via two-hop rewrite + chain",
        bob_channel_viewer,
    );
    log.check(
        PHASE,
        "bob (admin not owner) is denied Owner on the tenant",
        !bob_tenant_owner,
    );
    log.check(
        PHASE,
        "carol (member) reaches Viewer on the channel via two-hop rewrite",
        carol_channel_viewer,
    );
    log.check(
        PHASE,
        "carol does NOT reach Member on the channel (Viewer doesn't imply Member)",
        !carol_channel_member,
    );
    log.check(
        PHASE,
        "carol does NOT reach Editor on the channel",
        !carol_channel_editor,
    );
    log.check(
        PHASE,
        "carol does NOT reach Admin on the tenant",
        !carol_tenant_admin,
    );
    log.check(
        PHASE,
        "dave (channel editor) reaches Member via Editor->Member",
        dave_channel_alt_member,
    );
    log.check(
        PHASE,
        "dave reaches Viewer via Editor->...->Viewer",
        dave_channel_alt_viewer,
    );
    log.check(
        PHASE,
        "dave does NOT reach Admin (Editor doesn't imply Admin)",
        !dave_channel_alt_admin,
    );
    log.check(
        PHASE,
        "dave's editor binding is scope-local (no leak to main channel)",
        !dave_channel_main_viewer,
    );
    log.check(
        PHASE,
        "outsider eve is denied Viewer on tenant",
        !eve_tenant_viewer,
    );
    log.check(
        PHASE,
        "outsider eve is denied Viewer on domain",
        !eve_domain_viewer,
    );
    log.check(
        PHASE,
        "outsider eve is denied Viewer on channel",
        !eve_channel_viewer,
    );
    log.check(
        PHASE,
        "synthesis agent reaches Synthesizer on tenant via upsert",
        agent_tenant_synth,
    );
    log.check(
        PHASE,
        "synthesis agent reaches Editor on its agent object via custom namespace",
        agent_self_editor,
    );
    log.check(
        PHASE,
        "duplicate tuple insert returns DuplicateTuple",
        duplicate_rejected,
    );
    log.check(
        PHASE,
        "removing a phantom tuple returns NotFound",
        phantom_rejected,
    );
    log.check(PHASE, "first upsert inserts the tuple", upsert_inserted);
    log.check(
        PHASE,
        "second upsert is idempotent (returns false)",
        upsert_idempotent,
    );
    log.check(
        PHASE,
        "scratch tuple grants viewer to eve before removal",
        eve_alt_viewer_before_remove,
    );
    log.check(
        PHASE,
        "removing the scratch tuple revokes eve's access",
        !eve_alt_viewer_after_remove,
    );

    // -------- Reporting --------------------------------------------
    phase.timing = started.elapsed();
    phase.stat(
        "namespaces_registered",
        "tenant, domain, channel, agent".to_string(),
    );
    phase.stat("relation_tuples", store.len().to_string());
    phase.stat("reachability_checks_total", total_checks.to_string());
    phase.stat("reachability_checks_allowed", allowed_checks.to_string());
    phase.stat("reachability_checks_denied", denied_checks.to_string());
    phase.stat(
        "subjects",
        "alice, bob, carol, dave, eve, synthesis_agent".to_string(),
    );
    phase.note(
        "Tenant→Domain→Channel hierarchy with two userset rewrites \
         (domain#editor⇐tenant#admin, channel#viewer⇐domain#member) and \
         the default Owner⇒Admin⇒Editor⇒Member⇒Viewer namespace chain. \
         Verified positive paths, negative paths, outsider rejection, \
         duplicate-insert, phantom-remove, upsert idempotence, and \
         scope-local revocation.",
    );

    report.count("permission_tuples_seeded", store.len() as u64);
    report.count("permission_checks_total", total_checks);
    report.count("permission_checks_allowed", allowed_checks);
    report.count("permission_checks_denied", denied_checks);
    report.add_phase(phase);
    report.add_benchmark(
        "permission_reachability_checks",
        total_checks,
        benchmark_elapsed,
    );
}
