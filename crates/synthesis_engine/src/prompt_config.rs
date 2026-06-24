//! Per-tier synthesis prompt and grammar configuration.
//!
//! The synthesis engine operates across three hierarchy tiers
//! (Channel, Domain, Tenant), each consuming different input shapes
//! and producing different output formats. This module provides
//! tier-specific prompt templates and grammar constraints so the
//! SLM receives appropriately structured instructions for each tier.
//!
//! ## Tier divergence rationale
//!
//! * **Channel** — consumes raw evidence rows (chat messages, emails,
//!   tickets). The prompt emphasises extraction of decisions, action
//!   items, and open questions from conversational noise. Output
//!   grammar is a structured recap with `decisions`, `action_items`,
//!   and `open_questions` arrays.
//!
//! * **Domain** — consumes channel recaps. The prompt emphasises
//!   cross-channel synthesis: identifying themes, dependencies, and
//!   risks that span multiple channels. Output grammar is a domain
//!   summary with `themes`, `dependencies`, and `risks` arrays.
//!
//! * **Tenant** — consumes domain summaries and approved documents.
//!   The prompt emphasises institutional knowledge consolidation:
//!   canonical policies, product taxonomy, and stable org knowledge.
//!   Output grammar is a tenant recap with `policies`, `taxonomy`,
//!   and `org_knowledge` arrays.

use serde::{Deserialize, Serialize};

use synthesis_pipeline::WindowScopeTier;

/// Per-tier prompt template and grammar configuration.
///
/// Each tier gets its own system prompt, output grammar schema, and
/// token budget. The [`SynthesisPromptBuilder`] uses this config to
/// generate the final prompt string sent to the SLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynthesisPromptConfig {
    /// System prompt for channel-tier synthesis.
    pub channel_system_prompt: String,
    /// Grammar/schema constraint for channel-tier output.
    pub channel_grammar: String,
    /// Max tokens for channel-tier synthesis.
    pub channel_max_tokens: u32,

    /// System prompt for domain-tier synthesis.
    pub domain_system_prompt: String,
    /// Grammar/schema constraint for domain-tier output.
    pub domain_grammar: String,
    /// Max tokens for domain-tier synthesis.
    pub domain_max_tokens: u32,

    /// System prompt for tenant-tier synthesis.
    pub tenant_system_prompt: String,
    /// Grammar/schema constraint for tenant-tier output.
    pub tenant_grammar: String,
    /// Max tokens for tenant-tier synthesis.
    pub tenant_max_tokens: u32,
}

/// Default channel-tier system prompt.
pub const DEFAULT_CHANNEL_SYSTEM_PROMPT: &str = "\
You are a channel synthesis engine. Your task is to read the raw \
evidence messages from a communication channel and produce a \
structured recap.

Focus on extracting:
- Decisions: explicit choices made by the team.
- Action items: tasks assigned to specific people.
- Open questions: unresolved questions that need follow-up.

Ignore social chatter, greetings, and reactions unless they \
correlate with a decision or action item.

Output a JSON object with this shape:
{\"decisions\": [...], \"action_items\": [...], \"open_questions\": [...]}";

/// Default channel-tier grammar (JSON schema constraint).
pub const DEFAULT_CHANNEL_GRAMMAR: &str = "\
{\"type\":\"object\",\"properties\":{\"decisions\":{\"type\":\"array\",\
\"items\":{\"type\":\"string\"}},\"action_items\":{\"type\":\"array\",\
\"items\":{\"type\":\"object\",\"properties\":{\"task\":{\"type\":\"string\"},\
\"assignee\":{\"type\":\"string\"}}}},\"open_questions\":{\"type\":\"array\",\
\"items\":{\"type\":\"string\"}}},\"required\":[\"decisions\",\
\"action_items\",\"open_questions\"]}";

/// Default domain-tier system prompt.
pub const DEFAULT_DOMAIN_SYSTEM_PROMPT: &str = "\
You are a domain synthesis engine. Your task is to read channel \
recaps from multiple channels within a domain and produce a \
cross-channel domain summary.

Focus on identifying:
- Themes: recurring topics across channels.
- Dependencies: cross-channel work dependencies.
- Risks: risks that span multiple channels or affect the domain.

Synthesise — do not just concatenate. Identify patterns and \
contradictions across channels.

Output a JSON object with this shape:
{\"themes\": [...], \"dependencies\": [...], \"risks\": [...]}";

/// Default domain-tier grammar.
pub const DEFAULT_DOMAIN_GRAMMAR: &str = "\
{\"type\":\"object\",\"properties\":{\"themes\":{\"type\":\"array\",\
\"items\":{\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"},\
\"channels\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}}}},\
\"dependencies\":{\"type\":\"array\",\"items\":{\"type\":\"object\",\
\"properties\":{\"from\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}}}},\
\"risks\":{\"type\":\"array\",\"items\":{\"type\":\"object\",\
\"properties\":{\"description\":{\"type\":\"string\"},\"severity\":\
{\"type\":\"string\",\"enum\":[\"low\",\"medium\",\"high\"]}}}}},\
\"required\":[\"themes\",\"dependencies\",\"risks\"]}";

/// Default tenant-tier system prompt.
pub const DEFAULT_TENANT_SYSTEM_PROMPT: &str = "\
You are a tenant synthesis engine. Your task is to read domain \
summaries and approved documents to produce an institutional \
knowledge recap for the tenant.

Focus on consolidating:
- Policies: canonical rules and guidelines.
- Taxonomy: product / project categorisation.
- Org knowledge: stable facts about the organisation.

This is the highest synthesis tier — output should be concise, \
canonical, and suitable for long-term reference.

Output a JSON object with this shape:
{\"policies\": [...], \"taxonomy\": [...], \"org_knowledge\": [...]}";

/// Default tenant-tier grammar.
pub const DEFAULT_TENANT_GRAMMAR: &str = "\
{\"type\":\"object\",\"properties\":{\"policies\":{\"type\":\"array\",\
\"items\":{\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"},\
\"description\":{\"type\":\"string\"}}}},\"taxonomy\":{\"type\":\"array\",\
\"items\":{\"type\":\"object\",\"properties\":{\"category\":{\"type\":\"string\"},\
\"items\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}}}},\
\"org_knowledge\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\
\"required\":[\"policies\",\"taxonomy\",\"org_knowledge\"]}";

/// Default token budgets per tier.
pub const DEFAULT_CHANNEL_MAX_TOKENS: u32 = 1024;
/// Default max tokens for domain-tier synthesis.
pub const DEFAULT_DOMAIN_MAX_TOKENS: u32 = 2048;
/// Default max tokens for tenant-tier synthesis.
pub const DEFAULT_TENANT_MAX_TOKENS: u32 = 4096;

impl Default for SynthesisPromptConfig {
    fn default() -> Self {
        Self {
            channel_system_prompt: DEFAULT_CHANNEL_SYSTEM_PROMPT.to_string(),
            channel_grammar: DEFAULT_CHANNEL_GRAMMAR.to_string(),
            channel_max_tokens: DEFAULT_CHANNEL_MAX_TOKENS,
            domain_system_prompt: DEFAULT_DOMAIN_SYSTEM_PROMPT.to_string(),
            domain_grammar: DEFAULT_DOMAIN_GRAMMAR.to_string(),
            domain_max_tokens: DEFAULT_DOMAIN_MAX_TOKENS,
            tenant_system_prompt: DEFAULT_TENANT_SYSTEM_PROMPT.to_string(),
            tenant_grammar: DEFAULT_TENANT_GRAMMAR.to_string(),
            tenant_max_tokens: DEFAULT_TENANT_MAX_TOKENS,
        }
    }
}

impl SynthesisPromptConfig {
    /// Get the system prompt for a given tier.
    pub fn system_prompt(&self, tier: WindowScopeTier) -> &str {
        match tier {
            WindowScopeTier::Channel => &self.channel_system_prompt,
            WindowScopeTier::Domain => &self.domain_system_prompt,
            WindowScopeTier::Tenant => &self.tenant_system_prompt,
        }
    }

    /// Get the grammar schema for a given tier.
    pub fn grammar(&self, tier: WindowScopeTier) -> &str {
        match tier {
            WindowScopeTier::Channel => &self.channel_grammar,
            WindowScopeTier::Domain => &self.domain_grammar,
            WindowScopeTier::Tenant => &self.tenant_grammar,
        }
    }

    /// Get the max token budget for a given tier.
    pub fn max_tokens(&self, tier: WindowScopeTier) -> u32 {
        match tier {
            WindowScopeTier::Channel => self.channel_max_tokens,
            WindowScopeTier::Domain => self.domain_max_tokens,
            WindowScopeTier::Tenant => self.tenant_max_tokens,
        }
    }

    /// Override the channel-tier prompt.
    pub fn with_channel_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.channel_system_prompt = prompt.into();
        self
    }

    /// Override the domain-tier prompt.
    pub fn with_domain_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.domain_system_prompt = prompt.into();
        self
    }

    /// Override the tenant-tier prompt.
    pub fn with_tenant_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.tenant_system_prompt = prompt.into();
        self
    }
}

/// Builds the final prompt string for a synthesis request by
/// combining the tier-specific system prompt with the input object
/// previews.
pub struct SynthesisPromptBuilder<'a> {
    config: &'a SynthesisPromptConfig,
    tier: WindowScopeTier,
}

impl<'a> SynthesisPromptBuilder<'a> {
    /// Create a new builder for the given tier.
    pub fn new(config: &'a SynthesisPromptConfig, tier: WindowScopeTier) -> Self {
        Self { config, tier }
    }

    /// Build the full prompt string, interpolating input previews.
    pub fn build(&self, input_previews: &[String]) -> String {
        use std::fmt::Write;

        let system_prompt = self.config.system_prompt(self.tier);
        let tier_name = self.tier.as_str();

        let mut prompt = format!(
            "{system_prompt}\n\n\
             --- {tier_name} synthesis inputs ---\n"
        );

        for (i, preview) in input_previews.iter().enumerate() {
            let _ = writeln!(prompt, "[{i}] {preview}");
        }

        prompt.push_str("\n--- end inputs ---\n");
        let _ = write!(
            prompt,
            "Produce the {tier_name} synthesis output following the \
             grammar constraints. Output only the JSON object, no \
             preamble or explanation."
        );

        prompt
    }

    /// Get the grammar for this builder's tier.
    pub fn grammar(&self) -> &str {
        self.config.grammar(self.tier)
    }

    /// Get the max tokens for this builder's tier.
    pub fn max_tokens(&self) -> u32 {
        self.config.max_tokens(self.tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_divergent_prompts_per_tier() {
        let config = SynthesisPromptConfig::default();

        // Each tier should have a different system prompt.
        let channel = config.system_prompt(WindowScopeTier::Channel);
        let domain = config.system_prompt(WindowScopeTier::Domain);
        let tenant = config.system_prompt(WindowScopeTier::Tenant);

        assert_ne!(channel, domain);
        assert_ne!(domain, tenant);
        assert_ne!(channel, tenant);

        // Channel prompt should mention "decisions" and "Action items".
        assert!(channel.contains("decisions"));
        assert!(channel.contains("Action items"));

        // Domain prompt should mention "themes" and "dependencies".
        assert!(domain.contains("themes"));
        assert!(domain.contains("dependencies"));

        // Tenant prompt should mention "policies" and "taxonomy".
        assert!(tenant.contains("policies"));
        assert!(tenant.contains("taxonomy"));
    }

    #[test]
    fn default_config_has_divergent_grammars_per_tier() {
        let config = SynthesisPromptConfig::default();

        let channel_grammar = config.grammar(WindowScopeTier::Channel);
        let domain_grammar = config.grammar(WindowScopeTier::Domain);
        let tenant_grammar = config.grammar(WindowScopeTier::Tenant);

        assert_ne!(channel_grammar, domain_grammar);
        assert_ne!(domain_grammar, tenant_grammar);
        assert_ne!(channel_grammar, tenant_grammar);
    }

    #[test]
    fn token_budgets_increase_with_tier() {
        let config = SynthesisPromptConfig::default();

        let channel = config.max_tokens(WindowScopeTier::Channel);
        let domain = config.max_tokens(WindowScopeTier::Domain);
        let tenant = config.max_tokens(WindowScopeTier::Tenant);

        assert!(channel < domain);
        assert!(domain < tenant);
    }

    #[test]
    fn prompt_builder_includes_system_prompt_and_inputs() {
        let config = SynthesisPromptConfig::default();
        let builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Channel);

        let previews = vec![
            "We decided to use PostgreSQL.".to_string(),
            "Action item: Alice to set up the schema.".to_string(),
        ];

        let prompt = builder.build(&previews);

        // Should contain the system prompt.
        assert!(prompt.contains("channel synthesis engine"));
        // Should contain both input previews.
        assert!(prompt.contains("PostgreSQL"));
        assert!(prompt.contains("Alice"));
        // Should contain tier name.
        assert!(prompt.contains("channel"));
        // Should instruct JSON-only output.
        assert!(prompt.contains("JSON"));
    }

    #[test]
    fn prompt_builder_grammar_matches_tier() {
        let config = SynthesisPromptConfig::default();

        let channel_builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Channel);
        assert!(channel_builder.grammar().contains("decisions"));

        let domain_builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Domain);
        assert!(domain_builder.grammar().contains("themes"));

        let tenant_builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Tenant);
        assert!(tenant_builder.grammar().contains("policies"));
    }

    #[test]
    fn custom_prompt_override() {
        let config = SynthesisPromptConfig::default()
            .with_channel_prompt("Custom channel prompt for testing.");

        assert_eq!(
            config.system_prompt(WindowScopeTier::Channel),
            "Custom channel prompt for testing."
        );
        // Other tiers should be unaffected.
        assert_eq!(
            config.system_prompt(WindowScopeTier::Domain),
            DEFAULT_DOMAIN_SYSTEM_PROMPT
        );
    }

    #[test]
    fn prompt_builder_with_empty_inputs() {
        let config = SynthesisPromptConfig::default();
        let builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Domain);

        let prompt = builder.build(&[]);

        // Should still contain the system prompt and end-of-inputs marker.
        assert!(prompt.contains("domain synthesis engine"));
        assert!(prompt.contains("end inputs"));
    }

    #[test]
    fn prompt_builder_max_tokens_matches_config() {
        let config = SynthesisPromptConfig::default();

        let channel_builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Channel);
        assert_eq!(channel_builder.max_tokens(), DEFAULT_CHANNEL_MAX_TOKENS);

        let domain_builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Domain);
        assert_eq!(domain_builder.max_tokens(), DEFAULT_DOMAIN_MAX_TOKENS);

        let tenant_builder = SynthesisPromptBuilder::new(&config, WindowScopeTier::Tenant);
        assert_eq!(tenant_builder.max_tokens(), DEFAULT_TENANT_MAX_TOKENS);
    }
}
