# Knowledge Substrate End-to-End Demo Results

- Run started: 2026-05-08T15:37:04.019251195+00:00
- Total wall-clock: 138.744ms
- Synthetic messages: 51
- Assertions: 196 passed / 0 failed (pass rate 100.0%)

## Summary statistics

| Metric | Value |
|---|---|
| aead_round_trips | 1 |
| audit.action_types | 10 |
| audit.entries | 23 |
| channel_recap_objects | 2 |
| concept_canonical_seeds | 7 |
| concept_contradicted_total | 2 |
| concept_edges_total | 25 |
| concept_nodes_total | 60 |
| concept_superseded_total | 1 |
| connectors.events_total | 28 |
| connectors.exercised | 4 |
| connectors.subscriptions | 5 |
| connectors.webhook_events | 7 |
| domain_summary_objects | 1 |
| epoch_rotations | 2 |
| evidence_dedup_bodies | 2 |
| evidence_rows_body_table | 3 |
| evidence_rows_inline | 36 |
| evidence_rows_ring_buffer | 12 |
| evidence_rows_total | 39 |
| export_concepts_approved | 9 |
| export_profiles_created | 1 |
| export_simulations_run | 1 |
| export_views_rendered | 2 |
| hybrid_kem_round_trips | 1 |
| memory_canonical_after_forget | 8 |
| memory_canonicals | 9 |
| memory_decay_archived | 1 |
| memory_deleted_after_forget | 1 |
| memory_episodic_summaries | 5 |
| memory_seed_objects | 39 |
| memory_working_evicted | 8 |
| observations_promoted | 6 |
| observations_rejected_corroboration | 0 |
| observations_rejected_importance | 47 |
| observations_rejected_noise | 170 |
| observations_total | 223 |
| observations_type_claim | 0 |
| observations_type_decision | 19 |
| observations_type_entity | 150 |
| observations_type_fact | 39 |
| observations_type_question | 3 |
| observations_type_task | 12 |
| permission_checks_allowed | 10 |
| permission_checks_denied | 9 |
| permission_checks_total | 19 |
| permission_tuples_seeded | 10 |
| proposals_auto_promoted | 1 |
| proposals_manually_promoted | 2 |
| proposals_rejected | 2 |
| proposals_submitted | 5 |
| provenance_bundles_signed | 8 |
| scopes_forgotten | 1 |
| synthesis_hierarchy_rejections | 4 |
| tenant_summary_objects | 1 |

## Phases

### Phase 1: Evidence Ingestion (26.541ms)

| Stat | Value |
|---|---|
| messages | 51 |
| inline_rows | 36 |
| body_table_rows | 3 |
| ring_buffer_rows | 12 |
| dedup_body_rows | 2 |
| ring_buffer_bytes | 534 |
| class_critical | 13 |
| class_important | 11 |
| class_noise | 12 |
| class_useful | 15 |

- Stored evidence at /tmp/.tmpZbB8G0/evidence.sqlcipher (encrypted SQLCipher, master key derived in-process)

### Phase 2: Observation Extraction (1.100ms)

| Stat | Value |
|---|---|
| rows_processed | 39 |
| total_observations | 223 |
| promoted | 6 |
| rejected_below_importance | 47 |
| rejected_insufficient_corroboration | 0 |
| rejected_batch_too_noisy | 170 |
| type_claim | 0 |
| type_decision | 19 |
| type_entity | 150 |
| type_fact | 39 |
| type_question | 3 |
| type_task | 12 |

- LexiconExtractor (english_default) -> ChannelPromotionPolicy::default; corroboration scored against the full ingested batch.

### Phase 3: Memory Manager (75.294ms)

| Stat | Value |
|---|---|
| seed_objects | 39 |
| walked_to_canonical | 9 |
| pinned_score | 0.900 |
| candidate_score_unpinned | 0.250 |
| decay_swept_objects | 40 |
| decay_archived_candidates | 1 |
| decay_archived_superseded | 0 |
| decay_pre_sweep_total | 40 |
| canonical_after_forget | 8 |
| deleted_after_forget | 1 |
| working_memory_pre_evict | 8 |
| working_memory_evicted | 8 |
| working_memory_live_after | 0 |
| episodic_summaries | 5 |

- MemoryStateMachine + decay_sweep + UserMemoryObject CRUD + WorkingMemory (50ms TTL) + EpisodicMemory(StubSummarizer + SessionDetector::default).

### Phase 4: Concept Graph (24.773ms)

| Stat | Value |
|---|---|
| canonical_seeds | 7 |
| scope_local_clusters | 4 |
| scope_local_extra_nodes | 8 |
| scope_local_extra_edges | 4 |
| persisted_nodes_baseline | 58 |
| persisted_edges_baseline | 23 |
| total_nodes_in_memory | 60 |
| total_edges_in_memory | 25 |
| rehydrated_node_total | 58 |
| rehydrated_edge_total | 23 |
| rehydrated:user.alex:nodes | 2 |
| rehydrated:user.alex:edges | 1 |
| rehydrated:channel.platform:nodes | 2 |
| rehydrated:channel.platform:edges | 1 |
| rehydrated:channel.marketing:nodes | 2 |
| rehydrated:channel.marketing:edges | 1 |
| rehydrated:domain.engineering:nodes | 2 |
| rehydrated:domain.engineering:edges | 1 |
| rehydrated:tenant.acme:nodes | 50 |
| rehydrated:tenant.acme:edges | 19 |
| canonical_after_supersede | 49 |
| superseded_total | 1 |
| contradicted_total | 2 |
| edges:is_a | 10 |
| edges:part_of | 3 |
| edges:decided_by | 1 |
| edges:supersedes | 1 |
| edges:contradicts | 2 |
| edges:derived_from | 7 |
| edges:assigned_to | 1 |
| subgraph_views | 5 |
| subgraph_total_nodes | 60 |
| neighborhood_node_count | 9 |
| search_results | 3 |

- PersistentConceptGraph (SQLCipher) + IncrementalUpdateEngine + all 7 RelationTypes + visualization façade.
- Substrate-level canonical concepts persisted in tenant scope; per-scope intra-scope IsA clusters added so every dataset scope's load_scope round-trip is non-empty and scope-cohesive.

### Phase 5: Synthesis Pipeline (292.6µs)

| Stat | Value |
|---|---|
| channel_recaps | 2 |
| channel_pub_consume_failures | 0 |
| domain_summary_payload_bytes | 1632 |
| tenant_summary_payload_bytes | 1696 |
| scope_tiers_exercised | user, channel, domain, tenant |
| scope_total_messages | 23 |
| windows_complete | 4 |
| windows_pending | 1 |
| windows_in_progress | 0 |
| windows_failed | 0 |

- Channel (NoOpSynthesizer) -> Domain (ManagedEndpointSynthesizer) -> Tenant (ManagedEndpointSynthesizer) with AEAD publish/consume + four hierarchy-enforcement negative tests.

### Phase 6: Permission Service (55.1µs)

| Stat | Value |
|---|---|
| namespaces_registered | tenant, domain, channel, agent |
| relation_tuples | 10 |
| reachability_checks_total | 19 |
| reachability_checks_allowed | 10 |
| reachability_checks_denied | 9 |
| subjects | alice, bob, carol, dave, eve, synthesis_agent |

- Tenant→Domain→Channel hierarchy with two userset rewrites (domain#editor⇐tenant#admin, channel#viewer⇐domain#member) and the default Owner⇒Admin⇒Editor⇒Member⇒Viewer namespace chain. Verified positive paths, negative paths, outsider rejection, duplicate-insert, phantom-remove, upsert idempotence, and scope-local revocation.

### Phase 7: Crypto (709.9µs)

| Stat | Value |
|---|---|
| provenance_bundles_signed | 8 |
| provenance_verifications_passed | 8 |
| aead_round_trips | 1 |
| hybrid_kem_round_trips | 1 |
| scope_deks_destroyed | 1 |
| scopes_forgotten | 1 |
| epoch_dek_tombstones | 1 |
| current_epoch_after_rotations | 2 |
| epochs_listed_for_scope | 3 |

- Exercises TestSigner provenance round-trips (positive + wrong-key + tampered), hybrid X25519+ML-KEM-768 encap/decap (positive + wrong-recipient), XChaCha20-Poly1305 AEAD (positive + wrong-key + wrong-AAD + tampered), scope DEK destruction with cryptographic forgetting, single-epoch DEK destruction with tombstoning, and EpochManager rotation via force / size triggers.

### Phase 8: Export Plane (1.206ms)

| Stat | Value |
|---|---|
| canonical_concepts_total | 10 |
| controls_registered | 9 |
| approvals_minted | 9 |
| engine_approved_concepts | 9 |
| engine_rejected_concepts | 0 |
| view_concepts_only_concepts | 9 |
| view_with_summaries_summaries | 1 |
| simulator_included_concepts | 9 |
| simulator_excluded_concepts | 0 |
| simulator_included_summaries | 1 |
| simulator_size_estimate_bytes | 1191 |

- Deny-by-default ExportControlRegistry + PolicyEngine + PolicySimulator + ConceptApprovalWorkflow + ExportView render pipeline driven by Phase 4's canonical concepts.

### Phase 9: Agent Contract (5.125ms)

| Stat | Value |
|---|---|
| proposals_submitted | 5 |
| proposals_auto_promoted | 1 |
| proposals_manually_promoted | 2 |
| proposals_rejected | 2 |
| canonical_artifacts_derived | 3 (observation, concept, summary) |
| concept_corroboration_count | 2 |

- AgentProposal lifecycle exercised end-to-end: submission, duplicate-id refusal, corroboration bump, AutoPromotionPolicy match + miss, manual promote/reject, deterministic canonical artifact derivation, and TTL-expiry rejection.

### Phase 10: Reasoning Engine (3.044ms)

| Stat | Value |
|---|---|
| contradictions_flagged | 1 |
| contradictions_resolved | 1 |
| explore_paths | 16 |
| explore_visited | 17 |
| targeted_hits | 1 |
| got_thoughts | 5 |
| got_paths | 2 |
| got_confidence | 0.887 |
| communities_detected | 39 |
| community_levels | 2 |
| community_summaries | 44 |
| routed_summaries | 4 |
| plans_generated | 5 |
| plan_execute_succeeded | 5 |


### Phase 11: Connector Framework (124.7µs)

| Stat | Value |
|---|---|
| google_drive.initial_events | 3 |
| google_drive.incremental_events | 2 |
| google_drive.webhook_events | 1 |
| google_drive.subscriptions | 1 |
| jira.initial_events | 3 |
| jira.incremental_events | 2 |
| jira.webhook_events | 2 |
| jira.subscriptions | 1 |
| slack.initial_events | 3 |
| slack.incremental_events | 2 |
| slack.webhook_events | 1 |
| slack.subscriptions | 1 |
| email.initial_events | 4 |
| email.incremental_events | 2 |
| email.webhook_events | 3 |
| email.subscriptions | 2 |
| connectors.exercised | 4 |
| connectors.initial_events | 13 |
| connectors.incremental_events | 8 |
| connectors.webhook_events | 7 |
| connectors.subscriptions | 5 |
| connectors.events_total | 28 |


### Phase 12: Audit Service (13.7µs)

| Stat | Value |
|---|---|
| by_action.canonical_promotion | 2 |
| by_action.member_provisioned | 1 |
| by_action.policy_change | 2 |
| by_action.key_destruction | 2 |
| by_action.export_rendered | 1 |
| by_action.export_simulated | 1 |
| by_action.agent_proposal_submitted | 8 |
| by_action.agent_proposal_promoted | 3 |
| by_action.agent_proposal_rejected | 2 |
| by_action.tenant_lifecycle | 1 |
| scope_query.tenant.hits | 6 |
| action_query.agent_proposal_promoted.hits | 3 |
| time_query.since_lifecycle.hits | 1 |
| time_query.until_lifecycle.hits | 23 |
| actor_query.user.hits | 1 |
| composite_query.lifecycle.hits | 1 |
| audit_log.entries | 23 |
| audit_log.action_types | 10 |
| queries.executed | 5 |

- audit log carries 23 entries spanning 10 distinct action types
- demo-run-completed lifecycle entry id = 1da89647-92ed-4ca3-adc7-9daabc0e8e34
- demo run completed at 2026-05-08T15:37:04.157989909+00:00 UTC

## Benchmarks (per-operation timings)

| Operation | N | Total | Per-op |
|---|---|---|---|
| evidence_ingest_per_message | 51 | 22.460ms | 440.4µs |
| observation_extract_per_row | 39 | 1.090ms | 27.9µs |
| memory_state_machine_ops | 66 | 75.281ms | 1.141ms |
| concept_graph_propagations | 5 | 1.025ms | 205.0µs |
| synthesis_channel_tier | 2 | 125.6µs | 62.8µs |
| synthesis_domain_tier | 1 | 2.5µs | 2.5µs |
| synthesis_tenant_tier | 1 | 1.9µs | 1.9µs |
| permission_reachability_checks | 19 | 36.4µs | 1.9µs |
| provenance_sign_then_verify | 8 | 51.1µs | 6.4µs |
| hybrid_kem_encap_decap | 1 | 387.2µs | 387.2µs |
| aead_encrypt_decrypt | 1 | 5.0µs | 5.0µs |
| export_registry_inserts | 9 | 3.3µs | 0.4µs |
| export_concept_approvals | 9 | 18.3µs | 2.0µs |
| export_engine_evaluate | 1 | 2.3µs | 2.3µs |
| export_render_concepts_only | 1 | 3.0µs | 3.0µs |
| export_render_with_summaries | 1 | 4.5µs | 4.5µs |
| export_policy_simulate | 1 | 11.7µs | 11.7µs |
| export_audit_writes | 2 | 3.5µs | 1.7µs |
| agent_proposal_submits | 5 | 13.1µs | 2.6µs |
| agent_review_calls | 4 | 2.1µs | 0.5µs |
| agent_canonical_derivations | 4 | 2.0µs | 0.5µs |
| agent_audit_writes | 5 | 8.3µs | 1.7µs |
| phase10.contradiction.scan | 1 | 280.8µs | 280.8µs |
| phase10.contradiction.adjudicate | 1 | 6.8µs | 6.8µs |
| phase10.traversal | 2 | 22.0µs | 11.0µs |
| phase10.got.execute | 5 | 15.9µs | 3.2µs |
| phase10.community.detect_summarise | 44 | 214.6µs | 4.9µs |
| phase10.community.route | 1 | 62.4µs | 62.4µs |
| phase10.planner.plan | 5 | 3.7µs | 0.7µs |
| phase10.planner.execute | 5 | 1.0µs | 0.2µs |
| phase11.google_drive.initial_sync | 3 | 0.6µs | 0.2µs |
| phase11.google_drive.incremental_sync | 2 | 1.0µs | 0.5µs |
| phase11.google_drive.webhook | 1 | 11.3µs | 11.3µs |
| phase11.jira.initial_sync | 3 | 1.3µs | 0.4µs |
| phase11.jira.incremental_sync | 2 | 1.1µs | 0.5µs |
| phase11.jira.webhook | 2 | 19.5µs | 9.7µs |
| phase11.slack.initial_sync | 3 | 2.8µs | 0.9µs |
| phase11.slack.incremental_sync | 2 | 2.0µs | 1.0µs |
| phase11.slack.webhook | 1 | 5.0µs | 5.0µs |
| phase11.email.gmail.initial_sync | 2 | 1.1µs | 0.5µs |
| phase11.email.gmail.incremental_sync | 1 | 1.1µs | 1.1µs |
| phase11.email.gmail.webhook | 2 | 2.8µs | 1.4µs |
| phase11.email.graph.initial_sync | 2 | 0.6µs | 0.3µs |
| phase11.email.graph.incremental_sync | 1 | 0.6µs | 0.6µs |
| phase11.email.graph.webhook | 1 | 5.3µs | 5.3µs |
| phase12.audit.query_by_scope | 6 | 0.8µs | 0.1µs |
| phase12.audit.query_by_action | 3 | 0.3µs | 0.1µs |
| phase12.audit.query_by_time_range | 24 | 0.2µs | 0.0µs |
| phase12.audit.query_by_actor | 1 | 0.2µs | 0.2µs |
| phase12.audit.composite_query | 1 | 0.3µs | 0.3µs |

## Assertions

| Phase | Assertion | Status | Detail |
|---|---|---|---|
| phase01_evidence | evidence rows match inline+body_table ingest count | PASS |  |
| phase01_evidence | at least one inline row was created | PASS |  |
| phase01_evidence | at least one body-table row was created | PASS |  |
| phase01_evidence | at least one ring-buffer row was created | PASS |  |
| phase01_evidence | ring buffer length matches noise count | PASS |  |
| phase01_evidence | body_store dedup compresses duplicate long bodies | PASS |  |
| phase01_evidence | ring buffer holds bytes | PASS |  |
| phase01_evidence | all four scope tiers contributed evidence | PASS |  |
| phase02_observation | at least one observation extracted per non-noise row on average | PASS |  |
| phase02_observation | extractor produced at least one decision | PASS |  |
| phase02_observation | extractor produced at least one task | PASS |  |
| phase02_observation | extractor produced at least one fact | PASS |  |
| phase02_observation | extractor produced at least one entity | PASS |  |
| phase02_observation | promotion gate accepted at least one observation | PASS |  |
| phase02_observation | promotion gate rejected below-importance observations | PASS |  |
| phase02_observation | promoted + rejected == total observations | PASS |  |
| phase03_memory | all walked observations reached Canonical | PASS |  |
| phase03_memory | pinning enforces the >= 0.9 retention floor | PASS |  |
| phase03_memory | unpinned candidate score is below pinned floor | PASS |  |
| phase03_memory | decay sweep archived at least one ancient candidate | PASS |  |
| phase03_memory | archived state is reachable via MemoryFilter | PASS |  |
| phase03_memory | forget(canonical) marks the row Deleted (not removed) | PASS |  |
| phase03_memory | forget(non-canonical) physically removes the row | PASS |  |
| phase03_memory | post-walk Canonical count == initial canonicals - canonicals forgotten | PASS |  |
| phase03_memory | WorkingMemory evicted entries past TTL | PASS |  |
| phase03_memory | EpisodicMemory produced at least one summary | PASS |  |
| phase03_memory | every episodic summary carries non-empty key_observations | PASS |  |
| phase04_concept_graph | concept graph carries seven typed relation tags | PASS |  |
| phase04_concept_graph | supersession recorded the predecessor as Superseded | PASS |  |
| phase04_concept_graph | promotion flipped Candidate -> Canonical (no-op safe) | PASS |  |
| phase04_concept_graph | contradiction marked at least two nodes | PASS |  |
| phase04_concept_graph | edge removal recorded a removed_edges entry | PASS |  |
| phase04_concept_graph | explore_from produced a non-empty view from the tenant root | PASS |  |
| phase04_concept_graph | subgraph_for_scope returned per-scope nodes for every scope | PASS |  |
| phase04_concept_graph | every dataset scope rehydrated at least one node | PASS |  |
| phase04_concept_graph | neighborhood walk surfaced at least one neighbour | PASS |  |
| phase04_concept_graph | search_nodes located the seeded 'atlas' concept | PASS |  |
| phase04_concept_graph | PersistentConceptGraph rehydration matches persisted counts | PASS |  |
| phase04_concept_graph | removed_edges from EdgeRemoved propagation == 1 | PASS |  |
| phase05_synthesis | every channel scope produced one ChannelRecap synthesis object | PASS |  |
| phase05_synthesis | channel publish/consume AEAD round-trip succeeded for every recap | PASS |  |
| phase05_synthesis | domain synthesizer emitted a DomainSummary object | PASS |  |
| phase05_synthesis | domain summary AEAD round-trip preserved the object | PASS |  |
| phase05_synthesis | tenant synthesizer emitted a TenantSummary object | PASS |  |
| phase05_synthesis | tenant summary AEAD round-trip preserved the object | PASS |  |
| phase05_synthesis | domain window finished in Complete state | PASS |  |
| phase05_synthesis | tenant window finished in Complete state | PASS |  |
| phase05_synthesis | raw ChannelMemoryObject is rejected as a domain input | PASS |  |
| phase05_synthesis | channel-recap object is rejected as a tenant input | PASS |  |
| phase05_synthesis | channel-tier window cannot consume a DomainSynthesisInput | PASS |  |
| phase05_synthesis | off-scope handle is rejected even with the matching tier tag | PASS |  |
| phase05_synthesis | SynthesisWindow rejects zero-duration intervals | PASS |  |
| phase06_permissions | alice (owner) reaches Owner on the tenant | PASS |  |
| phase06_permissions | alice reaches Viewer on the channel via the chain | PASS |  |
| phase06_permissions | alice reaches Member on the alternate channel via Owner->...->Member | PASS |  |
| phase06_permissions | bob (admin) reaches Editor on the domain via tenant#admin rewrite | PASS |  |
| phase06_permissions | bob reaches Viewer on the channel via two-hop rewrite + chain | PASS |  |
| phase06_permissions | bob (admin not owner) is denied Owner on the tenant | PASS |  |
| phase06_permissions | carol (member) reaches Viewer on the channel via two-hop rewrite | PASS |  |
| phase06_permissions | carol does NOT reach Member on the channel (Viewer doesn't imply Member) | PASS |  |
| phase06_permissions | carol does NOT reach Editor on the channel | PASS |  |
| phase06_permissions | carol does NOT reach Admin on the tenant | PASS |  |
| phase06_permissions | dave (channel editor) reaches Member via Editor->Member | PASS |  |
| phase06_permissions | dave reaches Viewer via Editor->...->Viewer | PASS |  |
| phase06_permissions | dave does NOT reach Admin (Editor doesn't imply Admin) | PASS |  |
| phase06_permissions | dave's editor binding is scope-local (no leak to main channel) | PASS |  |
| phase06_permissions | outsider eve is denied Viewer on tenant | PASS |  |
| phase06_permissions | outsider eve is denied Viewer on domain | PASS |  |
| phase06_permissions | outsider eve is denied Viewer on channel | PASS |  |
| phase06_permissions | synthesis agent reaches Synthesizer on tenant via upsert | PASS |  |
| phase06_permissions | synthesis agent reaches Editor on its agent object via custom namespace | PASS |  |
| phase06_permissions | duplicate tuple insert returns DuplicateTuple | PASS |  |
| phase06_permissions | removing a phantom tuple returns NotFound | PASS |  |
| phase06_permissions | first upsert inserts the tuple | PASS |  |
| phase06_permissions | second upsert is idempotent (returns false) | PASS |  |
| phase06_permissions | scratch tuple grants viewer to eve before removal | PASS |  |
| phase06_permissions | removing the scratch tuple revokes eve's access | PASS |  |
| phase07_crypto | every signed bundle verifies under its own key | PASS |  |
| phase07_crypto | wrong-key verification fails for every signed bundle | PASS |  |
| phase07_crypto | tampered-entity-id bundles fail verification | PASS |  |
| phase07_crypto | hybrid KEM encap/decap produces matching shared secrets | PASS |  |
| phase07_crypto | hybrid KEM shared secret length is 32 bytes (AEAD_KEY_LEN) | PASS |  |
| phase07_crypto | wrong recipient secret cannot recover the hybrid shared secret | PASS |  |
| phase07_crypto | AEAD encrypt/decrypt round-trips cleanly with bound AAD | PASS |  |
| phase07_crypto | AEAD wrong key is rejected | PASS |  |
| phase07_crypto | AEAD wrong AAD is rejected | PASS |  |
| phase07_crypto | AEAD tampered ciphertext is rejected | PASS |  |
| phase07_crypto | scope DEK decrypts payload before destroy | PASS |  |
| phase07_crypto | scope DEK is dropped from the registry after destroy | PASS |  |
| phase07_crypto | scope is forgotten after destroy_scope_dek | PASS |  |
| phase07_crypto | destroy_scope_dek emitted at least one KeyDestructionEvent | PASS |  |
| phase07_crypto | decrypt with a zeroed key is rejected (forgetting holds) | PASS |  |
| phase07_crypto | destroy_scope_dek is idempotent | PASS |  |
| phase07_crypto | alternate scope started with two epoch DEKs | PASS |  |
| phase07_crypto | destroy_epoch_dek emits at least one event | PASS |  |
| phase07_crypto | destroyed epoch's DEK is gone | PASS |  |
| phase07_crypto | live epoch's DEK still resolves | PASS |  |
| phase07_crypto | tombstone is set for the destroyed epoch | PASS |  |
| phase07_crypto | single-epoch destroy does NOT mark the whole scope forgotten | PASS |  |
| phase07_crypto | epoch manager force_rotate advances the current epoch | PASS |  |
| phase07_crypto | epoch manager size trigger advances the current epoch | PASS |  |
| phase07_crypto | epoch manager lists every historical epoch | PASS |  |
| phase08_export | Phase 4 surfaced at least 3 canonical concepts to export | PASS |  |
| phase08_export | registry contains a control row for every approved canonical concept | PASS |  |
| phase08_export | deny-by-default: at least one canonical concept has no control row | PASS |  |
| phase08_export | registry rejects the un-registered canonical concept | PASS |  |
| phase08_export | approval workflow accepted every registered canonical concept | PASS |  |
| phase08_export | duplicate approval is rejected by the workflow | PASS |  |
| phase08_export | profile carries every approved concept | PASS |  |
| phase08_export | profile carries a non-empty constraint set | PASS |  |
| phase08_export | policy engine approves every concept under the demo policy | PASS |  |
| phase08_export | policy engine rejects no concept under the demo policy | PASS |  |
| phase08_export | policy engine refuses raw evidence by default | PASS |  |
| phase08_export | ConceptsOnly view surfaces every approved concept | PASS |  |
| phase08_export | ConceptsOnly view exposes no summaries or evidence pack | PASS |  |
| phase08_export | WithSummaries view exposes the supplied summary | PASS |  |
| phase08_export | WithSummaries view still surfaces the full approved concept set | PASS |  |
| phase08_export | WithEvidencePack is rejected when policy disallows raw evidence | PASS |  |
| phase08_export | simulator's included-concept set matches the engine's approved set | PASS |  |
| phase08_export | simulator surfaces the registered exportable summary | PASS |  |
| phase08_export | simulator excludes the non-exportable summary | PASS |  |
| phase08_export | simulator estimate is non-trivial when concepts are included | PASS |  |
| phase08_export | simulator does not authorise raw-evidence emission under the demo policy | PASS |  |
| phase09_agent | Phase 1 surfaced enough evidence rows to back agent proposals | PASS |  |
| phase09_agent | store accepted all four typed proposals | PASS |  |
| phase09_agent | store refuses to overwrite an existing proposal id | PASS |  |
| phase09_agent | corroboration count is bumped on each call | PASS |  |
| phase09_agent | high-confidence observation auto-promotes under permissive policy | PASS |  |
| phase09_agent | auto-promoted observation is in Promoted state | PASS |  |
| phase09_agent | below-threshold concept needs human review | PASS |  |
| phase09_agent | concept reaches Promoted via manual promote() | PASS |  |
| phase09_agent | below-threshold relation needs human review | PASS |  |
| phase09_agent | rejected relation lands in Rejected with explicit reason | PASS |  |
| phase09_agent | default policy admits to review without auto-promoting | PASS |  |
| phase09_agent | rejected once-promoted proposal is refused by the state machine | PASS |  |
| phase09_agent | canonical observation derives from observation proposal | PASS |  |
| phase09_agent | canonical concept derives from concept proposal | PASS |  |
| phase09_agent | canonical summary derives from summary proposal | PASS |  |
| phase09_agent | promote_to_canonical is deterministic across calls | PASS |  |
| phase09_agent | canonical artifact is refused for rejected proposal | PASS |  |
| phase09_agent | TTL-elapsed proposal is rejected with LifecycleError::Expired | PASS |  |
| phase09_agent | TTL-elapsed proposal lands in Rejected with reason `ttl_expired` | PASS |  |
| phase10_reasoning | ContradictionDetector flagged the seeded opposing pair | PASS |  |
| phase10_reasoning | adjudication state advanced to Resolved | PASS |  |
| phase10_reasoning | adjudication outcome marked the left side as winner | PASS |  |
| phase10_reasoning | exploratory traversal stayed within max_hops budget | PASS |  |
| phase10_reasoning | targeted traversal reached the seeded contradiction concept | PASS |  |
| phase10_reasoning | GoT executor produced a Conclusion-ending best path | PASS |  |
| phase10_reasoning | GoT executor stayed within budget (no exhaustion) | PASS |  |
| phase10_reasoning | GoT trace persisted into WorkflowMemory | PASS |  |
| phase10_reasoning | CommunityDetector returned at least one canonical cluster | PASS |  |
| phase10_reasoning | CommunityHierarchy levels start with the leaves at level 0 | PASS |  |
| phase10_reasoning | CommunitySummaryGenerator produced summaries for every community | PASS |  |
| phase10_reasoning | CommunityQueryRouter returned at least one visible summary | PASS |  |
| phase10_reasoning | CommunityQueryRouter excludes communities for a user with no grants | PASS |  |
| phase10_reasoning | QueryPlanner produced a non-empty fallback chain for every query | PASS |  |
| phase10_reasoning | QueryPlanner.execute stopped at the first Success in every chain | PASS |  |
| Phase 11: Connector Framework | google_drive.authenticate returns drive scope | PASS |  |
| Phase 11: Connector Framework | google_drive.initial_sync emits one event per file | PASS |  |
| Phase 11: Connector Framework | google_drive.initial_sync seeds new_start_page_token | PASS |  |
| Phase 11: Connector Framework | google_drive.incremental_sync surfaces removed change as DocumentDeleted | PASS |  |
| Phase 11: Connector Framework | google_drive.subscribe_webhook bound to instance | PASS |  |
| Phase 11: Connector Framework | google_drive.handle_webhook_event surfaces permission change | PASS |  |
| Phase 11: Connector Framework | jira.authenticate returns jira-work scope | PASS |  |
| Phase 11: Connector Framework | jira.initial_sync emits one DocumentCreated per issue | PASS |  |
| Phase 11: Connector Framework | jira.incremental_sync emits at least one DocumentUpdated | PASS |  |
| Phase 11: Connector Framework | jira.subscribe_webhook returns subscription bound to instance | PASS |  |
| Phase 11: Connector Framework | jira.handle_webhook_event(jira:issue_created) yields DocumentCreated | PASS |  |
| Phase 11: Connector Framework | jira.handle_webhook_event(permissionscheme_updated) yields PermissionChanged | PASS |  |
| Phase 11: Connector Framework | slack.authenticate returns channels:history scope | PASS |  |
| Phase 11: Connector Framework | slack.initial_sync emits one DocumentCreated per message | PASS |  |
| Phase 11: Connector Framework | slack.incremental_sync surfaces both update and delete | PASS |  |
| Phase 11: Connector Framework | slack.subscribe_webhook returns subscription bound to instance | PASS |  |
| Phase 11: Connector Framework | slack.handle_webhook_event(message) yields DocumentCreated | PASS |  |
| Phase 11: Connector Framework | email[gmail].authenticate returns gmail.readonly scope | PASS |  |
| Phase 11: Connector Framework | email[gmail].initial_sync emits one DocumentCreated per message | PASS |  |
| Phase 11: Connector Framework | email[gmail].incremental_sync emits at least one event | PASS |  |
| Phase 11: Connector Framework | email[gmail].subscribe_webhook bound to instance | PASS |  |
| Phase 11: Connector Framework | email[gmail].handle_webhook_event emits one event per messageId | PASS |  |
| Phase 11: Connector Framework | email[graph].authenticate returns Mail.Read scope | PASS |  |
| Phase 11: Connector Framework | email[graph].initial_sync seeds delta-link cursor | PASS |  |
| Phase 11: Connector Framework | email[graph].incremental_sync emits at least one event | PASS |  |
| Phase 11: Connector Framework | email[graph].subscribe_webhook bound to instance | PASS |  |
| Phase 11: Connector Framework | email[graph].handle_webhook_event emits DocumentCreated for created notifications | PASS |  |
| Phase 11: Connector Framework | all four connectors emit at least one event | PASS |  |
| Phase 11: Connector Framework | every connector registered at least one webhook subscription | PASS |  |
| phase12_audit | audit log accumulated entries from every audit-emitting phase | PASS |  |
| phase12_audit | audit log assigns strictly monotonic sequence numbers | PASS |  |
| phase12_audit | audit log surfaces at least four distinct action types | PASS |  |
| phase12_audit | scope-filtered query returns only tenant-scope entries | PASS |  |
| phase12_audit | action-filtered query returns at least one AgentProposalPromoted entry | PASS |  |
| phase12_audit | time-range (since) query reaches the just-appended lifecycle row | PASS |  |
| phase12_audit | time-range (until) query covers earlier phases' entries | PASS |  |
| phase12_audit | actor-filtered query returns only entries by the chosen actor | PASS |  |
| phase12_audit | composite query (scope + action + since) recovers the lifecycle row | PASS |  |
| phase12_audit | audit log is append-only (no entries were removed) | PASS |  |

