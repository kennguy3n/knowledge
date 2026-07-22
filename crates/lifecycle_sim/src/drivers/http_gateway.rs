//! HTTP gateway driver: drives the substrate through the loopback HTTP API.
//!
//! Requires the `http-driver` feature and a running substrate server.

use evidence_store::{EvidenceId, ImportanceClass, ScopeId};
use observation_engine::{Observation, ObservationExtractor, LexiconExtractor};
use serde::{Deserialize, Serialize};

use super::{
    ConceptGraphSnapshot, ContradictionResult, DecayResult, ExplainQueryResult, HealthCheck,
    IngestResult, LifecycleDriver, MemoryRecord, QueryHit, SynthesisResult, DriftResult,
};

/// JSON body for `POST /ingest`.
#[derive(Serialize)]
struct IngestBody {
    scope_id: String,
    body: String,
    source: String,
    importance: String,
}

/// JSON response from `POST /ingest`.
#[derive(Deserialize)]
struct IdResponse {
    id: String,
}

/// JSON body for `POST /query`.
#[derive(Serialize)]
struct QueryBody {
    scope_id: String,
    query_text: String,
    limit: u32,
}

/// JSON response from `POST /query`.
#[derive(Deserialize)]
struct QueryResult {
    evidence_id: String,
    score: f64,
}

/// JSON body for `POST /forget_scope`.
#[derive(Serialize)]
struct ForgetScopeBody {
    scope_id: String,
}

/// HTTP gateway driver that talks to a running substrate server.
pub struct HttpGatewayDriver {
    base_url: String,
    client: reqwest::blocking::Client,
    extractor: LexiconExtractor,
}

impl HttpGatewayDriver {
    /// Create a new driver pointing at `base_url` (e.g. `http://localhost:8080`).
    pub fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            extractor: LexiconExtractor::default(),
        }
    }

    fn importance_to_str(imp: ImportanceClass) -> &'static str {
        match imp {
            ImportanceClass::Critical => "Critical",
            ImportanceClass::Important => "Important",
            ImportanceClass::Useful => "Useful",
            ImportanceClass::Noise => "Noise",
        }
    }
}

impl LifecycleDriver for HttpGatewayDriver {
    fn ingest(
        &mut self,
        scope: ScopeId,
        body: &[u8],
        source: &str,
        importance: ImportanceClass,
    ) -> Result<IngestResult, String> {
        let body_str = String::from_utf8_lossy(body).to_string();
        let req = IngestBody {
            scope_id: scope.as_uuid().to_string(),
            body: body_str,
            source: source.to_string(),
            importance: Self::importance_to_str(importance).to_string(),
        };

        let resp: IdResponse = self
            .client
            .post(format!("{}/ingest", self.base_url))
            .json(&req)
            .send()
            .map_err(|e| format!("HTTP ingest request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP ingest status error: {e}"))?
            .json()
            .map_err(|e| format!("HTTP ingest parse error: {e}"))?;

        let evidence_id = EvidenceId(
            uuid::Uuid::parse_str(&resp.id).map_err(|e| format!("parse evidence id: {e}"))?,
        );

        Ok(IngestResult {
            evidence_id,
            storage_path: "http".to_string(),
        })
    }

    fn query(
        &mut self,
        scope: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<QueryHit>, String> {
        let req = QueryBody {
            scope_id: scope.as_uuid().to_string(),
            query_text: query.to_string(),
            limit: limit as u32,
        };

        let results: Vec<QueryResult> = self
            .client
            .post(format!("{}/query", self.base_url))
            .json(&req)
            .send()
            .map_err(|e| format!("HTTP query request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP query status error: {e}"))?
            .json()
            .map_err(|e| format!("HTTP query parse error: {e}"))?;

        Ok(results
            .into_iter()
            .filter_map(|r| {
                let uuid = uuid::Uuid::parse_str(&r.evidence_id).ok()?;
                Some(QueryHit {
                    evidence_id: EvidenceId(uuid),
                    score: r.score,
                })
            })
            .collect())
    }

    fn read_body(&self, id: EvidenceId) -> Result<Vec<u8>, String> {
        // GET /evidence/{id} returns the evidence record with decrypted body.
        let resp = self
            .client
            .get(format!(
                "{}/evidence/{}",
                self.base_url,
                id.as_uuid()
            ))
            .send()
            .map_err(|e| format!("HTTP get_evidence request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP get_evidence returned {}", resp.status()));
        }

        let json: serde_json::Value =
            resp.json().map_err(|e| format!("HTTP get_evidence parse error: {e}"))?;

        // The EvidenceRecord contains the decrypted body in its `body` field.
        let body = json
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or("no body field in evidence record")?;

        Ok(body.as_bytes().to_vec())
    }

    fn get_evidence(&self, id: EvidenceId) -> Result<Option<String>, String> {
        let resp = self
            .client
            .get(format!(
                "{}/evidence/{}",
                self.base_url,
                id.as_uuid()
            ))
            .send()
            .map_err(|e| format!("HTTP get_evidence request failed: {e}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !resp.status().is_success() {
            return Err(format!("HTTP get_evidence returned {}", resp.status()));
        }

        let json: serde_json::Value =
            resp.json().map_err(|e| format!("HTTP get_evidence parse error: {e}"))?;

        Ok(Some(serde_json::to_string_pretty(&json).unwrap_or_default()))
    }

    fn extract_observations(
        &mut self,
        text: &str,
        scope: ScopeId,
    ) -> Result<Vec<Observation>, String> {
        // Observation extraction runs client-side (same as RustNative driver).
        Ok(self.extractor.extract(text, scope))
    }

    fn search_fts(
        &self,
        scope: ScopeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EvidenceId>, String> {
        // Use the query endpoint and extract evidence IDs.
        let req = QueryBody {
            scope_id: scope.as_uuid().to_string(),
            query_text: query.to_string(),
            limit: limit as u32,
        };

        let results: Vec<QueryResult> = self
            .client
            .post(format!("{}/query", self.base_url))
            .json(&req)
            .send()
            .map_err(|e| format!("HTTP search_fts request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP search_fts status error: {e}"))?
            .json()
            .map_err(|e| format!("HTTP search_fts parse error: {e}"))?;

        Ok(results
            .into_iter()
            .filter_map(|r| {
                let uuid = uuid::Uuid::parse_str(&r.evidence_id).ok()?;
                Some(EvidenceId(uuid))
            })
            .collect())
    }

    fn evidence_count(&self) -> Result<usize, String> {
        // No direct endpoint for evidence count; use /internal/metrics.
        let resp = self
            .client
            .get(format!("{}/internal/metrics", self.base_url))
            .send()
            .map_err(|e| format!("HTTP metrics request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP metrics returned {}", resp.status()));
        }

        let json: serde_json::Value =
            resp.json().map_err(|e| format!("HTTP metrics parse error: {e}"))?;

        let count = json
            .get("evidence_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        Ok(count)
    }

    fn evidence_count_for_scope(&self, scope: ScopeId) -> Result<usize, String> {
        let resp = self
            .client
            .get(format!(
                "{}/internal/scope/{}/evidence-count",
                self.base_url,
                scope.as_uuid()
            ))
            .send()
            .map_err(|e| format!("HTTP scope evidence count request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!(
                "HTTP scope evidence count returned {}",
                resp.status()
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .map_err(|e| format!("HTTP scope evidence count parse error: {e}"))?;

        let count = json
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        Ok(count)
    }

    fn forget_scope(&mut self, scope: ScopeId) -> Result<(), String> {
        let req = ForgetScopeBody {
            scope_id: scope.as_uuid().to_string(),
        };

        let resp = self
            .client
            .post(format!("{}/forget_scope", self.base_url))
            .json(&req)
            .send()
            .map_err(|e| format!("HTTP forget_scope request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP forget_scope returned {}", resp.status()));
        }

        Ok(())
    }

    fn load_forgotten_scopes(&self) -> Result<Vec<ScopeId>, String> {
        // No dedicated endpoint for listing forgotten scopes.
        // In a real deployment this would query a management API.
        // For now, return empty — the verification framework will
        // note the absence and skip the tombstone check.
        Ok(Vec::new())
    }

    fn reopen(&mut self) -> Result<(), String> {
        // No-op for HTTP driver — the server manages its own lifecycle.
        Ok(())
    }

    // ── Synthesis ──────────────────────────────────────────────

    fn trigger_synthesis(&mut self, scope: ScopeId) -> Result<SynthesisResult, String> {
        let url = format!("{}/synthesis/trigger", self.base_url);
        let body = serde_json::json!({
            "scope_id": scope.as_uuid().to_string(),
            "trigger": "manual_user_action",
        });
        let resp = self.client.post(&url).json(&body).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP synthesis trigger returned {}", resp.status()));
        }
        let result: serde_json::Value = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(SynthesisResult {
            window_id: result.get("window_id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            status: result.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
        })
    }

    fn synthesis_status(&self, scope: ScopeId) -> Result<Vec<SynthesisResult>, String> {
        let url = format!("{}/synthesis/status/{}", self.base_url, scope.as_uuid());
        let resp = self.client.get(&url).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP synthesis status returned {}", resp.status()));
        }
        let windows: Vec<serde_json::Value> = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(windows.iter().map(|w| SynthesisResult {
            window_id: w.get("window_id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            status: w.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
        }).collect())
    }

    // ── Memory ─────────────────────────────────────────────────

    fn add_memory_observation(&mut self, scope: ScopeId, obs_type: &str, content: &str) -> Result<String, String> {
        let url = format!("{}/memory/add", self.base_url);
        let body = serde_json::json!({
            "scope_id": scope.as_uuid().to_string(),
            "observation_type": obs_type,
            "content": content,
        });
        let resp = self.client.post(&url).json(&body).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP memory add returned {}", resp.status()));
        }
        let result: serde_json::Value = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(result.get("id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string())
    }

    fn pin_memory(&mut self, id: &str) -> Result<(), String> {
        let url = format!("{}/memory/pin", self.base_url);
        let body = serde_json::json!({"id": id});
        let resp = self.client.post(&url).json(&body).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP pin returned {}", resp.status()));
        }
        Ok(())
    }

    fn unpin_memory(&mut self, id: &str) -> Result<(), String> {
        let url = format!("{}/memory/unpin", self.base_url);
        let body = serde_json::json!({"id": id});
        let resp = self.client.post(&url).json(&body).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP unpin returned {}", resp.status()));
        }
        Ok(())
    }

    fn list_memories(&self, scope: ScopeId) -> Result<Vec<MemoryRecord>, String> {
        let url = format!("{}/memory/list/{}", self.base_url, scope.as_uuid());
        let resp = self.client.get(&url).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP memory list returned {}", resp.status()));
        }
        let records: Vec<serde_json::Value> = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(records.iter().map(|r| MemoryRecord {
            id: r.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            scope_id: scope,
            state: r.get("state").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            observation_type: r.get("observation_type").and_then(|v| v.as_str()).map(|s| s.to_string()),
            content: r.get("content").and_then(|v| v.as_str()).map(|s| s.to_string()),
            pin_count: r.get("pin_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            retrieval_count: r.get("retrieval_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            corroboration_count: r.get("corroboration_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            sensitivity_class: r.get("sensitivity_class").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            superseded_by: r.get("superseded_by").and_then(|v| v.as_str()).map(|s| s.to_string()),
            archivable: r.get("archivable").and_then(|v| v.as_bool()).unwrap_or(false),
            retention_score: r.get("retention_score").and_then(|v| v.as_f64()).unwrap_or(0.0),
            pinning: r.get("pinning").and_then(|v| v.as_f64()).unwrap_or(0.0),
            retrieval_frequency: r.get("retrieval_frequency").and_then(|v| v.as_f64()).unwrap_or(0.0),
            corroboration: r.get("corroboration").and_then(|v| v.as_f64()).unwrap_or(0.0),
            contradiction: r.get("contradiction").and_then(|v| v.as_f64()).unwrap_or(0.0),
            age: r.get("age").and_then(|v| v.as_f64()).unwrap_or(0.0),
            non_use: r.get("non_use").and_then(|v| v.as_f64()).unwrap_or(0.0),
        }).collect())
    }

    fn run_decay_sweep(&mut self, scope: ScopeId) -> Result<DecayResult, String> {
        let url = format!("{}/memory/decay/{}", self.base_url, scope.as_uuid());
        let resp = self.client.post(&url).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP decay sweep returned {}", resp.status()));
        }
        let result: serde_json::Value = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(DecayResult {
            archived: result.get("archived").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            deleted: result.get("deleted").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            resurrected: result.get("resurrected").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            promoted_to_reinforced: result.get("promoted_to_reinforced").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            promoted_to_consolidated: result.get("promoted_to_consolidated").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            promoted_to_canonical: result.get("promoted_to_canonical").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        })
    }

    // ── Concept graph ──────────────────────────────────────────

    fn get_concept_graph(&self, scope: ScopeId) -> Result<ConceptGraphSnapshot, String> {
        let url = format!("{}/concept_graph/{}", self.base_url, scope.as_uuid());
        let resp = self.client.get(&url).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP concept graph returned {}", resp.status()));
        }
        let result: serde_json::Value = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(ConceptGraphSnapshot {
            node_count: result.get("node_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            edge_count: result.get("edge_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            node_states: result.get("node_states").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()).unwrap_or_default(),
        })
    }

    // ── Reasoning ──────────────────────────────────────────────

    fn reasoning_contradictions(&self, scope: ScopeId) -> Result<ContradictionResult, String> {
        let url = format!("{}/reasoning/contradictions/{}", self.base_url, scope.as_uuid());
        let resp = self.client.get(&url).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP contradictions returned {}", resp.status()));
        }
        let result: serde_json::Value = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(ContradictionResult {
            count: result.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        })
    }

    fn reasoning_drift(&self, scope: ScopeId) -> Result<DriftResult, String> {
        let url = format!("{}/reasoning/drift/{}", self.base_url, scope.as_uuid());
        let resp = self.client.get(&url).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP drift returned {}", resp.status()));
        }
        let result: serde_json::Value = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(DriftResult {
            count: result.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        })
    }

    fn reasoning_explain_query(&self, query: &str) -> Result<ExplainQueryResult, String> {
        let url = format!("{}/reasoning/explain", self.base_url);
        let body = serde_json::json!({"query": query});
        let resp = self.client.post(&url).json(&body).send().map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP explain query returned {}", resp.status()));
        }
        let result: serde_json::Value = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;
        Ok(ExplainQueryResult {
            query_class: result.get("query_class").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            step_count: result.get("step_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            steps: result.get("steps").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()).unwrap_or_default(),
        })
    }

    // ── Health & checkpointing ─────────────────────────────────

    fn health_check(&self) -> Result<HealthCheck, String> {
        let url = format!("{}/health", self.base_url);
        let resp = self.client.get(&url).send().map_err(|e| format!("HTTP error: {e}"))?;
        let healthy = resp.status().is_success();
        let evidence_count = self.evidence_count().unwrap_or(0);
        let forgotten = self.load_forgotten_scopes().unwrap_or_default();
        Ok(HealthCheck {
            healthy,
            evidence_count,
            forgotten_scopes: forgotten.len(),
        })
    }

    fn checkpoint(&self) -> Result<(), String> {
        // No-op for HTTP driver — the server manages its own persistence.
        Ok(())
    }

    fn restore(&mut self) -> Result<(), String> {
        // No-op for HTTP driver — the server manages its own persistence.
        Ok(())
    }
}
