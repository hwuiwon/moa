//! Shared fixtures for skill-injection unit tests.

use moa_core::{
    types::channel::Channel, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::memory::SkillMetadata, types::model::ModelCapabilities,
    types::model::TokenPricing, types::model::ToolCallFormat, types::session::SessionMeta,
};

use super::tier1_metadata::ResolvedSkillBudget;

pub(super) fn capabilities(context_window: usize) -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("claude-sonnet-4-6"),
        context_window,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}

pub(super) fn session() -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        tenant_id: TenantId::new(),
        channel: Channel::Chat,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

pub(super) fn resolved_budget(max_manifest_chars: usize) -> ResolvedSkillBudget {
    ResolvedSkillBudget {
        max_manifest_chars,
        max_per_skill_chars: 1_536,
        show_token_estimates: true,
    }
}

pub(super) fn test_skill(name: &str, description: &str) -> SkillMetadata {
    SkillMetadata {
        artifact_revision_uid: None,
        path: format!(".moa/skills/{name}/SKILL.md"),
        name: name.to_string(),
        description: description.to_string(),
        tags: vec!["ops".to_string(), "debug".to_string()],
        allowed_tools: vec!["bash".to_string()],
        actions: Vec::new(),
        has_procedure: false,
        estimated_tokens: 1_200,
    }
}

pub(super) fn test_skill_with_procedure(name: &str, description: &str) -> SkillMetadata {
    SkillMetadata {
        has_procedure: true,
        ..test_skill(name, description)
    }
}

pub(super) fn skills(entries: Vec<(&str, &str)>) -> Vec<SkillMetadata> {
    entries
        .into_iter()
        .map(|(name, description)| test_skill(name, description))
        .collect()
}
