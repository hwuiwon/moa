//! Shared fixtures for skill-injection unit tests.

use chrono::{TimeZone, Utc};
use moa_core::{
    ModelCapabilities, ModelId, Platform, SessionId, SessionMeta, SkillMetadata, TokenPricing,
    ToolCallFormat, UserId, WorkspaceId,
};

use super::tier1_metadata::ResolvedSkillBudget;

pub(super) fn fixed_time() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 16, 12, 0, 0)
        .single()
        .expect("fixed skill timestamp should be valid")
}

pub(super) fn older_time(days: i64) -> chrono::DateTime<Utc> {
    fixed_time() - chrono::Duration::days(days)
}

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
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        platform: Platform::Cli,
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

pub(super) fn test_skill(
    name: &str,
    description: &str,
    use_count: u32,
    last_used_days_ago: i64,
) -> SkillMetadata {
    SkillMetadata {
        path: format!("skills/{name}/SKILL.md"),
        name: name.to_string(),
        description: description.to_string(),
        tags: vec!["ops".to_string(), "debug".to_string()],
        allowed_tools: vec!["bash".to_string()],
        estimated_tokens: 1_200,
        use_count,
        last_used: Some(older_time(last_used_days_ago)),
        success_rate: 0.9,
        auto_generated: false,
    }
}

pub(super) fn skills(entries: Vec<(&str, &str, u32, i64)>) -> Vec<SkillMetadata> {
    entries
        .into_iter()
        .map(|(name, description, use_count, days)| test_skill(name, description, use_count, days))
        .collect()
}
