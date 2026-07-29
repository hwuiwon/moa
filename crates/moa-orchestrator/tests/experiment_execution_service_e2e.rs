//! Canonical execution-template experiment coverage through production Restate services.

#![cfg(feature = "integration")]

use moa_core::types::experiments::{ExperimentScorecard, ScorecardEffect, ScorecardRequirement};
use std::collections::BTreeSet;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionGoalTemplate, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionPlanTemplate, ExecutionRequirement, ExecutionTaskResult,
    RetryPolicy,
};
use moa_artifacts::reference::ArtifactRef;
use moa_core::events::Event;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::execution_planning::{
    ExecutionAuditReport, ExecutionCompileOutcome, ExecutionCompileSource,
    ExecutionPlanningAuditPayload, ExecutionRunAdmissionStatus, ExecutionSourceProvenance,
    PinnedExecutionTemplateRef,
};
use moa_core::types::identifiers::ModelId;
use moa_core::types::session::SessionStatus;
use moa_execution::state::{ExecutionTaskId, ExecutionTaskStatus};
use moa_execution::wire::{
    ExecutionPlanningContextSnapshot, ExecutionRunRequest, planning_context_hash,
};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentTarget, ExperimentVariant,
};
use moa_test_support::{FixtureCapabilityOptions, OrchestratorTestFixture, TestApiClient};
use moa_wire::experiments::{
    ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusRequest,
    ExperimentRunStatusResponse, ExperimentTrialsRequest, ExperimentTrialsResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::time::Instant;
use uuid::Uuid;

use crate::execution_execution_support::assertions::{
    JournalRequestRole, assert_completed_terminal, assert_strict_event_order, event_count,
    final_brain_response, journal_requests, journal_roles, planning_audits, sole_event_sequence,
};
use crate::execution_execution_support::fixtures::{
    POLL_INTERVAL, SERVICE_TIMEOUT, await_session_settled, list_execution_tasks, publish_skill,
    raw_events,
};

#[allow(dead_code)]
#[path = "execution_execution_support/mod.rs"]
mod execution_execution_support;

const TEMPLATE_SKILL_NAME: &str = "experiment-canonical-resolution";
const OBJECTIVE: &str = "Resolve the exact experiment case through its pinned execution template.";
const FINAL_RESPONSE: &str = "The canonical experiment execution completed.";

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn experiment_execution_template_runs_through_execution_run_service_e2e() -> Result<()> {
    // Pins: an API-imported and published exact skill revision admitted through Experiments/run
    // must use Execution/planning_context and Execution/start, persist ExperimentTemplate
    // provenance, export stable ExperimentRun -> execution-run correlation, finish its canonical
    // task, and register no removed procedure workflow or data.
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": {
                "completion": {
                    "content": FINAL_RESPONSE,
                    "tool_calls": []
                }
            }
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("canonical-experiment-template").await?;
    let session = test.client().get_session(session_id).await?;
    let tenant_id = session.tenant_id;
    let published = publish_skill(
        &fixture,
        test.client(),
        tenant_id,
        TEMPLATE_SKILL_NAME,
        template_skill_source(),
        template_skill_markdown(),
    )
    .await?;
    let exact_template = PinnedExecutionTemplateRef {
        skill_ref: published.skill_ref.clone(),
        revision_uid: published.revision_uid,
    };
    let run_input = json!({
        "case_id": "EXP-500",
        "resolution": "canonical-execution"
    });
    let target = ExperimentTarget::ExecutionTemplate {
        template: exact_template.clone(),
        objective: OBJECTIVE.to_string(),
        input: run_input.clone(),
        session_id: Some(session_id),
        idempotency_key: Some(format!("execution-template-{}", Uuid::now_v7())),
    };
    let variant = ExperimentVariant {
        name: "exact-published-template".to_string(),
        model: Some(ModelId::new("scripted-loadtest")),
        artifact_revision_uids: vec![published.revision_uid],
        skill_refs: Vec::new(),
        execution_template: Some(exact_template.clone()),
        metadata: json!({"lane": "experiment_execution_service_e2e"}),
    };
    let scorecard = ExperimentScorecard::new(vec![ScorecardRequirement {
        evaluator_id: "target_completed".to_string(),
        evaluator_version: "v1".to_string(),
        config: json!({}),
        effect: ScorecardEffect::Blocking,
    }])
    .expect("fixture scorecard is valid");
    let score_run_id = Uuid::now_v7();
    let experiment_idempotency_key = format!("experiment-execution-{}", Uuid::now_v7());
    let request = ExperimentRunRequest {
        tenant_id,
        name: "canonical execution-template service experiment".to_string(),
        plan_revision_uid: None,
        target: Some(serde_json::to_value(&target)?),
        variant: Some(serde_json::to_value(&variant)?),
        scorecard: Some(scorecard.clone()),
        score_run_id: Some(score_run_id),
        idempotency_key: Some(experiment_idempotency_key.clone()),
        agent_revision_variants: Vec::new(),
    };

    let otlp_capture = fixture.otlp_capture()?;
    otlp_capture.clear().await;
    let observability_service_name = otlp_capture.resource_name().to_string();
    fixture.reset_scripted_requests()?;
    let admitted: ExperimentRunResponse = test
        .client()
        .post_call("/Experiments/run", &request)
        .await
        .context("admit exact execution-template experiment")?;
    assert_eq!(admitted.tenant_id, tenant_id);
    assert_eq!(admitted.status, ExperimentRunStatus::Accepted.as_str());
    assert_eq!(admitted.score_run_id, score_run_id);
    assert_eq!(admitted.session_id, Some(session_id));
    assert_eq!(admitted.execution_run_uid, None);

    let terminal_experiment =
        await_experiment_terminal(test.client(), tenant_id, admitted.run_uid).await?;
    assert_eq!(
        terminal_experiment.status,
        ExperimentRunStatus::Completed.as_str(),
        "canonical experiment failed: {terminal_experiment:#?}"
    );
    assert_eq!(
        terminal_experiment.target_kind.as_deref(),
        Some("execution_template")
    );
    assert_eq!(terminal_experiment.score_run_id, Some(score_run_id));
    assert_eq!(terminal_experiment.session_id, Some(session_id));
    assert_eq!(terminal_experiment.error, None);
    let execution_run_uid = terminal_experiment
        .execution_run_uid
        .context("completed experiment omitted its canonical execution-run link")?;

    let experiment_record: ExperimentRunRecord =
        serde_json::from_value(terminal_experiment.run.clone())
            .context("decode full experiment status record")?;
    assert_eq!(
        experiment_record.scope,
        ActionRuleScope::Tenant { tenant_id }
    );
    assert_eq!(experiment_record.run_uid, admitted.run_uid);
    assert_eq!(experiment_record.status, ExperimentRunStatus::Completed);
    assert_eq!(experiment_record.target, target);
    assert_eq!(experiment_record.variant, variant);
    assert_eq!(experiment_record.scorecard, scorecard);
    assert_eq!(experiment_record.score_run_id, score_run_id);
    assert_eq!(experiment_record.session_id, Some(session_id));
    assert_eq!(experiment_record.execution_run_uid, Some(execution_run_uid));
    assert_eq!(
        experiment_record.artifact_revision_uids,
        vec![published.revision_uid]
    );
    assert_eq!(
        experiment_record.idempotency_key.as_deref(),
        Some(experiment_idempotency_key.as_str())
    );
    let trials: ExperimentTrialsResponse = test
        .client()
        .post_call(
            "/Experiments/trials",
            &ExperimentTrialsRequest {
                tenant_id,
                run_uid: admitted.run_uid,
                status: None,
                limit: Some(10),
            },
        )
        .await
        .context("list direct-target experiment trials")?;
    assert_eq!(trials.tenant_id, tenant_id);
    assert_eq!(trials.run_uid, admitted.run_uid);
    assert!(
        trials.trials.is_empty(),
        "a direct authoritative run target must not fabricate a plan trial"
    );

    let execution_request = ExecutionRunRequest {
        tenant_id,
        contact_id: None,
        session_id,
        run_uid: execution_run_uid,
    };
    let execution_status = test
        .client()
        .post_call("/Execution/status", &execution_request)
        .await
        .context("read linked canonical execution status")?;
    assert_completed_terminal(&execution_status, 1, 1);
    assert_eq!(execution_status.output, Some(run_input.clone()));
    assert_eq!(execution_status.run.total_tasks, 1);
    assert_eq!(execution_status.run.completed_tasks, 1);
    assert_eq!(execution_status.run.failed_tasks, 0);
    assert_eq!(execution_status.run.budget_ledger.consumed.tasks, 1);
    assert_eq!(execution_status.run.budget_ledger.consumed.cost_microusd, 0);
    assert_eq!(execution_status.run.budget_ledger.consumed.tokens, 0);
    assert_eq!(execution_status.run.budget_ledger.consumed.tool_calls, 0);
    assert_eq!(
        execution_status.run.budget_ledger.consumed.retrieved_bytes,
        0
    );
    assert_eq!(
        execution_status.run.budget_ledger.reserved,
        Default::default()
    );
    assert!(!execution_status.run.budget_ledger.overrun);

    let tasks = list_execution_tasks(test.client(), execution_request.clone()).await?;
    assert_eq!(tasks.next_cursor, None);
    assert_eq!(tasks.tasks.len(), 1);
    let task = &tasks.tasks[0];
    assert_eq!(
        task.task_id,
        ExecutionTaskId::derive(execution_run_uid, "output", "")?
    );
    assert_eq!(task.node_id, "output");
    assert_eq!(task.item_key, "");
    assert_eq!(task.status, ExecutionTaskStatus::Completed);
    assert_eq!(task.attempt, 1);
    assert_eq!(task.generation, 1);
    assert_eq!(task.input, json!({}));
    let outcome = task
        .outcome
        .as_ref()
        .context("completed output task omitted its typed outcome")?;
    assert_eq!(outcome.schema_version, 1);
    assert_eq!(outcome.usage.cost_microusd, 0);
    assert_eq!(outcome.usage.tokens, 0);
    assert_eq!(outcome.usage.tool_calls, 0);
    assert_eq!(outcome.usage.retrieved_bytes, 0);
    let ExecutionTaskResult::Completed { output, citations } = &outcome.result else {
        bail!("output task did not complete successfully: {outcome:?}");
    };
    assert_eq!(output, &run_input);
    assert!(citations.is_empty());

    let settled_status = match await_session_settled(test.client(), session_id).await {
        Ok(status) => status,
        Err(error) => {
            let events = raw_events(test.client(), session_id)
                .await
                .context("load experiment session events after settlement timeout")?;
            let scripted_requests = journal_requests(fixture.scripted_requests()?)?;
            bail!(
                "{error:#}; durable events: {events:#?}; scripted request roles: {:#?}",
                journal_roles(&scripted_requests)
            );
        }
    };
    assert_eq!(settled_status, SessionStatus::Paused);
    let events = raw_events(test.client(), session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, session_id).await?;
    assert_eq!(
        audits.len(),
        1,
        "experiment admission must compile exactly once"
    );
    let audit = &audits[0];
    let objective_sequence = sole_event_sequence(
        &events,
        "experiment objective",
        |event| matches!(event, Event::UserMessage { text, .. } if text == OBJECTIVE),
    );
    assert_eq!(audit.schema_version, 1);
    assert_eq!(audit.tenant_id, tenant_id);
    assert_eq!(audit.contact_id, None);
    assert_eq!(audit.session_id, Some(session_id));
    assert_eq!(audit.originating_sequence, Some(objective_sequence));
    let final_plan_hash = {
        let ExecutionPlanningAuditPayload::Compile {
            source,
            operation_key,
            run_uid,
            plan_revision,
            outcome,
            candidate_hash,
            final_plan_hash,
            validation_report,
            ..
        } = &audit.payload
        else {
            bail!("experiment audit was not a strict compiler record: {audit:?}");
        };
        assert_eq!(*source, ExecutionCompileSource::ExperimentTemplate);
        assert_eq!(
            operation_key,
            &format!("experiment:{}:{}:none", admitted.run_uid, score_run_id)
        );
        assert_eq!(*run_uid, None);
        assert_eq!(*plan_revision, None);
        assert_eq!(*outcome, ExecutionCompileOutcome::Accepted);
        assert_hex_hash("compile candidate", candidate_hash);
        let final_plan_hash = final_plan_hash
            .clone()
            .context("accepted experiment compile omitted final plan hash")?;
        assert_hex_hash("final plan", &final_plan_hash);
        let report: ExecutionAuditReport = serde_json::from_str(validation_report)
            .context("decode strict experiment compiler report")?;
        let ExecutionAuditReport::Compiler {
            violations,
            omitted_violations,
            full_report_hash,
        } = report
        else {
            bail!("experiment compile audit used a non-compiler report");
        };
        assert!(violations.is_empty());
        assert_eq!(omitted_violations, 0);
        assert_hex_hash("compiler report", &full_report_hash);
        final_plan_hash
    };

    let started_sequence = sole_event_sequence(&events, "execution run start", |event| {
        matches!(
            event,
            Event::ExecutionRunStarted(started)
                if started.run_uid == execution_run_uid
                    && started.originating_user_sequence_num == objective_sequence
                    && started.plan_revision == 1
                    && started.status == ExecutionRunAdmissionStatus::Queued
                    && started.confirmation.is_none()
        )
    });
    let completed_sequence = sole_event_sequence(&events, "execution completion", |event| {
        matches!(
            event,
            Event::ExecutionCompleted(summary) if summary.run_uid == execution_run_uid
        )
    });
    let synthesis_sequence = sole_event_sequence(&events, "execution synthesis", |event| {
        matches!(
            event,
            Event::ExecutionSynthesisRequested(requested)
                if requested.run_uid == execution_run_uid
                    && requested.originating_user_sequence_num == objective_sequence
        )
    });
    let response_sequence = sole_event_sequence(
        &events,
        "final brain response",
        |event| matches!(event, Event::BrainResponse { text, .. } if text == FINAL_RESPONSE),
    );
    assert_strict_event_order(&[
        ("objective", objective_sequence),
        ("execution start", started_sequence),
        ("execution completion", completed_sequence),
        ("synthesis request", synthesis_sequence),
        ("final response", response_sequence),
    ]);
    assert_eq!(
        event_count(&events, |event| matches!(event, Event::ToolCall { .. })),
        0
    );
    assert_eq!(
        event_count(&events, |event| matches!(event, Event::ToolResult { .. })),
        0
    );
    assert_eq!(final_brain_response(&events)?, FINAL_RESPONSE);

    let scripted_requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        journal_roles(&scripted_requests),
        vec![JournalRequestRole::Synthesis],
        "the exact template must not invoke a planner or task-local model"
    );
    assert!(scripted_requests.iter().all(|request| {
        request
            .response_format
            .as_ref()
            .is_none_or(|format| format.name != "generated_execution_candidate")
    }));

    assert_canonical_planning_and_provenance(
        &fixture.postgres_url,
        tenant_id,
        session_id,
        objective_sequence,
        execution_run_uid,
        admitted.run_uid,
        score_run_id,
        &exact_template,
        &run_input,
        &final_plan_hash,
    )
    .await?;
    assert_no_legacy_procedure_fields(
        "experiment status",
        &serde_json::to_value(&terminal_experiment)?,
    );
    assert_no_legacy_procedure_fields(
        "execution status",
        &serde_json::to_value(&execution_status)?,
    );
    assert_no_legacy_procedure_fields("execution tasks", &serde_json::to_value(&tasks)?);
    assert_no_legacy_procedure_fields("session events", &serde_json::to_value(&events)?);
    assert_registered_runtime_uses_execution_workflows(&fixture.admin_url).await?;

    let experiment_run_uid = admitted.run_uid.to_string();
    let execution_run_uid = execution_run_uid.to_string();
    let experiment_span = otlp_capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("ExperimentRun")
                && span.attribute("restate.handler") == Some("run")
                && span.attribute("moa.experiment.run_uid") == Some(experiment_run_uid.as_str())
                && span.attribute("moa.experiment.execution_run_uid")
                    == Some(execution_run_uid.as_str())
        })
        .await
        .context("wait for exported ExperimentRun span with canonical execution link")?;
    assert_eq!(
        experiment_span.attribute("moa.experiment.run_uid"),
        Some(experiment_run_uid.as_str())
    );
    assert_eq!(
        experiment_span.attribute("moa.experiment.execution_run_uid"),
        Some(execution_run_uid.as_str())
    );
    assert_eq!(
        experiment_span.resource_attribute("service.name"),
        Some(observability_service_name.as_str())
    );

    let execution_span = otlp_capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("ExecutionRun")
                && span.attribute("restate.handler") == Some("run")
                && span.attribute("moa.execution.run_uid") == Some(execution_run_uid.as_str())
        })
        .await
        .context("wait for exported ExecutionRun span with stable run UID")?;
    assert_eq!(
        execution_span.attribute("moa.execution.run_uid"),
        Some(execution_run_uid.as_str())
    );
    assert_eq!(
        execution_span.resource_attribute("service.name"),
        Some(observability_service_name.as_str())
    );
    let activation_span = otlp_capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("Session")
                && span.attribute("restate.handler") == Some("execution_run_started")
                && span.attribute("moa.execution.run_uid") == Some(execution_run_uid.as_str())
        })
        .await;
    let activation_span =
        activation_span.context("wait for Session execution-run activation span")?;
    let service_span = otlp_capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("Execution")
                && span.attribute("restate.handler") == Some("start")
                && span.attribute("moa.execution.run_uid") == Some(execution_run_uid.as_str())
        })
        .await
        .context("wait for Execution/start span with stable execution-run identity")?;
    assert_eq!(
        service_span.attribute("moa.execution.run_uid"),
        Some(execution_run_uid.as_str())
    );
    assert_eq!(
        activation_span.attribute("moa.execution.run_uid"),
        Some(execution_run_uid.as_str())
    );
    for span in [&service_span, &activation_span, &execution_span] {
        assert_eq!(
            span.resource_attribute("service.name"),
            Some(observability_service_name.as_str())
        );
    }

    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the final persistence assertion keeps every immutable experiment identity explicit"
)]
async fn assert_canonical_planning_and_provenance(
    postgres_url: &str,
    tenant_id: moa_core::types::identifiers::TenantId,
    session_id: moa_core::types::identifiers::SessionId,
    objective_sequence: u64,
    execution_run_uid: Uuid,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
    exact_template: &PinnedExecutionTemplateRef,
    run_input: &Value,
    expected_plan_hash: &str,
) -> Result<()> {
    let pool = PgPool::connect(postgres_url)
        .await
        .context("connect for final canonical execution assertions")?;
    let row = sqlx::query_as::<_, (Uuid, String, String, String, Value, Value, Value)>(
        r#"
        SELECT r.planning_context_uid,
               r.planning_context_hash,
               r.initial_plan_hash,
               r.active_plan_hash,
               r.source_provenance,
               r.input,
               c.snapshot
        FROM moa.execution_run AS r
        JOIN moa.execution_planning_context AS c
          ON c.planning_context_uid = r.planning_context_uid
         AND c.tenant_id = r.tenant_id
         AND c.contact_id IS NOT DISTINCT FROM r.contact_id
        WHERE r.run_uid = $1
        "#,
    )
    .bind(execution_run_uid)
    .fetch_one(&pool)
    .await
    .context("load final execution admission and immutable planning context")?;
    pool.close().await;

    assert_ne!(row.0, Uuid::nil());
    assert_hex_hash("planning context", &row.1);
    assert_eq!(row.2, expected_plan_hash);
    assert_eq!(row.3, expected_plan_hash);
    assert_eq!(row.5, *run_input);

    let provenance: ExecutionSourceProvenance =
        serde_json::from_value(row.4).context("decode exact execution source provenance")?;
    assert_eq!(
        provenance,
        ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref: exact_template.skill_ref.clone(),
            skill_template_revision_uid: exact_template.revision_uid,
            experiment_run_uid,
            score_run_id,
            trial_uid: None,
        }
    );

    let snapshot: ExecutionPlanningContextSnapshot =
        serde_json::from_value(row.6).context("decode immutable planning-context snapshot")?;
    snapshot
        .validate()
        .context("validate immutable planning-context snapshot")?;
    assert_eq!(snapshot.tenant_id, tenant_id);
    assert_eq!(snapshot.contact_id, None);
    assert_eq!(snapshot.session_id, session_id);
    assert_eq!(snapshot.originating_user_sequence_num, objective_sequence);
    assert_eq!(planning_context_hash(&snapshot)?.to_string(), row.1);
    assert_eq!(snapshot.execution_templates.len(), 1);
    let pinned = &snapshot.execution_templates[0];
    assert_eq!(pinned.skill_ref.to_string(), exact_template.skill_ref);
    assert_eq!(pinned.revision_uid, exact_template.revision_uid);
    let skill_ref = ArtifactRef::from_str(&exact_template.skill_ref)?;
    assert!(
        snapshot.authorization.skill_refs.contains(&skill_ref),
        "the exact experiment template revision must remain authorized in the immutable context"
    );
    Ok(())
}

async fn await_experiment_terminal(
    client: &TestApiClient,
    tenant_id: moa_core::types::identifiers::TenantId,
    run_uid: Uuid,
) -> Result<ExperimentRunStatusResponse> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let status: ExperimentRunStatusResponse = client
            .post_call(
                "/Experiments/status",
                &ExperimentRunStatusRequest { tenant_id, run_uid },
            )
            .await
            .context("poll canonical experiment status")?;
        if matches!(status.status.as_str(), "completed" | "failed" | "cancelled") {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!(
                "experiment {run_uid} did not become terminal within {SERVICE_TIMEOUT:?}; last status: {status:?}"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[derive(Deserialize)]
struct DeploymentsResponse {
    deployments: Vec<Deployment>,
}

#[derive(Deserialize)]
struct Deployment {
    services: Vec<RegisteredService>,
}

#[derive(Deserialize)]
struct RegisteredService {
    name: String,
}

async fn assert_registered_runtime_uses_execution_workflows(admin_url: &str) -> Result<()> {
    let response = reqwest::Client::new()
        .get(format!("{}/deployments", admin_url.trim_end_matches('/')))
        .send()
        .await
        .context("list Restate deployments for final workflow assertion")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read Restate deployment response")?;
    if !status.is_success() {
        bail!("Restate deployment list returned {status}: {body}");
    }
    let deployments: DeploymentsResponse =
        serde_json::from_str(&body).context("decode Restate deployment list")?;
    let names = deployments
        .deployments
        .into_iter()
        .flat_map(|deployment| deployment.services)
        .map(|service| service.name)
        .collect::<BTreeSet<_>>();
    for required in [
        "Artifacts",
        "Experiments",
        "ExperimentRun",
        "Execution",
        "ExecutionRun",
        "ExecutionTask",
    ] {
        assert!(
            names.contains(required),
            "canonical runtime omitted registered service/workflow {required}: {names:?}"
        );
    }
    assert!(
        names
            .iter()
            .all(|name| !name.to_ascii_lowercase().contains("procedure")),
        "legacy procedure workflow remained registered: {names:?}"
    );
    Ok(())
}

fn assert_no_legacy_procedure_fields(label: &str, value: &Value) {
    fn visit(path: &str, value: &Value, violations: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let child_path = format!("{path}/{key}");
                    if key.to_ascii_lowercase().contains("procedure") {
                        violations.push(child_path.clone());
                    }
                    if matches!(key.as_str(), "kind" | "type" | "event_type" | "source")
                        && child
                            .as_str()
                            .is_some_and(|text| text.to_ascii_lowercase().contains("procedure"))
                    {
                        violations.push(format!("{child_path}={child}"));
                    }
                    visit(&child_path, child, violations);
                }
            }
            Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    visit(&format!("{path}/{index}"), child, violations);
                }
            }
            _ => {}
        }
    }

    let mut violations = Vec::new();
    visit("$", value, &mut violations);
    assert!(
        violations.is_empty(),
        "{label} retained legacy procedure fields or discriminators: {violations:?}"
    );
}

fn assert_hex_hash(label: &str, value: &str) {
    assert_eq!(value.len(), 64, "{label} hash had the wrong length");
    assert!(
        value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{label} hash was not hexadecimal: {value}"
    );
}

fn template_skill_source() -> String {
    let template = ExecutionPlanTemplate {
        goal: ExecutionGoalTemplate {
            requirements: vec![ExecutionRequirement {
                id: "canonical_resolution".to_string(),
                description: "Return the exact experiment case resolution.".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "canonical_output_schema".to_string(),
                description: "The exact experiment result satisfies its schema.".to_string(),
                requirement_ids: vec!["canonical_resolution".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: template_io_schema(),
            output_schema: template_io_schema(),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["canonical_resolution".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: template_io_schema(),
                operation: ExecutionOperation::Output {
                    value: json!({
                        "case_id": {"$ref": "$.input.case_id"},
                        "resolution": {"$ref": "$.input.resolution"}
                    }),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
    };
    format!(
        "api_version: moa.artifact/v1\nkind: skill\nmetadata:\n  name: {TEMPLATE_SKILL_NAME}\n  description: Canonical execution template for experiment service coverage.\nstatus: draft\ndefinition:\n  type: skill\n  spec:\n    instructions:\n      path: SKILL.md\n    inputs: {}\n    outputs: {}\n    execution_plan: {}\n",
        serde_json::to_string(&template_io_schema()).expect("serialize template input schema"),
        serde_json::to_string(&template_io_schema()).expect("serialize template output schema"),
        serde_json::to_string(&template).expect("serialize exact execution template")
    )
}

fn template_io_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["case_id", "resolution"],
        "properties": {
            "case_id": {"type": "string"},
            "resolution": {"type": "string"}
        }
    })
}

fn template_skill_markdown() -> &'static str {
    r#"---
name: experiment-canonical-resolution
description: Canonical execution template for experiment service coverage.
---

# Experiment Canonical Resolution

Use the exact structured input supplied by the pinned experiment target.
"#
}
