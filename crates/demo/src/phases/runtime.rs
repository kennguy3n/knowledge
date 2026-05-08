//! Shared per-run state threaded through every demo phase.
//!
//! Each phase mutates a single [`RuntimeState`] instance owned by the
//! demo binary. State that needs to outlive a phase (the open
//! [`evidence_store::EvidenceStore`], the per-scope ingested rows, the
//! concept graph backing path, the [`audit_service::AuditLog`], …) is
//! stored here so later phases can drive against real data produced
//! by earlier phases instead of synthetic stand-ins.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use crypto::MasterKey;
use evidence_store::{EvidenceId, EvidenceStore, ImportanceClass, ScopeId};
use observation_engine::Observation;
use tempfile::TempDir;

use crate::dataset::ScopeTier;

/// Per-row metadata captured during evidence ingestion that downstream
/// phases (observation extraction, memory manager, audit) want to see.
#[derive(Debug, Clone)]
pub struct IngestedRow {
    /// Evidence-store row id returned from `EvidenceStore::ingest`.
    pub evidence_id: EvidenceId,
    /// Scope this row was ingested under.
    pub scope_id: ScopeId,
    /// Human-readable label of the scope (matches `Dataset.*_scope.label`).
    pub scope_label: &'static str,
    /// User / channel / domain / tenant tier the row belongs to.
    pub scope_tier: ScopeTier,
    /// Provenance source-ref string.
    pub source_ref: String,
    /// Plaintext message body. Carried across phases so observation
    /// extraction and memory-manager phases don't need a second pass
    /// against the encrypted store.
    pub body: String,
    /// Importance class assigned by the composite classifier.
    pub importance: ImportanceClass,
    /// Synthetic occurred-at timestamp from the dataset generator.
    pub occurred_at: DateTime<Utc>,
}

/// State shared across every phase of the demo run.
///
/// The struct is intentionally `pub` field-by-field; each phase reads
/// what it needs from prior phases and appends its own results.
pub struct RuntimeState {
    /// Per-user master key used to derive every SQLCipher / AEAD key
    /// in the demo run. Deterministically generated so the run is
    /// reproducible.
    pub master_key: MasterKey,

    /// Tempdir that holds the encrypted evidence database. Kept alive
    /// for the duration of the run.
    pub evidence_temp: Option<TempDir>,
    /// Open [`EvidenceStore`] (Phase 1 → Phase 2 / Phase 12).
    pub evidence_store: Option<EvidenceStore>,
    /// Rows captured during Phase 1 (Phase 1 → Phase 2 / Phase 3).
    pub ingested_rows: Vec<IngestedRow>,

    /// Observations produced by Phase 2 (Phase 2 → Phase 3 / Phase 4).
    pub observations: Vec<Observation>,

    /// Tempdir for the persistent concept graph (Phase 4).
    pub graph_temp: Option<TempDir>,
    /// On-disk path of the SQLCipher concept-graph database.
    pub graph_db_path: Option<PathBuf>,
    /// Count of canonical concept nodes promoted in Phase 4 (used by
    /// later phases to size the policy / export run).
    pub canonical_concept_count: u64,
    /// Total node count after Phase 4.
    pub concept_node_count: u64,
    /// Total edge count after Phase 4.
    pub concept_edge_count: u64,

    /// Memory-object counts surfaced by Phase 3.
    pub memory_object_count: u64,
    /// Canonical-memory count after the decay state machine runs in
    /// Phase 3.
    pub canonical_memory_count: u64,

    /// Synthesis-pipeline counts produced in Phase 5.
    pub channel_output_count: u64,
    pub domain_output_count: u64,
    pub tenant_output_count: u64,

    /// Canonical concept ids produced by Phase 4. Used by Phase 7
    /// when signing provenance bundles for canonical concepts.
    pub canonical_concept_ids: Vec<uuid::Uuid>,
    /// Phase 7 — provenance bundles signed.
    pub signed_provenance_bundles: u64,
    /// Phase 7 — AEAD encrypt/decrypt round-trips performed.
    pub aead_round_trips: u64,
    /// Phase 7 — hybrid KEM encap/decap round-trips performed.
    pub kem_roundtrips: u64,
    /// Phase 7 — scopes whose DEK was destroyed.
    pub scopes_forgotten: u64,
    /// Phase 7 — number of epoch rotations driven through the
    /// `EpochManager` (force / size / time).
    pub epoch_rotations: u64,

    /// Phase 8 — number of [`export_plane::PortableConceptProfile`]s
    /// minted during the run.
    pub export_profiles_created: u64,
    /// Phase 8 — number of canonical concepts approved through the
    /// [`export_plane::ConceptApprovalWorkflow`].
    pub export_concepts_approved: u64,
    /// Phase 8 — number of [`export_plane::ExportView`]s rendered via
    /// [`export_plane::ExportView::from_decision`].
    pub export_views_rendered: u64,
    /// Phase 8 — number of [`export_plane::PolicySimulator::simulate`]
    /// runs executed.
    pub export_simulations_run: u64,

    /// Phase 9 — number of [`agent_contract::AgentProposal`]s
    /// submitted to the [`agent_contract::ProposalStore`].
    pub proposals_submitted: u64,
    /// Phase 9 — number of proposals auto-promoted via the
    /// [`agent_contract::AutoPromotionPolicy`].
    pub proposals_auto_promoted: u64,
    /// Phase 9 — number of proposals manually promoted via
    /// [`agent_contract::ProposalStore::promote`].
    pub proposals_manually_promoted: u64,
    /// Phase 9 — number of proposals rejected (manual or
    /// TTL-expiry).
    pub proposals_rejected: u64,

    /// Phase 11 — number of distinct connectors exercised.
    pub connectors_exercised: u64,
    /// Phase 11 — total connector events emitted across initial /
    /// incremental syncs.
    pub connector_events_emitted: u64,
    /// Phase 11 — total webhook payloads parsed.
    pub connector_webhooks_parsed: u64,
    /// Phase 11 — total webhook subscriptions registered.
    pub connector_subscriptions: u64,

    /// Append-only audit log shared across phases. Phase 12 queries
    /// it; every phase that performs an audit-worthy action appends
    /// to it.
    pub audit_log: audit_service::AuditLog,
}

impl RuntimeState {
    /// Construct a fresh runtime with a deterministic master key.
    pub fn new() -> Self {
        let mut master_key_bytes = [0u8; 32];
        for (i, b) in master_key_bytes.iter_mut().enumerate() {
            *b = ((i as u32).wrapping_mul(17).wrapping_add(3) & 0xff) as u8;
        }
        Self {
            master_key: master_key_bytes,
            evidence_temp: None,
            evidence_store: None,
            ingested_rows: Vec::new(),
            observations: Vec::new(),
            graph_temp: None,
            graph_db_path: None,
            canonical_concept_count: 0,
            concept_node_count: 0,
            concept_edge_count: 0,
            memory_object_count: 0,
            canonical_memory_count: 0,
            channel_output_count: 0,
            domain_output_count: 0,
            tenant_output_count: 0,
            canonical_concept_ids: Vec::new(),
            signed_provenance_bundles: 0,
            aead_round_trips: 0,
            kem_roundtrips: 0,
            scopes_forgotten: 0,
            epoch_rotations: 0,
            export_profiles_created: 0,
            export_concepts_approved: 0,
            export_views_rendered: 0,
            export_simulations_run: 0,
            proposals_submitted: 0,
            proposals_auto_promoted: 0,
            proposals_manually_promoted: 0,
            proposals_rejected: 0,
            connectors_exercised: 0,
            connector_events_emitted: 0,
            connector_webhooks_parsed: 0,
            connector_subscriptions: 0,
            audit_log: audit_service::AuditLog::new(),
        }
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}
