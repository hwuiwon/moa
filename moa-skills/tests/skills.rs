//! Integration tests for skill parsing, registry loading, distillation, and improvement.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use moa_core::{
    CompletionRequest, CompletionResponse, CompletionStream, Event, EventRecord, LLMProvider,
    MemoryStore, Platform, Result, SessionId, SessionMeta, StopReason, TokenPricing,
    ToolCallFormat, UserId, WorkspaceId,
};
use moa_memory::FileMemoryStore;
use moa_skills::{
    SkillRegistry, build_skill_path, maybe_distill_skill, maybe_improve_skill,
    parse_skill_markdown, skill_from_wiki_page, wiki_page_from_skill,
};
use tempfile::tempdir;
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
description: "Investigate and fix OAuth refresh-token bugs"
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
            duration_ms: 1,
        }))
    }
}

fn session() -> SessionMeta {
    let timestamp = Utc.with_ymd_and_hms(2026, 4, 9, 16, 45, 0).unwrap();
    SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        platform: Platform::Tui,
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
                    tool_id: Uuid::new_v4(),
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
                id: Uuid::new_v4(),
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

#[test]
fn parses_skill_markdown() {
    let skill = parse_skill_markdown(DISTILLED_SKILL).unwrap();

    assert_eq!(skill.frontmatter.name, "debug-oauth-refresh");
    assert_eq!(skill.frontmatter.moa.estimated_tokens, 900);
}

#[tokio::test]
async fn registry_lists_skill_metadata() -> Result<()> {
    let dir = tempdir()?;
    let memory = Arc::new(FileMemoryStore::new(dir.path()).await?);
    let scope = moa_core::MemoryScope::Workspace(WorkspaceId::new("workspace"));
    let skill = parse_skill_markdown(DISTILLED_SKILL)?;
    let path = build_skill_path(&skill.frontmatter.name);
    let page = wiki_page_from_skill(&skill, Some(path.clone()))?;
    memory.write_page_in_scope(&scope, &path, page).await?;

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

    let distilled = maybe_distill_skill(&session, &events, memory.clone(), llm).await?;

    assert!(distilled.is_some());
    let metadata = distilled.unwrap();
    let stored = memory
        .read_page_in_scope(
            &moa_core::MemoryScope::Workspace(session.workspace_id.clone()),
            &metadata.path,
        )
        .await?;
    assert!(stored.content.contains("Debug OAuth refresh"));
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
    memory.write_page_in_scope(&scope, &path, page).await?;

    let llm: Arc<dyn LLMProvider> = Arc::new(MockLlm {
        response: Arc::new(Mutex::new(IMPROVED_SKILL.to_string())),
    });
    let existing = moa_skills::skill_metadata_from_document(path.clone(), &original);
    let improved = maybe_improve_skill(
        &session(),
        &existing,
        &tool_rich_events(),
        memory.clone(),
        llm,
    )
    .await?;

    assert!(improved.is_some());
    let updated = memory.read_page_in_scope(&scope, &path).await?;
    assert!(
        updated
            .content
            .contains("Add a regression test before changing code")
    );
    let reparsed = skill_from_wiki_page(&updated)?;
    assert_eq!(reparsed.frontmatter.version, "1.1");
    Ok(())
}
