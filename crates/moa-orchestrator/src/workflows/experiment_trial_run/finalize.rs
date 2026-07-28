//! Terminal scoring for one behavior-lab trial.
//!
//! Both target paths reduce to [`TrialTerminalEvidence`] and hand it here. This
//! is the only place a Behavior Lab score is derived, and the order it enforces
//! is the point of the module:
//!
//! 1. derive deterministic scores from the typed evidence, under one
//!    replay-stable timestamp;
//! 2. emit them as one durable `LineageEvent::Eval` batch through a
//!    score-capable lineage handle;
//! 3. poll Postgres until every exact score row is query-visible;
//! 4. only then persist the terminal trial status.
//!
//! Step 3 exists because step 2 returns on durable *enqueue*, not on SQL
//! visibility. A trial that treated the journal acknowledgement as proof would
//! report Completed while its evidence was still in flight, and a reader would
//! see a completed trial with no scores.

use super::status::stop_trial;
use super::*;

use crate::lineage::ScoreLineageHandle;
use crate::workflows::durable_utc_now;
use moa_core::types::experiments::ExperimentScorecard;
use moa_core::types::identifiers::StoragePartitionId;
use moa_experiments::evaluator::{EvaluatedScore, EvaluatedValue, evaluate_trial};
use moa_experiments::evidence::{TrialScoreTarget, TrialTerminalEvidence, TrialTerminalOutcome};
use moa_lineage_core::{
    ExperimentScoreProvenance, ExperimentScoreTarget, LineageEvent, ScoreRecord, ScoreSource,
    ScoreTarget, ScoreValue,
};
use moa_scoring::{ExperimentScoreRowsRef, exact_experiment_score_rows_for_tenant};
use std::collections::BTreeSet;

/// Longest a trial waits for its own score rows to become query-visible.
const SCORE_VISIBILITY_TIMEOUT: Duration = Duration::from_secs(60);
/// Delay between visibility polls.
const SCORE_VISIBILITY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The sink cannot store product scores, so no trial may claim complete evidence.
const FAILURE_SINK_NOT_SCORE_CAPABLE: &str = "experiment_score_sink_not_durable";
/// The pinned plan's scorecard names something this build cannot evaluate.
const FAILURE_SCORECARD_UNRUNNABLE: &str = "experiment_scorecard_unrunnable";
/// The durable score batch was not accepted by the lineage journal.
const FAILURE_SCORE_APPEND_REJECTED: &str = "experiment_score_append_rejected";
/// The score rows did not become query-visible within the bounded wait.
const FAILURE_SCORE_NOT_VISIBLE: &str = "experiment_score_not_visible";

/// Everything the finalizer needs that is not on the trial record.
pub(super) struct TrialFinalization<'a> {
    /// Trial being finalized.
    pub(super) trial: &'a ExperimentTrialRecord,
    /// Typed terminal evidence the target produced.
    pub(super) evidence: TrialTerminalEvidence,
    /// Status the trial reaches once its evidence is durable and visible.
    pub(super) terminal_status: ExperimentTrialStatus,
    /// Durable stop reason recorded with the terminal status.
    pub(super) stop_reason: ExperimentTrialStopReason,
    /// Terminal error message for failed trials.
    pub(super) error: Option<String>,
}

/// Derives, emits, and confirms one trial's scores, then persists its terminal status.
pub(super) async fn finalize_trial(
    ctx: &WorkflowContext<'_>,
    finalization: TrialFinalization<'_>,
    score_lineage: Option<&ScoreLineageHandle>,
    pool: &sqlx::PgPool,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let TrialFinalization {
        trial,
        evidence,
        terminal_status,
        stop_reason,
        error,
    } = finalization;

    // A telemetry-only sink drops or span-ifies these events. Refusing here is
    // what stops a null or OTLP deployment from producing trials that look
    // complete and have nothing to read back.
    let Some(score_lineage) = score_lineage else {
        return fail_trial(ctx, trial, FAILURE_SINK_NOT_SCORE_CAPABLE, pool).await;
    };

    let scorecard = match load_run_scorecard(ctx, trial, pool).await? {
        Ok(scorecard) => scorecard,
        Err(response) => return Ok(response),
    };

    // One timestamp for the whole batch, journaled so a replay reuses it. A
    // fresh `Utc::now()` per attempt would insert a second `analytics.scores`
    // row under the same score id, because that table is keyed `(score_id, ts)`.
    let now = durable_utc_now(ctx, "experiment_trial_score_ts").await?;

    let scores = match evaluate_trial(&scorecard, trial.score_run_id, &evidence) {
        Ok(scores) => scores,
        Err(evaluator_error) => {
            tracing::warn!(
                trial_uid = %trial.trial_uid,
                error = %evaluator_error,
                "experiment trial scorecard is not runnable in this build"
            );
            return fail_trial(ctx, trial, FAILURE_SCORECARD_UNRUNNABLE, pool).await;
        }
    };

    let events = scores
        .iter()
        .map(|score| score_event(trial, &evidence, score, now))
        .collect::<Result<Vec<_>, HandlerError>>()?;
    let expected_score_ids = scores
        .iter()
        .map(|score| score.score_id)
        .collect::<BTreeSet<_>>();

    let handle = score_lineage.handle().clone();
    let payloads = events
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            TerminalError::new(format!("experiment score lineage encoding failed: {error}"))
        })?;
    let appended = ctx
        .run(|| {
            let handle = handle.clone();
            let payloads = payloads.clone();
            async move {
                Ok::<_, HandlerError>(Json::from(
                    handle.record_durable_batch(payloads).await.is_ok(),
                ))
            }
        })
        .name("experiment_trial_emit_scores")
        .await?
        .into_inner();
    if !appended {
        return fail_trial(ctx, trial, FAILURE_SCORE_APPEND_REJECTED, pool).await;
    }

    if !await_score_visibility(ctx, trial, &expected_score_ids, pool).await? {
        return fail_trial(ctx, trial, FAILURE_SCORE_NOT_VISIBLE, pool).await;
    }

    stop_trial(
        ctx,
        trial.scope,
        trial.trial_uid,
        terminal_status,
        stop_reason,
        error,
        pool,
    )
    .await
}

/// Polls Postgres until every derived score row is query-visible.
///
/// Each poll is its own journaled step, so a workflow replay resumes the wait
/// rather than restarting it, and the sleep between polls is durable.
async fn await_score_visibility(
    ctx: &WorkflowContext<'_>,
    trial: &ExperimentTrialRecord,
    expected: &BTreeSet<Uuid>,
    pool: &sqlx::PgPool,
) -> Result<bool, HandlerError> {
    let polls = SCORE_VISIBILITY_TIMEOUT.as_secs() / SCORE_VISIBILITY_POLL_INTERVAL.as_secs();
    let tenant_id = trial.scope.tenant_id();
    let score_run_id = trial.score_run_id;
    for attempt in 0..polls {
        let poll_pool = pool.clone();
        let visible = ctx
            .run(|| {
                let poll_pool = poll_pool.clone();
                async move {
                    let rows = exact_experiment_score_rows_for_tenant(
                        &poll_pool,
                        ExperimentScoreRowsRef {
                            tenant_id,
                            score_run_id,
                        },
                    )
                    .await
                    .map_err(|error| {
                        TerminalError::new(format!(
                            "experiment score visibility query failed: {error}"
                        ))
                    })?;
                    Ok::<_, HandlerError>(Json::from(
                        rows.into_iter()
                            .map(|row| row.score_id)
                            .collect::<BTreeSet<_>>(),
                    ))
                }
            })
            .name("experiment_trial_score_visibility")
            .await?
            .into_inner();
        if expected.is_subset(&visible) {
            return Ok(true);
        }
        tracing::debug!(
            trial_uid = %trial.trial_uid,
            attempt,
            expected = expected.len(),
            visible = visible.len(),
            "waiting for experiment score rows to become query-visible"
        );
        ctx.sleep(SCORE_VISIBILITY_POLL_INTERVAL).await?;
    }
    Ok(false)
}

/// Loads and validates the pinned run's scorecard.
///
/// Returns `Err(response)` in the inner result when the run is missing or its
/// scorecard is unrunnable, so the caller can surface a failed trial rather than
/// an unhandled workflow error.
#[allow(
    clippy::type_complexity,
    reason = "one local two-layer control-flow result"
)]
async fn load_run_scorecard(
    ctx: &WorkflowContext<'_>,
    trial: &ExperimentTrialRecord,
    pool: &sqlx::PgPool,
) -> Result<Result<ExperimentScorecard, ExperimentTrialRunStatusResponse>, HandlerError> {
    let load_pool = pool.clone();
    let tenant_id = trial.scope.tenant_id();
    let run_uid = trial.run_uid;
    let scorecard = ctx
        .run(|| {
            let load_pool = load_pool.clone();
            async move {
                let run = ExperimentStore::new(load_pool)
                    .load_run_for_workflow(tenant_id, run_uid)
                    .await
                    .map_err(moa_error_to_handler_error)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(404, "parent experiment run not found")
                    })?;
                Ok::<_, HandlerError>(Json::from(run.scorecard))
            }
        })
        .name("experiment_trial_load_scorecard")
        .await?
        .into_inner();

    if let Err(error) = moa_experiments::eligibility::require_runnable_scorecard(&scorecard) {
        tracing::warn!(
            trial_uid = %trial.trial_uid,
            error = %error,
            "experiment run scorecard cannot be evaluated by this build"
        );
        return Ok(Err(fail_trial(
            ctx,
            trial,
            FAILURE_SCORECARD_UNRUNNABLE,
            pool,
        )
        .await?));
    }
    Ok(Ok(scorecard))
}

/// Persists a failed trial with a stable, PII-free failure code.
async fn fail_trial(
    ctx: &WorkflowContext<'_>,
    trial: &ExperimentTrialRecord,
    code: &'static str,
    pool: &sqlx::PgPool,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    stop_trial(
        ctx,
        trial.scope,
        trial.trial_uid,
        ExperimentTrialStatus::Failed,
        ExperimentTrialStopReason::Error,
        Some(code.to_string()),
        pool,
    )
    .await
}

fn score_event(
    trial: &ExperimentTrialRecord,
    evidence: &TrialTerminalEvidence,
    score: &EvaluatedScore,
    ts: chrono::DateTime<Utc>,
) -> Result<LineageEvent, HandlerError> {
    let value = match &score.value {
        EvaluatedValue::Numeric(value) => ScoreValue::Numeric(*value),
        EvaluatedValue::Boolean(value) => ScoreValue::Boolean(*value),
        EvaluatedValue::Categorical(value) => ScoreValue::Categorical(value.clone()),
    };
    let value_type = score.value.value_type().as_str().to_string();
    Ok(LineageEvent::Eval(ScoreRecord {
        score_id: score.score_id,
        ts,
        target: ScoreTarget::Session {
            session_id: evidence.session_id,
        },
        storage_partition_id: StoragePartitionId::for_tenant(trial.scope.tenant_id()),
        user_id: None,
        name: score.score_name.clone(),
        value,
        source: ScoreSource::ProductEvaluator,
        model_or_evaluator: format!("{}@{}", score.evaluator_id, score.evaluator_version),
        run_id: Some(trial.score_run_id),
        dataset_id: None,
        comment: None,
        experiment_provenance: Some(ExperimentScoreProvenance {
            experiment_run_uid: trial.run_uid,
            plan_revision_uid: trial.plan_revision_uid,
            trial_uid: trial.trial_uid,
            target: match evidence.target {
                TrialScoreTarget::Session { session_id } => {
                    ExperimentScoreTarget::Session { session_id }
                }
                TrialScoreTarget::ExecutionRun { execution_run_uid } => {
                    ExperimentScoreTarget::ExecutionRun { execution_run_uid }
                }
            },
            evaluator_id: score.evaluator_id.clone(),
            evaluator_version: score.evaluator_version.clone(),
            score_name: score.score_name.clone(),
            value_type,
            evidence_ref: evidence.reference(),
            evidence_hash: evidence.hash().to_vec(),
        }),
    }))
}

/// Classifies a target failure so the blocking evaluators see what actually broke.
pub(super) fn failure_outcome(error: Option<&str>) -> TrialTerminalOutcome {
    // A provider failure and a runtime failure both fail `target_completed`, but
    // they are different operator problems and the evidence records which one.
    match error {
        Some(message)
            if message.contains("provider")
                || message.contains("model")
                || message.contains("rate limit") =>
        {
            TrialTerminalOutcome::ProviderFailure
        }
        _ => TrialTerminalOutcome::RuntimeFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::experiments::ScorecardEffect;

    fn evidence() -> TrialTerminalEvidence {
        TrialTerminalEvidence {
            target: TrialScoreTarget::ExecutionRun {
                execution_run_uid: Uuid::from_u128(21),
            },
            session_id: SessionId(Uuid::from_u128(22)),
            outcome: TrialTerminalOutcome::Completed,
            stop_reason: ExperimentTrialStopReason::TargetTerminal,
            turn_count: 1,
            total_tokens: 10,
            total_cost_cents: 1,
            latest_sequence_num: 3,
            visible_output: Some("done".to_string()),
            failure_code: None,
        }
    }

    fn trial() -> ExperimentTrialRecord {
        ExperimentTrialRecord {
            scope: ActionRuleScope::Tenant {
                tenant_id: TenantId(Uuid::from_u128(1)),
            },
            trial_uid: Uuid::from_u128(2),
            run_uid: Uuid::from_u128(3),
            trial_key: "scenario/persona/profile/variant/0".to_string(),
            status: ExperimentTrialStatus::Running,
            target_kind: ExperimentTargetKind::ExecutionTemplate,
            variant_key: "baseline".to_string(),
            plan_revision_uid: Uuid::from_u128(4),
            persona_id: None,
            profile_id: None,
            scenario_id: None,
            data_bundle_ids: Vec::new(),
            artifact_revision_uids: Vec::new(),
            simulator: moa_experiments::model::ExperimentSimulatorConfig {
                model: ModelId::new("sim"),
                temperature: None,
                max_turns: 1,
                token_budget: None,
                metadata: Value::Null,
            },
            target_model: None,
            seed: None,
            session_id: None,
            execution_run_uid: None,
            score_run_id: Uuid::from_u128(5),
            turn_count: 1,
            stop_reason: None,
            error: None,
            trace_id: None,
            started_at: None,
            completed_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn score_event_carries_exact_provenance_and_never_the_target_output_offline() {
        // Pins: the emitted lineage event names the run, pinned plan revision,
        // trial, exact execution-run target, evaluator version, and evidence
        // digest — and carries no target output text anywhere in the payload.
        let evidence = evidence();
        let trial = trial();
        let score = EvaluatedScore {
            score_id: Uuid::from_u128(9),
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            score_name: "target_completed".to_string(),
            value: EvaluatedValue::Boolean(true),
            effect: ScorecardEffect::Blocking,
            passed: true,
        };
        let ts = Utc::now();

        let event = score_event(&trial, &evidence, &score, ts).expect("event builds");

        let LineageEvent::Eval(record) = &event else {
            panic!("expected an eval record, got {event:?}");
        };
        assert_eq!(record.score_id, Uuid::from_u128(9));
        assert_eq!(record.run_id, Some(Uuid::from_u128(5)));
        assert!(matches!(record.source, ScoreSource::ProductEvaluator));
        assert_eq!(record.model_or_evaluator, "target_completed@v1");
        let provenance = record
            .experiment_provenance
            .as_ref()
            .expect("product-evaluator scores carry provenance");
        assert_eq!(provenance.experiment_run_uid, Uuid::from_u128(3));
        assert_eq!(provenance.plan_revision_uid, Uuid::from_u128(4));
        assert_eq!(provenance.trial_uid, Uuid::from_u128(2));
        assert_eq!(
            provenance.target,
            ExperimentScoreTarget::ExecutionRun {
                execution_run_uid: Uuid::from_u128(21),
            }
        );
        assert_eq!(provenance.evaluator_version, "v1");
        assert_eq!(provenance.evidence_hash, evidence.hash().to_vec());

        let encoded = serde_json::to_string(&event).expect("event serializes");
        assert!(
            !encoded.contains("done"),
            "target output leaked into the score payload: {encoded}"
        );
    }

    #[test]
    fn every_failure_code_is_stable_and_carries_no_payload_offline() {
        // Pins: terminal failure codes are fixed identifiers an operator can
        // alert on, not formatted messages that could carry target content.
        for code in [
            FAILURE_SINK_NOT_SCORE_CAPABLE,
            FAILURE_SCORECARD_UNRUNNABLE,
            FAILURE_SCORE_APPEND_REJECTED,
            FAILURE_SCORE_NOT_VISIBLE,
        ] {
            assert!(code.starts_with("experiment_"), "unexpected code {code}");
            assert!(
                code.chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_'),
                "failure code {code} is not a stable identifier"
            );
        }
    }

    #[test]
    fn provider_failures_are_distinguished_from_runtime_failures_offline() {
        // Pins: both fail the completion blocker, but the evidence records which
        // subsystem broke rather than collapsing them into one opaque failure.
        assert_eq!(
            failure_outcome(Some("provider anthropic returned 529")),
            TrialTerminalOutcome::ProviderFailure
        );
        assert_eq!(
            failure_outcome(Some("execution run ended with status failed")),
            TrialTerminalOutcome::RuntimeFailure
        );
        assert_eq!(failure_outcome(None), TrialTerminalOutcome::RuntimeFailure);
    }
}
