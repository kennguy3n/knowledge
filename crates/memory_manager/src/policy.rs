//! Policy-driven retention, legal hold, event-based lifecycle, and
//! resurrection for memory objects.
//!
//! This module replaces the hardcoded `DEFAULT_CANDIDATE_ARCHIVE_THRESHOLD`
//! and `DEFAULT_SUPERSEDED_TTL_DAYS` with a configurable
//! [`RetentionPolicy`] system. Policies can be set per tenant, per scope,
//! per user, or globally, and they govern:
//!
//! * when `Candidate` objects may be archived,
//! * when `Superseded` objects may be archived,
//! * minimum and maximum retention durations,
//! * legal holds,
//! * event-based archive / delete / preserve triggers,
//! * automatic promotion thresholds,
//! * resurrection of `Archived` objects back to active memory.
//!
//! The design intentionally separates *lifecycle state* (candidate,
//! canonical, archived, ...) from *retention policy* (when and why a state
//! change is allowed).

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::object::MemoryObject;
use crate::retention::RetentionScore;
use crate::state::MemoryState;

/// Scope at which a retention policy applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyScope {
    /// Applies to every object unless a more specific policy overrides it.
    Global,
    /// Applies to every scope owned by a tenant.
    Tenant,
    /// Applies to one specific scope.
    Scope,
    /// Applies to objects owned by a specific user.
    User,
}

/// A retention policy that governs the lifecycle of memory objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Unique policy id.
    pub policy_id: Uuid,
    /// Human-readable name, e.g. "B2B-7yr-contract".
    pub name: String,
    /// What this policy applies to.
    pub scope: PolicyScope,
    /// The tenant, scope, or user id this policy targets. `None` means
    /// global.
    pub target_id: Option<Uuid>,
    /// Higher priority wins when multiple policies could apply.
    pub priority: u32,

    // ── Archive thresholds ────────────────────────────────────────
    /// Retention-score floor below which a `Candidate` may be archived.
    pub candidate_archive_threshold: f64,
    /// How long a `Superseded` object must stay reachable before it can
    /// be archived.
    pub superseded_archive_after: Duration,
    /// How long a `Canonical` object must be retained after creation,
    /// regardless of score.
    pub canonical_min_retention: Duration,
    /// Optional maximum time an `Archived` object may be kept before it
    /// must be hard-deleted (compliance-driven deletion deadline).
    pub archived_max_retention: Option<Duration>,

    // ── Promotion thresholds ──────────────────────────────────────
    /// Retrieval count that auto-promotes `Candidate -> Reinforced`.
    pub promote_to_reinforced_after_retrievals: u32,
    /// Independent corroboration sources that auto-promote
    /// `Reinforced -> Consolidated`.
    pub promote_to_consolidated_after_sources: u32,
    /// Time a `Consolidated` object must remain stable before it can be
    /// auto-promoted to `Canonical`.
    pub promote_to_canonical_after_stable: Option<Duration>,

    // ── Compliance and holds ──────────────────────────────────────
    /// If true, no object under this policy may be archived or forgotten.
    pub legal_hold: bool,
    /// Optional timestamp after which the legal hold expires.
    pub legal_hold_until: Option<DateTime<Utc>>,
    /// Objects may not be archived or forgotten before this timestamp.
    pub minimum_retention_until: Option<DateTime<Utc>>,
    /// Objects must be deleted by this timestamp (right-to-erasure,
    /// contractual deletion deadline).
    pub maximum_retention_until: Option<DateTime<Utc>>,

    // ── Resurrection ──────────────────────────────────────────────
    /// Whether `Archived` objects can be brought back to active memory
    /// when they are retrieved again.
    pub allow_resurrection: bool,
    /// Retrieval count in a single query context that triggers
    /// resurrection.
    pub resurrection_retrieval_threshold: u32,

    // ── Event-driven lifecycle ────────────────────────────────────
    /// When any of these events is active on the scope, eligible objects
    /// are archived (e.g. "project_closed").
    pub archive_on_events: Vec<String>,
    /// When any of these events is active, objects are hard-deleted.
    pub delete_on_events: Vec<String>,
    /// When any of these events is active, no object may be archived or
    /// deleted (e.g. "litigation_hold").
    pub preserve_on_events: Vec<String>,
}

impl RetentionPolicy {
    /// Default policy used when nothing else is configured.
    pub fn global_default() -> Self {
        Self {
            policy_id: Uuid::from_u128(0x0001_0000_0000_0000_0000_0000_0000_0001),
            name: "global-default".to_string(),
            scope: PolicyScope::Global,
            target_id: None,
            priority: 0,
            candidate_archive_threshold: 0.15,
            superseded_archive_after: Duration::days(90),
            canonical_min_retention: Duration::days(365),
            archived_max_retention: None,
            promote_to_reinforced_after_retrievals: 3,
            promote_to_consolidated_after_sources: 2,
            promote_to_canonical_after_stable: Some(Duration::days(7)),
            legal_hold: false,
            legal_hold_until: None,
            minimum_retention_until: None,
            maximum_retention_until: None,
            allow_resurrection: true,
            resurrection_retrieval_threshold: 1,
            archive_on_events: Vec::new(),
            delete_on_events: Vec::new(),
            preserve_on_events: Vec::new(),
        }
    }

    /// Conservative B2B policy: long retention, no auto-promotion to
    /// canonical, strong legal-hold support.
    pub fn b2b_default() -> Self {
        Self {
            policy_id: Uuid::new_v4(),
            name: "b2b-default".to_string(),
            scope: PolicyScope::Tenant,
            target_id: None,
            priority: 100,
            candidate_archive_threshold: 0.10,
            superseded_archive_after: Duration::days(365),
            canonical_min_retention: Duration::days(365 * 7),
            archived_max_retention: Some(Duration::days(365 * 10)),
            promote_to_reinforced_after_retrievals: 5,
            promote_to_consolidated_after_sources: 3,
            promote_to_canonical_after_stable: None, // human approval required
            legal_hold: false,
            legal_hold_until: None,
            minimum_retention_until: None,
            maximum_retention_until: None,
            allow_resurrection: true,
            resurrection_retrieval_threshold: 1,
            archive_on_events: vec!["project_closed".to_string()],
            delete_on_events: Vec::new(),
            preserve_on_events: vec!["litigation_hold".to_string()],
        }
    }

    /// Consumer B2C policy: shorter retention, automatic deletion
    /// deadlines, faster decay of noise.
    pub fn b2c_default() -> Self {
        Self {
            policy_id: Uuid::new_v4(),
            name: "b2c-default".to_string(),
            scope: PolicyScope::Tenant,
            target_id: None,
            priority: 100,
            candidate_archive_threshold: 0.20,
            superseded_archive_after: Duration::days(30),
            canonical_min_retention: Duration::days(365),
            archived_max_retention: Some(Duration::days(365 * 3)),
            promote_to_reinforced_after_retrievals: 2,
            promote_to_consolidated_after_sources: 2,
            promote_to_canonical_after_stable: Some(Duration::days(1)),
            legal_hold: false,
            legal_hold_until: None,
            minimum_retention_until: None,
            maximum_retention_until: None,
            allow_resurrection: true,
            resurrection_retrieval_threshold: 1,
            archive_on_events: vec!["account_closed".to_string()],
            delete_on_events: vec!["user_requested_deletion".to_string()],
            preserve_on_events: Vec::new(),
        }
    }

    /// `true` if the legal hold is active at `now`.
    pub fn legal_hold_active(&self, now: DateTime<Utc>) -> bool {
        if !self.legal_hold {
            return false;
        }
        match self.legal_hold_until {
            Some(until) => now < until,
            None => true,
        }
    }

    /// Minimum retention timestamp for an object created at `created_at`.
    fn minimum_retention_until_for(&self, created_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.minimum_retention_until
            .or_else(|| Some(created_at + self.canonical_min_retention))
    }

    /// `true` if the object is still within its minimum retention window.
    fn in_minimum_retention(&self, obj: &MemoryObject, now: DateTime<Utc>) -> bool {
        match self.minimum_retention_until_for(obj.created_at) {
            Some(until) => now < until,
            None => false,
        }
    }

    /// `true` only if an explicit `minimum_retention_until` is set and
    /// has not yet passed. Event-driven archive may bypass the default
    /// `canonical_min_retention` but must still respect explicit holds.
    fn in_explicit_minimum_retention(&self, _obj: &MemoryObject, now: DateTime<Utc>) -> bool {
        match self.minimum_retention_until {
            Some(until) => now < until,
            None => false,
        }
    }

    /// `true` if the object has passed its maximum retention deadline.
    fn past_maximum_retention(&self, _obj: &MemoryObject, now: DateTime<Utc>) -> bool {
        match self.maximum_retention_until {
            Some(until) => now >= until,
            None => false,
        }
    }
}

/// Mutable per-scope lifecycle state: which policy applies, legal holds,
/// and active events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeRetentionState {
    /// Scope this state describes.
    pub scope_id: ScopeId,
    /// Currently active policy id.
    pub policy_id: Option<Uuid>,
    /// Active legal hold (overrides policy's `legal_hold`).
    pub legal_hold: bool,
    /// When the legal hold expires, if known.
    pub legal_hold_until: Option<DateTime<Utc>>,
    /// Active lifecycle events on this scope (e.g. "project_closed").
    pub active_events: Vec<String>,
    /// When each event was triggered.
    pub event_timestamps: HashMap<String, DateTime<Utc>>,
}

impl Default for ScopeRetentionState {
    fn default() -> Self {
        Self {
            scope_id: ScopeId(Uuid::nil()),
            policy_id: None,
            legal_hold: false,
            legal_hold_until: None,
            active_events: Vec::new(),
            event_timestamps: HashMap::new(),
        }
    }
}

/// Result of evaluating one object against the policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    /// No state change.
    Keep,
    /// Object should be archived.
    Archive,
    /// Object should be deleted (hard / cryptographic forget).
    Delete,
    /// Object should be resurrected from Archived to active.
    Resurrect,
    /// Auto-promote `Candidate -> Reinforced`.
    PromoteToReinforced,
    /// Auto-promote `Reinforced -> Consolidated`.
    PromoteToConsolidated,
    /// Auto-promote `Consolidated -> Canonical`.
    PromoteToCanonical,
}

/// Engine that resolves policies and evaluates lifecycle decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyEngine {
    policies: HashMap<Uuid, RetentionPolicy>,
    scope_state: HashMap<ScopeId, ScopeRetentionState>,
    tenant_policies: HashMap<Uuid, Uuid>,
    global_default: RetentionPolicy,
}

impl PolicyEngine {
    /// Build an engine with the global default policy.
    pub fn new() -> Self {
        let default = RetentionPolicy::global_default();
        let mut policies = HashMap::new();
        policies.insert(default.policy_id, default.clone());
        Self {
            policies,
            scope_state: HashMap::new(),
            tenant_policies: HashMap::new(),
            global_default: default,
        }
    }

    /// Register a policy. Replaces any existing policy with the same id.
    pub fn register_policy(&mut self, policy: RetentionPolicy) {
        self.policies.insert(policy.policy_id, policy);
    }

    /// Assign a policy to a tenant.
    pub fn set_tenant_policy(&mut self, tenant_id: Uuid, policy_id: Uuid) -> Result<(), String> {
        if !self.policies.contains_key(&policy_id) {
            return Err(format!("policy {policy_id} not registered"));
        }
        self.tenant_policies.insert(tenant_id, policy_id);
        Ok(())
    }

    /// Assign a policy to a specific scope.
    pub fn set_scope_policy(&mut self, scope_id: ScopeId, policy_id: Uuid) -> Result<(), String> {
        if !self.policies.contains_key(&policy_id) {
            return Err(format!("policy {policy_id} not registered"));
        }
        let state = self
            .scope_state
            .entry(scope_id)
            .or_insert_with(ScopeRetentionState::default);
        state.scope_id = scope_id;
        state.policy_id = Some(policy_id);
        Ok(())
    }

    /// Set or clear a legal hold on a scope.
    pub fn set_legal_hold(
        &mut self,
        scope_id: ScopeId,
        hold: bool,
        until: Option<DateTime<Utc>>,
    ) {
        let state = self
            .scope_state
            .entry(scope_id)
            .or_insert_with(ScopeRetentionState::default);
        state.scope_id = scope_id;
        state.legal_hold = hold;
        state.legal_hold_until = until;
    }

    /// Add an event to a scope. Idempotent.
    pub fn add_event(&mut self, scope_id: ScopeId, event: &str, at: DateTime<Utc>) {
        let state = self
            .scope_state
            .entry(scope_id)
            .or_insert_with(ScopeRetentionState::default);
        state.scope_id = scope_id;
        if !state.active_events.iter().any(|e| e == event) {
            state.active_events.push(event.to_string());
        }
        state.event_timestamps.insert(event.to_string(), at);
    }

    /// Remove an event from a scope.
    pub fn clear_event(&mut self, scope_id: ScopeId, event: &str) {
        if let Some(state) = self.scope_state.get_mut(&scope_id) {
            state.active_events.retain(|e| e != event);
            state.event_timestamps.remove(event);
        }
    }

    /// Resolve the effective policy for a scope and tenant.
    pub fn resolve_policy(&self, scope_id: ScopeId, tenant_id: Option<Uuid>) -> &RetentionPolicy {
        // Scope-level wins.
        if let Some(state) = self.scope_state.get(&scope_id) {
            if let Some(pid) = state.policy_id {
                if let Some(policy) = self.policies.get(&pid) {
                    return policy;
                }
            }
        }
        // Tenant-level.
        if let Some(tid) = tenant_id {
            if let Some(pid) = self.tenant_policies.get(&tid) {
                if let Some(policy) = self.policies.get(pid) {
                    return policy;
                }
            }
        }
        // Global default.
        &self.global_default
    }

    /// Get mutable scope state.
    pub fn scope_state(&self, scope_id: ScopeId) -> Option<&ScopeRetentionState> {
        self.scope_state.get(&scope_id)
    }

    /// Evaluate the next lifecycle decision for an object.
    pub fn evaluate(
        &self,
        obj: &MemoryObject,
        tenant_id: Option<Uuid>,
        now: DateTime<Utc>,
        score: &RetentionScore,
    ) -> PolicyDecision {
        let policy = self.resolve_policy(obj.scope_id, tenant_id);
        let scope_state = self.scope_state.get(&obj.scope_id);

        // Legal hold / preserve events block any destructive transition.
        if policy.legal_hold_active(now)
            || scope_state
                .map(|s| s.legal_hold_active(now))
                .unwrap_or(false)
            || scope_state
                .map(|s| s.active_events.iter().any(|e| policy.preserve_on_events.contains(e)))
                .unwrap_or(false)
        {
            return PolicyDecision::Keep;
        }

        // Maximum retention deadline forces deletion.
        if policy.past_maximum_retention(obj, now) {
            return PolicyDecision::Delete;
        }

        // Event-driven deletion.
        if let Some(state) = scope_state {
            if state
                .active_events
                .iter()
                .any(|e| policy.delete_on_events.contains(e))
            {
                return PolicyDecision::Delete;
            }
        }

        // Event-driven archive. Project/account closure events allow
        // archiving even if the object is within its default canonical
        // retention, but explicit minimum-retention holds still win.
        if let Some(state) = scope_state {
            if state
                .active_events
                .iter()
                .any(|e| policy.archive_on_events.contains(e))
            {
                if !policy.in_explicit_minimum_retention(obj, now) {
                    return PolicyDecision::Archive;
                }
            }
        }

        // Resurrection: an Archived object with enough retrievals comes back.
        if obj.state == MemoryState::Archived
            && policy.allow_resurrection
            && obj.retrieval_count >= policy.resurrection_retrieval_threshold
        {
            return PolicyDecision::Resurrect;
        }

        // Promotion rules (only from lower to higher states).
        match obj.state {
            MemoryState::Candidate
                if obj.retrieval_count >= policy.promote_to_reinforced_after_retrievals =>
            {
                // If it has been retrieved enough, promote to Reinforced.
                if score.total >= 0.4 {
                    return PolicyDecision::PromoteToReinforced;
                }
            }
            MemoryState::Reinforced
                if obj.corroboration_count >= policy.promote_to_consolidated_after_sources =>
            {
                return PolicyDecision::PromoteToConsolidated;
            }
            MemoryState::Consolidated => {
                if let Some(stable) = policy.promote_to_canonical_after_stable {
                    let stable_since = obj
                        .consolidated_at
                        .unwrap_or(obj.created_at);
                    if now >= stable_since + stable {
                        return PolicyDecision::PromoteToCanonical;
                    }
                }
            }
            _ => {}
        }

        // Archive rules.
        match obj.state {
            MemoryState::Candidate => {
                if policy.in_minimum_retention(obj, now) {
                    return PolicyDecision::Keep;
                }
                if score.total < policy.candidate_archive_threshold {
                    return PolicyDecision::Archive;
                }
            }
            MemoryState::Superseded => {
                if policy.in_minimum_retention(obj, now) {
                    return PolicyDecision::Keep;
                }
                let superseded_at = obj.last_accessed_at;
                if now >= superseded_at + policy.superseded_archive_after {
                    return PolicyDecision::Archive;
                }
            }
            MemoryState::Archived => {
                if let Some(max_age) = policy.archived_max_retention {
                    if now >= obj.created_at + max_age {
                        return PolicyDecision::Delete;
                    }
                }
            }
            _ => {}
        }

        PolicyDecision::Keep
    }
}

impl ScopeRetentionState {
    /// `true` if the scope has an active legal hold at `now`.
    fn legal_hold_active(&self, now: DateTime<Utc>) -> bool {
        if !self.legal_hold {
            return false;
        }
        match self.legal_hold_until {
            Some(until) => now < until,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::SensitivityClass;
    use crate::retention::compute_retention_score;

    #[test]
    fn legal_hold_blocks_archive() {
        let mut engine = PolicyEngine::new();
        let scope = ScopeId::new_v4();
        engine.set_legal_hold(scope, true, None);

        let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
        obj.created_at = Utc::now() - Duration::days(365);
        obj.last_accessed_at = obj.created_at;
        let score = compute_retention_score(&obj, Utc::now());

        let decision = engine.evaluate(&obj, None, Utc::now(), &score);
        assert_eq!(decision, PolicyDecision::Keep);
    }

    #[test]
    fn archive_event_triggers_archive() {
        let mut engine = PolicyEngine::new();
        let scope = ScopeId::new_v4();

        let mut b2b = RetentionPolicy::b2b_default();
        b2b.policy_id = Uuid::new_v4();
        engine.register_policy(b2b.clone());
        engine.set_scope_policy(scope, b2b.policy_id).unwrap();
        engine.add_event(scope, "project_closed", Utc::now());

        let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
        obj.created_at = Utc::now() - Duration::days(365);
        obj.last_accessed_at = obj.created_at;
        let score = compute_retention_score(&obj, Utc::now());

        let decision = engine.evaluate(&obj, None, Utc::now(), &score);
        assert_eq!(decision, PolicyDecision::Archive);
    }

    #[test]
    fn resurrection_works() {
        let engine = PolicyEngine::new();
        let scope = ScopeId::new_v4();

        let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Useful);
        obj.state = MemoryState::Archived;
        obj.retrieval_count = 1;
        let score = compute_retention_score(&obj, Utc::now());

        let decision = engine.evaluate(&obj, None, Utc::now(), &score);
        assert_eq!(decision, PolicyDecision::Resurrect);
    }

    #[test]
    fn maximum_retention_forces_delete() {
        let mut engine = PolicyEngine::new();
        let scope = ScopeId::new_v4();

        let mut policy = RetentionPolicy::b2c_default();
        policy.policy_id = Uuid::new_v4();
        policy.maximum_retention_until = Some(Utc::now() - Duration::days(1));
        engine.register_policy(policy.clone());
        engine.set_scope_policy(scope, policy.policy_id).unwrap();

        let mut obj = MemoryObject::new_candidate(scope, SensitivityClass::Important);
        obj.created_at = Utc::now() - Duration::days(365 * 2);
        obj.last_accessed_at = obj.created_at;
        let score = compute_retention_score(&obj, Utc::now());

        let decision = engine.evaluate(&obj, None, Utc::now(), &score);
        assert_eq!(decision, PolicyDecision::Delete);
    }
}
