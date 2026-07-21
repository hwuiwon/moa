//! Postgres-backed coverage for detached skill-learning workflow wiring.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::ArtifactRegistry;
use moa_brain::learning::attribution::attributions_for_experience;
use moa_brain::learning::experience::experience_from_assessment;
use moa_config::MoaConfig;
use moa_core::{
    error::MoaError, events::Event, traits::LLMProvider, traits::SessionStore as _,
    types::action_policy::ActionRuleScope, types::channel::Attachment, types::channel::Channel,
    types::completion::CompletionContent, types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::completion::CompletionStream,
    types::completion::StopReason, types::completion::TokenUsage, types::contact::SessionActorRef,
    types::experience::LearningCandidate, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateType, types::experience::LearningRiskClass,
    types::experience::TaskFingerprint, types::identifiers::ModelId, types::identifiers::SegmentId,
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::ToolCallId, types::model::ModelCapabilities,
    types::model::TokenPricing, types::model::ToolCallFormat, types::provider::ModelTier,
    types::segment_assessment::AssessmentPhase, types::segment_assessment::SegmentAssessment,
    types::segment_assessment::SegmentEvidence, types::segment_assessment::SegmentEvidenceKind,
    types::segment_assessment::SegmentEvidencePolarity, types::segment_assessment::SegmentOutcome,
    types::segments::TaskSegment, types::session::SessionMeta, types::session::SessionStatus,
    types::tools::ToolOutput,
};
use moa_orchestrator::workflows::skill_learning::{
    RecurrenceDispatch, RecurrenceSiblingRef, RunSkillLearningRequest,
    record_skill_learning_failure, run_skill_learning_for_experience,
};
use moa_providers::ModelRouter;
use moa_skills::recurrence::{
    MergedRecurrenceCluster, RecurrenceThresholds, qualify_recurrence_cluster,
};
use moa_skills::registry::SkillRegistry;
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::json;
use uuid::Uuid;

mod skill_learning {
    use super::*;

    #[tokio::test]
    async fn skill_learning_workflow_creates_proposed_candidate_and_draft_only_db() {
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
            None,
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
    async fn skill_learning_failure_records_warning_without_failing_turn_db() {
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

    #[tokio::test]
    async fn recurrence_dispatch_files_below_floor_with_evidence_and_siblings_db() {
        // Pins: three sub-floor sessions sharing a fingerprint qualify as recurrence
        // through the real store grouping, dispatch distillation on the strongest
        // exemplar with the relaxed floor, file exactly one proposal carrying
        // recurrence evidence, and pool the other members as held-out siblings; a
        // second tick observes the open proposal and files nothing.
        let test_db = bootstrap_test_db().await.expect("bootstrap recurrence db");
        let mut config = MoaConfig::default();
        config.database.url = test_db.database_url().to_string();
        config.query_rewrite.enabled = false;
        let tenant_id = TenantId::new();
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
        let task = "Rotate the tenant deploy token";
        let now = Utc::now();

        // Each session holds 4 tool calls — below the single-session floor of 8, at
        // or above the relaxed recurrence floor of 3. Confidence orders the exemplar.
        let strong = seed_recurrence_member(
            &test_db,
            tenant_id,
            &storage_partition_id,
            task,
            4,
            0.95,
            now - chrono::Duration::days(2),
        )
        .await;
        let mid = seed_recurrence_member(
            &test_db,
            tenant_id,
            &storage_partition_id,
            task,
            4,
            0.90,
            now - chrono::Duration::days(1),
        )
        .await;
        let weak = seed_recurrence_member(
            &test_db,
            tenant_id,
            &storage_partition_id,
            task,
            4,
            0.85,
            now,
        )
        .await;
        assert_eq!(strong.fingerprint_hash, mid.fingerprint_hash);
        assert_eq!(strong.fingerprint_hash, weak.fingerprint_hash);

        // Real cron discovery: the store grouping query plus the pure qualifier.
        let thresholds = RecurrenceThresholds::from_config(&config.learning.recurrence);
        let since = now - chrono::Duration::days(config.learning.recurrence.lookback_days);
        let clusters = test_db
            .store()
            .list_candidate_experience_groups(
                &tenant_id,
                since,
                config.learning.recurrence.max_candidate_groups,
            )
            .await
            .expect("group recurring clusters");
        assert_eq!(clusters.len(), 1, "the three members form one cluster");
        assert_eq!(clusters[0].members.len(), 3);
        let decisions = test_db
            .store()
            .list_skill_candidate_decisions_for_fingerprint(&tenant_id, &strong.fingerprint_hash)
            .await
            .expect("candidate decisions");
        assert!(
            decisions.is_empty(),
            "no prior candidate for this fingerprint"
        );
        let plan = qualify_recurrence_cluster(
            &MergedRecurrenceCluster::single(&clusters[0]),
            &decisions,
            &thresholds,
            now,
        )
        .expect("cluster qualifies for dispatch");
        assert_eq!(
            plan.exemplar.experience_id, strong.experience_id,
            "the highest-confidence member is the exemplar"
        );
        assert_eq!(plan.siblings.len(), 2);

        // Build the request the cron would send and run the workflow body directly.
        let request = RunSkillLearningRequest {
            session_id: plan.exemplar.session_id,
            experience_id: plan.exemplar.experience_id,
            recurrence: Some(RecurrenceDispatch {
                occurrences: plan.occurrences,
                merged_fingerprints: plan.merged_fingerprints.clone(),
                first_seen: plan.first_seen,
                last_seen: plan.last_seen,
                siblings: plan
                    .siblings
                    .iter()
                    .map(|sibling| RecurrenceSiblingRef {
                        session_id: sibling.session_id,
                        experience_id: sibling.experience_id,
                    })
                    .collect(),
            }),
        };
        let skill_name = unique_name("recurrence-skill");
        let skill = skill_markdown(
            &skill_name,
            "Rotate the tenant deploy token safely",
            "Follow the recurring rotation steps and verify the new token.",
        );
        // One create response for the exemplar, then one re-synthesis response per
        // sibling (kept UNCHANGED so the suite still accumulates as held-out).
        let report = run_skill_learning_for_experience(
            &config,
            Arc::new(test_db.store().clone()),
            scripted_router([skill, "UNCHANGED".to_string(), "UNCHANGED".to_string()]),
            None,
            request,
        )
        .await
        .expect("run recurrence learning");

        assert_eq!(report.outcome, "proposed");
        let candidate_id = report.candidate_id.expect("recurrence candidate id");
        let candidate = test_db
            .store()
            .get_learning_candidate(&tenant_id, candidate_id)
            .await
            .expect("load recurrence candidate")
            .expect("candidate exists");
        let recurrence_evidence = &candidate.payload["evidence"]["recurrence"];
        assert_eq!(recurrence_evidence["source"], "recurrence_mined");
        assert_eq!(recurrence_evidence["occurrences"], 3);
        assert_eq!(
            recurrence_evidence["member_experience_ids"]
                .as_array()
                .expect("member ids")
                .len(),
            3,
            "exemplar plus two siblings are recorded for the reviewer"
        );
        let sibling_suites = candidate.payload["accumulated_regression_suites"]
            .as_array()
            .expect("accumulated sibling suites");
        assert_eq!(
            sibling_suites.len(),
            2,
            "both cluster siblings pooled as held-out material"
        );

        // Second cron tick: the open proposal now suppresses re-dispatch.
        let decisions_after = test_db
            .store()
            .list_skill_candidate_decisions_for_fingerprint(&tenant_id, &strong.fingerprint_hash)
            .await
            .expect("candidate decisions after filing");
        assert!(
            decisions_after
                .iter()
                .any(|decision| decision.status == LearningCandidateStatus::Proposed),
            "the filed proposal is now visible to the next tick"
        );
        assert!(
            qualify_recurrence_cluster(
                &MergedRecurrenceCluster::single(&clusters[0]),
                &decisions_after,
                &thresholds,
                now
            )
            .is_none(),
            "an open proposal suppresses the next recurrence tick"
        );
    }

    #[tokio::test]
    async fn recurrence_recently_rejected_fingerprint_is_suppressed_db() {
        // Pins: a fingerprint whose candidate a reviewer rejected within the
        // cooldown is not re-dispatched even though it keeps recurring, so recurring
        // rejection cannot spam the review queue.
        let test_db = bootstrap_test_db()
            .await
            .expect("bootstrap recurrence rejection db");
        let config = MoaConfig::default();
        let tenant_id = TenantId::new();
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
        let task = "Reconcile the billing ledger";
        let now = Utc::now();
        let member = seed_recurrence_member(
            &test_db,
            tenant_id,
            &storage_partition_id,
            task,
            4,
            0.95,
            now - chrono::Duration::days(2),
        )
        .await;
        seed_recurrence_member(
            &test_db,
            tenant_id,
            &storage_partition_id,
            task,
            4,
            0.92,
            now - chrono::Duration::days(1),
        )
        .await;
        seed_recurrence_member(
            &test_db,
            tenant_id,
            &storage_partition_id,
            task,
            4,
            0.90,
            now,
        )
        .await;

        // A reviewer rejected this fingerprint's candidate today.
        let rejected = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id,
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            status: LearningCandidateStatus::Rejected,
            target_id: None,
            target_label: Some("reconcile-ledger".to_string()),
            task_fingerprint: Some(TaskFingerprint {
                hash: member.fingerprint_hash.clone(),
                normalized_summary: task.to_ascii_lowercase(),
                policy_version: "experience_v1".to_string(),
            }),
            task_facets: None,
            payload: json!({ "kind": "skill_draft_proposal" }),
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: None,
            risk_class: LearningRiskClass::Medium,
            promotion_requirements: vec!["human_review".to_string()],
            status_reason: Some("reviewer declined".to_string()),
            batch_id: None,
            created_at: now,
            updated_at: now,
        };
        test_db
            .store()
            .append_learning_candidate(&rejected)
            .await
            .expect("append rejected candidate");

        let thresholds = RecurrenceThresholds::from_config(&config.learning.recurrence);
        let since = now - chrono::Duration::days(config.learning.recurrence.lookback_days);
        let clusters = test_db
            .store()
            .list_candidate_experience_groups(
                &tenant_id,
                since,
                config.learning.recurrence.max_candidate_groups,
            )
            .await
            .expect("group recurring clusters");
        assert_eq!(clusters.len(), 1);
        let decisions = test_db
            .store()
            .list_skill_candidate_decisions_for_fingerprint(&tenant_id, &member.fingerprint_hash)
            .await
            .expect("candidate decisions");
        assert!(
            decisions
                .iter()
                .any(|decision| decision.status == LearningCandidateStatus::Rejected)
        );
        assert!(
            qualify_recurrence_cluster(
                &MergedRecurrenceCluster::single(&clusters[0]),
                &decisions,
                &thresholds,
                now
            )
            .is_none(),
            "a recently rejected fingerprint is suppressed within the cooldown"
        );
    }
}

async fn seed_experience_fixture(
    test_db: &moa_test_support::postgres::TestDb,
    _label: &str,
) -> (MoaConfig, RunSkillLearningRequest, StoragePartitionId) {
    let mut config = MoaConfig::default();
    config.database.url = test_db.database_url().to_string();
    config.query_rewrite.enabled = false;
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let member = seed_recurrence_member(
        test_db,
        tenant_id,
        &storage_partition_id,
        "Distill a reusable Rust workflow",
        config.learning.skills.min_tool_calls,
        0.95,
        Utc::now(),
    )
    .await;
    (
        config,
        RunSkillLearningRequest {
            session_id: member.session_id,
            experience_id: member.experience_id,
            recurrence: None,
        },
        storage_partition_id,
    )
}

/// One seeded recurrence cluster member: the identifiers the cron threads on.
struct SeededMember {
    session_id: SessionId,
    experience_id: Uuid,
    fingerprint_hash: String,
}

/// Seeds one assessed, resolved experience with a controllable tool-call count,
/// confidence, and creation time, sharing a fingerprint with any other member
/// seeded from the same task summary and tool set.
async fn seed_recurrence_member(
    test_db: &moa_test_support::postgres::TestDb,
    tenant_id: TenantId,
    storage_partition_id: &StoragePartitionId,
    task_summary: &str,
    tool_calls: usize,
    confidence: f64,
    created_at: chrono::DateTime<Utc>,
) -> SeededMember {
    let creator_id = Uuid::now_v7();
    let session = SessionMeta {
        id: SessionId::new(),
        tenant_id,
        title: Some(task_summary.to_string()),
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
                    task_summary: Some(task_summary.to_string()),
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
    for index in 0..tool_calls {
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
                    llm_ttft_ms: None,
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
                    task_summary: Some(task_summary.to_string()),
                    turn_count: 1,
                    tools_used: tools_used.clone(),
                    skills_activated: Vec::new(),
                    skills_used: Vec::new(),
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
        confidence,
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
        skills_used: Vec::new(),
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
        created_at,
    );
    let attributions = attributions_for_experience(&experience, &events, created_at);
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

    SeededMember {
        session_id: session.id,
        experience_id: experience.id,
        fingerprint_hash: experience.task_fingerprint.hash,
    }
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

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
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
