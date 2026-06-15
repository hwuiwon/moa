//! Multi-turn transcript runner for long-conversation eval cases.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use moa_brain::{StreamedTurnResult, run_streamed_turn_with_signals};
use moa_core::transcript::Transcript;
use moa_core::{
    Event, EventRange, EventRecord, LLMProvider, MoaConfig, RuntimeEvent, SessionId, SessionMeta,
    SessionSignal, record_broadcast_lag,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::memory_metrics::{count_consolidation_outcomes, count_pages_written};
use super::provider_recorded::RecordedScriptedProvider;
use super::score_card::{
    CacheScores, ContextScores, CostScores, FunctionalScores, LatencyScores, MemoryScores,
    SafetyScores, ScoreCard, ToolScores,
};
use super::scripted_user::{ScriptedApprovalDecision, ScriptedUserScript, ScriptedUserTurn};
use crate::collector::{CollectedExecution, TrajectoryCollector};
use crate::setup::build_agent_environment_with_provider;
use crate::{
    AgentConfig, EngineOptions, EvalError, EvalResult, EvalScore, EvalStatus, LongConversationMode,
    LongSessionInterleaving, LongTestCase, Result, ScoreValue, TestCase,
};

const MAX_LONG_CONVERSATION_AGENT_TURNS: usize = 32;

/// Result of running a recorded long-conversation scenario.
#[derive(Debug, Clone)]
pub struct LongRunReport {
    /// Eval result returned through the existing eval engine surface.
    pub result: EvalResult,
    /// Structured long-conversation score card.
    pub score_card: ScoreCard,
    /// Lineage score rows ready for `analytics.scores`.
    pub score_records: Vec<moa_lineage_core::ScoreRecord>,
    /// Persisted event payloads emitted during the run.
    pub events: Vec<Event>,
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
    Ok(finish_report(FinishReportInput {
        case,
        agent_config,
        options,
        environment: &environment,
        events,
        user_turn_count: primary.user_turn_count,
        started_at,
        completed_at,
    }))
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
    let secondary_provider = Arc::new(RecordedScriptedProvider::with_strict_matching(
        secondary_transcript.clone(),
    ));
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
    let events =
        collect_events_for_sessions(&environment, &[primary.session_id, secondary.session_id])
            .await?;
    Ok(finish_report(FinishReportInput {
        case,
        agent_config,
        options,
        environment: &environment,
        events,
        user_turn_count: primary.user_turn_count + secondary.user_turn_count,
        started_at,
        completed_at,
    }))
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
    let report = finish_report(FinishReportInput {
        case,
        agent_config,
        options,
        environment: &environment,
        events,
        user_turn_count: primary.user_turn_count,
        started_at,
        completed_at,
    });
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

fn finish_report(input: FinishReportInput<'_>) -> LongRunReport {
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
    );
    let score_records = score_card.to_score_records(
        input.environment.workspace_id.clone(),
        input.environment.user_id.clone(),
        input.environment.session_id,
    );
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
        events: event_payloads,
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
    signal_state: &mut TurnSignalState,
    cancel_token: &CancellationToken,
    hard_cancel_token: &CancellationToken,
) -> Result<()> {
    for turn_index in 0..MAX_LONG_CONVERSATION_AGENT_TURNS {
        let outcome = run_streamed_turn_with_signals(
            session_id,
            environment.session_store.clone(),
            llm_provider.clone(),
            &environment.pipeline,
            Some(environment.tool_router.clone()),
            runtime_tx,
            None,
            &mut signal_state.signal_rx,
            &mut signal_state.turn_requested,
            &mut signal_state.soft_cancel_requested,
            Some(cancel_token.clone()),
            Some(hard_cancel_token.clone()),
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
            StreamedTurnResult::NeedsApproval(request) => {
                return Err(EvalError::ApprovalRequired {
                    tool: request.tool_name,
                });
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
    signal_tx: mpsc::Sender<SessionSignal>,
    signal_state: TurnSignalState,
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
        let (signal_tx, signal_rx) = mpsc::channel::<SessionSignal>(16);
        Self {
            session_id,
            llm_provider,
            transcript,
            provider_turn_index: 0,
            user_turn_count: 0,
            runtime_tx,
            signal_tx,
            signal_state: TurnSignalState::new(signal_rx),
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
        spawn_scripted_signal_task(
            case,
            user_text.as_str(),
            None,
            &self.runtime_tx,
            &self.signal_tx,
        );
        drive_one_turn(
            environment,
            self.session_id,
            self.llm_provider.clone(),
            &self.runtime_tx,
            &mut self.signal_state,
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
    signal_tx: mpsc::Sender<SessionSignal>,
    signal_state: TurnSignalState,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
}

impl ScriptedUserSession {
    fn new(session_id: SessionId, llm_provider: Arc<dyn LLMProvider>) -> Self {
        let (runtime_tx, _) = broadcast::channel::<RuntimeEvent>(256);
        let (signal_tx, signal_rx) = mpsc::channel::<SessionSignal>(16);
        Self {
            session_id,
            llm_provider,
            user_turn_count: 0,
            runtime_tx,
            signal_tx,
            signal_state: TurnSignalState::new(signal_rx),
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
        spawn_scripted_signal_task(
            case,
            user_text.as_str(),
            turn.approval.as_ref(),
            &self.runtime_tx,
            &self.signal_tx,
        );
        drive_one_turn(
            environment,
            self.session_id,
            self.llm_provider.clone(),
            &self.runtime_tx,
            &mut self.signal_state,
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
        workspace_id: environment.workspace_id.clone(),
        user_id: environment.user_id.clone(),
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

struct TurnSignalState {
    signal_rx: mpsc::Receiver<SessionSignal>,
    turn_requested: bool,
    soft_cancel_requested: bool,
}

impl TurnSignalState {
    fn new(signal_rx: mpsc::Receiver<SessionSignal>) -> Self {
        Self {
            signal_rx,
            turn_requested: false,
            soft_cancel_requested: false,
        }
    }
}

fn spawn_scripted_signal_task(
    case: &TestCase,
    user_text: &str,
    turn_decision: Option<&ScriptedApprovalDecision>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    signal_tx: &mpsc::Sender<SessionSignal>,
) {
    let scripted_decision = turn_decision
        .cloned()
        .or_else(|| scripted_approval_decision(case, user_text));
    let Some(scripted_decision) = scripted_decision else {
        return;
    };

    let case_name = case.name.clone();
    let mut runtime_rx = runtime_tx.subscribe();
    let signal_tx = signal_tx.clone();
    tokio::spawn(async move {
        loop {
            match runtime_rx.recv().await {
                Ok(RuntimeEvent::ApprovalRequested(prompt)) => {
                    let decision = scripted_decision.to_approval_decision(&prompt.pattern);
                    if let Err(error) = signal_tx
                        .send(SessionSignal::ApprovalDecided {
                            request_id: prompt.request.request_id,
                            decision,
                        })
                        .await
                    {
                        tracing::warn!(
                            scenario = %case_name,
                            error = %error,
                            "failed to send scripted long-conversation approval signal"
                        );
                    }
                    break;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    record_broadcast_lag("runtime", "skip_with_gap", dropped);
                    tracing::warn!(
                        scenario = %case_name,
                        dropped,
                        "runtime stream subscriber fell behind before scripted approval request"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        scenario = %case_name,
                        error = %error,
                        "runtime stream closed before scripted approval request"
                    );
                    break;
                }
            }
        }
    });
}

fn scripted_approval_decision(
    case: &TestCase,
    user_text: &str,
) -> Option<ScriptedApprovalDecision> {
    if let Some(value) = case
        .metadata
        .get("scripted_approval_decisions")
        .and_then(Value::as_object)
        .and_then(|decisions| decisions.get(user_text))
    {
        return parse_scripted_decision(value);
    }

    (case.metadata.get("approval_turn").and_then(Value::as_str) == Some(user_text))
        .then_some(ScriptedApprovalDecision::AllowOnce)
}

fn parse_scripted_decision(value: &Value) -> Option<ScriptedApprovalDecision> {
    let mut normalized = match value {
        Value::Object(_) => value.clone(),
        _ => serde_json::json!({ "decision": "allow_once" }),
    };
    if let Value::Object(object) = &mut normalized {
        object
            .entry("decision".to_string())
            .or_insert_with(|| Value::String("allow_once".to_string()));
    }
    match serde_json::from_value(normalized) {
        Ok(decision) => Some(decision),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "unknown scripted long-conversation approval decision"
            );
            None
        }
    }
}

fn build_score_card(
    case: &TestCase,
    provider: &str,
    turn_count: usize,
    event_records: &[EventRecord],
    execution: &CollectedExecution,
    timestamp: chrono::DateTime<Utc>,
) -> ScoreCard {
    let events = event_records
        .iter()
        .map(|record| record.event.clone())
        .collect::<Vec<_>>();
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
            errors_preserved: true,
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
            planted_fact_recall: 0.0,
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

fn eval_score_value(value: serde_json::Value) -> ScoreValue {
    match value {
        serde_json::Value::Bool(value) => ScoreValue::Boolean(value),
        serde_json::Value::Number(value) => ScoreValue::Numeric(value.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(value) => ScoreValue::Categorical(value),
        other => ScoreValue::Categorical(other.to_string()),
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
    use moa_core::{CacheReport, EventType, ModelId};

    use super::*;
    use crate::EvalMetrics;

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

    #[test]
    fn score_card_uses_cache_reports_to_detect_long_conversation_prefix_drift() {
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

        let score_card = build_score_card(&case, "recorded", 2, &records, &execution, Utc::now());

        assert!(
            !score_card.cache.prefix_stable,
            "second provider request in the same session did not reuse the stable prefix"
        );
        assert_eq!(score_card.cache.stable_prefix_bytes, 512);
        assert_eq!(score_card.cost.input_tokens, 200);
        assert_eq!(score_card.cost.cached_input_tokens, 40);
        assert_eq!(score_card.cache.input_cached_ratio, 0.2);
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
