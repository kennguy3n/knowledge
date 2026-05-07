//! Tenant Memory Object — institutional memory for the highest scope
//! in the B2B hierarchy.
//!
//! Per `PROPOSAL.md` §4.3 and §6.2, tenant / institutional memory
//! captures the **canonical policy, product taxonomy, and stable org
//! knowledge** that defines the tenant. Items default to
//! [`SensitivityClass::Critical`] which has, per `PROPOSAL.md` §4.3,
//! "**no ordinary decay — only explicit deprecation**".
//!
//! That rule is enforced at the type level here: every constructor
//! pins the underlying [`MemoryObject`] to `Critical` and the only
//! lifecycle paths off the active list are `deprecate_*` methods
//! (which take a deprecating `superseded_by` reference). There is no
//! `decay_sweep` for tenant memory — calling the global
//! [`crate::decay::decay_sweep`] over a slice of `Critical` objects
//! is a no-op because that sweep only acts on `Candidate` and
//! `Superseded` rows.
//!
//! Tenant synthesis consumes domain outputs + **approved official
//! docs** only (see [`ApprovedDocumentRef`] and `PROPOSAL.md` §6.3
//! rule 3). The list of source domain scopes and the list of
//! approved-document refs are tracked here so the tenant memory
//! object enumerates its legal input contract for the synthesis
//! pipeline.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::object::{MemoryObject, SensitivityClass};

/// Errors raised by tenant-memory mutations that violate the
/// "explicit deprecation only" rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TenantMemoryError {
    /// The requested item id is not present in this tenant memory.
    #[error("tenant memory item not found: {0}")]
    NotFound(Uuid),

    /// A non-`Critical` memory object was offered to a tenant memory
    /// constructor / setter. Tenant memory items must be `Critical`
    /// because tenant scope has no passive decay.
    #[error("tenant memory rejects non-critical item (got {0:?})")]
    NotCritical(SensitivityClass),

    /// A passive-decay path was invoked on tenant memory; tenant
    /// memory has no passive decay — only explicit deprecation.
    #[error("tenant memory has no passive decay; use deprecate_* instead")]
    PassiveDecayForbidden,
}

/// Convenience result alias for tenant-memory mutations.
pub type Result<T, E = TenantMemoryError> = std::result::Result<T, E>;

/// Canonical tenant policy — e.g. `"PII never leaves jurisdiction"`.
///
/// Always `Critical`-class. Constructed via [`Self::new`], which
/// pins the sensitivity class. Deprecated via
/// [`TenantMemoryObject::deprecate_policy`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalPolicy {
    /// Underlying memory object (always `Critical`).
    pub memory: MemoryObject,
    /// Human-readable policy text.
    pub text: String,
    /// `Some(when)` once the policy has been deprecated. Tenant
    /// policy never decays passively; deprecation is the only off
    /// switch.
    pub deprecated_at: Option<DateTime<Utc>>,
    /// If this policy was superseded by a newer canonical policy, the
    /// id of the newer policy.
    pub superseded_by: Option<Uuid>,
}

impl CanonicalPolicy {
    /// Construct a fresh `Critical`-class canonical policy.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Critical),
            text: text.into(),
            deprecated_at: None,
            superseded_by: None,
        }
    }

    /// Whether this policy has been deprecated.
    pub fn is_deprecated(&self) -> bool {
        self.deprecated_at.is_some()
    }
}

/// One entry in the tenant's product taxonomy. Each entry maps a
/// product label to its parent (if any), forming a tree.
///
/// Always `Critical`-class. Taxonomy entries are stable knowledge —
/// no passive decay; deprecation only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductTaxonomyEntry {
    /// Underlying memory object (always `Critical`).
    pub memory: MemoryObject,
    /// Product / category label (e.g. `"Identity / IAM / OIDC"`).
    pub label: String,
    /// Parent entry id, if this label has one.
    pub parent: Option<Uuid>,
    /// `Some(when)` once the entry has been deprecated.
    pub deprecated_at: Option<DateTime<Utc>>,
}

impl ProductTaxonomyEntry {
    /// Construct a fresh `Critical`-class taxonomy entry.
    pub fn new(scope_id: ScopeId, label: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Critical),
            label: label.into(),
            parent: None,
            deprecated_at: None,
        }
    }

    /// Set the parent entry id.
    pub fn with_parent(mut self, parent: Uuid) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Whether this entry has been deprecated.
    pub fn is_deprecated(&self) -> bool {
        self.deprecated_at.is_some()
    }
}

/// Stable org knowledge — a labelled fact about how the org operates
/// (e.g. `"on-call rotation handoff: Tuesdays 09:00 UTC"`).
///
/// Always `Critical`-class. Stable knowledge does not decay; it is
/// only explicitly deprecated or superseded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StableOrgKnowledge {
    /// Underlying memory object (always `Critical`).
    pub memory: MemoryObject,
    /// Surface text of the fact.
    pub text: String,
    /// `Some(when)` once the fact has been deprecated.
    pub deprecated_at: Option<DateTime<Utc>>,
    /// If this fact was superseded by a newer canonical fact, the id
    /// of the newer fact.
    pub superseded_by: Option<Uuid>,
}

impl StableOrgKnowledge {
    /// Construct a fresh `Critical`-class fact.
    pub fn new(scope_id: ScopeId, text: impl Into<String>) -> Self {
        Self {
            memory: MemoryObject::new_candidate(scope_id, SensitivityClass::Critical),
            text: text.into(),
            deprecated_at: None,
            superseded_by: None,
        }
    }

    /// Whether this fact has been deprecated.
    pub fn is_deprecated(&self) -> bool {
        self.deprecated_at.is_some()
    }
}

/// Reference to an "approved official document" that has been
/// explicitly admitted to feed tenant synthesis. Per `PROPOSAL.md`
/// §6.3 rule 3, **tenant synthesis consumes domain objects + approved
/// official docs only**; raw evidence and channel objects are out of
/// contract. The synthesis pipeline uses the [`ApprovedDocumentRef`]
/// at type level (see `synthesis_pipeline::TenantSynthesisInput`) to
/// reject non-approved sources.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApprovedDocumentRef {
    /// Unique id of the document (UUID v4).
    pub id: Uuid,
    /// Stable label / title (e.g. `"Tenant Policy v3.2"`).
    pub label: String,
    /// Wall-clock time at which the document was approved for tenant
    /// synthesis.
    pub approved_at: DateTime<Utc>,
    /// Free-form approver reference (e.g. `"compliance-officer"`).
    pub approver: String,
}

impl ApprovedDocumentRef {
    /// Construct a fresh approved-document reference.
    pub fn new(label: impl Into<String>, approver: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            label: label.into(),
            approved_at: Utc::now(),
            approver: approver.into(),
        }
    }
}

/// Tenant memory object — institutional memory for the highest scope
/// in the B2B hierarchy. Items are *always* `Critical`-class; the
/// only lifecycle paths are explicit deprecation / supersession.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantMemoryObject {
    /// Unique id (UUID v4).
    pub id: Uuid,
    /// Scope this tenant memory belongs to.
    pub scope_id: ScopeId,
    /// Latest tenant-level summary text.
    pub summary: String,
    /// Canonical tenant policies.
    pub policies: Vec<CanonicalPolicy>,
    /// Product taxonomy entries.
    pub taxonomy: Vec<ProductTaxonomyEntry>,
    /// Stable org-knowledge facts.
    pub stable_facts: Vec<StableOrgKnowledge>,
    /// Domain scopes whose synthesis outputs feed this tenant memory.
    pub domain_scopes: Vec<ScopeId>,
    /// Approved official documents admitted to tenant synthesis.
    pub approved_documents: Vec<ApprovedDocumentRef>,
    /// Window id of the synthesis run that produced the current
    /// summary. `None` until the first synthesis.
    pub last_synthesis_window: Option<Uuid>,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock last update time.
    pub updated_at: DateTime<Utc>,
}

impl TenantMemoryObject {
    /// Construct a fresh empty tenant memory for `scope_id`.
    pub fn new(scope_id: ScopeId) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            scope_id,
            summary: String::new(),
            policies: Vec::new(),
            taxonomy: Vec::new(),
            stable_facts: Vec::new(),
            domain_scopes: Vec::new(),
            approved_documents: Vec::new(),
            last_synthesis_window: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Replace the tenant summary text with the latest synthesizer
    /// output.
    pub fn update_summary(&mut self, summary: impl Into<String>, synthesis_window: Option<Uuid>) {
        self.summary = summary.into();
        self.last_synthesis_window = synthesis_window;
        self.updated_at = Utc::now();
    }

    /// Register a domain scope that feeds this tenant memory.
    pub fn attach_domain_scope(&mut self, domain: ScopeId) {
        if !self.domain_scopes.contains(&domain) {
            self.domain_scopes.push(domain);
            self.updated_at = Utc::now();
        }
    }

    /// Admit an approved official document into tenant synthesis.
    /// Idempotent on document id.
    pub fn admit_approved_document(&mut self, doc: ApprovedDocumentRef) {
        if !self.approved_documents.iter().any(|d| d.id == doc.id) {
            self.approved_documents.push(doc);
            self.updated_at = Utc::now();
        }
    }

    /// Revoke a previously admitted approved document. Tenant
    /// synthesis windows opened after this call must reject the
    /// revoked document.
    pub fn revoke_approved_document(&mut self, doc_id: Uuid) -> Result<()> {
        let before = self.approved_documents.len();
        self.approved_documents.retain(|d| d.id != doc_id);
        if self.approved_documents.len() == before {
            return Err(TenantMemoryError::NotFound(doc_id));
        }
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Append a canonical policy. Returns the underlying memory id.
    ///
    /// # Errors
    ///
    /// [`TenantMemoryError::NotCritical`] if the supplied policy is
    /// not pinned to `Critical` sensitivity. (`CanonicalPolicy::new`
    /// always pins `Critical`, but if a caller mutates the inner
    /// `memory.sensitivity_class` this guard catches that.)
    pub fn add_policy(&mut self, policy: CanonicalPolicy) -> Result<Uuid> {
        require_critical(&policy.memory)?;
        let id = policy.memory.id;
        self.policies.push(policy);
        self.updated_at = Utc::now();
        Ok(id)
    }

    /// Append a product taxonomy entry. See [`Self::add_policy`] for
    /// the `Critical` invariant.
    pub fn add_taxonomy_entry(&mut self, entry: ProductTaxonomyEntry) -> Result<Uuid> {
        require_critical(&entry.memory)?;
        let id = entry.memory.id;
        self.taxonomy.push(entry);
        self.updated_at = Utc::now();
        Ok(id)
    }

    /// Append a stable org-knowledge fact.
    pub fn add_stable_fact(&mut self, fact: StableOrgKnowledge) -> Result<Uuid> {
        require_critical(&fact.memory)?;
        let id = fact.memory.id;
        self.stable_facts.push(fact);
        self.updated_at = Utc::now();
        Ok(id)
    }

    /// Explicitly deprecate a canonical policy. The `superseded_by`
    /// argument records the id of the newer canonical policy (if
    /// any); pass `None` for an outright retirement.
    pub fn deprecate_policy(&mut self, policy_id: Uuid, superseded_by: Option<Uuid>) -> Result<()> {
        let p = self
            .policies
            .iter_mut()
            .find(|p| p.memory.id == policy_id)
            .ok_or(TenantMemoryError::NotFound(policy_id))?;
        p.deprecated_at = Some(Utc::now());
        p.superseded_by = superseded_by;
        p.memory.last_accessed_at = Utc::now();
        if let Some(s) = superseded_by {
            p.memory.superseded_by = Some(s);
        }
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Explicitly deprecate a product-taxonomy entry.
    pub fn deprecate_taxonomy_entry(&mut self, entry_id: Uuid) -> Result<()> {
        let e = self
            .taxonomy
            .iter_mut()
            .find(|e| e.memory.id == entry_id)
            .ok_or(TenantMemoryError::NotFound(entry_id))?;
        e.deprecated_at = Some(Utc::now());
        e.memory.last_accessed_at = Utc::now();
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Explicitly deprecate a stable org-knowledge fact.
    pub fn deprecate_stable_fact(
        &mut self,
        fact_id: Uuid,
        superseded_by: Option<Uuid>,
    ) -> Result<()> {
        let f = self
            .stable_facts
            .iter_mut()
            .find(|f| f.memory.id == fact_id)
            .ok_or(TenantMemoryError::NotFound(fact_id))?;
        f.deprecated_at = Some(Utc::now());
        f.superseded_by = superseded_by;
        f.memory.last_accessed_at = Utc::now();
        if let Some(s) = superseded_by {
            f.memory.superseded_by = Some(s);
        }
        self.updated_at = Utc::now();
        Ok(())
    }

    /// All policies that have not been deprecated.
    pub fn list_active_policies(&self) -> Vec<&CanonicalPolicy> {
        self.policies
            .iter()
            .filter(|p| !p.is_deprecated())
            .collect()
    }

    /// All taxonomy entries that have not been deprecated.
    pub fn list_active_taxonomy(&self) -> Vec<&ProductTaxonomyEntry> {
        self.taxonomy
            .iter()
            .filter(|e| !e.is_deprecated())
            .collect()
    }

    /// All stable facts that have not been deprecated.
    pub fn list_active_stable_facts(&self) -> Vec<&StableOrgKnowledge> {
        self.stable_facts
            .iter()
            .filter(|f| !f.is_deprecated())
            .collect()
    }

    /// Tenant memory has **no passive decay**. Calling this method is
    /// always an error — callers must use `deprecate_*` instead.
    /// Exposed so the type system can refuse passive-decay code paths
    /// at the call site rather than relying on a comment.
    ///
    /// # Errors
    ///
    /// Always returns [`TenantMemoryError::PassiveDecayForbidden`].
    pub fn try_passive_decay(&self) -> Result<()> {
        Err(TenantMemoryError::PassiveDecayForbidden)
    }
}

fn require_critical(memory: &MemoryObject) -> Result<()> {
    if memory.sensitivity_class != SensitivityClass::Critical {
        return Err(TenantMemoryError::NotCritical(memory.sensitivity_class));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tenant_memory_is_empty() {
        let scope = ScopeId::new_v4();
        let t = TenantMemoryObject::new(scope);
        assert_eq!(t.scope_id, scope);
        assert!(t.policies.is_empty());
    }

    #[test]
    fn passive_decay_is_forbidden() {
        let t = TenantMemoryObject::new(ScopeId::new_v4());
        assert_eq!(
            t.try_passive_decay().unwrap_err(),
            TenantMemoryError::PassiveDecayForbidden
        );
    }

    #[test]
    fn add_policy_rejects_non_critical_memory() {
        let scope = ScopeId::new_v4();
        let mut t = TenantMemoryObject::new(scope);
        let mut policy = CanonicalPolicy::new(scope, "PII stays in jurisdiction");
        // Mutate the underlying class out from under the constructor.
        policy.memory.sensitivity_class = SensitivityClass::Important;
        let err = t.add_policy(policy).unwrap_err();
        assert_eq!(
            err,
            TenantMemoryError::NotCritical(SensitivityClass::Important)
        );
    }
}
