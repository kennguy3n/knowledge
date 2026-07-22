//! Deterministic dataset generator: produces a `WorldDataset` at any scale.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use rand::{RngExt, SeedableRng};
use uuid::Uuid;

use evidence_store::{ImportanceClass, ScopeId};
use crate::media::MediaFile;
use crate::scenarios::{fill_turn_with, infer_obs_type, LANGUAGES, SCENARIOS};
use crate::world::{
    ScopeKind, SimScope, SimTenant, SimUser, UserRole, World,
};

/// Scale presets for the simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ScalePreset {
    /// 10K messages, 3 tenants, ~2 min.
    Quick,
    /// 100K messages, 10 tenants, ~15 min.
    Standard,
    /// 1M messages, 50 tenants, ~2 hours.
    Stress,
}

impl ScalePreset {
    /// Convert to a `SimConfig`.
    pub fn config(self) -> SimConfig {
        match self {
            Self::Quick => SimConfig {
                target_messages: 10_000,
                num_tenants: 3,
                users_per_tenant: 15,
                scopes_per_tenant: 30,
                seed: 42,
            },
            Self::Standard => SimConfig {
                target_messages: 100_000,
                num_tenants: 10,
                users_per_tenant: 50,
                scopes_per_tenant: 200,
                seed: 42,
            },
            Self::Stress => SimConfig {
                target_messages: 1_000_000,
                num_tenants: 50,
                users_per_tenant: 200,
                scopes_per_tenant: 1000,
                seed: 42,
            },
        }
    }
}

/// Simulation configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimConfig {
    /// Target total message count.
    pub target_messages: usize,
    /// Number of tenants.
    pub num_tenants: usize,
    /// Users per tenant.
    pub users_per_tenant: usize,
    /// Scopes per tenant.
    pub scopes_per_tenant: usize,
    /// RNG seed for deterministic output.
    pub seed: u64,
}

/// One turn in the replay — a single message to ingest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    /// Scope to ingest into.
    pub scope_id: ScopeId,
    /// Sender user ID.
    pub sender_id: Uuid,
    /// Tenant ID.
    pub tenant_id: Uuid,
    /// Message content (text body).
    pub content: String,
    /// Importance class.
    pub importance: ImportanceClass,
    /// Language tag (BCP-47 primary subtag).
    pub language: String,
    /// Simulated timestamp.
    pub timestamp: DateTime<Utc>,
    /// Scenario ID.
    pub scenario_id: String,
    /// Scenario instance index.
    pub scenario_instance: usize,
    /// Turn index within the scenario instance.
    pub turn_index: usize,
    /// Media attachment (if any).
    pub media: Option<MediaFile>,
    /// Expected observation types (from scenario template + per-turn inference).
    pub expected_obs_types: Vec<String>,
    /// Expected retrieval terms.
    pub expected_retrieval_terms: Vec<String>,
    /// Per-turn expected observation type (ground truth).
    pub expected_obs_type: String,
    /// Whether this turn is code-switched (bilingual).
    pub code_switched: bool,
    /// Source reference.
    pub source_ref: String,
}

/// The complete generated dataset: the world model + all turns.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorldDataset {
    /// The simulated world.
    pub world: World,
    /// All turns to replay, in order.
    pub turns: Vec<Turn>,
    /// All media fixtures.
    pub media: Vec<MediaFile>,
    /// Configuration used to generate this dataset.
    pub config: SimConfig,
}

/// Generate a complete dataset from the given configuration.
pub fn generate_dataset(config: SimConfig) -> WorldDataset {
    let mut rng = rand::rngs::StdRng::seed_from_u64(config.seed);
    let start_time = Utc::now();

    // Generate media fixtures once.
    let media = crate::media::load_media();

    // Generate the world.
    let world = generate_world(&mut rng, &config, start_time);

    // Generate turns.
    let turns = generate_turns(&mut rng, &config, &world, &media, start_time);

    WorldDataset {
        world,
        turns,
        media,
        config,
    }
}

fn generate_world(
    _rng: &mut rand::rngs::StdRng,
    config: &SimConfig,
    start_time: DateTime<Utc>,
) -> World {
    let mut tenants = Vec::with_capacity(config.num_tenants);
    let mut user_to_tenant = HashMap::new();
    let mut scope_index = HashMap::new();

    let industries = ["tech", "finance", "healthcare", "retail", "manufacturing"];
    let regions = ["Americas", "EMEA", "APAC"];

    for t_idx in 0..config.num_tenants {
        let tenant_id = Uuid::new_v4();
        let tenant_scope = ScopeId::new_v4();
        let primary_lang = LANGUAGES[t_idx % LANGUAGES.len()].0.to_string();

        // Generate users.
        let mut users = Vec::with_capacity(config.users_per_tenant);
        for u_idx in 0..config.users_per_tenant {
            let user_id = Uuid::new_v4();
            let lang = LANGUAGES[(t_idx + u_idx) % LANGUAGES.len()].0;
            let role = match u_idx % 10 {
                0 => UserRole::Admin,
                1..=3 => UserRole::Editor,
                4..=7 => UserRole::Member,
                _ => UserRole::Viewer,
            };
            let name = format!("User_{t_idx}_{u_idx}");
            users.push(SimUser {
                id: user_id,
                name,
                language: lang.to_string(),
                role,
            });
            user_to_tenant.insert(user_id, tenant_id);
        }

        // Generate scopes.
        let mut scopes = Vec::with_capacity(config.scopes_per_tenant);
        for s_idx in 0..config.scopes_per_tenant {
            let scope_id = ScopeId::new_v4();
            let kind = match s_idx % 10 {
                0..=4 => ScopeKind::DirectMessage,
                5..=7 => ScopeKind::GroupMessage,
                8 => ScopeKind::Community,
                9 => ScopeKind::Domain,
                _ => ScopeKind::DirectMessage,
            };

            let member_count = match kind {
                ScopeKind::DirectMessage => 2,
                ScopeKind::GroupMessage => 3 + (s_idx % 6),
                ScopeKind::Community => 20 + (s_idx % 80),
                ScopeKind::Domain => 1,
                ScopeKind::Tenant => 1,
            };

            let members: Vec<Uuid> = users
                .iter()
                .take(member_count.min(users.len()))
                .map(|u| u.id)
                .collect();

            let scope = SimScope {
                scope_id,
                kind,
                tenant_id,
                members,
                parent_domain: None,
                parent_tenant: Some(tenant_scope),
            };

            scope_index.insert(scope_id, (tenant_id, scopes.len()));
            scopes.push(scope);
        }

        // Assign domain parents.
        let domain_scopes: Vec<ScopeId> = scopes
            .iter()
            .filter(|s| s.kind == ScopeKind::Domain)
            .map(|s| s.scope_id)
            .collect();
        for scope in scopes.iter_mut() {
            if !domain_scopes.is_empty() {
                scope.parent_domain =
                    Some(domain_scopes[s_idx_hash(scope.scope_id) % domain_scopes.len()]);
            }
        }

        tenants.push(SimTenant {
            id: tenant_id,
            name: format!("Tenant_{t_idx}"),
            industry: industries[t_idx % industries.len()].to_string(),
            region: regions[t_idx % regions.len()].to_string(),
            primary_language: primary_lang,
            users,
            scopes,
            tenant_scope,
        });
    }

    let mut world = World {
        tenants,
        user_to_tenant,
        scope_index,
        tuples: permission_service::TupleStore::new(),
        namespaces: permission_service::NamespaceRegistry::with_defaults(),
        start_time,
    };
    world.build_permissions();
    world
}

fn s_idx_hash(scope: ScopeId) -> usize {
    let uuid = scope.as_uuid();
    let bytes = uuid.as_bytes();
    bytes[0] as usize
}

fn generate_turns(
    rng: &mut rand::rngs::StdRng,
    config: &SimConfig,
    world: &World,
    media: &[MediaFile],
    start_time: DateTime<Utc>,
) -> Vec<Turn> {
    let mut turns = Vec::with_capacity(config.target_messages);
    let mut turn_global = 0usize;

    // Collect all channel scopes across all tenants.
    let all_scopes: Vec<(ScopeId, Uuid, &[SimUser])> = world
        .tenants
        .iter()
        .flat_map(|t| {
            t.scopes
                .iter()
                .filter(|s| {
                    s.kind == ScopeKind::DirectMessage
                        || s.kind == ScopeKind::GroupMessage
                        || s.kind == ScopeKind::Community
                })
                .map(move |s| {
                    let users: &[SimUser] = &t.users;
                    (s.scope_id, t.id, users)
                })
        })
        .collect();

    if all_scopes.is_empty() {
        return turns;
    }

    // Distribute messages across scopes proportionally.
    let msgs_per_scope = config.target_messages / all_scopes.len().max(1);

    for (scope_id, tenant_id, users) in &all_scopes {
        if users.is_empty() {
            continue;
        }

        let mut scope_turn = 0usize;
        let mut scenario_instance = 0usize;

        while scope_turn < msgs_per_scope && turn_global < config.target_messages {
            // Pick a scenario.
            let scenario = &SCENARIOS[rng.random_range(0..SCENARIOS.len())];

            // Pick a language for this instance.
            let (lang, _) = LANGUAGES[rng.random_range(0..LANGUAGES.len())];

            // Decide whether this scenario instance uses code-switching (~15%).
            let instance_code_switch = rng.random_range(0..100) < 15;

            // Run through the scenario turns.
            for (t_idx, turn_tmpl) in scenario.turns.iter().enumerate() {
                if turn_global >= config.target_messages {
                    break;
                }

                let sender = &users[rng.random_range(0..users.len())];

                // Per-turn code-switching: ~20% of turns in a code-switched
                // instance are bilingual, plus ~5% globally for organic mixing.
                let turn_code_switch =
                    (instance_code_switch && rng.random_range(0..100) < 20)
                        || rng.random_range(0..100) < 5;

                let content = fill_turn_with(
                    turn_tmpl.text,
                    lang,
                    t_idx,
                    turn_code_switch,
                );

                let importance = match turn_tmpl.importance {
                    "critical" => ImportanceClass::Critical,
                    "important" => ImportanceClass::Important,
                    "noise" => ImportanceClass::Noise,
                    _ => ImportanceClass::Useful,
                };

                // Probabilistic media distribution:
                // - If the template specifies media, use it (preserves scenario intent).
                // - Otherwise, ~10% chance of attaching a random media file
                //   to simulate organic file sharing (text 80%, files 10%,
                //   images 5%, audio 3%, video 2%).
                let media_file = if turn_tmpl.has_media {
                    crate::media::media_for_hint(media, turn_tmpl.media_hint)
                        .cloned()
                } else if importance != ImportanceClass::Noise
                    && rng.random_range(0..100) < 10
                {
                    // Pick a random media type based on the distribution.
                    let roll = rng.random_range(0..100);
                    let hint = if roll < 50 {
                        "pdf"
                    } else if roll < 75 {
                        "png"
                    } else if roll < 90 {
                        "csv"
                    } else if roll < 96 {
                        "wav"
                    } else {
                        "mp4"
                    };
                    crate::media::media_for_hint(media, hint).cloned()
                } else {
                    None
                };

                // Per-turn observation ground truth.
                let per_turn_obs = infer_obs_type(turn_tmpl.text);

                // Time progression: 30 seconds per message, advancing the
                // simulated clock to enable decay/aging tests.
                let timestamp =
                    start_time + Duration::seconds(turn_global as i64 * 30);

                turns.push(Turn {
                    scope_id: *scope_id,
                    sender_id: sender.id,
                    tenant_id: *tenant_id,
                    content,
                    importance,
                    language: lang.to_string(),
                    timestamp,
                    scenario_id: scenario.id.to_string(),
                    scenario_instance,
                    turn_index: t_idx,
                    media: media_file,
                    expected_obs_types: scenario
                        .expected_obs_types
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    expected_retrieval_terms: scenario
                        .expected_retrieval_terms
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    expected_obs_type: per_turn_obs.to_string(),
                    code_switched: turn_code_switch,
                    source_ref: format!(
                        "sim:{}:{}:{}",
                        scenario.id, scenario_instance, t_idx
                    ),
                });

                turn_global += 1;
                scope_turn += 1;
            }

            scenario_instance += 1;
        }
    }

    turns
}
