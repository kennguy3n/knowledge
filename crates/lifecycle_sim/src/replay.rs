//! Replay engine: drives the substrate turn-by-turn and collects results.

use std::collections::HashMap;

use evidence_store::{EvidenceId, ImportanceClass, ScopeId};
use serde::{Deserialize, Serialize};

use crate::dataset::{ScalePreset, SimConfig, WorldDataset};
use crate::drivers::{DriverKind, LifecycleDriver};
use crate::export::ExportData;
use crate::verify::{
    build_turn_verification, verify_checkpoint_restore, verify_concept_graph_empty,
    verify_cross_scope_isolation, verify_forget, verify_health_check, verify_ingest,
    verify_language_tag, verify_memory_lifecycle, verify_observations, verify_other_tenants_unaffected,
    verify_reasoning, verify_retrieval, verify_synthesis, verify_synthesis_status,
    verify_tombstone_persistence, Assertion, TurnVerification,
};

/// Summary of the simulation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimSummary {
    /// Total turns replayed.
    pub total_turns: usize,
    /// Total tenants.
    pub total_tenants: usize,
    /// Total users.
    pub total_users: usize,
    /// Total scopes.
    pub total_scopes: usize,
    /// Fraction of turns that passed all assertions.
    pub pass_rate: f64,
    /// Number of failed assertions.
    pub failed_assertions: usize,
    /// Total assertions run.
    pub total_assertions: usize,
    /// Duration in seconds.
    pub duration_secs: f64,
}

/// Per-tenant results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerTenantResult {
    /// Tenant ID.
    pub tenant_id: uuid::Uuid,
    /// Turns in this tenant.
    pub turns: usize,
    /// Pass rate.
    pub pass_rate: f64,
}

/// Per-language results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerLanguageResult {
    /// Language tag.
    pub language: String,
    /// Message count.
    pub messages: usize,
    /// Observations per message.
    pub obs_per_msg: f64,
    /// Pass rate.
    pub pass_rate: f64,
}

/// Per-scenario results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerScenarioResult {
    /// Scenario ID.
    pub scenario: String,
    /// Instance count.
    pub instances: usize,
    /// Pass rate.
    pub pass_rate: f64,
}

/// The complete simulation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimReport {
    /// Run configuration.
    pub run_config: RunConfig,
    /// Summary statistics.
    pub summary: SimSummary,
    /// Per-tenant breakdown.
    pub per_tenant: Vec<PerTenantResult>,
    /// Per-language breakdown.
    pub per_language: Vec<PerLanguageResult>,
    /// Per-scenario breakdown.
    pub per_scenario: Vec<PerScenarioResult>,
    /// All turn verifications.
    pub turn_verifications: Vec<TurnVerification>,
    /// Failures (first N for readability).
    pub failures: Vec<FailureEntry>,
    /// Captured raw data for CSV/Excel export. Not serialized to JSON.
    #[serde(skip)]
    pub(crate) export_data: ExportData,
}

/// Run configuration recorded in the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    /// Preset name.
    pub preset: String,
    /// Scale (message count).
    pub scale: String,
    /// Driver kind.
    pub driver: String,
    /// RNG seed.
    pub seed: u64,
}

/// A failure entry for the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    /// Turn index.
    pub turn: usize,
    /// Scope ID.
    pub scope: ScopeId,
    /// Assertion name.
    pub assertion: String,
    /// Expected.
    pub expected: String,
    /// Actual.
    pub actual: String,
}

/// Run the full simulation.
pub fn run_simulation(
    preset: ScalePreset,
    driver_kind: DriverKind,
    seed: u64,
    output_dir: Option<&str>,
) -> SimReport {
    let mut config = preset.config();
    config.seed = seed;
    run_simulation_with_config(config, driver_kind, seed, output_dir, false)
}

/// Run the simulation with a custom `SimConfig`.
/// If `resume` is true, the driver will attempt to restore from a
/// checkpoint before running the simulation.
pub fn run_simulation_with_config(
    config: SimConfig,
    driver_kind: DriverKind,
    seed: u64,
    output_dir: Option<&str>,
    resume: bool,
) -> SimReport {
    let config = {
        let mut c = config;
        c.seed = seed;
        c
    };
    let dataset = crate::dataset::generate_dataset(config.clone());
    let start = std::time::Instant::now();

    let report = match driver_kind {
        DriverKind::RustNative => {
            let dir = tempfile::tempdir().expect("tempdir");
            let db_path = dir.path().join("lifecycle_sim.db");
            let mut driver = crate::drivers::rust_native::RustNativeDriver::new(db_path);
            driver.configure_for_world(&dataset.world);
            if resume {
                match driver.restore() {
                    Ok(()) => eprintln!("[lifecycle_sim] Restored from checkpoint"),
                    Err(e) => eprintln!("[lifecycle_sim] No checkpoint to restore: {e}"),
                }
            }
            run_replay(driver, &dataset, &config, driver_kind, seed)
        }
        #[cfg(feature = "http-driver")]
        DriverKind::HttpGateway => {
            let driver = crate::drivers::http_gateway::HttpGatewayDriver::new(
                std::env::var("SUBSTRATE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string()),
            );
            run_replay(driver, &dataset, &config, driver_kind, seed)
        }
    };

    let duration = start.elapsed().as_secs_f64();
    let mut report = report;
    report.summary.duration_secs = duration;

    // Write reports if output_dir is specified.
    if let Some(dir) = output_dir {
        let _ = crate::report::write_reports(&report, dir);
    }

    report
}

fn run_replay<D: LifecycleDriver>(
    mut driver: D,
    dataset: &WorldDataset,
    config: &SimConfig,
    driver_kind: DriverKind,
    seed: u64,
) -> SimReport {
    let mut turn_verifications = Vec::with_capacity(dataset.turns.len());
    let mut total_failed = 0usize;
    let mut total_assertions = 0usize;

    // Track per-tenant, per-language, per-scenario stats.
    let mut tenant_stats: HashMap<uuid::Uuid, (usize, usize)> = HashMap::new(); // (total, passed)
    let mut lang_stats: HashMap<String, (usize, usize, usize)> = HashMap::new(); // (msgs, obs, passed)
    let mut scenario_stats: HashMap<String, (usize, usize)> = HashMap::new(); // (instances, passed)

    // Track evidence IDs per scope for retrieval verification.
    // (evidence_id, content, is_noise)
    let mut scope_evidence: HashMap<ScopeId, Vec<(EvidenceId, String, bool)>> = HashMap::new();

    // Captured raw data for CSV/Excel validation export.
    let mut export_data = ExportData::default();

    // Track which scopes to forget (one per tenant, at the end).
    let scopes_to_forget: Vec<ScopeId> = dataset
        .world
        .tenants
        .iter()
        .filter_map(|t| t.scopes.first().map(|s| s.scope_id))
        .collect();

    let total_turns = dataset.turns.len();

    for (idx, turn) in dataset.turns.iter().enumerate() {
        // 1. Ingest.
        let body_bytes = if let Some(ref media) = turn.media {
            // For media: ingest the raw binary bytes.
            media.bytes.clone()
        } else {
            turn.content.as_bytes().to_vec()
        };

        let ingest_result = driver
            .ingest(turn.scope_id, &body_bytes, &turn.source_ref, turn.importance)
            .unwrap_or_else(|e| {
                panic!("ingest failed at turn {idx}: {e}")
            });

        let body_readable = turn.importance != ImportanceClass::Noise
            && driver.read_body(ingest_result.evidence_id).is_ok();

        let mut assertions = verify_ingest(idx, turn, &ingest_result, body_readable);

        // Language tag verification.
        assertions.extend(verify_language_tag(turn));

        // 2. Extract observations (for text content only).
        if turn.media.is_none() && turn.importance != ImportanceClass::Noise {
            let observations = driver
                .extract_observations(&turn.content, turn.scope_id)
                .unwrap_or_default();

            let obs_count = observations.len();
            assertions.extend(verify_observations(turn, &observations));

            // Capture observations for CSV/Excel validation export.
            for obs in &observations {
                let expected = turn.expected_obs_type.as_str();
                let type_match = obs.observation_type.as_str() == expected;
                export_data.observations.push(crate::export::ObservationRow {
                    turn_idx: idx,
                    scope_id: obs.scope_id.to_string(),
                    scenario_id: turn.scenario_id.clone(),
                    language: turn.language.clone(),
                    evidence_id: ingest_result.evidence_id.to_string(),
                    obs_type: obs.observation_type.as_str().to_string(),
                    content: obs.content.clone(),
                    expected_obs_type: expected.to_string(),
                    type_match,
                });
            }

            // Update language stats.
            let entry = lang_stats
                .entry(turn.language.clone())
                .or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 += obs_count;
        } else {
            // Still track the message in language stats.
            let entry = lang_stats
                .entry(turn.language.clone())
                .or_insert((0, 0, 0));
            entry.0 += 1;
        }

        // Track evidence for later retrieval checks.
        if turn.importance != ImportanceClass::Noise {
            scope_evidence
                .entry(turn.scope_id)
                .or_default()
                .push((ingest_result.evidence_id, turn.content.clone(), false));
        } else {
            scope_evidence
                .entry(turn.scope_id)
                .or_default()
                .push((ingest_result.evidence_id, turn.content.clone(), true));
        }

        // 3. Periodic retrieval check (every 50 turns per scope).
        let scope_turns = scope_evidence.get(&turn.scope_id).map(|v| v.len()).unwrap_or(0);
        if scope_turns > 0 && scope_turns % 50 == 0 && !turn.expected_retrieval_terms.is_empty() {
            let query = turn.expected_retrieval_terms[0].as_str();
            let hits = driver.query(turn.scope_id, query, 10).unwrap_or_default();

            // Check that at least one of our evidence IDs appears in hits.
            let our_evidence = scope_evidence.get(&turn.scope_id).cloned().unwrap_or_default();
            let our_ids: Vec<EvidenceId> = our_evidence.iter().map(|(id, _, _)| *id).collect();
            let any_found = hits.iter().any(|h| our_ids.contains(&h.evidence_id));
            assertions.extend(verify_retrieval(turn, &hits, any_found));

            // Cross-scope isolation: query a different scope and verify
            // none of OUR evidence IDs leak into the other scope's results.
            let other_scope = dataset
                .world
                .tenants
                .iter()
                .flat_map(|t| t.scopes.iter())
                .find(|s| s.scope_id != turn.scope_id)
                .map(|s| s.scope_id);
            if let Some(other) = other_scope {
                let other_hits = driver.query(other, query, 10).unwrap_or_default();
                let leaked: Vec<EvidenceId> = other_hits
                    .iter()
                    .filter(|h| our_ids.contains(&h.evidence_id))
                    .map(|h| h.evidence_id)
                    .collect();
                assertions.extend(verify_cross_scope_isolation(
                    turn.scope_id,
                    other,
                    query,
                    &leaked,
                ));
            }
        }

        // 4. Build turn verification.
        let tv = build_turn_verification(idx, turn, assertions);
        total_assertions += tv.assertions.len();
        if !tv.passed {
            total_failed += tv.assertions.iter().filter(|a| !a.passed).count();
        }

        // Update tenant stats.
        let tenant_entry = tenant_stats
            .entry(turn.tenant_id)
            .or_insert((0, 0));
        tenant_entry.0 += 1;
        if tv.passed {
            tenant_entry.1 += 1;
        }

        // Update scenario stats.
        let scenario_key = format!("{}:{}", turn.scenario_id, turn.scenario_instance);
        let scenario_entry = scenario_stats
            .entry(scenario_key)
            .or_insert((0, 0));
        scenario_entry.0 += 1;
        if tv.passed {
            scenario_entry.1 += 1;
        }

        // Update language pass stats.
        let lang_entry = lang_stats
            .entry(turn.language.clone())
            .or_insert((0, 0, 0));
        if tv.passed {
            lang_entry.2 += 1;
        }

        turn_verifications.push(tv);

        // 5. Periodic synthesis + memory + reasoning verification (every 200 turns).
        if idx > 0 && idx % 200 == 0 && turn.importance != ImportanceClass::Noise {
            let mut phase4_assertions = Vec::new();

            // Synthesis.
            if let Ok(synth_result) = driver.trigger_synthesis(turn.scope_id) {
                phase4_assertions.extend(verify_synthesis(turn.scope_id, &synth_result));
                if let Ok(status_windows) = driver.synthesis_status(turn.scope_id) {
                    phase4_assertions.extend(verify_synthesis_status(turn.scope_id, &status_windows));

                    // Capture synthesis window for CSV export.
                    for w in status_windows {
                        export_data.synthesis_windows.push(crate::export::SynthesisWindowRow {
                            scope_id: turn.scope_id.to_string(),
                            window_id: w.window_id,
                            status: w.status,
                            opened_at: None,
                            closed_at: None,
                            recap: String::new(),
                            decisions: String::new(),
                            open_questions: String::new(),
                            active_tasks: String::new(),
                        });
                    }
                }
            }

            // Memory lifecycle.
            if let Ok(mem_id) = driver.add_memory_observation(
                turn.scope_id,
                &turn.expected_obs_type,
                &turn.content,
            ) {
                let list_after_add = driver.list_memories(turn.scope_id).unwrap_or_default();
                let _ = driver.pin_memory(&mem_id);
                let list_after_pin = driver.list_memories(turn.scope_id).unwrap_or_default();
                let _ = driver.unpin_memory(&mem_id);
                let _decay = driver.run_decay_sweep(turn.scope_id).unwrap_or_else(|_| {
                    crate::drivers::DecayResult {
                        archived: 0,
                        deleted: 0,
                        resurrected: 0,
                        promoted_to_reinforced: 0,
                        promoted_to_consolidated: 0,
                        promoted_to_canonical: 0,
                    }
                });
                let list_after_decay = driver.list_memories(turn.scope_id).unwrap_or_default();
                phase4_assertions.extend(verify_memory_lifecycle(
                    turn.scope_id,
                    &mem_id,
                    &list_after_add,
                    &list_after_pin,
                    &list_after_decay,
                ));

                // Note: memory state and retention score snapshots are
                // captured once at the end of the replay to avoid
                // duplication across phase-4 turns.
            }

            // Reasoning.
            let contradictions = driver.reasoning_contradictions(turn.scope_id).unwrap_or_else(|_| {
                crate::drivers::ContradictionResult { count: 0 }
            });
            let drift = driver.reasoning_drift(turn.scope_id).unwrap_or_else(|_| {
                crate::drivers::DriftResult { count: 0 }
            });
            let explain = driver.reasoning_explain_query(&turn.content).unwrap_or_else(|_| {
                crate::drivers::ExplainQueryResult {
                    query_class: String::new(),
                    step_count: 0,
                    steps: Vec::new(),
                }
            });
            phase4_assertions.extend(verify_reasoning(
                turn.scope_id,
                &contradictions,
                &drift,
                &explain,
            ));

            let phase4_passed = phase4_assertions.iter().all(|a| a.passed);
            let phase4_tv = TurnVerification {
                turn_idx: idx,
                scope_id: turn.scope_id,
                scenario_id: turn.scenario_id.clone(),
                language: turn.language.clone(),
                assertions: phase4_assertions,
                passed: phase4_passed,
            };
            total_assertions += phase4_tv.assertions.len();
            total_failed += phase4_tv.assertions.iter().filter(|a| !a.passed).count();
            turn_verifications.push(phase4_tv);
        }

        // 6. Progress output.
        if idx > 0 && idx % 1000 == 0 {
            eprintln!("[lifecycle_sim] {idx}/{total_turns} turns processed...");
        }
    }

    // Capture a final, deduplicated snapshot of memory states and
    // retention scores for CSV/Excel validation export. This is done
    // once after all turns instead of inside every phase-4 turn to
    // avoid duplication and to use the real retention components
    // computed by the driver.
    for scope in dataset
        .world
        .tenants
        .iter()
        .flat_map(|t| t.scopes.iter())
        .map(|s| s.scope_id)
    {
        if let Ok(records) = driver.list_memories(scope) {
            for rec in records {
                export_data.memory_states.push(crate::export::MemoryStateRow {
                    scope_id: rec.scope_id.to_string(),
                    memory_id: rec.id.clone(),
                    state: rec.state.clone(),
                    superseded_by: rec.superseded_by.clone(),
                    pin_count: rec.pin_count,
                    retrieval_count: rec.retrieval_count,
                    corroboration_count: rec.corroboration_count,
                    sensitivity_class: rec.sensitivity_class.clone(),
                    content: rec.content.clone().unwrap_or_default(),
                    archivable: rec.archivable,
                });
                export_data.retention_scores.push(crate::export::RetentionScoreRow {
                    scope_id: rec.scope_id.to_string(),
                    memory_id: rec.id,
                    total: rec.retention_score,
                    pinning: rec.pinning,
                    retrieval_frequency: rec.retrieval_frequency,
                    corroboration: rec.corroboration,
                    contradiction: rec.contradiction,
                    age: rec.age,
                    non_use: rec.non_use,
                });
            }
        }
    }

    // 7. Cryptographic forgetting for selected scopes.
    // Find a scope from a different tenant for the "other tenants unaffected" check.
    let other_tenant_scope = dataset
        .world
        .tenants
        .iter()
        .flat_map(|t| t.scopes.iter())
        .find(|s| !scopes_to_forget.contains(&s.scope_id))
        .map(|s| s.scope_id);

    for &scope in &scopes_to_forget {
        // Record other tenant's evidence count before forget (per-scope, not global).
        let other_count_before = other_tenant_scope
            .and_then(|os| driver.evidence_count_for_scope(os).ok())
            .unwrap_or(0);

        let _ = driver.forget_scope(scope);

        // Verify: try to read a body from this scope.
        // Use a non-noise evidence ID, since noise messages are stored
        // inline and remain readable after DEK deletion.
        let evidence_ids = scope_evidence.get(&scope).cloned().unwrap_or_default();
        let body_result = if let Some((eid, _, false)) = evidence_ids.iter().find(|(_, _, is_noise)| !is_noise) {
            driver.read_body(*eid)
        } else {
            Err("no non-noise evidence to test".to_string())
        };

        let fts_result = driver.search_fts(scope, "test", 100).unwrap_or_default();
        let tombstones = driver.load_forgotten_scopes().unwrap_or_default();

        let forget_assertions = verify_forget(scope, &body_result, &fts_result, &tombstones);

        // Reopen and verify tombstone persistence.
        let _ = driver.reopen();
        let tombstones_after = driver.load_forgotten_scopes().unwrap_or_default();
        let persist_assertions = verify_tombstone_persistence(scope, &tombstones_after);

        // Concept graph should be empty for forgotten scope.
        let graph = driver.get_concept_graph(scope).unwrap_or_else(|_| {
            crate::drivers::ConceptGraphSnapshot {
                node_count: 0,
                edge_count: 0,
                node_states: Vec::new(),
            }
        });
        let graph_assertions = verify_concept_graph_empty(scope, &graph);

        // Other tenants unaffected: verify per-scope evidence count
        // for the other scope did not change.
        let other_assertions = if let Some(other) = other_tenant_scope {
            let other_count_after = driver.evidence_count_for_scope(other).unwrap_or(0);
            verify_other_tenants_unaffected(scope, other, other_count_before, other_count_after)
        } else {
            Vec::new()
        };

        // Build turn verification with correct passed flag.
        let all_assertions: Vec<Assertion> = forget_assertions
            .into_iter()
            .chain(persist_assertions)
            .chain(graph_assertions)
            .chain(other_assertions)
            .collect();
        let all_passed = all_assertions.iter().all(|a| a.passed);
        total_assertions += all_assertions.len();
        total_failed += all_assertions.iter().filter(|a| !a.passed).count();

        turn_verifications.push(TurnVerification {
            turn_idx: total_turns,
            scope_id: scope,
            scenario_id: "forget".to_string(),
            language: "n/a".to_string(),
            assertions: all_assertions,
            passed: all_passed,
        });
    }

    // 8. Health check + checkpoint/restore verification.
    {
        let mut final_assertions = Vec::new();

        // Health check.
        if let Ok(health) = driver.health_check() {
            final_assertions.extend(verify_health_check(&health));
        }

        // Checkpoint/restore round-trip.
        let checkpoint_scope = other_tenant_scope;
        if let Some(cp_scope) = checkpoint_scope {
            let memories_before = driver.list_memories(cp_scope).unwrap_or_default();
            let _ = driver.checkpoint();
            let _ = driver.restore();
            let memories_after = driver.list_memories(cp_scope).unwrap_or_default();
            final_assertions.extend(verify_checkpoint_restore(&memories_before, &memories_after));
        }

        if !final_assertions.is_empty() {
            total_assertions += final_assertions.len();
            total_failed += final_assertions.iter().filter(|a| !a.passed).count();
            turn_verifications.push(TurnVerification {
                turn_idx: total_turns + 1,
                scope_id: checkpoint_scope.unwrap_or_else(|| scopes_to_forget.first().copied().unwrap_or_else(ScopeId::new_v4)),
                scenario_id: "health_check".to_string(),
                language: "n/a".to_string(),
                assertions: final_assertions.clone(),
                passed: final_assertions.iter().all(|a| a.passed),
            });
        }
    }

    // Build summary.
    let passed_turns = turn_verifications.iter().filter(|tv| tv.passed).count();
    let pass_rate = if turn_verifications.is_empty() {
        1.0
    } else {
        passed_turns as f64 / turn_verifications.len() as f64
    };

    let total_users = dataset
        .world
        .tenants
        .iter()
        .map(|t| t.users.len())
        .sum::<usize>();
    let total_scopes = dataset
        .world
        .tenants
        .iter()
        .map(|t| t.scopes.len())
        .sum::<usize>();

    // Build per-tenant results.
    let per_tenant: Vec<PerTenantResult> = dataset
        .world
        .tenants
        .iter()
        .map(|t| {
            let (total, passed) = tenant_stats.get(&t.id).copied().unwrap_or((0, 0));
            PerTenantResult {
                tenant_id: t.id,
                turns: total,
                pass_rate: if total > 0 {
                    passed as f64 / total as f64
                } else {
                    1.0
                },
            }
        })
        .collect();

    // Build per-language results.
    let per_language: Vec<PerLanguageResult> = lang_stats
        .iter()
        .map(|(lang, (msgs, obs, passed))| PerLanguageResult {
            language: lang.clone(),
            messages: *msgs,
            obs_per_msg: if *msgs > 0 {
                *obs as f64 / *msgs as f64
            } else {
                0.0
            },
            pass_rate: if *msgs > 0 {
                *passed as f64 / *msgs as f64
            } else {
                1.0
            },
        })
        .collect();

    // Build per-scenario results.
    let per_scenario: Vec<PerScenarioResult> = {
        let mut scenario_map: HashMap<String, (usize, usize)> = HashMap::new();
        for (key, (total, passed)) in &scenario_stats {
            let scenario_id = key.split(':').next().unwrap_or(key).to_string();
            let entry = scenario_map.entry(scenario_id).or_insert((0, 0));
            entry.0 += total;
            entry.1 += passed;
        }
        scenario_map
            .into_iter()
            .map(|(scenario, (total, passed))| PerScenarioResult {
                scenario,
                instances: total,
                pass_rate: if total > 0 {
                    passed as f64 / total as f64
                } else {
                    1.0
                },
            })
            .collect()
    };

    // Collect failures (first 100).
    let failures: Vec<FailureEntry> = turn_verifications
        .iter()
        .flat_map(|tv| {
            tv.assertions.iter().filter(|a| !a.passed).map(|a| {
                FailureEntry {
                    turn: tv.turn_idx,
                    scope: tv.scope_id,
                    assertion: a.name.clone(),
                    expected: "pass".to_string(),
                    actual: a.detail.clone().unwrap_or_default(),
                }
            })
        })
        .take(100)
        .collect();

    // Derive preset name from config for the report.
    let preset_name = match config.target_messages {
        10_000 => "quick",
        100_000 => "standard",
        1_000_000 => "stress",
        _ => "custom",
    };
    let scale_label = if config.target_messages >= 1_000_000 {
        format!("{}M", config.target_messages / 1_000_000)
    } else {
        format!("{}K", config.target_messages / 1000)
    };

    SimReport {
        run_config: RunConfig {
            preset: preset_name.to_string(),
            scale: scale_label,
            driver: format!("{driver_kind:?}"),
            seed,
        },
        summary: SimSummary {
            total_turns: turn_verifications.len(),
            total_tenants: dataset.world.tenants.len(),
            total_users,
            total_scopes,
            pass_rate,
            failed_assertions: total_failed,
            total_assertions,
            duration_secs: 0.0, // Set by caller
        },
        per_tenant,
        per_language,
        per_scenario,
        turn_verifications,
        failures,
        export_data,
    }
}
