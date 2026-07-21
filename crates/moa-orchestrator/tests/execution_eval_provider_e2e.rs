//! Paid, sampled routing/planner/task-quality execution evaluation.

#[path = "execution_execution_support/evaluation.rs"]
mod execution_evaluation;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::GeneratedExecutionCandidate;
use moa_core::types::execution_planning::{
    ExecutionPlannerOutcome, ExecutionPlanningAuditPayload, ExecutionRouteKind,
    ExecutionRouteProvenance, ExecutionRouteStage, ExecutionStrategy,
};
use moa_core::types::identifiers::ModelId;
use moa_eval::execution::{
    EXECUTION_LIVE_REPETITIONS, ExecutionCalibrationArtifact, ExecutionEvalCaseResult,
    ExecutionEvalProvider, ExecutionInvariantSpec, ExecutionJudgeCalibrationStatus,
    ExecutionLiveRunOutcome, ExecutionRoutingLabel, ExecutionTaskQualityCase,
    aggregate_live_execution_outcomes, forecast_live_execution_cost, load_execution_corpus,
    score_contract_case, score_execution_calibration,
};
use moa_execution::repository::{ExecutionRepository, ExecutionScope};
use moa_execution::wire::{ExecutionRunRequest, ExecutionStatusResponse};
use moa_test_support::OrchestratorTestFixture;
use moa_wire::turn::{StartTurnRequest, TurnOutcomeKind};
use sha2::{Digest, Sha256};

use execution_evaluation::collect_execution_eval_snapshot;

const TURN_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_EXECUTION_EVALS=1, an explicit budget, and provider credentials"]
async fn execution_eval_live_provider_repeated_task_quality_provider_e2e() -> Result<()> {
    // Pins: paid execution evaluation authorizes the complete 20x5 batch before
    // dispatch and persists every independent result without making one sample a gate.
    require_live_authorization()?;
    let manifest_path = execution_manifest_path();
    let corpus = load_execution_corpus(&manifest_path)
        .await
        .context("load checked live execution corpus")?;
    let budget_usd = required_budget_usd()?;
    let forecast = forecast_live_execution_cost(
        &corpus.task_quality_cases,
        EXECUTION_LIVE_REPETITIONS,
        budget_usd,
    )
    .context("authorize complete live execution-eval forecast before fixture startup")?;
    eprintln!(
        "authorized live execution eval: runs={} forecast_usd={:.4} budget_usd={:.4}",
        forecast.run_count, forecast.ledger.est_usd, forecast.ledger.budget_usd
    );

    let (calibration_status, calibration_hash) = load_calibration_status()?;
    let (provider_name, model) = configured_live_provider()?;
    let fixture = OrchestratorTestFixture::with_live_execution_fixture()
        .await
        .context("start live-provider execution fixture without scripted override")?;
    let repository = ExecutionRepository::new(
        sqlx::PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect live execution eval repository")?,
    );
    let mut outcomes = Vec::with_capacity(forecast.run_count as usize);
    for case in &corpus.task_quality_cases {
        for repetition in 1..=EXECUTION_LIVE_REPETITIONS {
            let outcome = run_live_case(
                &fixture,
                &repository,
                &corpus.contract_cases,
                case,
                repetition,
                &model,
            )
            .await
            .with_context(|| format!("run live case {} repetition {repetition}", case.case_id))?;
            eprintln!(
                "live execution outcome: case={} repetition={} passed={} route={:?} status={:?} tasks={} cost_microusd={}",
                case.case_id,
                repetition,
                outcome.result.passed,
                outcome.observed_route,
                outcome.result.observed_run_status,
                outcome.result.task_count,
                outcome.result.cost_microusd
            );
            outcomes.push(outcome);
        }
    }

    let mut hashes = BTreeMap::from([
        (
            "routing".to_string(),
            corpus.manifest.routing.sha256.clone(),
        ),
        (
            "contract".to_string(),
            corpus.manifest.contract.sha256.clone(),
        ),
        (
            "task_quality".to_string(),
            corpus.manifest.task_quality.sha256.clone(),
        ),
    ]);
    if let Some(hash) = calibration_hash {
        hashes.insert("judge_calibration".to_string(), hash);
    }
    let report = aggregate_live_execution_outcomes(
        &corpus.task_quality_cases,
        &outcomes,
        EXECUTION_LIVE_REPETITIONS,
        hashes,
        calibration_status,
        ExecutionEvalProvider {
            provider: provider_name.to_string(),
            model: model.as_str().to_string(),
            prompt_version: "execution-live".to_string(),
        },
    )
    .context("aggregate strict live execution report")?;
    let output = live_report_path();
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create live report directory {}", parent.display()))?;
    }
    tokio::fs::write(&output, report.canonical_json()?.as_bytes())
        .await
        .with_context(|| format!("write live execution report {}", output.display()))?;
    eprintln!(
        "wrote live execution report: path={} pass_at_1={:.4} pass_all_k={:.4} respond_on_execute_rate={:.4} durable_strategy_recall={:.4}",
        output.display(),
        report.metrics.pass_at_1.unwrap_or_default(),
        report.metrics.pass_all_k.unwrap_or_default(),
        report.metrics.respond_on_execute_rate.unwrap_or_default(),
        report.metrics.durable_strategy_recall.unwrap_or_default()
    );
    Ok(())
}

async fn run_live_case(
    fixture: &OrchestratorTestFixture,
    repository: &ExecutionRepository,
    contract_cases: &[moa_eval::execution::ExecutionContractCase],
    case: &ExecutionTaskQualityCase,
    repetition: u32,
    model: &ModelId,
) -> Result<ExecutionLiveRunOutcome> {
    let started_at = Instant::now();
    let test = fixture.isolated().await;
    let session_id = test
        .create_session_with_model(
            &format!("execution-live-{}-{repetition}", case.case_id),
            model.clone(),
        )
        .await?;
    let session = test.client().get_session(session_id).await?;
    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                user_message: case.objective.clone(),
                attachments: Vec::new(),
                model: Some(model.as_str().to_string()),
                contact: None,
                max_turns: None,
                execution_template: None,
            },
            None,
        )
        .await
        .context("start independent live execution turn")?;
    if started.queued {
        bail!("new live execution session unexpectedly queued its first turn");
    }
    let turn_id = started
        .turn_id
        .context("live execution turn did not return a turn ID")?;
    let turn = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(&turn_id, TURN_TIMEOUT, POLL_INTERVAL)
        .await
        .context("await live execution root-turn outcome")?;
    let audits = moa_test_support::execution_audits::load_execution_planning_audits(
        &fixture.postgres_url,
        session_id,
    )
    .await
    .context("load live execution planning audits")?;
    let (observed_route, observed_strategy, provenance) = initial_route(&audits)?;
    let contract = score_generated_contract(contract_cases, case, &audits)?;
    let latency_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let result_id = format!("{}#run={repetition}", case.case_id);

    let mut result = match turn.kind {
        TurnOutcomeKind::Accepted { execution_run_uid } => {
            let request = ExecutionRunRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                run_uid: execution_run_uid,
            };
            let _last_status = await_run_settled_or_deadline(test.client(), &request).await?;
            let snapshot = collect_execution_eval_snapshot(
                repository,
                ExecutionScope::Tenant {
                    tenant_id: session.tenant_id,
                },
                &fixture.postgres_url,
                test.client(),
                &request,
                None,
            )
            .await
            .context("collect production live execution snapshot")?;
            let mut specs = vec![
                ExecutionInvariantSpec::TerminalStatusIn {
                    statuses: case.allowed_terminal_statuses.clone(),
                },
                ExecutionInvariantSpec::BudgetWithinApproved,
                ExecutionInvariantSpec::ProgressMatchesTasks,
                ExecutionInvariantSpec::NoRawTaskOutputEvents,
            ];
            if case.tags.iter().any(|tag| tag == "honest-partial")
                && !case
                    .allowed_terminal_statuses
                    .contains(&moa_execution::state::ExecutionRunStatus::Completed)
            {
                specs.push(ExecutionInvariantSpec::MustNotComplete);
            }
            ExecutionEvalCaseResult::evaluate(result_id.clone(), &snapshot, &specs, latency_ms)
                .context("evaluate live execution invariants")?
        }
        TurnOutcomeKind::Completed | TurnOutcomeKind::Cancelled | TurnOutcomeKind::Failed => {
            ExecutionEvalCaseResult {
                case_id: result_id,
                passed: false,
                contract_omission: None,
                contract_score: None,
                impossible_case: false,
                execution_false_completion: false,
                observed_run_status: None,
                observed_route: Some(route_kind(observed_route)),
                observed_strategy,
                route_provenance: Some(provenance.clone()),
                invariants: Vec::new(),
                cost_microusd: 0,
                latency_ms,
                task_count: 0,
                terminal_output_hash: None,
                final_response_hash: None,
            }
        }
    };
    result.observed_route = Some(route_kind(observed_route));
    result.observed_strategy = observed_strategy;
    result.route_provenance = Some(provenance.clone());
    result.cost_microusd = result
        .cost_microusd
        .checked_add(provenance.cost_microusd)
        .context("live route plus execution cost overflowed u64")?;
    if let Some((score, omission)) = contract {
        result.contract_score = Some(score);
        result.contract_omission = Some(omission);
    }
    let route_passed =
        observed_route == case.expected_route && observed_strategy == case.expected_strategy;
    let status_passed = match (case.expected_route, case.expected_strategy) {
        (ExecutionRoutingLabel::Execute, Some(ExecutionStrategy::Durable)) => result
            .observed_run_status
            .is_some_and(|status| case.allowed_terminal_statuses.contains(&status)),
        _ => result.observed_run_status.is_none(),
    };
    let task_passed =
        result.task_count >= case.min_task_count && result.task_count <= case.max_task_count;
    result.passed = route_passed
        && status_passed
        && task_passed
        && !result.execution_false_completion
        && result.contract_omission != Some(true)
        && result.invariants.iter().all(|invariant| invariant.passed);
    Ok(ExecutionLiveRunOutcome {
        case_id: case.case_id.clone(),
        repetition,
        observed_route,
        observed_strategy,
        result,
    })
}

async fn await_run_settled_or_deadline(
    client: &moa_test_support::TestApiClient,
    request: &ExecutionRunRequest,
) -> Result<ExecutionStatusResponse> {
    let timeout = std::env::var("MOA_EXECUTION_EVAL_RUN_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(TURN_TIMEOUT);
    let deadline = Instant::now() + timeout;
    loop {
        let status: ExecutionStatusResponse = client
            .post_call("/Execution/status", request)
            .await
            .context("poll live Execution/status")?;
        if status.run.status.is_terminal() || Instant::now() >= deadline {
            return Ok(status);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn initial_route(
    audits: &[moa_core::types::execution_planning::ExecutionPlanningAuditEnvelope],
) -> Result<(
    ExecutionRoutingLabel,
    Option<ExecutionStrategy>,
    ExecutionRouteProvenance,
)> {
    audits
        .iter()
        .find_map(|audit| match &audit.payload {
            ExecutionPlanningAuditPayload::Route {
                stage: ExecutionRouteStage::Initial,
                decision,
                strategy,
                provenance,
                ..
            } => {
                let route = match (*decision, *strategy) {
                    (ExecutionRouteKind::Respond, None) => ExecutionRoutingLabel::Respond,
                    (ExecutionRouteKind::Execute, Some(_)) => ExecutionRoutingLabel::Execute,
                    (ExecutionRouteKind::NeedsInput, None) => ExecutionRoutingLabel::NeedsInput,
                    _ => return None,
                };
                Some((route, *strategy, provenance.clone()))
            }
            _ => None,
        })
        .context("live execution case has no valid persisted initial route audit")
}

fn score_generated_contract(
    contract_cases: &[moa_eval::execution::ExecutionContractCase],
    task_case: &ExecutionTaskQualityCase,
    audits: &[moa_core::types::execution_planning::ExecutionPlanningAuditEnvelope],
) -> Result<Option<(f64, bool)>> {
    let Some(contract_case_id) = task_case.contract_case_id.as_deref() else {
        return Ok(None);
    };
    let Some(candidate_json) = audits.iter().find_map(|audit| match &audit.payload {
        ExecutionPlanningAuditPayload::PlannerCall {
            outcome: ExecutionPlannerOutcome::Accepted,
            candidate_json: Some(candidate_json),
            ..
        } => Some(candidate_json.as_str()),
        _ => None,
    }) else {
        return Ok(None);
    };
    let candidate = serde_json::from_str::<GeneratedExecutionCandidate>(candidate_json)
        .context("parse generated live planner candidate from persisted audit")?;
    let mut gold = contract_cases
        .iter()
        .find(|case| case.case_id == contract_case_id)
        .with_context(|| format!("task-quality case references unknown `{contract_case_id}`"))?
        .clone();
    gold.candidate = candidate;
    let score = score_contract_case(&gold).context("score generated live goal contract")?;
    Ok(Some((score.macro_f1, score.contract_omission)))
}

const fn route_kind(label: ExecutionRoutingLabel) -> ExecutionRouteKind {
    match label {
        ExecutionRoutingLabel::Respond => ExecutionRouteKind::Respond,
        ExecutionRoutingLabel::Execute => ExecutionRouteKind::Execute,
        ExecutionRoutingLabel::NeedsInput => ExecutionRouteKind::NeedsInput,
    }
}

fn require_live_authorization() -> Result<()> {
    if std::env::var("MOA_RUN_LIVE_EXECUTION_EVALS").as_deref() != Ok("1") {
        bail!("execution live eval requires MOA_RUN_LIVE_EXECUTION_EVALS=1");
    }
    Ok(())
}

fn required_budget_usd() -> Result<f64> {
    let raw = std::env::var("MOA_EXECUTION_EVAL_BUDGET_USD")
        .context("live execution eval requires MOA_EXECUTION_EVAL_BUDGET_USD")?;
    let value = raw
        .parse::<f64>()
        .context("MOA_EXECUTION_EVAL_BUDGET_USD must be a number")?;
    if !value.is_finite() || value <= 0.0 {
        bail!("MOA_EXECUTION_EVAL_BUDGET_USD must be positive and finite");
    }
    Ok(value)
}

fn configured_live_provider() -> Result<(&'static str, ModelId)> {
    for (credential, provider, model) in [
        ("MOA_ANTHROPIC_API_KEY", "anthropic", "claude-sonnet-4-6"),
        ("MOA_OPENAI_API_KEY", "openai", "gpt-5.4-mini"),
        ("MOA_GOOGLE_API_KEY", "google", "gemini-3-flash-preview"),
    ] {
        if std::env::var(credential).is_ok_and(|value| !value.trim().is_empty()) {
            return Ok((provider, ModelId::new(model)));
        }
    }
    bail!(
        "live execution eval requires MOA_ANTHROPIC_API_KEY, MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY"
    )
}

fn load_calibration_status() -> Result<(ExecutionJudgeCalibrationStatus, Option<String>)> {
    let Ok(path) = std::env::var("MOA_EXECUTION_EVAL_CALIBRATION") else {
        return Ok((ExecutionJudgeCalibrationStatus::Unavailable, None));
    };
    let bytes =
        std::fs::read(&path).with_context(|| format!("read execution judge calibration {path}"))?;
    let artifact = serde_json::from_slice::<ExecutionCalibrationArtifact>(&bytes)
        .with_context(|| format!("parse execution judge calibration {path}"))?;
    let report = score_execution_calibration(&artifact)
        .context("score execution judge calibration artifact")?;
    Ok((report.status, Some(format!("{:x}", Sha256::digest(&bytes)))))
}

fn execution_manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../moa-eval/scenarios/execution/manifest.toml")
}

fn live_report_path() -> PathBuf {
    std::env::var("MOA_EXECUTION_EVAL_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/execution-eval/live.json")
        })
}
