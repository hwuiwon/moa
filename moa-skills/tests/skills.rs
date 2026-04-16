//! Integration tests for skill parsing, registry loading, distillation, and improvement.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use moa_core::{
    CompletionRequest, CompletionResponse, CompletionStream, Event, EventRecord, LLMProvider,
    MemoryStore, MoaConfig, Platform, Result, SessionId, SessionMeta, StopReason, TokenPricing,
    TokenUsage, ToolCallFormat, UserId, WorkspaceId,
};
use moa_memory::FileMemoryStore;
use moa_skills::{
    SkillRegistry, build_skill_path, maybe_distill_skill, maybe_improve_skill,
    parse_skill_markdown, skill_from_wiki_page, wiki_page_from_skill,
};
use tempfile::tempdir;
use tokio::fs;
use tokio::sync::Mutex;
use uuid::Uuid;

const DISTILLED_SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate and fix OAuth refresh-token bugs"
compatibility: "Requires local repo access"
allowed-tools: bash file_read file_search
metadata:
  moa-version: "1.0"
  moa-one-liner: "Repeatable OAuth refresh-token debugging workflow"
  moa-tags: "oauth, auth, debugging"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T16:00:00Z"
  moa-auto-generated: "true"
  moa-source-session: "session-1"
  moa-use-count: "0"
  moa-last-used: "2026-04-09T16:00:00Z"
  moa-success-rate: "1.0"
  moa-brain-affinity: "coding"
  moa-sandbox-tier: "container"
  moa-estimated-tokens: "900"
---

# Debug OAuth refresh

1. Reproduce the bug.
2. Inspect the refresh-token path.
3. Verify the fix.
"#;

const IMPROVED_SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate and fix OAuth refresh-token bugs with regression checks"
compatibility: "Requires local repo access"
allowed-tools: bash file_read file_search file_write
metadata:
  moa-version: "1.0"
  moa-one-liner: "Repeatable OAuth refresh-token debugging workflow with regression checks"
  moa-tags: "oauth, auth, debugging"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T16:30:00Z"
  moa-auto-generated: "true"
  moa-source-session: "session-2"
  moa-use-count: "0"
  moa-last-used: "2026-04-09T16:30:00Z"
  moa-success-rate: "1.0"
  moa-brain-affinity: "coding"
  moa-sandbox-tier: "container"
  moa-estimated-tokens: "950"
---

# Debug OAuth refresh

1. Reproduce the bug.
2. Add a regression test before changing code.
3. Inspect the refresh-token path.
4. Verify the fix and the new test.
"#;

const REGRESSED_SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate and fix OAuth refresh-token bugs quickly"
compatibility: "Requires local repo access"
allowed-tools: bash file_read
metadata:
  moa-version: "1.0"
  moa-one-liner: "Shortened OAuth refresh-token workflow"
  moa-tags: "oauth, auth, debugging"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T16:30:00Z"
  moa-auto-generated: "true"
  moa-source-session: "session-2"
  moa-use-count: "0"
  moa-last-used: "2026-04-09T16:30:00Z"
  moa-success-rate: "1.0"
  moa-brain-affinity: "coding"
  moa-sandbox-tier: "container"
  moa-estimated-tokens: "700"
---

# Debug OAuth refresh

1. Reproduce the bug.
2. Inspect the refresh-token path.
3. Ship the fix quickly.
"#;

fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input_tokens,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens,
    }
}

#[derive(Clone)]
struct MockLlm {
    response: Arc<Mutex<String>>,
}

#[async_trait]
impl LLMProvider for MockLlm {
    fn name(&self) -> &str {
        "mock-skills"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: "mock".to_string(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: false,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
        let text = self.response.lock().await.clone();
        Ok(CompletionStream::from_response(CompletionResponse {
            text: text.clone(),
            content: vec![moa_core::CompletionContent::Text(text)],
            stop_reason: StopReason::EndTurn,
            model: "mock".to_string(),
            input_tokens: 10,
            output_tokens: 20,
            cached_input_tokens: 0,
            usage: token_usage(10, 20),
            duration_ms: 1,
            thought_signature: None,
        }))
    }
}

#[derive(Clone)]
struct ImprovementAndEvalLlm {
    improvement_response: Arc<Mutex<String>>,
    input_per_mtok: f64,
    output_per_mtok: f64,
}

#[async_trait]
impl LLMProvider for ImprovementAndEvalLlm {
    fn name(&self) -> &str {
        "mock-skill-regression"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: "mock".to_string(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: self.input_per_mtok,
                output_per_mtok: self.output_per_mtok,
                cached_input_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let prompt = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let text = if prompt.contains("You are improving an existing MOA Agent Skill.") {
            self.improvement_response.lock().await.clone()
        } else if prompt.contains("regression checks") {
            "Regression verified and fix confirmed".to_string()
        } else {
            "Fix confirmed".to_string()
        };

        Ok(CompletionStream::from_response(CompletionResponse {
            text: text.clone(),
            content: vec![moa_core::CompletionContent::Text(text)],
            stop_reason: StopReason::EndTurn,
            model: "mock".to_string(),
            input_tokens: 10,
            output_tokens: 20,
            cached_input_tokens: 0,
            usage: token_usage(10, 20),
            duration_ms: 1,
            thought_signature: None,
        }))
    }
}

fn session() -> SessionMeta {
    let timestamp = Utc.with_ymd_and_hms(2026, 4, 9, 16, 45, 0).unwrap();
    SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        platform: Platform::Desktop,
        model: "claude-sonnet-4-6".to_string(),
        created_at: timestamp,
        updated_at: timestamp,
        ..SessionMeta::default()
    }
}

fn tool_rich_events() -> Vec<EventRecord> {
    let session_id = SessionId::new();
    (0..7)
        .map(|index| {
            let event = if index == 0 {
                Event::UserMessage {
                    text: "Debug the OAuth refresh token failure and update the notes.".to_string(),
                    attachments: Vec::new(),
                }
            } else {
                Event::ToolCall {
                    tool_id: Uuid::now_v7(),
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: if index % 2 == 0 {
                        "bash".to_string()
                    } else {
                        "file_read".to_string()
                    },
                    input: serde_json::json!({ "step": index }),
                    hand_id: None,
                }
            };
            EventRecord {
                id: Uuid::now_v7(),
                session_id: session_id.clone(),
                sequence_num: index as u64,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            }
        })
        .collect()
}

async fn write_skill_suite(
    root: &std::path::Path,
    skill_path: &moa_core::MemoryPath,
) -> Result<()> {
    let relative = std::path::Path::new(skill_path.as_str())
        .parent()
        .unwrap_or_else(|| std::path::Path::new("skills"));
    let suite_path = root.join(relative).join("tests").join("suite.toml");
    if let Some(parent) = suite_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(
        suite_path,
        r#"[suite]
name = "debug-oauth-refresh-regression"
default_timeout_seconds = 30

[[cases]]
name = "regression-check"
input = "Debug the OAuth refresh token failure."
timeout_seconds = 30
expected_trajectory = []

[cases.expected_output]
contains = ["regression"]
"#,
    )
    .await?;
    Ok(())
}

#[test]
fn parses_skill_markdown() {
    let skill = parse_skill_markdown(DISTILLED_SKILL).unwrap();

    assert_eq!(skill.frontmatter.name, "debug-oauth-refresh");
    assert_eq!(skill.frontmatter.estimated_tokens(&skill.body), 900);
}

#[tokio::test]
async fn registry_lists_skill_metadata() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let skill = parse_skill_markdown(DISTILLED_SKILL)?;
    let path = build_skill_path(&skill.frontmatter.name);
    let page = wiki_page_from_skill(&skill, Some(path.clone()))?;
    memory.write_page(scope.clone(), &path, page).await?;

    let registry_memory: Arc<dyn MemoryStore> = memory.clone();
    let registry = SkillRegistry::new(registry_memory);
    let skills = registry
        .list_for_pipeline(&WorkspaceId::new("workspace"))
        .await?;

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "debug-oauth-refresh");
    assert_eq!(skills[0].estimated_tokens, 900);
    Ok(())
}

#[tokio::test]
async fn distills_skill_after_tool_heavy_session() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let llm: Arc<dyn LLMProvider> = Arc::new(MockLlm {
        response: Arc::new(Mutex::new(DISTILLED_SKILL.to_string())),
    });
    let session = session();
    let events = tool_rich_events();

    let distilled = maybe_distill_skill(
        &MoaConfig::default(),
        &session,
        &events,
        memory.clone(),
        llm,
    )
    .await?;

    assert!(distilled.is_some());
    let metadata = distilled.unwrap();
    let stored = memory
        .read_page(
            moa_core::MemoryScope::Workspace(session.workspace_id.clone()),
            &metadata.path,
        )
        .await?;
    assert!(stored.content.contains("Debug OAuth refresh"));
    let suite_path = dir
        .path()
        .join("workspaces")
        .join(session.workspace_id.as_str())
        .join("memory")
        .join("skills")
        .join("debug-oauth-refresh")
        .join("tests")
        .join("suite.toml");
    assert!(fs::try_exists(suite_path).await?);
    Ok(())
}

#[tokio::test]
async fn improves_existing_skill_when_better_flow_is_found() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let original = parse_skill_markdown(DISTILLED_SKILL)?;
    let path = build_skill_path(&original.frontmatter.name);
    let page = wiki_page_from_skill(&original, Some(path.clone()))?;
    memory.write_page(scope.clone(), &path, page).await?;

    let llm: Arc<dyn LLMProvider> = Arc::new(MockLlm {
        response: Arc::new(Mutex::new(IMPROVED_SKILL.to_string())),
    });
    let existing = moa_skills::skill_metadata_from_document(path.clone(), &original);
    let improved = maybe_improve_skill(
        &MoaConfig::default(),
        &session(),
        &existing,
        &tool_rich_events(),
        memory.clone(),
        llm,
    )
    .await?;

    assert!(improved.is_some());
    let updated = memory.read_page(scope.clone(), &path).await?;
    assert!(
        updated
            .content
            .contains("Add a regression test before changing code")
    );
    let reparsed = skill_from_wiki_page(&updated)?;
    assert_eq!(reparsed.frontmatter.version(), "1.1");
    Ok(())
}

#[tokio::test]
async fn improvement_accepted_when_scores_better() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let original = parse_skill_markdown(DISTILLED_SKILL)?;
    let path = build_skill_path(&original.frontmatter.name);
    let page = wiki_page_from_skill(&original, Some(path.clone()))?;
    memory.write_page(scope.clone(), &path, page).await?;
    write_skill_suite(
        &dir.path()
            .join("workspaces")
            .join("workspace")
            .join("memory"),
        &path,
    )
    .await?;

    let llm: Arc<dyn LLMProvider> = Arc::new(ImprovementAndEvalLlm {
        improvement_response: Arc::new(Mutex::new(IMPROVED_SKILL.to_string())),
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
    });
    let existing = moa_skills::skill_metadata_from_document(path.clone(), &original);

    let improved = maybe_improve_skill(
        &MoaConfig::default(),
        &session(),
        &existing,
        &tool_rich_events(),
        memory.clone(),
        llm,
    )
    .await?;

    assert!(improved.is_some());
    let updated = memory.read_page(scope.clone(), &path).await?;
    assert!(
        updated
            .metadata
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .contains("regression checks")
    );
    Ok(())
}

#[tokio::test]
async fn improvement_rejected_on_regression() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let original = parse_skill_markdown(IMPROVED_SKILL)?;
    let path = build_skill_path(&original.frontmatter.name);
    let page = wiki_page_from_skill(&original, Some(path.clone()))?;
    memory.write_page(scope.clone(), &path, page).await?;
    write_skill_suite(
        &dir.path()
            .join("workspaces")
            .join("workspace")
            .join("memory"),
        &path,
    )
    .await?;

    let llm: Arc<dyn LLMProvider> = Arc::new(ImprovementAndEvalLlm {
        improvement_response: Arc::new(Mutex::new(REGRESSED_SKILL.to_string())),
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
    });
    let existing = moa_skills::skill_metadata_from_document(path.clone(), &original);

    let improved = maybe_improve_skill(
        &MoaConfig::default(),
        &session(),
        &existing,
        &tool_rich_events(),
        memory.clone(),
        llm,
    )
    .await?;

    assert!(improved.is_none());
    let restored = memory.read_page(scope.clone(), &path).await?;
    let restored_skill = skill_from_wiki_page(&restored)?;
    assert!(
        restored_skill
            .frontmatter
            .description
            .contains("regression checks")
    );
    assert_eq!(restored_skill.frontmatter.regression_count(), 1);
    Ok(())
}

#[tokio::test]
async fn no_tests_means_unconditional_accept() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let original = parse_skill_markdown(DISTILLED_SKILL)?;
    let path = build_skill_path(&original.frontmatter.name);
    let page = wiki_page_from_skill(&original, Some(path.clone()))?;
    memory.write_page(scope.clone(), &path, page).await?;

    let llm: Arc<dyn LLMProvider> = Arc::new(MockLlm {
        response: Arc::new(Mutex::new(IMPROVED_SKILL.to_string())),
    });
    let existing = moa_skills::skill_metadata_from_document(path.clone(), &original);

    let improved = maybe_improve_skill(
        &MoaConfig::default(),
        &session(),
        &existing,
        &tool_rich_events(),
        memory.clone(),
        llm,
    )
    .await?;

    assert!(improved.is_some());
    Ok(())
}

#[tokio::test]
async fn log_entry_written_for_regression_attempt() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let original = parse_skill_markdown(DISTILLED_SKILL)?;
    let path = build_skill_path(&original.frontmatter.name);
    let page = wiki_page_from_skill(&original, Some(path.clone()))?;
    memory.write_page(scope.clone(), &path, page).await?;
    write_skill_suite(
        &dir.path()
            .join("workspaces")
            .join("workspace")
            .join("memory"),
        &path,
    )
    .await?;

    let llm: Arc<dyn LLMProvider> = Arc::new(ImprovementAndEvalLlm {
        improvement_response: Arc::new(Mutex::new(IMPROVED_SKILL.to_string())),
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
    });
    let existing = moa_skills::skill_metadata_from_document(path.clone(), &original);

    maybe_improve_skill(
        &MoaConfig::default(),
        &session(),
        &existing,
        &tool_rich_events(),
        memory.clone(),
        llm,
    )
    .await?;

    let log = memory
        .load_scope_log(&moa_core::MemoryScope::Workspace(WorkspaceId::new(
            "workspace",
        )))
        .await?;
    assert!(log.contains("skill_improvement"));
    assert!(log.contains("debug-oauth-refresh"));
    Ok(())
}

#[tokio::test]
async fn budget_limit_skips_expensive_tests() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let original = parse_skill_markdown(DISTILLED_SKILL)?;
    let path = build_skill_path(&original.frontmatter.name);
    let page = wiki_page_from_skill(&original, Some(path.clone()))?;
    memory.write_page(scope.clone(), &path, page).await?;
    write_skill_suite(
        &dir.path()
            .join("workspaces")
            .join("workspace")
            .join("memory"),
        &path,
    )
    .await?;

    let llm: Arc<dyn LLMProvider> = Arc::new(ImprovementAndEvalLlm {
        improvement_response: Arc::new(Mutex::new(IMPROVED_SKILL.to_string())),
        input_per_mtok: 50_000.0,
        output_per_mtok: 50_000.0,
    });
    let existing = moa_skills::skill_metadata_from_document(path.clone(), &original);

    let improved = maybe_improve_skill(
        &MoaConfig::default(),
        &session(),
        &existing,
        &tool_rich_events(),
        memory.clone(),
        llm,
    )
    .await?;

    assert!(improved.is_some());
    let log = memory
        .load_scope_log(&moa_core::MemoryScope::Workspace(WorkspaceId::new(
            "workspace",
        )))
        .await?;
    assert!(log.contains("exceeds budget"));
    Ok(())
}
