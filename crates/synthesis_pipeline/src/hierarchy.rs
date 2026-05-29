//! Synthesis-hierarchy enforcement.
//!
//! Per `docs/DESIGN.md` §6.3 the substrate has three rules about what
//! each synthesis tier may consume:
//!
//! 1. **Channel synthesis** consumes raw evidence (messages /
//!    observations).
//! 2. **Domain synthesis consumes channel outputs.** Channel objects
//!    are the input contract for domain synthesis; raw evidence and
//!    user-scope objects are out of contract.
//! 3. **Tenant synthesis consumes domain objects + approved official
//!    docs.** No back-channel access to raw evidence at tenant scope.
//!
//! The hierarchy lifts those rules into the type system. The
//! [`DomainSynthesisInput`] type can only be constructed from
//! [`memory_manager::ChannelMemoryObject`] outputs (and their derived
//! [`SynthesisObject`]s); the [`TenantSynthesisInput`] type can only
//! be constructed from [`memory_manager::DomainMemoryObject`] outputs
//! plus [`memory_manager::ApprovedDocumentRef`]s. The
//! [`SynthesisWindowManager`]'s scope-aware open/validation methods
//! refuse to admit channel objects into tenant windows or raw rows
//! into domain windows.
//!
//! [`SynthesisObject`]: crate::object::SynthesisObject
//! [`SynthesisWindowManager`]: crate::window::SynthesisWindowManager

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;
use memory_manager::{
    ApprovedDocumentRef, ChannelMemoryObject, DomainMemoryObject, TenantMemoryObject,
};

use crate::error::{PipelineError, Result};
use crate::object::{SynthesisObject, SynthesisObjectType};
use crate::window::{SynthesisWindow, SynthesisWindowManager, WindowId, WindowStatus};

/// One channel-synthesis output admitted as a domain-synthesis input.
///
/// Constructed only via [`Self::from_channel_object`], which checks
/// that the source object's [`SynthesisObjectType`] is
/// [`SynthesisObjectType::ChannelRecap`]. The hierarchy module
/// re-exports no public constructor that bypasses that check, so
/// downstream callers cannot smuggle a tenant or episodic object into
/// a domain window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelOutput {
    object: SynthesisObject,
    /// Scope of the channel that emitted the object. Tracked
    /// separately so the domain-window validator can refuse outputs
    /// from a channel scope that isn't part of the domain's input
    /// contract.
    pub channel_scope: ScopeId,
}

impl ChannelOutput {
    /// Wrap a [`SynthesisObject`] as a channel output. Returns
    /// [`PipelineError::HierarchyViolation`] if the object is not a
    /// [`SynthesisObjectType::ChannelRecap`].
    pub fn from_channel_object(object: SynthesisObject) -> Result<Self> {
        if object.object_type != SynthesisObjectType::ChannelRecap {
            return Err(PipelineError::HierarchyViolation(format!(
                "expected channel_recap synthesis object, got {}",
                object.object_type.as_str()
            )));
        }
        let channel_scope = object.scope_id;
        Ok(Self {
            object,
            channel_scope,
        })
    }

    /// The wrapped synthesis object.
    pub fn object(&self) -> &SynthesisObject {
        &self.object
    }
}

/// One domain-synthesis output admitted as a tenant-synthesis input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainOutput {
    object: SynthesisObject,
    /// Scope of the domain that emitted the object.
    pub domain_scope: ScopeId,
}

impl DomainOutput {
    /// Wrap a [`SynthesisObject`] as a domain output. Returns
    /// [`PipelineError::HierarchyViolation`] if the object is not a
    /// [`SynthesisObjectType::DomainSummary`].
    pub fn from_domain_object(object: SynthesisObject) -> Result<Self> {
        if object.object_type != SynthesisObjectType::DomainSummary {
            return Err(PipelineError::HierarchyViolation(format!(
                "expected domain_summary synthesis object, got {}",
                object.object_type.as_str()
            )));
        }
        let domain_scope = object.scope_id;
        Ok(Self {
            object,
            domain_scope,
        })
    }

    /// The wrapped synthesis object.
    pub fn object(&self) -> &SynthesisObject {
        &self.object
    }
}

/// Input bundle for a domain-scope synthesis run.
///
/// Per `docs/DESIGN.md` §6.3 rule 2, domain synthesis consumes channel
/// outputs only. The [`Self::new`] constructor takes a domain
/// [`DomainMemoryObject`] (which carries the registered channel
/// scopes) and a slice of [`ChannelOutput`]s; it rejects any output
/// whose `channel_scope` is not in the domain's
/// `channel_scopes` list, and it rejects raw / cross-scope objects
/// at the type level (the only constructor for [`ChannelOutput`]
/// requires a channel-recap object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainSynthesisInput {
    /// Domain scope this input bundle targets.
    pub domain_scope: ScopeId,
    /// Channel outputs admitted into this run.
    pub channel_outputs: Vec<ChannelOutput>,
    /// Wall-clock time at which the bundle was assembled.
    pub assembled_at: DateTime<Utc>,
}

impl DomainSynthesisInput {
    /// Construct a fresh domain-synthesis input bundle.
    ///
    /// # Errors
    ///
    /// * [`PipelineError::HierarchyViolation`] if any
    ///   [`ChannelOutput`] is from a channel scope not registered on
    ///   `domain.channel_scopes`.
    pub fn new(domain: &DomainMemoryObject, channel_outputs: Vec<ChannelOutput>) -> Result<Self> {
        for o in &channel_outputs {
            if !domain.channel_scopes.contains(&o.channel_scope) {
                return Err(PipelineError::HierarchyViolation(format!(
                    "channel scope {} is not registered on domain {}",
                    o.channel_scope, domain.scope_id,
                )));
            }
        }
        Ok(Self {
            domain_scope: domain.scope_id,
            channel_outputs,
            assembled_at: Utc::now(),
        })
    }

    /// Refuse a raw [`ChannelMemoryObject`] as input. Domain synthesis
    /// consumes the channel's *outputs* (channel-recap synthesis
    /// objects), not the channel-memory state itself; this method
    /// makes that rejection explicit at the type level.
    ///
    /// # Errors
    ///
    /// Always returns [`PipelineError::HierarchyViolation`].
    pub fn reject_raw_channel_memory(_: &ChannelMemoryObject) -> Result<Self> {
        Err(PipelineError::HierarchyViolation(
            "domain synthesis cannot consume raw ChannelMemoryObject; \
             feed channel-recap SynthesisObject outputs instead"
                .into(),
        ))
    }
}

/// One approved official document admitted as a tenant-synthesis
/// input. The contract is "approved-doc reference + opaque
/// blob"; downstream code carries the blob through into the
/// synthesizer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedDocument {
    /// Reference (id, label, approver, approved_at).
    pub reference: ApprovedDocumentRef,
    /// Opaque payload bytes (PDF, markdown, ...). Kept opaque; the
    /// SLM adapter will parse it later.
    pub payload: Vec<u8>,
}

impl ApprovedDocument {
    /// Construct a fresh approved document with `payload`.
    pub fn new(reference: ApprovedDocumentRef, payload: Vec<u8>) -> Self {
        Self { reference, payload }
    }
}

/// Input bundle for a tenant-scope synthesis run.
///
/// Per `docs/DESIGN.md` §6.3 rule 3, tenant synthesis consumes domain
/// objects + approved official docs. The [`Self::new`] constructor
/// takes a [`TenantMemoryObject`] (which carries the registered
/// domain scopes and the admitted approved-document refs) and slices
/// of [`DomainOutput`]s and [`ApprovedDocument`]s; it rejects any
/// domain output whose `domain_scope` is not in the tenant's
/// `domain_scopes` list, and rejects approved-document refs that
/// have not been admitted via
/// [`TenantMemoryObject::admit_approved_document`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantSynthesisInput {
    /// Tenant scope this input bundle targets.
    pub tenant_scope: ScopeId,
    /// Domain outputs admitted into this run.
    pub domain_outputs: Vec<DomainOutput>,
    /// Approved official documents admitted into this run.
    pub approved_documents: Vec<ApprovedDocument>,
    /// Wall-clock time at which the bundle was assembled.
    pub assembled_at: DateTime<Utc>,
}

impl TenantSynthesisInput {
    /// Construct a fresh tenant-synthesis input bundle.
    ///
    /// # Errors
    ///
    /// * [`PipelineError::HierarchyViolation`] if any
    ///   [`DomainOutput`] is from a domain scope not registered on
    ///   `tenant.domain_scopes`.
    /// * [`PipelineError::HierarchyViolation`] if any
    ///   [`ApprovedDocument`] is not in `tenant.approved_documents`.
    pub fn new(
        tenant: &TenantMemoryObject,
        domain_outputs: Vec<DomainOutput>,
        approved_documents: Vec<ApprovedDocument>,
    ) -> Result<Self> {
        for o in &domain_outputs {
            if !tenant.domain_scopes.contains(&o.domain_scope) {
                return Err(PipelineError::HierarchyViolation(format!(
                    "domain scope {} is not registered on tenant {}",
                    o.domain_scope, tenant.scope_id,
                )));
            }
        }
        for d in &approved_documents {
            if !tenant
                .approved_documents
                .iter()
                .any(|a| a.id == d.reference.id)
            {
                return Err(PipelineError::HierarchyViolation(format!(
                    "approved document {} is not admitted on tenant {}",
                    d.reference.id, tenant.scope_id,
                )));
            }
        }
        Ok(Self {
            tenant_scope: tenant.scope_id,
            domain_outputs,
            approved_documents,
            assembled_at: Utc::now(),
        })
    }

    /// Refuse a raw [`SynthesisObject`] of channel type as input.
    /// Tenant synthesis cannot consume channel objects directly —
    /// they must first be folded into a domain summary. This method
    /// makes that rejection explicit at the type level.
    ///
    /// # Errors
    ///
    /// Always returns [`PipelineError::HierarchyViolation`].
    pub fn reject_channel_object(object: &SynthesisObject) -> Result<Self> {
        Err(PipelineError::HierarchyViolation(format!(
            "tenant synthesis cannot consume {} synthesis objects; \
             channel outputs must first be folded into a domain summary",
            object.object_type.as_str()
        )))
    }
}

/// Scope-tier tag for a synthesis window. Used by the hierarchy-aware
/// window-manager methods to enforce that the right input type goes
/// into the right window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowScopeTier {
    /// Channel-scope window (consumes raw evidence).
    Channel,
    /// Domain-scope window (consumes channel outputs).
    Domain,
    /// Tenant-scope window (consumes domain outputs + approved docs).
    Tenant,
}

impl WindowScopeTier {
    /// Stable lowercase string tag matching the JSON `snake_case`
    /// representation. Provided so FFI layers and metrics can emit
    /// the tier without re-deriving the mapping at every call site.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Domain => "domain",
            Self::Tenant => "tenant",
        }
    }
}

/// Hierarchy-aware extension trait on [`SynthesisWindowManager`].
///
/// Keeps the underlying [`SynthesisWindowManager`] storage
/// untouched; the `_tiered` methods on this trait wrap the existing
/// open/lifecycle methods with a scope-tier tag. Domain windows
/// require a [`DomainSynthesisInput`] to be marked complete; tenant
/// windows require a [`TenantSynthesisInput`].
pub trait HierarchyEnforcedWindowManager {
    /// Open a window tagged with `tier`.
    fn open_tiered_window(
        &mut self,
        scope_id: ScopeId,
        tier: WindowScopeTier,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<TieredWindowHandle>;

    /// Validate that `input` is admissible for the domain-tier window
    /// `handle`. Returns [`PipelineError::HierarchyViolation`] if
    /// `handle` is not a domain window or if the input's
    /// `domain_scope` doesn't match.
    fn validate_domain_input(
        &self,
        handle: &TieredWindowHandle,
        input: &DomainSynthesisInput,
    ) -> Result<()>;

    /// Validate that `input` is admissible for the tenant-tier window
    /// `handle`. Returns [`PipelineError::HierarchyViolation`] if
    /// `handle` is not a tenant window or if the input's
    /// `tenant_scope` doesn't match.
    fn validate_tenant_input(
        &self,
        handle: &TieredWindowHandle,
        input: &TenantSynthesisInput,
    ) -> Result<()>;
}

/// Handle for a window opened via
/// [`HierarchyEnforcedWindowManager::open_tiered_window`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TieredWindowHandle {
    /// Underlying window id.
    pub window_id: WindowId,
    /// Scope this window targets.
    pub scope_id: ScopeId,
    /// Scope-tier tag.
    pub tier: WindowScopeTier,
}

impl HierarchyEnforcedWindowManager for SynthesisWindowManager {
    fn open_tiered_window(
        &mut self,
        scope_id: ScopeId,
        tier: WindowScopeTier,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<TieredWindowHandle> {
        let id = self.open_window(scope_id, window_start, window_end)?;
        // Stamp the tier on the freshly opened window so the
        // persisted shape carries the tier through to rehydration,
        // letting callers (e.g. the FFI `synthesis_status` /
        // `list_recent_syntheses` paths) report the synthesis tier
        // even before the `Complete` synthesis object exists. The
        // `unwrap_or_else` arm is defensive only: `open_window`
        // either errors out or inserts the window, so a `None`
        // lookup here would indicate a manager-internal invariant
        // break rather than a recoverable runtime condition.
        if let Err(e) = self.set_tier(id, tier) {
            debug_assert!(
                false,
                "open_window inserted id={id:?} but set_tier failed: {e}"
            );
        }
        Ok(TieredWindowHandle {
            window_id: id,
            scope_id,
            tier,
        })
    }

    fn validate_domain_input(
        &self,
        handle: &TieredWindowHandle,
        input: &DomainSynthesisInput,
    ) -> Result<()> {
        if handle.tier != WindowScopeTier::Domain {
            return Err(PipelineError::HierarchyViolation(format!(
                "{:?} window cannot consume DomainSynthesisInput",
                handle.tier
            )));
        }
        if handle.scope_id != input.domain_scope {
            return Err(PipelineError::HierarchyViolation(format!(
                "domain input targets {} but window is for {}",
                input.domain_scope, handle.scope_id,
            )));
        }
        let window = self
            .get(handle.window_id)
            .ok_or(PipelineError::WindowNotFound(handle.window_id.0))?;
        // Cross-check: `TieredWindowHandle` has public fields, so a
        // caller can build (or mutate) a handle whose `window_id`
        // points at scope S1 while `scope_id` claims S2. Refuse before
        // the synthesis engine touches the wrong window's lifecycle.
        if window.scope_id != handle.scope_id {
            return Err(PipelineError::HierarchyViolation(format!(
                "window {} belongs to scope {} but handle claims scope {}",
                handle.window_id.0, window.scope_id, handle.scope_id,
            )));
        }
        if window.status == WindowStatus::Complete {
            return Err(PipelineError::InvalidWindowTransition);
        }
        Ok(())
    }

    fn validate_tenant_input(
        &self,
        handle: &TieredWindowHandle,
        input: &TenantSynthesisInput,
    ) -> Result<()> {
        if handle.tier != WindowScopeTier::Tenant {
            return Err(PipelineError::HierarchyViolation(format!(
                "{:?} window cannot consume TenantSynthesisInput",
                handle.tier
            )));
        }
        if handle.scope_id != input.tenant_scope {
            return Err(PipelineError::HierarchyViolation(format!(
                "tenant input targets {} but window is for {}",
                input.tenant_scope, handle.scope_id,
            )));
        }
        let window = self
            .get(handle.window_id)
            .ok_or(PipelineError::WindowNotFound(handle.window_id.0))?;
        // See `validate_domain_input` for the rationale; the same
        // public-field smuggling concern applies at tenant tier.
        if window.scope_id != handle.scope_id {
            return Err(PipelineError::HierarchyViolation(format!(
                "window {} belongs to scope {} but handle claims scope {}",
                handle.window_id.0, window.scope_id, handle.scope_id,
            )));
        }
        if window.status == WindowStatus::Complete {
            return Err(PipelineError::InvalidWindowTransition);
        }
        Ok(())
    }
}

/// Convenience: build a [`SynthesisObject`] of type
/// [`SynthesisObjectType::DomainSummary`] for a window opened on a
/// domain scope. Used by the synthesis-engine skeleton to emit the
/// right object type without exposing the full constructor surface.
pub fn build_domain_summary_object(
    domain_scope: ScopeId,
    window_id: WindowId,
    payload: Vec<u8>,
    provenance_ref: Uuid,
) -> SynthesisObject {
    SynthesisObject::new(
        domain_scope,
        window_id,
        SynthesisObjectType::DomainSummary,
        payload,
        provenance_ref,
    )
}

/// Convenience: build a [`SynthesisObject`] of type
/// [`SynthesisObjectType::TenantSummary`] for a window opened on a
/// tenant scope.
pub fn build_tenant_summary_object(
    tenant_scope: ScopeId,
    window_id: WindowId,
    payload: Vec<u8>,
    provenance_ref: Uuid,
) -> SynthesisObject {
    SynthesisObject::new(
        tenant_scope,
        window_id,
        SynthesisObjectType::TenantSummary,
        payload,
        provenance_ref,
    )
}

/// Convenience: a fresh window covering the most recent `secs` for a
/// domain scope, opened on `mgr` with the [`WindowScopeTier::Domain`]
/// tag.
pub fn open_domain_window(
    mgr: &mut SynthesisWindowManager,
    domain: &DomainMemoryObject,
    duration: chrono::Duration,
) -> Result<TieredWindowHandle> {
    let now = Utc::now();
    let window = SynthesisWindow::new(domain.scope_id, now - duration, now)?;
    let handle = mgr.open_tiered_window(
        domain.scope_id,
        WindowScopeTier::Domain,
        window.window_start,
        window.window_end,
    )?;
    Ok(handle)
}

/// Convenience: a fresh window covering the most recent `secs` for a
/// tenant scope, opened on `mgr` with the [`WindowScopeTier::Tenant`]
/// tag.
pub fn open_tenant_window(
    mgr: &mut SynthesisWindowManager,
    tenant: &TenantMemoryObject,
    duration: chrono::Duration,
) -> Result<TieredWindowHandle> {
    let now = Utc::now();
    let window = SynthesisWindow::new(tenant.scope_id, now - duration, now)?;
    let handle = mgr.open_tiered_window(
        tenant.scope_id,
        WindowScopeTier::Tenant,
        window.window_start,
        window.window_end,
    )?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_recap_object(scope: ScopeId) -> SynthesisObject {
        SynthesisObject::new(
            scope,
            WindowId::new_v4(),
            SynthesisObjectType::ChannelRecap,
            b"recap".to_vec(),
            Uuid::nil(),
        )
    }

    fn domain_summary_object(scope: ScopeId) -> SynthesisObject {
        SynthesisObject::new(
            scope,
            WindowId::new_v4(),
            SynthesisObjectType::DomainSummary,
            b"summary".to_vec(),
            Uuid::nil(),
        )
    }

    #[test]
    fn channel_output_constructor_rejects_non_channel_objects() {
        let scope = ScopeId::new_v4();
        let domain_obj = domain_summary_object(scope);
        let err = ChannelOutput::from_channel_object(domain_obj).unwrap_err();
        assert!(matches!(err, PipelineError::HierarchyViolation(_)));
    }

    #[test]
    fn domain_input_rejects_unregistered_channel_scope() {
        let domain_scope = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        let registered_channel = ScopeId::new_v4();
        let unregistered_channel = ScopeId::new_v4();
        domain.attach_channel_scope(registered_channel);

        let stray =
            ChannelOutput::from_channel_object(channel_recap_object(unregistered_channel)).unwrap();
        let err = DomainSynthesisInput::new(&domain, vec![stray]).unwrap_err();
        assert!(matches!(err, PipelineError::HierarchyViolation(_)));
    }

    #[test]
    fn domain_input_rejects_raw_channel_memory_at_type_level() {
        let scope = ScopeId::new_v4();
        let channel_mem = ChannelMemoryObject::new(scope);
        let err = DomainSynthesisInput::reject_raw_channel_memory(&channel_mem).unwrap_err();
        assert!(matches!(err, PipelineError::HierarchyViolation(_)));
    }

    #[test]
    fn validate_domain_input_rejects_handle_pointing_at_other_scope_window() {
        // `TieredWindowHandle` has public fields; a caller could
        // build a handle whose `window_id` points at scope S1 while
        // `scope_id` claims S2 (matching `input.domain_scope`). The
        // validator must cross-check the underlying window's scope.
        let domain_a = ScopeId::new_v4();
        let domain_b = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();
        let now = Utc::now();
        let handle_b = mgr
            .open_tiered_window(
                domain_b,
                WindowScopeTier::Domain,
                now - chrono::Duration::seconds(60),
                now,
            )
            .unwrap();

        // Smuggled handle: window_id from B's window, scope_id forged
        // to A. `input` also targets A so the existing
        // handle.scope_id == input.domain_scope check passes.
        let smuggled = TieredWindowHandle {
            window_id: handle_b.window_id,
            scope_id: domain_a,
            tier: WindowScopeTier::Domain,
        };
        let mut domain_a_obj = DomainMemoryObject::new(domain_a);
        let channel = ScopeId::new_v4();
        domain_a_obj.attach_channel_scope(channel);
        let chan_out = ChannelOutput::from_channel_object(channel_recap_object(channel)).unwrap();
        let input = DomainSynthesisInput::new(&domain_a_obj, vec![chan_out]).unwrap();

        let err = mgr.validate_domain_input(&smuggled, &input).unwrap_err();
        assert!(matches!(err, PipelineError::HierarchyViolation(_)));
    }
}
