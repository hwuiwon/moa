//! Shared fixtures and helpers for skill self-improvement integration tests.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    Attachment, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, Event,
    EventRecord, LLMProvider, MemoryScope, MoaConfig, MoaError, ModelCapabilities, ModelId,
    ModelTier, Platform, SessionId, SessionMeta, SessionStatus, StopReason, TokenPricing,
    TokenUsage, ToolCallFormat, ToolCallId, ToolOutput, UserId, WorkspaceId,
};
use moa_eval_core::{ExpectedOutput, TestCase, TestSuite};
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;
use moa_skills::format::{
    build_skill_path, parse_skill_markdown, render_skill_markdown, skill_metadata_from_document,
    slugify_skill_name,
};
use moa_skills::registry::{NewSkill, Skill, SkillRegistry};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

/// Successful session fixture with exactly five tool calls.
pub const SESSION_WITH_5_TOOL_CALLS: &str = include_str!("fixtures/session_with_5_tool_calls.json");
/// Successful session fixture below the distillation threshold.
pub const SESSION_WITH_4_TOOL_CALLS: &str = include_str!("fixtures/session_with_4_tool_calls.json");
/// Baseline skill fixture used by improvement and regression tests.
pub const BASELINE_SKILL: &str = include_str!("fixtures/baseline_skill.md");
/// Known-good improvement fixture returned by the scripted LLM.
pub const IMPROVED_SKILL: &str = include_str!("fixtures/improved_skill_diff.md");
/// Known-bad improvement fixture returned by the scripted LLM.
pub const REGRESSED_SKILL: &str = include_str!("fixtures/regressed_skill_diff.md");

#[derive(Debug, Deserialize)]
struct SessionFixture {
    session_id: Uuid,
    workspace_id: String,
    user_id: String,
    task: String,
    final_response: String,
    tool_calls: Vec<ToolCallFixture>,
}

#[derive(Debug, Deserialize)]
struct ToolCallFixture {
    tool_name: String,
    input: Value,
    output: String,
}

/// One parsed session fixture ready for distillation tests.
pub struct LoadedSession {
    /// Session metadata.
    pub session: SessionMeta,
    /// Event log records.
    pub events: Vec<EventRecord>,
}

/// Returns a configured Postgres test database when the opt-in URL is set.
pub async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap skills Postgres test database"),
    )
}

/// Builds an isolated config and memory directory for skill tests.
pub fn test_config(test_db: &TestDb) -> (MoaConfig, TempDir) {
    let temp_dir = tempfile::tempdir().expect("create skill test tempdir");
    let mut config = MoaConfig::default();
    config.database.url = test_db.database_url().to_string();
    config.local.memory_dir = temp_dir
        .path()
        .join("memory")
        .to_string_lossy()
        .into_owned();
    config.query_rewrite.enabled = false;
    (config, temp_dir)
}

/// Returns an owned learning store handle for the isolated test database.
pub fn learning_store(test_db: &TestDb) -> Arc<PostgresSessionStore> {
    Arc::new(test_db.store().clone())
}

/// Loads a session JSON fixture into typed session metadata and records.
pub fn load_session_fixture(json_text: &str) -> LoadedSession {
    let fixture: SessionFixture =
        serde_json::from_str(json_text).expect("parse skill session fixture");
    let workspace_id = WorkspaceId::new(format!(
        "{}-{}",
        fixture.workspace_id,
        Uuid::now_v7().simple()
    ));
    let user_id = UserId::new(fixture.user_id);
    let session = SessionMeta {
        id: SessionId(fixture.session_id),
        workspace_id: workspace_id.clone(),
        user_id: user_id.clone(),
        title: Some(fixture.task.clone()),
        status: SessionStatus::Completed,
        platform: Platform::Api,
        model: ModelId::new("scripted-skill-model"),
        ..SessionMeta::default()
    };
    let mut events = Vec::new();
    push_event(
        &mut events,
        session.id,
        Event::UserMessage {
            text: fixture.task,
            attachments: Vec::<Attachment>::new(),
        },
    );
    for tool_call in fixture.tool_calls {
        let tool_id = ToolCallId::new();
        push_event(
            &mut events,
            session.id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: tool_call.tool_name,
                input: tool_call.input,
                hand_id: None,
            },
        );
        push_event(
            &mut events,
            session.id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text(tool_call.output, Duration::from_millis(1)),
                original_output_tokens: None,
                success: true,
                duration_ms: 1,
            },
        );
    }
    push_event(
        &mut events,
        session.id,
        Event::BrainResponse {
            text: fixture.final_response,
            thought_signature: None,
            model: ModelId::new("scripted-skill-model"),
            model_tier: ModelTier::Auxiliary,
            input_tokens_uncached: 128,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 32,
            cost_cents: 0,
            duration_ms: 1,
        },
    );
    LoadedSession { session, events }
}

/// Returns a failed copy of a loaded session, preserving enough tool calls to pass the threshold.
pub fn failed_session(mut loaded: LoadedSession) -> LoadedSession {
    loaded.session.status = SessionStatus::Failed;
    let tool_id = ToolCallId::new();
    push_event(
        &mut loaded.events,
        loaded.session.id,
        Event::ToolError {
            tool_id,
            provider_tool_use_id: None,
            tool_name: "bash".to_string(),
            error: "final verification failed".to_string(),
            retryable: false,
        },
    );
    loaded
}

/// Builds a model router backed by deterministic text responses.
pub fn scripted_router(responses: impl IntoIterator<Item = impl Into<String>>) -> Arc<ModelRouter> {
    let provider = TestProvider::new(responses.into_iter().map(Into::into).collect());
    Arc::new(ModelRouter::new(Arc::new(provider), None))
}

/// Returns a complete skill markdown document for ad hoc test skills.
pub fn skill_markdown(name: &str, description: &str, body: &str, version: &str) -> String {
    format!(
        "---\n\
         name: {name}\n\
         description: \"{description}\"\n\
         allowed-tools: bash file_search file_read\n\
         metadata:\n\
           moa-version: \"{version}\"\n\
           moa-one-liner: \"{description}\"\n\
           moa-tags: \"auth, regression\"\n\
           moa-created: \"2026-04-09T14:30:00Z\"\n\
           moa-updated: \"2026-04-09T14:30:00Z\"\n\
           moa-auto-generated: \"true\"\n\
           moa-use-count: \"0\"\n\
           moa-success-rate: \"1.0\"\n\
           moa-estimated-tokens: \"300\"\n\
         ---\n\n\
         # {name}\n\n\
         {body}\n"
    )
}

/// Seeds one skill and returns its pipeline metadata.
pub async fn seed_skill(
    test_db: &TestDb,
    scope: MemoryScope,
    markdown: &str,
) -> moa_core::SkillMetadata {
    let document = parse_skill_markdown(markdown).expect("parse seed skill");
    let rendered = render_skill_markdown(&document).expect("render seed skill");
    let registry = SkillRegistry::new(test_db.store().pool().clone());
    registry
        .upsert_by_name(NewSkill::from_skill_markdown(scope, rendered))
        .await
        .expect("seed skill");
    skill_metadata_from_document(build_skill_path(&document.frontmatter.name), &document)
}

/// Loads the active skill row by name.
pub async fn load_active_skill(test_db: &TestDb, scope: &MemoryScope, skill_name: &str) -> Skill {
    SkillRegistry::new(test_db.store().pool().clone())
        .load_by_name(scope, skill_name)
        .await
        .expect("load active skill")
        .expect("active skill exists")
}

/// Loads the active skill row by name when one exists.
pub async fn load_optional_active_skill(
    test_db: &TestDb,
    scope: &MemoryScope,
    skill_name: &str,
) -> Option<Skill> {
    SkillRegistry::new(test_db.store().pool().clone())
        .load_by_name(scope, skill_name)
        .await
        .expect("load optional active skill")
}

/// Loads the active skill's required `SKILL.md` markdown by name.
pub async fn load_active_skill_markdown(
    test_db: &TestDb,
    scope: &MemoryScope,
    skill_name: &str,
) -> String {
    let registry = SkillRegistry::new(test_db.store().pool().clone());
    let row = registry
        .load_by_name(scope, skill_name)
        .await
        .expect("load active skill")
        .expect("active skill exists");
    registry
        .load_skill_markdown(scope, row.skill_uid)
        .await
        .expect("load active skill markdown")
}

/// Writes a compact output-matching regression suite for a skill.
pub async fn write_output_suite(config: &MoaConfig, workspace_id: &WorkspaceId, skill_name: &str) {
    let suite = TestSuite {
        name: format!("{skill_name}-quality"),
        description: Some("Skill regression fixture suite".to_string()),
        cases: vec![TestCase {
            name: "quality".to_string(),
            input: "Run the auth refresh regression workflow".to_string(),
            expected_output: Some(ExpectedOutput {
                contains: vec!["kept".to_string(), "validated".to_string()],
                ..ExpectedOutput::default()
            }),
            timeout_seconds: Some(10),
            ..TestCase::default()
        }],
        default_timeout_seconds: 10,
        tags: vec!["skill".to_string(), skill_name.to_string()],
    };
    let path = suite_path(config, workspace_id, skill_name);
    tokio::fs::create_dir_all(path.parent().expect("suite path has parent"))
        .await
        .expect("create suite directory");
    let rendered = toml::to_string_pretty(&suite).expect("render suite");
    tokio::fs::write(path, rendered).await.expect("write suite");
}

/// Returns the active semantic version parsed from the skill markdown.
pub async fn active_semantic_version(
    test_db: &TestDb,
    scope: &MemoryScope,
    skill_name: &str,
) -> String {
    let markdown = load_active_skill_markdown(test_db, scope, skill_name).await;
    parse_skill_markdown(&markdown)
        .expect("parse active skill")
        .frontmatter
        .version()
}

/// Counts active and historical workspace rows for one skill name.
pub async fn skill_row_count(
    test_db: &TestDb,
    workspace_id: &WorkspaceId,
    skill_name: &str,
) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM moa.skill WHERE workspace_id = $1 AND name = $2")
        .bind(workspace_id.as_str())
        .bind(skill_name)
        .fetch_one(test_db.store().pool())
        .await
        .expect("count skill rows")
}

/// Counts artifact revisions for one workspace skill artifact.
pub async fn artifact_revision_count(
    test_db: &TestDb,
    workspace_id: &WorkspaceId,
    skill_name: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) \
         FROM moa.artifact a \
         JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid \
         WHERE a.workspace_id = $1 AND a.kind = 'skill' AND a.name = $2",
    )
    .bind(workspace_id.as_str())
    .bind(skill_name)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count skill artifact revisions")
}

/// Removes all active, historical, and mirrored artifact rows for one test skill name.
pub async fn purge_skill_name(test_db: &TestDb, skill_name: &str) {
    sqlx::query("DELETE FROM moa.skill WHERE name = $1")
        .bind(skill_name)
        .execute(test_db.store().pool())
        .await
        .expect("purge test skill rows");
    sqlx::query("DELETE FROM moa.artifact WHERE kind = 'skill' AND name = $1")
        .bind(skill_name)
        .execute(test_db.store().pool())
        .await
        .expect("purge test skill artifact rows");
}

fn push_event(events: &mut Vec<EventRecord>, session_id: SessionId, event: Event) {
    events.push(EventRecord {
        id: Uuid::now_v7(),
        session_id,
        sequence_num: events.len() as u64 + 1,
        event_type: event.event_type(),
        event,
        timestamp: Utc::now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    });
}

fn suite_path(config: &MoaConfig, workspace_id: &WorkspaceId, skill_name: &str) -> PathBuf {
    PathBuf::from(&config.local.memory_dir)
        .join("workspaces")
        .join(workspace_id.as_str())
        .join("skills")
        .join(slugify_skill_name(skill_name))
        .join("tests")
        .join("suite.toml")
}

struct TestProvider {
    responses: Mutex<VecDeque<String>>,
}

impl TestProvider {
    fn new(responses: VecDeque<String>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LLMProvider for TestProvider {
    fn name(&self) -> &str {
        "skill-test"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("scripted-skill-model"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, _request: CompletionRequest) -> moa_core::Result<CompletionStream> {
        let text = self
            .responses
            .lock()
            .map_err(|error| MoaError::ProviderError(format!("test provider poisoned: {error}")))?
            .pop_front()
            .ok_or_else(|| {
                MoaError::ProviderError("skill test provider ran out of responses".to_string())
            })?;
        let output_tokens = text.chars().count().div_ceil(4);
        Ok(CompletionStream::from_response(CompletionResponse {
            text: text.clone(),
            content: vec![CompletionContent::Text(text)],
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("scripted-skill-model"),
            usage: TokenUsage {
                input_tokens_uncached: 32,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens,
            },
            duration_ms: 1,
            thought_signature: None,
        }))
    }
}

/// Returns a workspace scope for tests.
pub fn workspace_scope(workspace_id: &WorkspaceId) -> MemoryScope {
    MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    }
}

/// Returns a user scope for tests.
pub fn user_scope(workspace_id: &WorkspaceId, user_id: &UserId) -> MemoryScope {
    MemoryScope::User {
        workspace_id: workspace_id.clone(),
        user_id: user_id.clone(),
    }
}

pub mod skill_graph;
