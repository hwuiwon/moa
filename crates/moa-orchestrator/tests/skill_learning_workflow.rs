//! Focused coverage for detached skill-learning workflow wiring.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::ArtifactRegistry;
use moa_brain::learning::attribution::attributions_for_experience;
use moa_brain::learning::experience::experience_from_assessment;
use moa_core::{
    ActionRuleScope, AssessmentPhase, Attachment, Channel, CompletionContent, CompletionRequest,
    CompletionResponse, CompletionStream, Event, LLMProvider, MoaConfig, MoaError,
    ModelCapabilities, ModelId, ModelTier, SegmentAssessment, SegmentEvidence, SegmentEvidenceKind,
    SegmentEvidencePolarity, SegmentId, SegmentOutcome, SessionActorRef, SessionId, SessionMeta,
    SessionStatus, SessionStore as _, StopReason, StoragePartitionId, TaskSegment, TenantId,
    TokenPricing, TokenUsage, ToolCallFormat, ToolCallId, ToolOutput,
};
use moa_orchestrator::workflows::skill_learning::{
    RunSkillLearningRequest, record_skill_learning_failure, run_skill_learning_for_experience,
};
use moa_providers::ModelRouter;
use moa_skills::registry::SkillRegistry;
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::json;
use uuid::Uuid;

mod skill_learning {
    use super::*;

    #[tokio::test]
    async fn skill_learning_workflow_creates_proposed_candidate_and_draft_only() {
        // Pins: the detached skill-learning body creates a proposed candidate and draft artifact without activating the skill.
        let test_db = bootstrap_test_db()
            .await
            .expect("bootstrap skill-learning db");
        let skill_name = unique_name("workflow-draft");
        let proposed = skill_markdown(
            &skill_name,
            "Create draft skill proposals from assessed experiences",
            "Follow the bounded experience evidence, generate the draft, and wait for review.",
        );
        let (config, request, storage_partition_id) =
            seed_experience_fixture(&test_db, "workflow-proposed").await;

        let report = run_skill_learning_for_experience(
            &config,
            Arc::new(test_db.store().clone()),
            scripted_router([proposed]),
            request.clone(),
        )
        .await
        .expect("run skill learning");

        assert_eq!(report.outcome, "proposed");
        let candidate_id = report.candidate_id.expect("proposal candidate id");
        let draft_uid = report
            .draft_artifact_revision_uid
            .expect("draft artifact revision id");
        let candidate = test_db
            .store()
            .get_learning_candidate(
                &tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate_id,
            )
            .await
            .expect("load proposed candidate")
            .expect("candidate exists");
        assert_eq!(candidate.status.as_str(), "proposed");
        assert_eq!(candidate.candidate_type.as_str(), "skill");
        assert_eq!(candidate.payload["operation"], "skill_created");
        assert_eq!(
            candidate.payload["source_experience_ids"][0],
            request.experience_id.to_string()
        );

        let scope = tenant_scope(&storage_partition_id);
        let revision = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft_uid)
            .await
            .expect("load draft revision")
            .expect("draft revision exists");
        assert_eq!(revision.kind, ArtifactKind::Skill);
        assert_eq!(revision.status, ArtifactStatus::Draft);
        assert!(
            SkillRegistry::new(test_db.store().pool().clone())
                .load_by_name(&scope, &skill_name)
                .await
                .expect("load optional active skill")
                .is_none(),
            "skill learning must not publish or materialize active skills"
        );
    }

    #[tokio::test]
    async fn skill_learning_failure_records_warning_without_failing_turn() {
        // Pins: skill-learning failures are warning events, not turn-failing errors.
        let test_db = bootstrap_test_db()
            .await
            .expect("bootstrap skill-learning warning db");
        let (_config, request, _storage_partition_id) =
            seed_experience_fixture(&test_db, "workflow-warning").await;
        let error = "scripted proposal generation failed";

        let warning = record_skill_learning_failure(
            test_db.store(),
            request.session_id,
            request.experience_id,
            error,
        )
        .await
        .expect("record warning");

        match warning.event {
            Event::Warning { message } => {
                assert!(message.contains("skill learning proposal generation failed"));
                assert!(message.contains(&request.experience_id.to_string()));
                assert!(message.contains(error));
            }
            other => panic!("expected warning event, got {other:?}"),
        }
        let session = test_db
            .store()
            .get_session(request.session_id)
            .await
            .expect("load session after warning");
        assert_eq!(session.status, SessionStatus::Completed);
    }
}

async fn seed_experience_fixture(
    test_db: &moa_test_support::postgres::TestDb,
    _label: &str,
) -> (MoaConfig, RunSkillLearningRequest, StoragePartitionId) {
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let creator_id = Uuid::now_v7();
    let session = SessionMeta {
        id: SessionId::new(),
        tenant_id,
        title: Some("Distill a reusable Rust workflow".to_string()),
        status: SessionStatus::Completed,
        channel: Channel::Chat,
        model: ModelId::new("scripted-skill-model"),
        created_by: Some(SessionActorRef::Identity { id: creator_id }),
        ..SessionMeta::default()
    };
    test_db
        .store()
        .create_session(session.clone())
        .await
        .expect("create session");
    let segment_id = SegmentId::new();
    let mut events = Vec::new();
    events.push(
        test_db
            .store()
            .emit_event_record(
                session.id,
                Event::SegmentStarted {
                    segment_id,
                    segment_index: 0,
                    task_summary: Some("Implement a reusable Rust workflow".to_string()),
                    previous_segment_id: None,
                },
                None,
            )
            .await
            .expect("append segment start"),
    );
    events.push(
        test_db
            .store()
            .emit_event_record(
                session.id,
                Event::UserMessage {
                    text: "Implement and test the Rust workflow".to_string(),
                    attachments: Vec::<Attachment>::new(),
                },
                None,
            )
            .await
            .expect("append user message"),
    );
    let mut tools_used = Vec::new();
    for index in 0..5 {
        let tool_name = format!("bash-{index}");
        let tool_id = ToolCallId::new();
        tools_used.push(tool_name.clone());
        events.push(
            test_db
                .store()
                .emit_event_record(
                    session.id,
                    Event::ToolCall {
                        tool_id,
                        provider_tool_use_id: None,
                        provider_thought_signature: None,
                        tool_name,
                        input: json!({ "cmd": "cargo test" }),
                        hand_id: None,
                    },
                    None,
                )
                .await
                .expect("append tool call"),
        );
        events.push(
            test_db
                .store()
                .emit_event_record(
                    session.id,
                    Event::ToolResult {
                        tool_id,
                        provider_tool_use_id: None,
                        output: ToolOutput::text("tests passed", Duration::from_millis(1)),
                        original_output_tokens: None,
                        success: true,
                        duration_ms: 1,
                    },
                    None,
                )
                .await
                .expect("append tool result"),
        );
    }
    events.push(
        test_db
            .store()
            .emit_event_record(
                session.id,
                Event::BrainResponse {
                    text: "Implemented, tested, and verified.".to_string(),
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
                None,
            )
            .await
            .expect("append assistant response"),
    );
    events.push(
        test_db
            .store()
            .emit_event_record(
                session.id,
                Event::SegmentCompleted {
                    segment_id,
                    segment_index: 0,
                    task_summary: Some("Implement a reusable Rust workflow".to_string()),
                    turn_count: 1,
                    tools_used: tools_used.clone(),
                    skills_activated: Vec::new(),
                    token_cost: 256,
                    duration_ms: 1_000,
                },
                None,
            )
            .await
            .expect("append segment complete"),
    );
    let assessment = SegmentAssessment {
        outcome: SegmentOutcome::Resolved,
        confidence: 0.95,
        phase: AssessmentPhase::Immediate,
        evidence: vec![SegmentEvidence {
            kind: SegmentEvidenceKind::Verification,
            polarity: SegmentEvidencePolarity::SupportsResolved,
            strength: 0.9,
            summary: "Focused tests passed".to_string(),
        }],
        assessed_at: Utc::now(),
        policy_version: "test-assessor".to_string(),
    };
    let segment = TaskSegment {
        id: segment_id,
        session_id: session.id,
        tenant_id: storage_partition_id.to_string(),
        segment_index: 0,
        task_summary: Some("Implement a reusable Rust workflow".to_string()),
        started_at: events[0].timestamp,
        ended_at: Some(Utc::now()),
        turn_count: 1,
        tools_used,
        skills_activated: Vec::new(),
        token_cost: 256,
        previous_segment_id: None,
        outcome: Some(SegmentOutcome::Resolved.as_str().to_string()),
        assessment: Some(assessment.clone()),
        outcome_confidence: Some(assessment.confidence),
    };
    test_db
        .store()
        .create_segment(&segment)
        .await
        .expect("create task segment");
    let experience = experience_from_assessment(
        &session,
        &segment,
        &assessment,
        &events,
        None,
        Some(1_000),
        Utc::now(),
    );
    let attributions = attributions_for_experience(&experience, &events, Utc::now());
    test_db
        .store()
        .append_experience_record(&experience)
        .await
        .expect("append experience");
    test_db
        .store()
        .append_experience_attributions(&attributions)
        .await
        .expect("append attributions");

    let mut config = MoaConfig::default();
    config.database.url = test_db.database_url().to_string();
    config.query_rewrite.enabled = false;
    (
        config,
        RunSkillLearningRequest {
            session_id: session.id,
            experience_id: experience.id,
        },
        storage_partition_id,
    )
}

fn scripted_router(responses: impl IntoIterator<Item = impl Into<String>>) -> Arc<ModelRouter> {
    Arc::new(ModelRouter::new(
        Arc::new(TestProvider {
            responses: Mutex::new(responses.into_iter().map(Into::into).collect()),
        }),
        None,
    ))
}

fn skill_markdown(name: &str, description: &str, body: &str) -> String {
    format!(
        "---\n\
         name: {name}\n\
         description: \"{description}\"\n\
         allowed-tools: bash file_search file_read\n\
         metadata:\n\
           moa-version: \"1.0\"\n\
           moa-tags: \"rust, workflow\"\n\
           moa-estimated-tokens: \"300\"\n\
         ---\n\n\
         # {name}\n\n\
         {body}\n"
    )
}

fn tenant_scope(storage_partition_id: &StoragePartitionId) -> ActionRuleScope {
    ActionRuleScope::Tenant {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
    }
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::now_v7().simple())
}

struct TestProvider {
    responses: Mutex<VecDeque<String>>,
}

#[async_trait]
impl LLMProvider for TestProvider {
    fn name(&self) -> &str {
        "skill-learning-test"
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
                MoaError::ProviderError(
                    "skill-learning test provider ran out of responses".to_string(),
                )
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
