//! Driver abstraction: pluggable backends for the replay engine.

pub mod rust_native;

#[cfg(feature = "http-driver")]
pub mod http_gateway;

use evidence_store::{EvidenceId, ImportanceClass, ScopeId};
use observation_engine::Observation;

/// Result of a synthesis trigger.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SynthesisResult {
    /// Window ID assigned.
    pub window_id: String,
    /// Status of the window after triggering.
    pub status: String,
}

/// A memory record returned by the driver.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryRecord {
    /// Memory object ID.
    pub id: String,
    /// Scope ID.
    pub scope_id: ScopeId,
    /// Memory state.
    pub state: String,
    /// Observation type (if available).
    pub observation_type: Option<String>,
    /// Content summary (if available).
    pub content: Option<String>,
    /// Pin count.
    pub pin_count: u32,
    /// Retrieval count.
    pub retrieval_count: u32,
    /// Corroboration count.
    pub corroboration_count: u32,
    /// Sensitivity class name.
    pub sensitivity_class: String,
    /// Superseded-by memory id, if any.
    pub superseded_by: Option<String>,
    /// Whether the current policy would archive this object now.
    pub archivable: bool,
    /// Retention score.
    pub retention_score: f64,
    /// Pinning retention component.
    pub pinning: f64,
    /// Retrieval-frequency retention component.
    pub retrieval_frequency: f64,
    /// Corroboration retention component.
    pub corroboration: f64,
    /// Contradiction retention component.
    pub contradiction: f64,
    /// Age decay retention component.
    pub age: f64,
    /// Non-use decay retention component.
    pub non_use: f64,
}

/// Result of a decay sweep.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DecayResult {
    /// Number of objects archived (candidate + superseded + policy-driven).
    pub archived: u32,
    /// Number of objects deleted by policy.
    pub deleted: u32,
    /// Number of objects resurrected from Archived.
    pub resurrected: u32,
    /// Number of objects promoted to Reinforced.
    pub promoted_to_reinforced: u32,
    /// Number of objects promoted to Consolidated.
    pub promoted_to_consolidated: u32,
    /// Number of objects promoted to Canonical.
    pub promoted_to_canonical: u32,
}

/// Concept graph snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConceptGraphSnapshot {
    /// Number of nodes.
    pub node_count: usize,
    /// Number of edges.
    pub edge_count: usize,
    /// Node states present.
    pub node_states: Vec<String>,
}

/// Result of a contradiction scan.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContradictionResult {
    /// Number of contradiction pairs found.
    pub count: usize,
}

/// Result of a drift scan.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DriftResult {
    /// Number of drift markers found.
    pub count: usize,
}

/// Result of a query plan explanation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExplainQueryResult {
    /// Query class assigned by the classifier.
    pub query_class: String,
    /// Number of retrieval steps in the plan.
    pub step_count: usize,
    /// Retrieval modes in order.
    pub steps: Vec<String>,
}

/// Health check result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HealthCheck {
    /// Whether the driver is healthy.
    pub healthy: bool,
    /// Evidence count.
    pub evidence_count: usize,
    /// Number of forgotten scopes.
    pub forgotten_scopes: usize,
}

/// Which driver to use for the simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DriverKind {
    /// In-process Rust crate driver (fast, no server needed).
    RustNative,
    /// HTTP gateway driver (requires running server).
    #[cfg(feature = "http-driver")]
    HttpGateway,
}

/// A single query hit from a retrieval.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryHit {
    /// Evidence ID.
    pub evidence_id: EvidenceId,
    /// Retrieval score.
    pub score: f64,
}

/// Result of an ingest operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IngestResult {
    /// Evidence ID assigned.
    pub evidence_id: EvidenceId,
    /// Storage path taken.
    pub storage_path: String,
}

/// The trait that all drivers implement. The replay engine calls
/// these methods to drive the substrate through its lifecycle.
pub trait LifecycleDriver {
    /// Ingest a message body into a scope.
    fn ingest(
        &mut self,
        scope: ScopeId,
        body: &[u8],
        source: &str,
        importance: ImportanceClass,
    ) -> Result<IngestResult, String>;

    /// Query for evidence in a scope.
    fn query(&mut self, scope: ScopeId, query: &str, limit: usize) -> Result<Vec<QueryHit>, String>;

    /// Read the body of an evidence row.
    fn read_body(&self, id: EvidenceId) -> Result<Vec<u8>, String>;

    /// Get evidence metadata.
    fn get_evidence(&self, id: EvidenceId) -> Result<Option<String>, String>;

    /// Extract observations from text.
    fn extract_observations(
        &mut self,
        text: &str,
        scope: ScopeId,
    ) -> Result<Vec<Observation>, String>;

    /// Search FTS for evidence IDs.
    fn search_fts(
        &self,
        scope: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceId>, String>;

    /// Count evidence rows in the store.
    fn evidence_count(&self) -> Result<usize, String>;

    /// Count evidence rows belonging to a specific scope.
    fn evidence_count_for_scope(&self, scope: ScopeId) -> Result<usize, String>;

    /// Cryptographic forget: purge CEK wraps, FTS, record tombstone, destroy DEK.
    fn forget_scope(&mut self, scope: ScopeId) -> Result<(), String>;

    /// Load forgotten scope tombstones.
    fn load_forgotten_scopes(&self) -> Result<Vec<ScopeId>, String>;

    /// Close and reopen the store (to test tombstone persistence).
    fn reopen(&mut self) -> Result<(), String>;

    // ── Synthesis ──────────────────────────────────────────────

    /// Trigger a synthesis cycle for a scope.
    fn trigger_synthesis(&mut self, scope: ScopeId) -> Result<SynthesisResult, String>;

    /// Check synthesis window status.
    fn synthesis_status(&self, scope: ScopeId) -> Result<Vec<SynthesisResult>, String>;

    // ── Memory ─────────────────────────────────────────────────

    /// Add an observation to user memory for a scope.
    fn add_memory_observation(
        &mut self,
        scope: ScopeId,
        obs_type: &str,
        content: &str,
    ) -> Result<String, String>;

    /// Pin a memory object by ID.
    fn pin_memory(&mut self, id: &str) -> Result<(), String>;

    /// Unpin a memory object by ID.
    fn unpin_memory(&mut self, id: &str) -> Result<(), String>;

    /// List memory objects for a scope.
    fn list_memories(&self, scope: ScopeId) -> Result<Vec<MemoryRecord>, String>;

    /// Run a decay sweep over a scope's memory.
    fn run_decay_sweep(&mut self, scope: ScopeId) -> Result<DecayResult, String>;

    // ── Concept graph ──────────────────────────────────────────

    /// Get a concept graph snapshot for a scope.
    fn get_concept_graph(&self, scope: ScopeId) -> Result<ConceptGraphSnapshot, String>;

    // ── Reasoning ──────────────────────────────────────────────

    /// Detect contradictions in a scope's concept graph.
    fn reasoning_contradictions(&self, scope: ScopeId) -> Result<ContradictionResult, String>;

    /// Detect drift in a scope's concept graph.
    fn reasoning_drift(&self, scope: ScopeId) -> Result<DriftResult, String>;

    /// Explain a query plan.
    fn reasoning_explain_query(&self, query: &str) -> Result<ExplainQueryResult, String>;

    // ── Health & checkpointing ─────────────────────────────────

    /// Check driver health.
    fn health_check(&self) -> Result<HealthCheck, String>;

    /// Checkpoint state to disk.
    fn checkpoint(&self) -> Result<(), String>;

    /// Restore state from disk.
    fn restore(&mut self) -> Result<(), String>;
}
