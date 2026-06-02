//! Synthetic dataset generator for the end-to-end demo.
//!
//! Produces ~50+ synthetic messages spread across user / channel /
//! domain / tenant scopes, designed to exercise all three evidence
//! storage paths (inline, body-table, ring-buffer) and to populate
//! every observation type.

use chrono::{DateTime, Duration, TimeZone, Utc};
use evidence_store::ScopeId;
use uuid::Uuid;

/// Tier of the substrate scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScopeTier {
    User,
    Channel,
    Domain,
    Tenant,
}

impl ScopeTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            ScopeTier::User => "user",
            ScopeTier::Channel => "channel",
            ScopeTier::Domain => "domain",
            ScopeTier::Tenant => "tenant",
        }
    }
}

/// Named scope used by the dataset generator.
#[derive(Debug, Clone)]
pub struct NamedScope {
    pub id: ScopeId,
    pub label: &'static str,
}

/// A single synthetic message.
#[derive(Debug, Clone)]
pub struct SyntheticMessage {
    pub scope_label: &'static str,
    pub scope_tier: ScopeTier,
    pub source_ref: String,
    pub body: String,
    pub occurred_at: DateTime<Utc>,
}

/// The full synthetic corpus.
#[derive(Debug, Clone)]
pub struct Dataset {
    pub user_scope: NamedScope,
    pub channel_scope: NamedScope,
    pub channel_alt_scope: NamedScope,
    pub domain_scope: NamedScope,
    pub tenant_scope: NamedScope,
    pub messages: Vec<SyntheticMessage>,
}

const ATLAS_LONG_DOC: &str = "Project Atlas Q3 launch plan. \
The migration from the legacy Aurora database to the new sharded Postgres cluster will ship Friday. \
Decision: Sara is the launch owner; Eng is responsible for backups; Anna is responsible for the rollout drill. \
Risks include data loss during the cutover, partial replication during the live failover window, \
and the need for explicit policy approval from Legal before customer data flows through the new pipeline. \
The decision was approved at the Q3 planning offsite. Budget confirmed at $250,000. \
Owners: @Sara (engineering lead), @Anna (program management), @LegalOps (regulatory clearance). \
Action: draft the cutover runbook and circulate to the team by Wednesday next week. \
The migration will block all write traffic for at most 90 seconds during the failover. \
Mitigation plan: deploy a read-only banner in the app, queue writes in the journal, and replay them \
from the journal once Postgres confirms the new primary is ready to accept writes. \
This document is the canonical record of the launch plan and supersedes the prior draft. \
Compliance: this change has been ratified under our internal data-handling policy.";

const HUNDRED_DAY_DOC: &str = "Hundred-day plan for Project Helios. \
Helios is the codename for our consumer subscription rollout. \
 (days 0-30): platform readiness — finalize the billing schema, \
ship the new entitlement service, and ratify the regional pricing matrix. \
 (days 30-60): private beta — enroll 250 users from the waitlist, \
ship a referral program, and integrate the new Stripe payment intents flow. \
 (days 60-100): general availability — open signup to the public, \
launch the press tour, and begin paid acquisition through the marketing channel. \
Decision: Anna will own day-to-day execution. Engineering reports to Sara. \
Marketing reports to Priya. Legal sign-off is required before the GA milestone. \
Budget confirmed at $1.2M for the first hundred days, with a contingency of $300K. \
Risks: regulatory pushback in the EU, customer churn after the initial onboarding, \
and the dependency on the new entitlement service which is still in private beta. \
This plan has been signed off by the executive team and is the canonical record.";

/// Convert a small loop index into an `i64` minute offset for the
/// synthetic dataset. The dataset has fewer than a thousand rows per
/// channel so the cast cannot wrap on any realistic target; `try_from`
/// keeps the cast lints honest and saturates at `i64::MAX` rather
/// than wrapping in the pathological case.
fn idx_as_i64(i: usize) -> i64 {
    i64::try_from(i).unwrap_or(i64::MAX)
}

pub fn build_dataset() -> Dataset {
    let user_scope = NamedScope {
        id: ScopeId(Uuid::new_v4()),
        label: "user.alex",
    };
    let channel_scope = NamedScope {
        id: ScopeId(Uuid::new_v4()),
        label: "channel.platform",
    };
    let channel_alt_scope = NamedScope {
        id: ScopeId(Uuid::new_v4()),
        label: "channel.marketing",
    };
    let domain_scope = NamedScope {
        id: ScopeId(Uuid::new_v4()),
        label: "domain.engineering",
    };
    let tenant_scope = NamedScope {
        id: ScopeId(Uuid::new_v4()),
        label: "tenant.acme",
    };

    // Anchor timestamp: 2026-04-20 09:00 UTC. Kept deterministic so
    // tests can verify ordering without flake.
    let base = Utc.with_ymd_and_hms(2026, 4, 20, 9, 0, 0).unwrap();

    let mut msgs: Vec<SyntheticMessage> = Vec::new();

    // ---- channel.platform: rich technical discussion ----
    let plat = channel_scope.label;
    let plat_lines: &[&str] = &[
        "Decision: we are migrating from Aurora to sharded Postgres next Friday.",
        "@Sara please draft the cutover runbook and share it by Wednesday.",
        "Action: Anna will lead the rollout drill on Thursday afternoon.",
        "The migration is approved by Legal under our data-handling policy.",
        "Risk: write traffic will pause for ~90s during the failover window.",
        "Owner: @Eng holds the deadline for the new entitlement service.",
        "Budget confirmed: $250,000 allocated for the migration.",
        "FYI action: please review the runbook PR by Tuesday next week.",
        "Decision: we will roll back if replication lag exceeds 30 seconds.",
        "Question: do we need a Q3 2026 review of the regional pricing matrix?",
        "Reminder: deadline for the launch communications is May 12.",
        "Update: the staging cluster is green, deploy is queued for Friday.",
        "todo: investigate the slow query on the user_metrics table",
        "Decision: ratified — the new sharded layout is canonical.",
    ];
    for (i, line) in plat_lines.iter().enumerate() {
        msgs.push(SyntheticMessage {
            scope_label: plat,
            scope_tier: ScopeTier::Channel,
            source_ref: format!("slack/{plat}/m{i:03}"),
            body: (*line).to_string(),
            occurred_at: base + Duration::minutes(idx_as_i64(i) * 4),
        });
    }

    // ---- channel.marketing ----
    let mkt = channel_alt_scope.label;
    let mkt_lines: &[&str] = &[
        "Approved: the Helios launch press kit goes out Friday.",
        "@Priya please send the embargo email by Thursday.",
        "Decision: we will use the new brand voice in the GA campaign.",
        "Risk: legal hold on the comparative-pricing slide until Compliance reviews.",
        "Owner: @LegalOps confirms the regulatory copy by Wednesday.",
        "Reminder: schedule the kickoff call with the press list on Monday.",
        "Action: draft the tweet thread for the GA milestone.",
        "Question: which timezone do we ship the embargo at, US-East or UTC?",
        "Update: the landing page is in review with the design team.",
    ];
    for (i, line) in mkt_lines.iter().enumerate() {
        msgs.push(SyntheticMessage {
            scope_label: mkt,
            scope_tier: ScopeTier::Channel,
            source_ref: format!("slack/{mkt}/m{i:03}"),
            body: (*line).to_string(),
            occurred_at: base + Duration::minutes(2 + idx_as_i64(i) * 6),
        });
    }

    // ---- noise / chatter scattered across both channels ----
    let noise: &[(&str, &str)] = &[
        (plat, "hi"),
        (plat, "thanks!"),
        (plat, "+1"),
        (plat, "good morning"),
        (mkt, "lol"),
        (mkt, "ok"),
        (plat, "👍"),
        (mkt, "thank you"),
        (plat, "yo"),
        (mkt, "kk"),
        (plat, "great"),
        (mkt, "nope"),
    ];
    for (i, (scope, line)) in noise.iter().enumerate() {
        msgs.push(SyntheticMessage {
            scope_label: scope,
            scope_tier: ScopeTier::Channel,
            source_ref: format!("slack/{scope}/noise{i:03}"),
            body: (*line).to_string(),
            occurred_at: base + Duration::minutes(60 + idx_as_i64(i) * 3),
        });
    }

    // ---- domain-level rollups + duplicate body for dedup test ----
    let dom = domain_scope.label;
    let dom_lines: &[&str] = &[
        "Domain: engineering owns the migration from Aurora to sharded Postgres.",
        "Approved: cross-team sync every Monday at 10:00 UTC.",
        "Risk: the new entitlement service is a single point of failure during GA.",
        "Owner: @Sara is the engineering lead for Q3 2026.",
    ];
    for (i, line) in dom_lines.iter().enumerate() {
        msgs.push(SyntheticMessage {
            scope_label: dom,
            scope_tier: ScopeTier::Domain,
            source_ref: format!("doc/{dom}/d{i:03}"),
            body: (*line).to_string(),
            occurred_at: base + Duration::minutes(120 + idx_as_i64(i) * 5),
        });
    }
    // Long body that triggers the body-table path (>512 bytes).
    msgs.push(SyntheticMessage {
        scope_label: dom,
        scope_tier: ScopeTier::Domain,
        source_ref: format!("doc/{dom}/atlas-launch-plan-v2"),
        body: ATLAS_LONG_DOC.to_string(),
        occurred_at: base + Duration::minutes(150),
    });
    // Duplicate of the long body — should hit the dedup index in
    // body_store and bump ref_count.
    msgs.push(SyntheticMessage {
        scope_label: dom,
        scope_tier: ScopeTier::Domain,
        source_ref: format!("doc/{dom}/atlas-launch-plan-v2-mirror"),
        body: ATLAS_LONG_DOC.to_string(),
        occurred_at: base + Duration::minutes(151),
    });

    // ---- tenant-level long-form record ----
    let tnt = tenant_scope.label;
    let tnt_lines: &[&str] = &[
        "Policy ratified: data residency must be EU for all new tenants.",
        "Compliance signed: SOC 2 Type II audit window opens Q3 2026.",
        "Decision: legal hold issued on the marketing channel until November.",
    ];
    for (i, line) in tnt_lines.iter().enumerate() {
        msgs.push(SyntheticMessage {
            scope_label: tnt,
            scope_tier: ScopeTier::Tenant,
            source_ref: format!("policy/{tnt}/p{i:03}"),
            body: (*line).to_string(),
            occurred_at: base + Duration::minutes(200 + idx_as_i64(i) * 10),
        });
    }
    msgs.push(SyntheticMessage {
        scope_label: tnt,
        scope_tier: ScopeTier::Tenant,
        source_ref: format!("policy/{tnt}/helios-100-day-plan"),
        body: HUNDRED_DAY_DOC.to_string(),
        occurred_at: base + Duration::minutes(240),
    });

    // ---- user-scope personal notes ----
    let usr = user_scope.label;
    let usr_lines: &[&str] = &[
        "todo: review the cutover runbook before Wednesday",
        "Decision: I will pin the Atlas launch plan in my personal notes.",
        "Reminder: schedule a 1:1 with Sara to discuss the rollout.",
        "Question: am I assigned to any GA milestone reviews?",
        "Action: draft the personal status update for Friday's standup.",
        "Hello team, just got back from PTO.",
    ];
    for (i, line) in usr_lines.iter().enumerate() {
        msgs.push(SyntheticMessage {
            scope_label: usr,
            scope_tier: ScopeTier::User,
            source_ref: format!("note/{usr}/n{i:03}"),
            body: (*line).to_string(),
            occurred_at: base + Duration::minutes(280 + idx_as_i64(i) * 4),
        });
    }

    Dataset {
        user_scope,
        channel_scope,
        channel_alt_scope,
        domain_scope,
        tenant_scope,
        messages: msgs,
    }
}
