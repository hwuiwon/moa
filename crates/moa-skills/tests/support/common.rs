//! Shared skill distillation/improvement integration-test fixtures.
//!
//! Consolidates the session-fixture, scripted-provider, and skill-seeding helpers
//! previously duplicated across the `distillation`, `draft_proposals`, `improver`,
//! and `regression` test binaries. Each binary uses only a subset of these helpers,
//! so the module allows dead code rather than warning per binary.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    ActionRuleScope, Attachment, Channel, CompletionContent, CompletionRequest, CompletionResponse,
    CompletionStream, Event, EventRecord, LLMProvider, MoaConfig, MoaError, ModelCapabilities,
    ModelId, ModelTier, SessionId, SessionMeta, SessionStatus, StopReason, StoragePartitionId,
    TenantId, TokenPricing, TokenUsage, ToolCallFormat, ToolCallId, ToolOutput,
};
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;
use moa_skills::format::{
    build_skill_path, parse_skill_markdown, render_skill_markdown, skill_metadata_from_document,
};
use moa_skills::registry::{NewSkill, Skill, SkillRegistry};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
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
/// Improvement fixture that renames the target skill; the improver must reject it.
pub const RENAMED_SKILL: &str = include_str!("fixtures/renamed_skill_diff.md");

#[derive(Debug, Deserialize)]
struct SessionFixture {
    session_id: Uuid,
    storage_partition_id: String,
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

/// Bootstraps an isolated Postgres test database, failing loudly when Postgres is unavailable.
///
/// These tests run only in the `db-memory` nextest lane. A missing database is a hard error
/// (panic) rather than a silent skip so the suite can never report a vacuous green without
/// exercising the real distillation/improvement path.
pub async fn setup_test_db() -> TestDb {
    bootstrap_test_db().await.expect(
        "bootstrap skills Postgres test database; start the compose Postgres or set MOA_DATABASE_URL",
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
    let storage_partition_id = StoragePartitionId::new(format!(
        "{}-{}",
        fixture.storage_partition_id,
        Uuid::now_v7().simple()
    ));
    let _user_id = fixture.user_id;
    let session = SessionMeta {
        id: SessionId(fixture.session_id),
        tenant_id: tenant_id_from_storage_partition(&storage_partition_id),
        title: Some(fixture.task.clone()),
        status: SessionStatus::Completed,
        channel: Channel::Chat,
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
           moa-tags: \"auth, regression\"\n\
           moa-estimated-tokens: \"300\"\n\
         ---\n\n\
         # {name}\n\n\
         {body}\n"
    )
}

/// Seeds one skill and returns its pipeline metadata.
pub async fn seed_skill(
    test_db: &TestDb,
    scope: ActionRuleScope,
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

/// Loads the active skill row by name when one exists.
pub async fn load_optional_active_skill(
    test_db: &TestDb,
    scope: &ActionRuleScope,
    skill_name: &str,
) -> Option<Skill> {
    SkillRegistry::new(test_db.store().pool().clone())
        .load_by_name(scope, skill_name)
        .await
        .expect("load optional active skill")
}

/// Returns the active semantic version parsed from the skill markdown.
pub async fn active_semantic_version(
    test_db: &TestDb,
    scope: &ActionRuleScope,
    skill_name: &str,
) -> String {
    let markdown = load_active_skill_markdown(test_db, scope, skill_name).await;
    parse_skill_markdown(&markdown)
        .expect("parse active skill")
        .frontmatter
        .version()
}

/// Counts artifact revisions for one tenant skill name.
pub async fn skill_row_count(
    test_db: &TestDb,
    storage_partition_id: &StoragePartitionId,
    skill_name: &str,
) -> i64 {
    artifact_revision_count(test_db, storage_partition_id, skill_name).await
}

/// Counts artifact revisions for one tenant skill artifact.
pub async fn artifact_revision_count(
    test_db: &TestDb,
    storage_partition_id: &StoragePartitionId,
    skill_name: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) \
         FROM moa.artifact a \
         JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid \
         WHERE a.storage_partition_id = $1 AND a.kind = 'skill' AND a.name = $2",
    )
    .bind(storage_partition_id.as_str())
    .bind(skill_name)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count skill artifact revisions")
}

async fn load_active_skill_markdown(
    test_db: &TestDb,
    scope: &ActionRuleScope,
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

/// Returns a tenant artifact-visibility scope for tests.
pub fn tenant_scope(storage_partition_id: &StoragePartitionId) -> ActionRuleScope {
    ActionRuleScope::Tenant {
        tenant_id: tenant_id_from_storage_partition(storage_partition_id),
    }
}

/// Returns the tenant storage key for session-scoped learning rows.
pub fn session_storage_partition_id(session: &SessionMeta) -> StoragePartitionId {
    StoragePartitionId::for_tenant(session.tenant_id)
}

fn tenant_id_from_storage_partition(storage_partition_id: &StoragePartitionId) -> TenantId {
    if let Ok(uuid) = Uuid::parse_str(storage_partition_id.as_str()) {
        return TenantId::from(uuid);
    }
    let digest = Sha256::digest(storage_partition_id.as_str().as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TenantId::from(Uuid::from_bytes(bytes))
}
