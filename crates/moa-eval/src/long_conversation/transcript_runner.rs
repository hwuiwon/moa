//! Multi-turn transcript runner for long-conversation eval cases.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use moa_brain::{StreamedTurnResult, run_streamed_turn};
use moa_core::{Event, EventRange, LLMProvider, MoaConfig, RuntimeEvent};
use moa_test_support::transcript::Transcript;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::memory_metrics::{count_consolidation_outcomes, count_pages_written};
use super::score_card::{
    CacheScores, ContextScores, CostScores, FunctionalScores, LatencyScores, MemoryScores,
    SafetyScores, ScoreCard, ToolScores,
};
use crate::collector::{CollectedExecution, TrajectoryCollector};
use crate::setup::build_agent_environment_with_provider;
use crate::{
    AgentConfig, EngineOptions, EvalError, EvalResult, EvalScore, EvalStatus, Result, ScoreValue,
    TestCase,
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
    if !long_case.mode.is_recorded() {
        return Err(EvalError::InvalidConfig(format!(
            "long conversation mode {:?} is not implemented yet",
            long_case.mode
        )));
    }

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

    let outcome = drive_transcript(case, agent_config, options, transcript, environment).await;
    let cleanup = cleanup_workspace(&run_root).await;
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
    let cancel_token = CancellationToken::new();
    let hard_cancel_token = CancellationToken::new();
    let (runtime_tx, _) = broadcast::channel::<RuntimeEvent>(256);

    for (turn_index, turn) in transcript.turns.iter().enumerate() {
        emit_user_turn(&environment, turn_index, &turn.user.text).await?;
        drive_one_turn(&environment, &runtime_tx, &cancel_token, &hard_cancel_token).await?;
    }

    let completed_at = Utc::now();
    let events = environment
        .session_store
        .get_events(environment.session_id, EventRange::all())
        .await?;
    let event_payloads = events
        .iter()
        .map(|record| record.event.clone())
        .collect::<Vec<_>>();
    let mut collector = TrajectoryCollector::new(
        Some(environment.llm_provider.capabilities().pricing.clone()),
        options.capture_content,
        options.content_max_bytes,
    );
    collector.process_events(&events);
    let execution = collector.finish();
    let score_card = build_score_card(
        case,
        environment.llm_provider.name(),
        transcript.turns.len(),
        &event_payloads,
        &execution,
        started_at,
    );
    let score_records = score_card.to_score_records(
        environment.workspace_id.clone(),
        environment.user_id.clone(),
        environment.session_id,
    );
    let result = EvalResult {
        test_case: case.name.clone(),
        agent_config: agent_config.name.clone(),
        status: EvalStatus::Passed,
        response: execution.response,
        trajectory: execution.trajectory,
        scores: score_card_scores(&score_card),
        metrics: execution.metrics,
        trace_id: None,
        error: None,
        started_at,
        completed_at,
    };

    Ok(LongRunReport {
        result,
        score_card,
        score_records,
    })
}

async fn emit_user_turn(
    environment: &crate::AgentEnvironment,
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
        .emit_event(environment.session_id, event)
        .await?;
    Ok(())
}

async fn drive_one_turn(
    environment: &crate::AgentEnvironment,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: &CancellationToken,
    hard_cancel_token: &CancellationToken,
) -> Result<()> {
    for turn_index in 0..MAX_LONG_CONVERSATION_AGENT_TURNS {
        let outcome = run_streamed_turn(
            environment.session_id,
            environment.session_store.clone(),
            environment.llm_provider.clone(),
            &environment.pipeline,
            Some(environment.tool_router.clone()),
            runtime_tx,
            None,
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

fn build_score_card(
    case: &TestCase,
    provider: &str,
    turn_count: usize,
    events: &[Event],
    execution: &CollectedExecution,
    timestamp: chrono::DateTime<Utc>,
) -> ScoreCard {
    let error_count = events
        .iter()
        .filter(|event| matches!(event, Event::Error { .. }))
        .count() as u32;
    let compaction_count = events
        .iter()
        .filter(|event| matches!(event, Event::Checkpoint { .. }))
        .count();
    let consolidation = count_consolidation_outcomes(events);
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
            input_tokens: execution.metrics.input_tokens,
            output_tokens: execution.metrics.output_tokens,
            cached_input_tokens,
            cost_cents: brain_cost_cents,
        },
        cache: CacheScores {
            input_cached_ratio: if execution.metrics.input_tokens == 0 {
                0.0
            } else {
                cached_input_tokens as f64 / execution.metrics.input_tokens as f64
            },
            prefix_stable: true,
            stable_prefix_bytes: 0,
        },
        context: ContextScores {
            max_context_tokens: execution.metrics.input_tokens,
            compaction_count,
            errors_preserved_strict: true,
        },
        memory: MemoryScores {
            planted_fact_recall: 0.0,
            pages_written: count_pages_written(events),
            consolidation_successes: consolidation.successes,
            consolidation_failures: consolidation.failures,
        },
        tools: ToolScores {
            tool_call_count: execution.metrics.tool_call_count,
            tool_success_count,
            tool_error_count: execution.metrics.tool_error_count,
            success_rate,
        },
        safety: SafetyScores::default(),
    }
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
