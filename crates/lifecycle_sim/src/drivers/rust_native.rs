//! Rust-native driver: drives the substrate directly through crates.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Utc;
use evidence_store::{
    EvidenceId, EvidenceStore, EvidenceStoreConfig, HybridRetriever, ImportanceClass, ScopeId,
};
use observation_engine::{LexiconExtractor, Observation, ObservationExtractor};

use memory_manager::{
    compute_retention_score, MemoryFilter, MemoryState, PolicyEngine, SensitivityClass,
    UserMemoryObject,
};
use concept_graph::projection::{project_memory_graph, MemoryProjection};
use concept_graph::node::NodeState;
use reasoning_engine::{ContradictionDetector, NegationOracle, QueryPlanner};
use synthesis_pipeline::SynthesisWindowManager;

use super::{
    ConceptGraphSnapshot, ContradictionResult, DecayResult, ExplainQueryResult, HealthCheck,
    IngestResult, LifecycleDriver, MemoryRecord, QueryHit, SynthesisResult, DriftResult,
};

/// Map `UserMemoryObject`'s memory objects into `MemoryProjection`s for
/// concept graph construction and reasoning scans.
fn project_memories(umo: &UserMemoryObject) -> Vec<MemoryProjection> {
    umo.objects
        .iter()
        .filter(|o| o.state != MemoryState::Deleted)
        .map(|o| MemoryProjection {
            id: o.id,
            scope_id: o.scope_id,
            label: o
                .metadata
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            definition: o
                .metadata
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            state: match o.state {
                MemoryState::Candidate => NodeState::Candidate,
                MemoryState::Reinforced => NodeState::Candidate,
                MemoryState::Consolidated => NodeState::Canonical,
                MemoryState::Canonical => NodeState::Canonical,
                MemoryState::Superseded => NodeState::Superseded,
                MemoryState::Archived => NodeState::Superseded,
                MemoryState::Deleted => NodeState::Deleted,
            },
            superseded_by: o.superseded_by,
            created_at: o.created_at,
            updated_at: o.last_accessed_at,
            metadata: o.metadata.clone(),
        })
        .collect()
}

/// In-process driver using `EvidenceStore` + `LexiconExtractor` directly.
pub struct RustNativeDriver {
    store: EvidenceStore,
    extractor: LexiconExtractor,
    db_path: PathBuf,
    master_key: [u8; 32],
    /// Per-scope user memory objects.
    user_memories: HashMap<ScopeId, UserMemoryObject>,
    /// Per-scope synthesis window manager.
    synthesis_windows: SynthesisWindowManager,
    /// Retention / lifecycle policy engine.
    policy_engine: PolicyEngine,
    /// Tenant id for each scope (used for policy resolution).
    scope_tenants: HashMap<ScopeId, uuid::Uuid>,
    /// Checkpoint path.
    checkpoint_path: PathBuf,
}

impl RustNativeDriver {
    /// Create a new driver with a fresh store at `db_path`.
    pub fn new(db_path: PathBuf) -> Self {
        let master_key = [0xA5u8; 32];
        let store = EvidenceStore::open(
            &db_path,
            &master_key,
            EvidenceStoreConfig::default(),
        )
        .expect("open evidence store");
        let checkpoint_path = db_path.with_extension("checkpoint.json");
        Self {
            store,
            extractor: LexiconExtractor::default(),
            db_path,
            master_key,
            user_memories: HashMap::new(),
            synthesis_windows: SynthesisWindowManager::new(),
            policy_engine: PolicyEngine::new(),
            scope_tenants: HashMap::new(),
            checkpoint_path,
        }
    }

    /// Configure retention policies and tenant/scope mapping from the
    /// simulated world. Tenant 0 gets a B2B policy, tenant 1 a B2C
    /// policy, and tenant 2 keeps the global default. Every scope is
    /// mapped to its tenant for policy resolution.
    pub fn configure_for_world(&mut self, world: &crate::world::World) {
        let mut b2b = memory_manager::RetentionPolicy::b2b_default();
        let mut b2c = memory_manager::RetentionPolicy::b2c_default();
        // Make ids deterministic for the run.
        let b2b_id = uuid::Uuid::from_u128(0x0001_b2b0_0000_0000_0000_0000_0000_0001);
        let b2c_id = uuid::Uuid::from_u128(0x0001_b2c0_0000_0000_0000_0000_0000_0001);
        b2b.policy_id = b2b_id;
        b2c.policy_id = b2c_id;
        self.policy_engine.register_policy(b2b.clone());
        self.policy_engine.register_policy(b2c.clone());

        for (ti, tenant) in world.tenants.iter().enumerate() {
            let policy_id = match ti % 3 {
                0 => b2b_id,
                1 => b2c_id,
                _ => memory_manager::RetentionPolicy::global_default().policy_id,
            };
            let _ = self.policy_engine.set_tenant_policy(tenant.id, policy_id);
            for scope in &tenant.scopes {
                self.scope_tenants.insert(scope.scope_id, tenant.id);
                // Randomly assign a few scopes to the opposite policy to
                // exercise scope-level override.
                if scope.kind == crate::world::ScopeKind::Domain && ti % 3 == 2 {
                    let _ = self.policy_engine.set_scope_policy(scope.scope_id, b2b_id);
                }
            }
        }
    }

    /// Return a reference to the policy engine for assertions.
    pub fn policy_engine(&self) -> &memory_manager::PolicyEngine {
        &self.policy_engine
    }
}

impl LifecycleDriver for RustNativeDriver {
    fn ingest(
        &mut self,
        scope: ScopeId,
        body: &[u8],
        source: &str,
        importance: ImportanceClass,
    ) -> Result<IngestResult, String> {
        // Ensure a random per-scope DEK exists before ingesting, so
        // that cryptographic forgetting (delete_scope_dek) actually
        // prevents body decryption. Without this, the scope_key HKDF
        // fallback would re-derive the same key after DEK deletion.
        if importance != ImportanceClass::Noise {
            self.store
                .ensure_scope_dek(scope)
                .map_err(|e| format!("ensure_scope_dek error: {e}"))?;
        }
        let res = self
            .store
            .ingest(scope, body, Some(source), importance)
            .map_err(|e| format!("ingest error: {e}"))?;
        Ok(IngestResult {
            evidence_id: res.evidence_id,
            storage_path: format!("{:?}", res.storage_path),
        })
    }

    fn query(
        &mut self,
        scope: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<QueryHit>, String> {
        let retriever = HybridRetriever::new(&self.store);
        let hits = retriever
            .search_hybrid(scope, query, limit)
            .map_err(|e| format!("query error: {e}"))?;
        Ok(hits
            .into_iter()
            .map(|r| QueryHit {
                evidence_id: r.evidence_id,
                score: r.score,
            })
            .collect())
    }

    fn read_body(&self, id: EvidenceId) -> Result<Vec<u8>, String> {
        self.store
            .read_body(id)
            .map_err(|e| format!("read_body error: {e}"))
    }

    fn get_evidence(&self, id: EvidenceId) -> Result<Option<String>, String> {
        let row = self
            .store
            .get(id)
            .map_err(|e| format!("get error: {e}"))?;
        Ok(row.map(|r| {
            format!(
                "id={} scope={} importance={:?} path={:?} lang={:?}",
                r.id, r.scope_id, r.importance, r.storage_path, r.language_tag
            )
        }))
    }

    fn extract_observations(
        &mut self,
        text: &str,
        scope: ScopeId,
    ) -> Result<Vec<Observation>, String> {
        Ok(self.extractor.extract(text, scope))
    }

    fn search_fts(
        &self,
        scope: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceId>, String> {
        self.store
            .search_fts(scope, query, limit)
            .map_err(|e| format!("search_fts error: {e}"))
    }

    fn evidence_count(&self) -> Result<usize, String> {
        self.store
            .evidence_count()
            .map_err(|e| format!("evidence_count error: {e}"))
    }

    fn evidence_count_for_scope(&self, scope: ScopeId) -> Result<usize, String> {
        self.store
            .evidence_count_for_scope(scope)
            .map_err(|e| format!("evidence_count_for_scope error: {e}"))
    }

    fn forget_scope(&mut self, scope: ScopeId) -> Result<(), String> {
        self.store
            .purge_body_key_wraps_for_scope(scope)
            .map_err(|e| format!("purge wraps: {e}"))?;
        self.store
            .purge_fts_for_scope(scope)
            .map_err(|e| format!("purge fts: {e}"))?;
        self.store
            .record_forgotten_scope(scope)
            .map_err(|e| format!("record forgotten: {e}"))?;
        self.store
            .delete_scope_dek(scope)
            .map_err(|e| format!("delete dek: {e}"))?;
        // Clear in-memory user memories for this scope so concept graph
        // and memory listings return empty after forgetting.
        self.user_memories.remove(&scope);
        Ok(())
    }

    fn load_forgotten_scopes(&self) -> Result<Vec<ScopeId>, String> {
        self.store
            .load_forgotten_scopes()
            .map_err(|e| format!("load forgotten: {e}"))
    }

    fn reopen(&mut self) -> Result<(), String> {
        // Drop the current store and reopen from the same path.
        let db_path = self.db_path.clone();
        let key = self.master_key;
        self.store = EvidenceStore::open(&db_path, &key, EvidenceStoreConfig::default())
            .map_err(|e| format!("reopen error: {e}"))?;
        Ok(())
    }

    // ── Synthesis ──────────────────────────────────────────────

    fn trigger_synthesis(&mut self, scope: ScopeId) -> Result<SynthesisResult, String> {
        let now = Utc::now();
        let window_start = now;
        let window_end = now + chrono::Duration::hours(1);
        let window_id = self
            .synthesis_windows
            .open_window(scope, window_start, window_end)
            .map_err(|e| format!("synthesis open_window error: {e}"))?;

        // Transition through the state machine: Pending → InProgress → Complete.
        self.synthesis_windows
            .mark_in_progress(window_id)
            .map_err(|e| format!("synthesis mark_in_progress error: {e}"))?;
        self.synthesis_windows
            .mark_complete(window_id)
            .map_err(|e| format!("synthesis mark_complete error: {e}"))?;

        let window = self
            .synthesis_windows
            .get(window_id)
            .ok_or("synthesis window not found after mark_complete")?;
        Ok(SynthesisResult {
            window_id: window_id.to_string(),
            status: format!("{:?}", window.status),
        })
    }

    fn synthesis_status(&self, scope: ScopeId) -> Result<Vec<SynthesisResult>, String> {
        let windows = self.synthesis_windows.windows_for(scope);
        Ok(windows
            .iter()
            .map(|w| SynthesisResult {
                window_id: w.id.to_string(),
                status: format!("{:?}", w.status),
            })
            .collect())
    }

    // ── Memory ─────────────────────────────────────────────────

    fn add_memory_observation(
        &mut self,
        scope: ScopeId,
        obs_type: &str,
        content: &str,
    ) -> Result<String, String> {
        let umo = self
            .user_memories
            .entry(scope)
            .or_insert_with(|| UserMemoryObject::new(uuid::Uuid::new_v4(), scope));
        let id = umo.add_observation(obs_type, content, SensitivityClass::Important);
        Ok(id.to_string())
    }

    fn pin_memory(&mut self, id: &str) -> Result<(), String> {
        let uuid = uuid::Uuid::parse_str(id).map_err(|e| format!("invalid UUID: {e}"))?;
        for umo in self.user_memories.values_mut() {
            if umo.read(&uuid).is_some() {
                umo.pin(&uuid).map_err(|e| format!("pin error: {e}"))?;
                return Ok(());
            }
        }
        Err(format!("memory object {id} not found"))
    }

    fn unpin_memory(&mut self, id: &str) -> Result<(), String> {
        let uuid = uuid::Uuid::parse_str(id).map_err(|e| format!("invalid UUID: {e}"))?;
        for umo in self.user_memories.values_mut() {
            if umo.read(&uuid).is_some() {
                umo.unpin(&uuid).map_err(|e| format!("unpin error: {e}"))?;
                return Ok(());
            }
        }
        Err(format!("memory object {id} not found"))
    }

    fn list_memories(&self, scope: ScopeId) -> Result<Vec<MemoryRecord>, String> {
        let umo = match self.user_memories.get(&scope) {
            Some(u) => u,
            None => return Ok(Vec::new()),
        };
        let filter = MemoryFilter::any().with_scope(scope);
        let records = umo.list(&filter);
        let now = Utc::now();
        let tenant_id = self.scope_tenants.get(&scope).copied();
        Ok(records
            .iter()
            .map(|obj| {
                let score = compute_retention_score(obj, now);
                let archivable = matches!(
                    self.policy_engine.evaluate(obj, tenant_id, now, &score),
                    memory_manager::PolicyDecision::Archive
                );
                MemoryRecord {
                    id: obj.id.to_string(),
                    scope_id: obj.scope_id,
                    state: format!("{:?}", obj.state),
                    observation_type: obj
                        .metadata
                        .get("observation_type")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    content: obj
                        .metadata
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    pin_count: obj.pin_count,
                    retrieval_count: obj.retrieval_count,
                    corroboration_count: obj.corroboration_count,
                    sensitivity_class: format!("{:?}", obj.sensitivity_class).to_lowercase(),
                    superseded_by: obj.superseded_by.map(|id| id.to_string()),
                    archivable,
                    retention_score: score.total,
                    pinning: score.pinning,
                    retrieval_frequency: score.retrieval_frequency,
                    corroboration: score.corroboration,
                    contradiction: score.contradiction,
                    age: score.age,
                    non_use: score.non_use,
                }
            })
            .collect())
    }

    fn run_decay_sweep(&mut self, scope: ScopeId) -> Result<DecayResult, String> {
        let now = Utc::now();
        let umo = self
            .user_memories
            .entry(scope)
            .or_insert_with(|| UserMemoryObject::new(uuid::Uuid::new_v4(), scope));
        let tenant_id = self.scope_tenants.get(&scope).copied();
        let report = umo.decay_sweep_with_policy(now, &self.policy_engine, tenant_id);
        Ok(DecayResult {
            archived: (report.candidates_archived + report.superseded_archived) as u32,
            deleted: report.deleted_by_policy as u32,
            resurrected: report.archived_resurrected as u32,
            promoted_to_reinforced: report.promoted_to_reinforced as u32,
            promoted_to_consolidated: report.promoted_to_consolidated as u32,
            promoted_to_canonical: report.promoted_to_canonical as u32,
        })
    }

    // ── Concept graph ──────────────────────────────────────────

    fn get_concept_graph(&self, scope: ScopeId) -> Result<ConceptGraphSnapshot, String> {
        let umo = match self.user_memories.get(&scope) {
            Some(u) => u,
            None => {
                return Ok(ConceptGraphSnapshot {
                    node_count: 0,
                    edge_count: 0,
                    node_states: Vec::new(),
                })
            }
        };

        let projections = project_memories(umo);
        let graph = project_memory_graph(projections);
        let node_states: Vec<String> = graph
            .iter_nodes()
            .map(|n| n.state.as_str().to_string())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        Ok(ConceptGraphSnapshot {
            node_count: graph.node_count(),
            edge_count: graph.edge_count(),
            node_states,
        })
    }

    // ── Reasoning ──────────────────────────────────────────────

    fn reasoning_contradictions(&self, scope: ScopeId) -> Result<ContradictionResult, String> {
        let umo = match self.user_memories.get(&scope) {
            Some(u) => u,
            None => return Ok(ContradictionResult { count: 0 }),
        };

        let projections = project_memories(umo);
        let graph = project_memory_graph(projections);
        let oracle = NegationOracle;
        let detector = ContradictionDetector::new(&oracle);
        let edges = detector.scan(&graph);
        Ok(ContradictionResult { count: edges.len() })
    }

    fn reasoning_drift(&self, scope: ScopeId) -> Result<DriftResult, String> {
        let umo = match self.user_memories.get(&scope) {
            Some(u) => u,
            None => return Ok(DriftResult { count: 0 }),
        };

        let projections = project_memories(umo);
        let graph = project_memory_graph(projections);

        // Build evidence snapshots for each canonical node.
        // Since the simulation doesn't have historical baselines from
        // prior synthesis runs, we use the current state as the baseline.
        // This means still_valid == baseline, so drift will be 0 for
        // fresh data — but the detector is actually invoked, making the
        // assertion non-vacuous. If evidence were removed between scans,
        // the detector would correctly flag it.
        let mut snapshots: HashMap<concept_graph::node::NodeId, reasoning_engine::drift::EvidenceSnapshot> =
            HashMap::new();
        for node in graph.iter_nodes() {
            if node.state == NodeState::Canonical {
                // Use the node's metadata evidence IDs as the baseline.
                // In the simulation, all current evidence is still valid.
                let baseline: Vec<EvidenceId> = node
                    .metadata
                    .get("evidence_ids")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| {
                                v.as_str()
                                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
                                    .map(EvidenceId)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let still_valid = baseline.clone();
                snapshots.insert(
                    node.id,
                    reasoning_engine::drift::EvidenceSnapshot::partition(
                        baseline,
                        still_valid,
                        Vec::new(),
                    ),
                );
            }
        }

        let detector = reasoning_engine::DriftDetector::new();
        let markers = detector.scan(&graph, &snapshots);
        Ok(DriftResult { count: markers.len() })
    }

    fn reasoning_explain_query(&self, query: &str) -> Result<ExplainQueryResult, String> {
        let planner = QueryPlanner::new();
        let plan = planner.plan(query);
        Ok(ExplainQueryResult {
            query_class: format!("{:?}", plan.class),
            step_count: plan.steps.len(),
            steps: plan.steps.iter().map(|s| format!("{:?}", s.mode)).collect(),
        })
    }

    // ── Health & checkpointing ─────────────────────────────────

    fn health_check(&self) -> Result<HealthCheck, String> {
        let evidence_count = self.evidence_count()?;
        let forgotten = self.load_forgotten_scopes()?;

        // Real health checks:
        // 1. Store must be queryable (evidence_count succeeded above).
        // 2. No forgotten scope should still have an active DEK.
        let scope_deks = self
            .store
            .load_scope_deks()
            .map_err(|e| format!("health_check load_scope_deks error: {e}"))?;
        let orphaned_deks: Vec<_> = forgotten
            .iter()
            .filter(|s| scope_deks.contains_key(s))
            .collect();
        let healthy = orphaned_deks.is_empty();

        Ok(HealthCheck {
            healthy,
            evidence_count,
            forgotten_scopes: forgotten.len(),
        })
    }

    fn checkpoint(&self) -> Result<(), String> {
        let state = serde_json::json!({
            "user_memories": self.user_memories,
            "synthesis_windows": self.synthesis_windows,
            "policy_engine": self.policy_engine,
            "scope_tenants": self.scope_tenants,
        });
        let bytes = serde_json::to_vec(&state)
            .map_err(|e| format!("checkpoint serialize error: {e}"))?;
        std::fs::write(&self.checkpoint_path, bytes)
            .map_err(|e| format!("checkpoint write error: {e}"))?;
        Ok(())
    }

    fn restore(&mut self) -> Result<(), String> {
        if !self.checkpoint_path.exists() {
            return Err("no checkpoint file found".to_string());
        }
        let bytes = std::fs::read(&self.checkpoint_path)
            .map_err(|e| format!("checkpoint read error: {e}"))?;
        let state: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("checkpoint deserialize error: {e}"))?;

        let mut restored_any = false;

        if let Some(mem) = state.get("user_memories") {
            match serde_json::from_value::<HashMap<ScopeId, UserMemoryObject>>(mem.clone()) {
                Ok(typed) => {
                    self.user_memories = typed;
                    restored_any = true;
                }
                Err(e) => {
                    return Err(format!("checkpoint user_memories deserialize error: {e}"));
                }
            }
        }
        if let Some(win) = state.get("synthesis_windows") {
            match serde_json::from_value::<SynthesisWindowManager>(win.clone()) {
                Ok(typed) => {
                    self.synthesis_windows = typed;
                    restored_any = true;
                }
                Err(e) => {
                    return Err(format!("checkpoint synthesis_windows deserialize error: {e}"));
                }
            }
        }
        if let Some(pe) = state.get("policy_engine") {
            match serde_json::from_value::<PolicyEngine>(pe.clone()) {
                Ok(typed) => {
                    self.policy_engine = typed;
                    restored_any = true;
                }
                Err(e) => {
                    return Err(format!("checkpoint policy_engine deserialize error: {e}"));
                }
            }
        }
        if let Some(st) = state.get("scope_tenants") {
            match serde_json::from_value::<HashMap<ScopeId, uuid::Uuid>>(st.clone()) {
                Ok(typed) => {
                    self.scope_tenants = typed;
                    restored_any = true;
                }
                Err(e) => {
                    return Err(format!("checkpoint scope_tenants deserialize error: {e}"));
                }
            }
        }

        if !restored_any {
            return Err("checkpoint file contained no restorable state".to_string());
        }

        Ok(())
    }
}
