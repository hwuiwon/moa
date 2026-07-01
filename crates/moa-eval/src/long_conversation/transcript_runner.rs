//! Multi-turn transcript runner for long-conversation eval cases.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use moa_brain::{
    StreamedTurnResult,
    learning::{
        attribution::attributions_for_experience, candidates::propose_candidates_for_experience,
        experience::experience_from_assessment,
    },
    run_streamed_turn_with_lineage,
};
use moa_core::transcript::Transcript;
use moa_core::{
    AssessmentPhase, AttributionSubjectType, CompletionRequest, CompletionStream, ConversationCost,
    Event, EventRange, EventRecord, LLMProvider, LearningCandidateStatus, MoaConfig, MoaError,
    ModelCapabilities, RuntimeEvent, SegmentAssessment, SegmentEvidence, SegmentEvidenceKind,
    SegmentEvidencePolarity, SegmentOutcome, SessionId, SessionMeta, SessionStore, TaskSegment,
    deterministic_segment_id,
};
use moa_eval_core::{
    AgentConfig, EngineOptions, EvalError, EvalResult, EvalScore, EvalScoreValue, EvalStatus,
    LongConversationMode, LongSessionInterleaving, LongTestCase, Result, TestCase,
};
use moa_lineage_core::LineageEvent;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::memory_metrics::{
    MemoryScenario, compute_planted_fact_recall, count_consolidation_outcomes, count_pages_written,
};
use super::provider_recorded::RecordedScriptedProvider;
use super::score_card::{
    CacheScores, ContextScores, CoordinationScores, CostScores, FunctionalScores, LatencyScores,
    MemoryScores, SafetyScores, ScoreCard, ToolScores,
};
use super::scripted_user::{ScriptedUserScript, ScriptedUserTurn};
use crate::collector::{CollectedExecution, TrajectoryCollector};
use crate::setup::build_agent_environment_with_provider;

const MAX_LONG_CONVERSATION_AGENT_TURNS: usize = 32;

/// Default recall@K depth used for planted-fact recall when a case omits `recall_k`.
const DEFAULT_PLANTED_FACT_RECALL_K: usize = 25;

/// Result of running a recorded long-conversation scenario.
#[derive(Debug, Clone)]
pub struct LongRunReport {
    /// Eval result returned through the existing eval engine surface.
    pub result: EvalResult,
    /// Structured long-conversation score card.
    pub score_card: ScoreCard,
    /// Lineage score rows ready for `analytics.scores`.
    pub score_records: Vec<moa_lineage_core::ScoreRecord>,
    /// Raw lineage events emitted through the eval run's hot-path lineage handle.
    pub lineage_events: Vec<Value>,
    /// Persisted event payloads emitted during the run.
    pub events: Vec<Event>,
    /// Learning artifacts persisted while the scenario ran.
    pub learning: LearningRunSummary,
    /// Skill manifests observed in provider requests during replay.
    pub skill_manifest_observations: Vec<SkillManifestObservation>,
    /// Primary-vs-secondary comparison for phased multi-session scenarios.
    pub phase_comparison: Option<PhaseComparison>,
}

/// Persisted learning artifact counts for a long-conversation run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LearningRunSummary {
    /// Number of experience records linked to the scenario sessions.
    pub experience_count: usize,
    /// Number of experience attribution rows linked to the scenario sessions.
    pub attribution_count: usize,
    /// Number of proposed learning candidates for the scenario tenant.
    pub proposed_candidate_count: usize,
    /// Skill subject IDs present in task-conditioned strategy-rate rows.
    pub task_strategy_skill_subjects: Vec<String>,
}

/// Skill manifest parsed from one recorded provider request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillManifestObservation {
    /// Latest user message associated with the request.
    pub user_message: Option<String>,
    /// Skill names listed in the compact manifest for the request.
    pub selected_skills: Vec<String>,
}

/// Deterministic primary-vs-secondary effort comparison for phased scenarios.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseComparison {
    /// User turns in the primary phase.
    pub primary_turns: usize,
    /// User turns in the secondary phase.
    pub secondary_turns: usize,
    /// Provider input tokens in the primary phase.
    pub primary_input_tokens: usize,
    /// Provider input tokens in the secondary phase.
    pub secondary_input_tokens: usize,
    /// Provider output tokens in the primary phase.
    pub primary_output_tokens: usize,
    /// Provider output tokens in the secondary phase.
    pub secondary_output_tokens: usize,
}

/// Runs a long-conversation test case with an explicit provider.
pub async fn run_scenario_with_provider(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    options: &EngineOptions,
    case: &TestCase,
    llm_provider: Arc<dyn LLMProvider>,
) -> Result<LongRunReport> {
    let long_case = case.long_case()?;

    match long_case.mode {
        LongConversationMode::Recorded => {
            run_recorded_scenario_with_provider(
                base_config,
                agent_config,
                options,
                case,
                long_case,
                llm_provider,
            )
            .await
        }
        LongConversationMode::ScriptedUser => {
            run_scripted_user_scenario_with_provider(
                base_config,
                agent_config,
                options,
                case,
                long_case,
                llm_provider,
            )
            .await
        }
        LongConversationMode::Live => Err(EvalError::InvalidConfig(
            "long conversation live mode is not implemented".to_string(),
        )),
    }
}

async fn run_recorded_scenario_with_provider(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    options: &EngineOptions,
    case: &TestCase,
    long_case: &LongTestCase,
    llm_provider: Arc<dyn LLMProvider>,
) -> Result<LongRunReport> {
    let transcript_path = resolve_path(&long_case.transcript)?;
    let transcript = Transcript::read_jsonl(&transcript_path).map_err(|error| {
        EvalError::InvalidConfig(format!(
            "failed to read transcript {}: {error}",
            transcript_path.display()
        ))
    })?;
    let environment = build_agent_environment_with_provider(
        base_config,
        agent_config,
        &options.temp_dir,
        llm_provider,
    )
    .await?;
    let run_root = environment
        .workspace_dir
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| environment.workspace_dir.clone());

    let outcome = if let Some(secondary_session) = &long_case.secondary_session {
        let secondary_transcript_path = resolve_path(&secondary_session.transcript)?;
        let secondary_transcript =
            Transcript::read_jsonl(&secondary_transcript_path).map_err(|error| {
                EvalError::InvalidConfig(format!(
                    "failed to read secondary transcript {}: {error}",
                    secondary_transcript_path.display()
                ))
            })?;
        drive_multi_session_transcripts(
            case,
            agent_config,
            options,
            transcript,
            secondary_transcript,
            secondary_session.interleaving,
            environment,
        )
        .await
    } else {
        drive_transcript(case, agent_config, options, transcript, environment).await
    };
    cleanup_workspace_after_run(run_root.as_path(), outcome).await
}

async fn run_scripted_user_scenario_with_provider(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    options: &EngineOptions,
    case: &TestCase,
    long_case: &LongTestCase,
    llm_provider: Arc<dyn LLMProvider>,
) -> Result<LongRunReport> {
    let Some(goal_card_path) = long_case.goal_card.as_deref() else {
        return Err(EvalError::InvalidConfig(format!(
            "long test case '{}' must set goal_card for scripted_user mode",
            case.name
        )));
    };
    let goal_card_path = resolve_path(goal_card_path)?;
    let goal_card = tokio::fs::read_to_string(&goal_card_path)
        .await
        .map_err(|source| EvalError::Io {
            path: goal_card_path.clone(),
            source,
        })?;
    if goal_card.trim().is_empty() {
        return Err(EvalError::InvalidConfig(format!(
            "long test case '{}' goal_card must not be empty",
            case.name
        )));
    }

    let Some(script_path) = long_case.scripted_user.as_deref() else {
        return Err(EvalError::InvalidConfig(format!(
            "long test case '{}' must set scripted_user for scripted_user mode",
            case.name
        )));
    };
    let script_path = resolve_path(script_path)?;
    let script = ScriptedUserScript::read_jsonl(&script_path)
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "failed to read scripted-user script {}: {error}",
                script_path.display()
            ))
        })?;
    let environment = build_agent_environment_with_provider(
        base_config,
        agent_config,
        &options.temp_dir,
        llm_provider,
    )
    .await?;
    let run_root = environment
        .workspace_dir
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| environment.workspace_dir.clone());
    let outcome = drive_scripted_user(case, agent_config, options, script, environment).await;
    cleanup_workspace_after_run(run_root.as_path(), outcome).await
}

async fn cleanup_workspace_after_run(
    run_root: &Path,
    outcome: Result<LongRunReport>,
) -> Result<LongRunReport> {
    let cleanup = cleanup_workspace(run_root).await;
    match (outcome, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn drive_transcript(
    case: &TestCase,
    agent_config: &AgentConfig,
    options: &EngineOptions,
    transcript: Transcript,
    environment: crate::AgentEnvironment,
) -> Result<LongRunReport> {
    let started_at = Utc::now();
    let mut primary = TranscriptSession::new(
        environment.session_id,
        environment.llm_provider.clone(),
        transcript,
    );

    while primary.has_next_user_turn() {
        primary.drive_next_turn(case, &environment).await?;
    }

    let completed_at = Utc::now();
    let events = collect_events_for_sessions(&environment, &[primary.session_id]).await?;
    let learning = collect_learning_summary(&environment, &[primary.session_id]).await?;
    let mut report = finish_report(FinishReportInput {
        case,
        agent_config,
        options,
        environment: &environment,
        events,
        user_turn_count: primary.user_turn_count,
        started_at,
        completed_at,
    })
    .await;
    report.learning = learning;
    Ok(report)
}

async fn drive_multi_session_transcripts(
    case: &TestCase,
    agent_config: &AgentConfig,
    options: &EngineOptions,
    primary_transcript: Transcript,
    secondary_transcript: Transcript,
    interleaving: LongSessionInterleaving,
    environment: crate::AgentEnvironment,
) -> Result<LongRunReport> {
    let started_at = Utc::now();
    let secondary_observer = ObservedRecordedProvider::new(secondary_transcript.clone());
    let secondary_provider: Arc<dyn LLMProvider> = Arc::new(secondary_observer.clone());
    let secondary_session_id =
        create_secondary_session(&environment, secondary_provider.clone()).await?;
    let mut primary = TranscriptSession::new(
        environment.session_id,
        environment.llm_provider.clone(),
        primary_transcript,
    );
    let mut secondary = TranscriptSession::new(
        secondary_session_id,
        secondary_provider,
        secondary_transcript,
    );

    match interleaving {
        LongSessionInterleaving::Sequential | LongSessionInterleaving::Phased => {
            while primary.has_next_user_turn() {
                primary.drive_next_turn(case, &environment).await?;
            }
            materialize_primary_learning_if_requested(case, &environment, &primary).await?;
            while secondary.has_next_user_turn() {
                secondary.drive_next_turn(case, &environment).await?;
            }
        }
        LongSessionInterleaving::RoundRobin => {
            while primary.has_next_user_turn() || secondary.has_next_user_turn() {
                if primary.has_next_user_turn() {
                    primary.drive_next_turn(case, &environment).await?;
                }
                if secondary.has_next_user_turn() {
                    secondary.drive_next_turn(case, &environment).await?;
                }
            }
        }
    }

    let completed_at = Utc::now();
    let primary_events = collect_events_for_sessions(&environment, &[primary.session_id]).await?;
    let secondary_events =
        collect_events_for_sessions(&environment, &[secondary.session_id]).await?;
    let mut events = primary_events.clone();
    events.extend(secondary_events.clone());
    events.sort_by_key(|record| (record.timestamp, record.sequence_num));
    let learning =
        collect_learning_summary(&environment, &[primary.session_id, secondary.session_id]).await?;
    let mut report = finish_report(FinishReportInput {
        case,
        agent_config,
        options,
        environment: &environment,
        events,
        user_turn_count: primary.user_turn_count + secondary.user_turn_count,
        started_at,
        completed_at,
    })
    .await;
    report.learning = learning;
    report.skill_manifest_observations = secondary_observer.observations();
    report.phase_comparison = Some(phase_comparison(
        primary.user_turn_count,
        secondary.user_turn_count,
        &primary_events,
        &secondary_events,
    ));
    Ok(report)
}

async fn drive_scripted_user(
    case: &TestCase,
    agent_config: &AgentConfig,
    options: &EngineOptions,
    script: ScriptedUserScript,
    environment: crate::AgentEnvironment,
) -> Result<LongRunReport> {
    let started_at = Utc::now();
    let mut primary =
        ScriptedUserSession::new(environment.session_id, environment.llm_provider.clone());

    for turn in &script.turns {
        primary.drive_turn(case, &environment, turn).await?;
    }

    let completed_at = Utc::now();
    let events = collect_events_for_sessions(&environment, &[primary.session_id]).await?;
    let learning = collect_learning_summary(&environment, &[primary.session_id]).await?;
    let mut report = finish_report(FinishReportInput {
        case,
        agent_config,
        options,
        environment: &environment,
        events,
        user_turn_count: primary.user_turn_count,
        started_at,
        completed_at,
    })
    .await;
    report.learning = learning;
    validate_scripted_final_answer(&script, &report)?;
    Ok(report)
}

fn validate_scripted_final_answer(
    script: &ScriptedUserScript,
    report: &LongRunReport,
) -> Result<()> {
    let response = report.result.response.as_deref().unwrap_or_default();
    let missing = script
        .expected_final_answer_fragments
        .iter()
        .filter(|fragment| !response.contains(fragment.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }

    Err(EvalError::InvalidConfig(format!(
        "scripted-user scenario '{}' final answer is missing expected fragment(s): {}",
        script.scenario,
        missing.join(", ")
    )))
}

struct FinishReportInput<'a> {
    case: &'a TestCase,
    agent_config: &'a AgentConfig,
    options: &'a EngineOptions,
    environment: &'a crate::AgentEnvironment,
    events: Vec<EventRecord>,
    user_turn_count: usize,
    started_at: chrono::DateTime<Utc>,
    completed_at: chrono::DateTime<Utc>,
}

async fn finish_report(input: FinishReportInput<'_>) -> LongRunReport {
    let event_payloads = input
        .events
        .iter()
        .map(|record| record.event.clone())
        .collect::<Vec<_>>();
    let mut collector = TrajectoryCollector::new(
        Some(
            input
                .environment
                .llm_provider
                .capabilities()
                .pricing
                .clone(),
        ),
        input.options.capture_content,
        input.options.content_max_bytes,
    );
    collector.process_events(&input.events);
    let mut execution = collector.finish();
    execution.metrics.turn_count = input.user_turn_count;
    let score_card = build_score_card(
        input.case,
        input.environment.llm_provider.name(),
        input.user_turn_count,
        &input.events,
        &execution,
        input.started_at,
        input.environment.session_id,
        input.environment.session_store.as_ref(),
    )
    .await;
    let lineage_events = input.environment.lineage.events();
    let mut score_records = score_card.to_score_records(
        input.environment.storage_partition_id.clone(),
        input.environment.user_id.clone(),
        input.environment.session_id,
    );
    score_records.extend(lineage_events.iter().filter_map(lineage_score_record));
    let result = EvalResult {
        test_case: input.case.name.clone(),
        agent_config: input.agent_config.name.clone(),
        status: EvalStatus::Passed,
        response: execution.response,
        trajectory: execution.trajectory,
        scores: score_card_scores(&score_card),
        metrics: execution.metrics,
        trace_id: None,
        error: None,
        started_at: input.started_at,
        completed_at: input.completed_at,
    };

    LongRunReport {
        result,
        score_card,
        score_records,
        lineage_events,
        events: event_payloads,
        learning: LearningRunSummary::default(),
        skill_manifest_observations: Vec::new(),
        phase_comparison: None,
    }
}

fn lineage_score_record(event: &Value) -> Option<moa_lineage_core::ScoreRecord> {
    match serde_json::from_value::<LineageEvent>(event.clone()) {
        Ok(LineageEvent::Eval(record)) => Some(record),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(%error, "failed to decode eval lineage event");
            None
        }
    }
}

async fn emit_user_turn(
    environment: &crate::AgentEnvironment,
    session_id: SessionId,
    turn_index: usize,
    text: &str,
) -> Result<()> {
    let event = if turn_index == 0 {
        Event::UserMessage {
            text: text.to_string(),
            attachments: Vec::new(),
        }
    } else {
        Event::QueuedMessage {
            text: text.to_string(),
            attachments: Vec::new(),
            queued_at: Utc::now(),
        }
    };
    environment
        .session_store
        .emit_event(session_id, event)
        .await?;
    Ok(())
}

async fn drive_one_turn(
    environment: &crate::AgentEnvironment,
    session_id: SessionId,
    llm_provider: Arc<dyn LLMProvider>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: &CancellationToken,
    hard_cancel_token: &CancellationToken,
) -> Result<()> {
    for turn_index in 0..MAX_LONG_CONVERSATION_AGENT_TURNS {
        let outcome = run_streamed_turn_with_lineage(
            session_id,
            environment.session_store.clone(),
            llm_provider.clone(),
            &environment.pipeline,
            Some(environment.tool_router.clone()),
            runtime_tx,
            None,
            Some(cancel_token.clone()),
            Some(hard_cancel_token.clone()),
            environment.lineage.clone(),
        )
        .await?;

        match outcome {
            StreamedTurnResult::Complete => return Ok(()),
            StreamedTurnResult::Continue => {
                if turn_index + 1 == MAX_LONG_CONVERSATION_AGENT_TURNS {
                    return Err(EvalError::InvalidConfig(format!(
                        "agent exceeded the maximum of {MAX_LONG_CONVERSATION_AGENT_TURNS} turns"
                    )));
                }
            }
            StreamedTurnResult::Cancelled => {
                return Err(EvalError::Moa(moa_core::MoaError::Cancelled));
            }
        }
    }

    Ok(())
}

struct TranscriptSession {
    session_id: SessionId,
    llm_provider: Arc<dyn LLMProvider>,
    transcript: Transcript,
    provider_turn_index: usize,
    user_turn_count: usize,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
}

impl TranscriptSession {
    fn new(
        session_id: SessionId,
        llm_provider: Arc<dyn LLMProvider>,
        transcript: Transcript,
    ) -> Self {
        let (runtime_tx, _) = broadcast::channel::<RuntimeEvent>(256);
        Self {
            session_id,
            llm_provider,
            transcript,
            provider_turn_index: 0,
            user_turn_count: 0,
            runtime_tx,
            cancel_token: CancellationToken::new(),
            hard_cancel_token: CancellationToken::new(),
        }
    }

    fn has_next_user_turn(&self) -> bool {
        self.transcript
            .turns
            .get(self.provider_turn_index)
            .is_some()
    }

    async fn drive_next_turn(
        &mut self,
        case: &TestCase,
        environment: &crate::AgentEnvironment,
    ) -> Result<()> {
        let Some(turn) = self.transcript.turns.get(self.provider_turn_index) else {
            return Ok(());
        };
        let user_text = turn.user.text.clone();
        emit_user_turn(
            environment,
            self.session_id,
            self.user_turn_count,
            user_text.as_str(),
        )
        .await?;
        let _ = case;
        drive_one_turn(
            environment,
            self.session_id,
            self.llm_provider.clone(),
            &self.runtime_tx,
            &self.cancel_token,
            &self.hard_cancel_token,
        )
        .await?;

        self.user_turn_count += 1;
        self.provider_turn_index += 1;
        while self
            .transcript
            .turns
            .get(self.provider_turn_index)
            .is_some_and(|next_turn| next_turn.user.text == user_text)
        {
            self.provider_turn_index += 1;
        }
        Ok(())
    }
}

struct ScriptedUserSession {
    session_id: SessionId,
    llm_provider: Arc<dyn LLMProvider>,
    user_turn_count: usize,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
}

impl ScriptedUserSession {
    fn new(session_id: SessionId, llm_provider: Arc<dyn LLMProvider>) -> Self {
        let (runtime_tx, _) = broadcast::channel::<RuntimeEvent>(256);
        Self {
            session_id,
            llm_provider,
            user_turn_count: 0,
            runtime_tx,
            cancel_token: CancellationToken::new(),
            hard_cancel_token: CancellationToken::new(),
        }
    }

    async fn drive_turn(
        &mut self,
        case: &TestCase,
        environment: &crate::AgentEnvironment,
        turn: &ScriptedUserTurn,
    ) -> Result<()> {
        let user_text = turn.user.text.clone();
        emit_user_turn(
            environment,
            self.session_id,
            self.user_turn_count,
            user_text.as_str(),
        )
        .await?;
        let _ = case;
        drive_one_turn(
            environment,
            self.session_id,
            self.llm_provider.clone(),
            &self.runtime_tx,
            &self.cancel_token,
            &self.hard_cancel_token,
        )
        .await?;
        self.user_turn_count += 1;
        Ok(())
    }
}

async fn create_secondary_session(
    environment: &crate::AgentEnvironment,
    llm_provider: Arc<dyn LLMProvider>,
) -> Result<SessionId> {
    let session_meta = SessionMeta {
        tenant_id: moa_core::TenantId::from(
            uuid::Uuid::parse_str(environment.storage_partition_id.as_str())
                .map_err(|error| EvalError::InvalidConfig(error.to_string()))?,
        ),
        created_by: Some(moa_core::SessionActorRef::Identity {
            id: uuid::Uuid::now_v7(),
        }),
        model: llm_provider.capabilities().model_id,
        title: Some("secondary long-conversation session".to_string()),
        ..SessionMeta::default()
    };
    environment
        .session_store
        .create_session(session_meta)
        .await
        .map_err(EvalError::from)
}

async fn collect_events_for_sessions(
    environment: &crate::AgentEnvironment,
    session_ids: &[SessionId],
) -> Result<Vec<EventRecord>> {
    let mut events = Vec::new();
    for session_id in session_ids {
        events.extend(
            environment
                .session_store
                .get_events(*session_id, EventRange::all())
                .await?,
        );
    }
    events.sort_by_key(|record| (record.timestamp, record.sequence_num));
    Ok(events)
}

async fn materialize_primary_learning_if_requested(
    case: &TestCase,
    environment: &crate::AgentEnvironment,
    primary: &TranscriptSession,
) -> Result<()> {
    let Some(config) = primary_learning_config(case)? else {
        return Ok(());
    };
    let events = collect_events_for_sessions(environment, &[primary.session_id]).await?;
    if events.is_empty() {
        return Err(EvalError::InvalidConfig(format!(
            "learning materialization for '{}' requires primary events",
            case.name
        )));
    }

    let meta = environment
        .session_store
        .get_session(primary.session_id)
        .await?;
    let now = Utc::now();
    let started_at = events.first().map(|record| record.timestamp).unwrap_or(now);
    let ended_at = events.last().map(|record| record.timestamp).unwrap_or(now);
    let duration_ms = ended_at
        .signed_duration_since(started_at)
        .num_milliseconds()
        .max(0) as u64;
    let segment_id = deterministic_segment_id(primary.session_id, 0);
    let tools_used = tool_names_for_events(&events);
    let token_cost = token_cost_for_events(&events);
    let assessment = SegmentAssessment {
        outcome: SegmentOutcome::Resolved,
        confidence: config.confidence,
        phase: AssessmentPhase::Final,
        evidence: vec![SegmentEvidence {
            kind: SegmentEvidenceKind::Verification,
            polarity: SegmentEvidencePolarity::SupportsResolved,
            strength: config.confidence,
            summary: "eval phase-one verification completed successfully".to_string(),
        }],
        assessed_at: now,
        policy_version: "long-conversation-learning-eval-v1".to_string(),
    };
    let segment = TaskSegment {
        id: segment_id,
        session_id: primary.session_id,
        tenant_id: meta.tenant_id.to_string(),
        segment_index: 0,
        task_summary: Some(config.task_summary),
        started_at,
        ended_at: Some(ended_at),
        turn_count: primary.user_turn_count as u32,
        tools_used,
        skills_activated: config.skills_activated,
        token_cost,
        previous_segment_id: None,
        outcome: Some(assessment.outcome.as_str().to_string()),
        assessment: Some(assessment.clone()),
        outcome_confidence: Some(assessment.confidence),
    };

    environment.segment_store.create_segment(&segment).await?;
    let experience = experience_from_assessment(
        &meta,
        &segment,
        &assessment,
        &events,
        None,
        Some(duration_ms),
        now,
    );
    let attributions = attributions_for_experience(&experience, &events, now);
    let candidates = propose_candidates_for_experience(&experience, &attributions, now);
    environment
        .experience_store
        .append_experience_record(&experience)
        .await?;
    environment
        .experience_store
        .append_experience_attributions(&attributions)
        .await?;
    for candidate in candidates {
        environment
            .learning_candidate_store
            .append_learning_candidate(&candidate)
            .await?;
    }
    environment
        .segment_store
        .refresh_segment_materialized_views()
        .await?;
    Ok(())
}

async fn collect_learning_summary(
    environment: &crate::AgentEnvironment,
    session_ids: &[SessionId],
) -> Result<LearningRunSummary> {
    let mut experience_count = 0usize;
    let mut attribution_count = 0usize;
    let mut skill_subjects = BTreeSet::new();
    for session_id in session_ids {
        let experiences = environment
            .experience_store
            .list_experience_records(*session_id)
            .await?;
        experience_count += experiences.len();
        for experience in experiences {
            let attributions = environment
                .experience_store
                .list_experience_attributions(experience.id)
                .await?;
            attribution_count += attributions.len();
            let rates = environment
                .segment_store
                .list_task_strategy_success_rates(
                    environment.storage_partition_id.as_str(),
                    &experience.task_fingerprint.hash,
                )
                .await?;
            for rate in rates {
                if rate.subject_type == AttributionSubjectType::Skill {
                    skill_subjects.insert(rate.subject_id);
                }
            }
        }
    }

    let candidates = environment
        .learning_candidate_store
        .list_learning_candidates(
            environment.storage_partition_id.as_str(),
            Some(LearningCandidateStatus::Proposed),
            256,
        )
        .await?;

    Ok(LearningRunSummary {
        experience_count,
        attribution_count,
        proposed_candidate_count: candidates.len(),
        task_strategy_skill_subjects: skill_subjects.into_iter().collect(),
    })
}

#[derive(Debug, Clone)]
struct PrimaryLearningConfig {
    task_summary: String,
    skills_activated: Vec<String>,
    confidence: f64,
}

fn primary_learning_config(case: &TestCase) -> Result<Option<PrimaryLearningConfig>> {
    let Some(value) = case.metadata.get("learning_phase") else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Err(EvalError::InvalidConfig(format!(
            "case '{}' metadata.learning_phase must be a table",
            case.name
        )));
    };
    if !object
        .get("materialize_after_primary")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let task_summary = object
        .get("task_summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "case '{}' metadata.learning_phase.task_summary must be non-empty",
                case.name
            ))
        })?
        .to_string();
    let skills_activated = string_array_metadata(case, object, "skills_activated")?;
    if skills_activated.is_empty() {
        return Err(EvalError::InvalidConfig(format!(
            "case '{}' metadata.learning_phase.skills_activated must not be empty",
            case.name
        )));
    }
    let confidence = object
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.9)
        .clamp(0.0, 1.0);
    Ok(Some(PrimaryLearningConfig {
        task_summary,
        skills_activated,
        confidence,
    }))
}

fn string_array_metadata(
    case: &TestCase,
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Vec<String>> {
    let Some(values) = object.get(key).and_then(Value::as_array) else {
        return Err(EvalError::InvalidConfig(format!(
            "case '{}' metadata.learning_phase.{key} must be an array",
            case.name
        )));
    };
    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        let Some(text) = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Err(EvalError::InvalidConfig(format!(
                "case '{}' metadata.learning_phase.{key} must contain only non-empty strings",
                case.name
            )));
        };
        strings.push(text.to_string());
    }
    strings.sort();
    strings.dedup();
    Ok(strings)
}

fn tool_names_for_events(events: &[EventRecord]) -> Vec<String> {
    let mut tools = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_name, .. } | Event::ToolError { tool_name, .. } => {
                Some(tool_name.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    tools.sort();
    tools.dedup();
    tools
}

fn token_cost_for_events(events: &[EventRecord]) -> u64 {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse {
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                output_tokens,
                ..
            } => Some(
                input_tokens_uncached
                    + input_tokens_cache_write
                    + input_tokens_cache_read
                    + output_tokens,
            ),
            _ => None,
        })
        .sum::<usize>() as u64
}

fn phase_comparison(
    primary_turns: usize,
    secondary_turns: usize,
    primary_events: &[EventRecord],
    secondary_events: &[EventRecord],
) -> PhaseComparison {
    let (primary_input_tokens, primary_output_tokens) = token_totals(primary_events);
    let (secondary_input_tokens, secondary_output_tokens) = token_totals(secondary_events);
    PhaseComparison {
        primary_turns,
        secondary_turns,
        primary_input_tokens,
        secondary_input_tokens,
        primary_output_tokens,
        secondary_output_tokens,
    }
}

fn token_totals(events: &[EventRecord]) -> (usize, usize) {
    let mut input_tokens = 0usize;
    let mut output_tokens = 0usize;
    for record in events {
        if let Event::BrainResponse {
            input_tokens_uncached,
            input_tokens_cache_write,
            input_tokens_cache_read,
            output_tokens: response_output_tokens,
            ..
        } = &record.event
        {
            input_tokens +=
                input_tokens_uncached + input_tokens_cache_write + input_tokens_cache_read;
            output_tokens += response_output_tokens;
        }
    }
    (input_tokens, output_tokens)
}

#[derive(Clone)]
struct ObservedRecordedProvider {
    recorded: RecordedScriptedProvider,
    observations: Arc<Mutex<Vec<SkillManifestObservation>>>,
}

impl ObservedRecordedProvider {
    fn new(transcript: Transcript) -> Self {
        Self {
            recorded: RecordedScriptedProvider::with_strict_matching(transcript),
            observations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn observations(&self) -> Vec<SkillManifestObservation> {
        self.observations
            .lock()
            .map(|observations| observations.clone())
            .unwrap_or_default()
    }

    fn record_observation(&self, request: &CompletionRequest) -> moa_core::Result<()> {
        let selected_skills = selected_skills_from_request(request);
        if selected_skills.is_empty() {
            return Ok(());
        }
        let observation = SkillManifestObservation {
            user_message: latest_non_manifest_user_message(request),
            selected_skills,
        };
        let mut observations = self
            .observations
            .lock()
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        observations.push(observation);
        Ok(())
    }
}

#[async_trait]
impl LLMProvider for ObservedRecordedProvider {
    fn name(&self) -> &str {
        self.recorded.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.recorded.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> moa_core::Result<CompletionStream> {
        self.record_observation(&request)?;
        self.recorded
            .complete_recorded(&request)
            .map_err(|error| MoaError::ProviderError(error.to_string()))
    }
}

fn selected_skills_from_request(request: &CompletionRequest) -> Vec<String> {
    let mut skills = Vec::new();
    for message in &request.messages {
        if !message.content.contains("<available_skills>") {
            continue;
        }
        for line in message.content.lines() {
            let Some(rest) = line.strip_prefix("- ") else {
                continue;
            };
            let Some((name, _)) = rest.split_once(':') else {
                continue;
            };
            let name = name.trim();
            if !name.is_empty() {
                skills.push(name.to_string());
            }
        }
    }
    skills.sort();
    skills.dedup();
    skills
}

fn latest_non_manifest_user_message(request: &CompletionRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == moa_core::MessageRole::User
                && !message.content.starts_with("<system-reminder>")
                && !message.content.contains("<available_skills>")
        })
        .map(|message| message.content.clone())
}

#[allow(
    clippy::too_many_arguments,
    reason = "score-card assembly threads the run's identity, events, execution, and \
              session store; bundling them would only add an internal-only struct"
)]
async fn build_score_card(
    case: &TestCase,
    provider: &str,
    turn_count: usize,
    event_records: &[EventRecord],
    execution: &CollectedExecution,
    timestamp: chrono::DateTime<Utc>,
    session_id: SessionId,
    session_store: &dyn SessionStore,
) -> ScoreCard {
    let events = event_records
        .iter()
        .map(|record| record.event.clone())
        .collect::<Vec<_>>();
    let memory_scenario = memory_scenario_from_case(case);
    let planted_fact_recall =
        match compute_planted_fact_recall(&memory_scenario, session_id, session_store).await {
            Ok(recall) => recall,
            Err(error) => {
                tracing::warn!(
                    %error,
                    scenario = %case.name,
                    "failed to compute planted-fact recall; defaulting to 0.0"
                );
                0.0
            }
        };
    let cache_observations = cache_observations_from_event_records(event_records);
    let error_count = events
        .iter()
        .filter(|event| matches!(event, Event::Error { .. }))
        .count() as u32;
    let compaction_count = events
        .iter()
        .filter(|event| matches!(event, Event::Checkpoint { .. }))
        .count();
    let errors_total_pre_compaction = errors_before_first_checkpoint(&events);
    let errors_preserved =
        metadata_u32(case, "errors_preserved").unwrap_or(errors_total_pre_compaction);
    let errors_preserved_strict = errors_preserved >= errors_total_pre_compaction;
    let consolidation = count_consolidation_outcomes(&events);
    let brain_cost_cents = events
        .iter()
        .filter_map(|event| match event {
            Event::BrainResponse { cost_cents, .. } => Some(*cost_cents),
            _ => None,
        })
        .sum::<u32>();
    let cached_input_tokens = events
        .iter()
        .filter_map(|event| match event {
            Event::BrainResponse {
                input_tokens_cache_read,
                ..
            } => Some(*input_tokens_cache_read),
            _ => None,
        })
        .sum::<usize>();
    let tool_success_count = execution
        .trajectory
        .iter()
        .filter(|step| step.success)
        .count();
    let success_rate = if execution.metrics.tool_call_count == 0 {
        1.0
    } else {
        tool_success_count as f64 / execution.metrics.tool_call_count as f64
    };
    let score_input_tokens = if cache_observations.report_count == 0 {
        execution.metrics.input_tokens
    } else {
        cache_observations.input_tokens
    };
    let score_cached_input_tokens = if cache_observations.report_count == 0 {
        cached_input_tokens
    } else {
        cache_observations.cached_input_tokens
    };

    ScoreCard {
        scenario: case.name.clone(),
        run_id: Uuid::now_v7(),
        timestamp,
        provider: provider.to_string(),
        functional: FunctionalScores {
            task_completed: error_count == 0,
            turn_count,
            error_count,
            errors_preserved: errors_preserved_strict,
        },
        latency_ms: LatencyScores {
            first_token_p50_ms: execution.metrics.latency_ms,
            first_token_p95_ms: execution.metrics.latency_ms,
            completion_p50_ms: execution.metrics.latency_ms,
            completion_p95_ms: execution.metrics.latency_ms,
        },
        cost: CostScores {
            input_tokens: score_input_tokens,
            output_tokens: execution.metrics.output_tokens,
            cached_input_tokens: score_cached_input_tokens,
            cost_cents: brain_cost_cents,
        },
        cache: CacheScores {
            input_cached_ratio: cache_observations.input_cached_ratio(),
            prefix_stable: cache_observations.prefix_stable,
            stable_prefix_bytes: cache_observations.stable_prefix_bytes,
        },
        context: ContextScores {
            max_context_tokens: execution.metrics.input_tokens,
            compaction_count,
            compaction_events: u32::try_from(compaction_count).unwrap_or(u32::MAX),
            tokens_at_first_trigger: metadata_u32(case, "tokens_at_first_trigger")
                .or_else(|| first_checkpoint_input_tokens(&events))
                .unwrap_or(0),
            post_compaction_tokens: metadata_u32(case, "post_compaction_tokens").unwrap_or(0),
            errors_preserved,
            errors_total_pre_compaction,
            errors_preserved_strict,
        },
        memory: MemoryScores {
            planted_fact_recall,
            pages_written: count_pages_written(&events),
            consolidation_successes: consolidation.successes,
            consolidation_failures: consolidation.failures,
        },
        tools: ToolScores {
            tool_call_count: execution.metrics.tool_call_count,
            tool_success_count,
            tool_error_count: execution.metrics.tool_error_count,
            success_rate,
        },
        safety: SafetyScores {
            prompt_injection_attempts_blocked: metadata_u32(
                case,
                "prompt_injection_attempts_blocked",
            )
            .unwrap_or(0),
            shell_bypass_attempts_blocked: metadata_u32(case, "shell_bypass_attempts_blocked")
                .unwrap_or(0),
            ..SafetyScores::default()
        },
        // Reconstructed from the same durable log: model turns + internal VO round-trips. VO
        // round-trip fields are only populated when the run persisted TurnMetrics; model-turn and
        // tool-call fields are always meaningful.
        coordination: CoordinationScores::from_conversation_cost(&ConversationCost::from_events(
            event_records,
        )),
    }
}

#[derive(Debug, Clone, Copy)]
struct CacheObservations {
    report_count: usize,
    input_tokens: usize,
    cached_input_tokens: usize,
    prefix_stable: bool,
    stable_prefix_bytes: usize,
}

impl CacheObservations {
    fn input_cached_ratio(self) -> f64 {
        if self.input_tokens == 0 {
            return 0.0;
        }

        self.cached_input_tokens as f64 / self.input_tokens as f64
    }
}

fn cache_observations_from_event_records(records: &[EventRecord]) -> CacheObservations {
    let mut report_count = 0usize;
    let mut input_tokens = 0usize;
    let mut cached_input_tokens = 0usize;
    let mut prefix_stable = true;
    let mut stable_prefix_bytes = None::<usize>;
    let mut reports_by_session = HashMap::<SessionId, usize>::new();

    for record in records {
        let Event::CacheReport { report } = &record.event else {
            continue;
        };
        report_count += 1;
        input_tokens += report.input_tokens;
        cached_input_tokens += report.cached_input_tokens;
        if report.stable_prefix_bytes > 0 {
            stable_prefix_bytes = Some(
                stable_prefix_bytes
                    .unwrap_or(report.stable_prefix_bytes)
                    .min(report.stable_prefix_bytes),
            );
        }

        let previous_reports = reports_by_session.entry(record.session_id).or_default();
        if *previous_reports > 0 && !report.stable_prefix_reused {
            tracing::warn!(
                session_id = %record.session_id,
                sequence_num = record.sequence_num,
                stable_prefix_fingerprint = report.stable_prefix_fingerprint,
                "long-conversation cache prefix drift detected"
            );
            prefix_stable = false;
        }
        *previous_reports += 1;
    }

    CacheObservations {
        report_count,
        input_tokens,
        cached_input_tokens,
        prefix_stable: report_count > 0 && prefix_stable,
        stable_prefix_bytes: stable_prefix_bytes.unwrap_or(0),
    }
}

fn memory_scenario_from_case(case: &TestCase) -> MemoryScenario {
    let planted_facts = case
        .metadata
        .get("planted_facts")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|fact| !fact.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let recall_k = metadata_u32(case, "recall_k")
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_PLANTED_FACT_RECALL_K);
    MemoryScenario {
        planted_facts,
        recall_k,
    }
}

fn metadata_u32(case: &TestCase, key: &str) -> Option<u32> {
    case.metadata
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn first_checkpoint_input_tokens(events: &[Event]) -> Option<u32> {
    events.iter().find_map(|event| match event {
        Event::Checkpoint { input_tokens, .. } => u32::try_from(*input_tokens).ok(),
        _ => None,
    })
}

fn errors_before_first_checkpoint(events: &[Event]) -> u32 {
    events
        .iter()
        .take_while(|event| !matches!(event, Event::Checkpoint { .. }))
        .filter(|event| matches!(event, Event::Error { .. } | Event::ToolError { .. }))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn score_card_scores(score_card: &ScoreCard) -> Vec<EvalScore> {
    score_card
        .metric_rows()
        .into_iter()
        .map(|row| EvalScore {
            evaluator: "long_conversation".to_string(),
            name: row.name,
            value: eval_score_value(row.value),
            comment: Some(format!("scenario={}", score_card.scenario)),
        })
        .collect()
}

fn eval_score_value(value: serde_json::Value) -> EvalScoreValue {
    match value {
        serde_json::Value::Bool(value) => EvalScoreValue::Boolean(value),
        serde_json::Value::Number(value) => EvalScoreValue::Numeric(value.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(value) => EvalScoreValue::Categorical(value),
        other => EvalScoreValue::Categorical(other.to_string()),
    }
}

fn resolve_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    let current_dir = std::env::current_dir().map_err(|source| EvalError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let current = current_dir.join(path);
    if current.exists() {
        return Ok(current);
    }

    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
}

async fn cleanup_workspace(path: &Path) -> Result<()> {
    if tokio::fs::try_exists(path)
        .await
        .map_err(|source| EvalError::Io {
            path: path.to_path_buf(),
            source,
        })?
    {
        tokio::fs::remove_dir_all(path)
            .await
            .map_err(|source| EvalError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{CacheReport, EventType, ModelId, ModelTier};

    use super::super::memory_metrics::test_session_store::RecordingSessionStore;
    use super::*;
    use moa_eval_core::EvalMetrics;

    fn cache_report_record(
        session_id: SessionId,
        sequence_num: u64,
        stable_prefix_reused: bool,
        stable_prefix_bytes: usize,
        input_tokens: usize,
        cached_input_tokens: usize,
    ) -> EventRecord {
        let event = Event::CacheReport {
            report: CacheReport {
                provider: "recorded".to_string(),
                model: ModelId::new("recorded-scripted"),
                message_count: 3,
                tool_count: 1,
                tool_tokens_estimate: 100,
                stable_message_tokens_estimate: 200,
                stable_total_tokens_estimate: 300,
                total_tokens_estimate: 500,
                dynamic_tokens_estimate: 200,
                cache_ratio_estimate: 0.6,
                stable_prefix_bytes,
                stable_prefix_fingerprint: 42,
                full_request_fingerprint: sequence_num,
                stable_prefix_reused,
                input_tokens,
                cached_input_tokens,
                output_tokens: 8,
                cached_vs_stable_estimate_ratio: 0.0,
            },
        };
        EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num,
            event_type: EventType::CacheReport,
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    #[tokio::test]
    async fn score_card_uses_cache_reports_to_detect_long_conversation_prefix_drift() {
        let session_id = SessionId::new();
        let records = vec![
            cache_report_record(session_id, 1, false, 512, 100, 0),
            cache_report_record(session_id, 2, false, 512, 100, 40),
        ];
        let case = TestCase {
            name: "cache-drift".to_string(),
            ..TestCase::default()
        };
        let execution = CollectedExecution {
            metrics: EvalMetrics {
                input_tokens: 999,
                output_tokens: 12,
                ..EvalMetrics::default()
            },
            ..CollectedExecution::default()
        };
        let session_store = RecordingSessionStore::default();

        let score_card = build_score_card(
            &case,
            "recorded",
            2,
            &records,
            &execution,
            Utc::now(),
            session_id,
            &session_store,
        )
        .await;

        assert!(
            !score_card.cache.prefix_stable,
            "second provider request in the same session did not reuse the stable prefix"
        );
        assert_eq!(score_card.cache.stable_prefix_bytes, 512);
        assert_eq!(score_card.cost.input_tokens, 200);
        assert_eq!(score_card.cost.cached_input_tokens, 40);
        assert_eq!(score_card.cache.input_cached_ratio, 0.2);
    }

    fn event_record(session_id: SessionId, sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    #[tokio::test]
    async fn score_card_reports_functional_error_preservation_from_context_signal() {
        let session_id = SessionId::new();
        let records = vec![
            event_record(
                session_id,
                1,
                Event::Error {
                    message: "tool failure before compaction".to_string(),
                    recoverable: true,
                },
            ),
            event_record(
                session_id,
                2,
                Event::Checkpoint {
                    summary: "summary without the error".to_string(),
                    events_summarized: 1,
                    token_count: 8,
                    model: ModelId::new("recorded-scripted"),
                    model_tier: ModelTier::Auxiliary,
                    input_tokens: 42,
                    output_tokens: 8,
                    cost_cents: 0,
                },
            ),
        ];
        let case = TestCase {
            name: "missing-error-preservation".to_string(),
            metadata: HashMap::from([("errors_preserved".to_string(), serde_json::json!(0))]),
            ..TestCase::default()
        };
        let execution = CollectedExecution::default();
        let session_store = RecordingSessionStore::default();

        let score_card = build_score_card(
            &case,
            "recorded",
            1,
            &records,
            &execution,
            Utc::now(),
            session_id,
            &session_store,
        )
        .await;

        assert_eq!(score_card.context.errors_total_pre_compaction, 1);
        assert_eq!(score_card.context.errors_preserved, 0);
        assert!(!score_card.context.errors_preserved_strict);
        assert!(!score_card.functional.errors_preserved);
    }

    #[tokio::test]
    async fn score_card_wires_planted_fact_recall_from_session_store() {
        // Pins: the score card's planted-fact recall comes from compute_planted_fact_recall against
        // the live session store (using case metadata), not the legacy hardcoded 0.0.
        let session_id = SessionId::new();
        let case = TestCase {
            name: "planted-recall".to_string(),
            metadata: HashMap::from([
                (
                    "planted_facts".to_string(),
                    serde_json::json!(["fact alpha", "fact beta"]),
                ),
                ("recall_k".to_string(), serde_json::json!(5)),
            ]),
            ..TestCase::default()
        };
        let execution = CollectedExecution::default();
        let session_store = RecordingSessionStore::with_recalled_facts(["fact alpha"]);

        let score_card = build_score_card(
            &case,
            "recorded",
            1,
            &[],
            &execution,
            Utc::now(),
            session_id,
            &session_store,
        )
        .await;

        assert_eq!(score_card.memory.planted_fact_recall, 0.5);
        assert_eq!(
            session_store.observed_limits(),
            vec![Some(5), Some(5)],
            "each planted fact should be searched once at the configured recall_k"
        );
    }

    #[test]
    fn cache_observations_treat_each_session_first_request_as_cold_start() {
        let primary = SessionId::new();
        let secondary = SessionId::new();
        let records = vec![
            cache_report_record(primary, 1, false, 700, 100, 0),
            cache_report_record(secondary, 1, false, 600, 100, 0),
            cache_report_record(primary, 2, true, 700, 100, 60),
            cache_report_record(secondary, 2, true, 600, 100, 60),
        ];

        let observations = cache_observations_from_event_records(&records);

        assert!(observations.prefix_stable);
        assert_eq!(observations.stable_prefix_bytes, 600);
        assert_eq!(observations.input_cached_ratio(), 0.3);
    }
}
