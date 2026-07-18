//! Real OTLP service coverage for durable execution trace identity and causality.

use anyhow::{Context, Result};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionRequirement, RetryPolicy,
};
use moa_core::{
    config::ExecutionConfig, events::Event, types::execution_planning::ExecutionSourceProvenance,
};
use moa_execution::{
    compiler::{CompileExecutionRequest, compile},
    repository::{ExecutionRepository, ExecutionScope},
    wire::{
        ExecutionPlanningContextRequest, ExecutionPlanningContextResponse, ExecutionRunRequest,
        ExecutionStartRequest, ExecutionStartResponse,
    },
};
use moa_test_support::{FixtureCapabilityOptions, OrchestratorTestFixture};
use serde_json::json;

use crate::execution_execution_support::fixtures::{
    SERVICE_TIMEOUT, await_execution_terminal, list_execution_tasks,
};
use crate::execution_execution_support::{
    assertions::planning_audits, evaluation::collect_execution_eval_snapshot, fixtures::raw_events,
};

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn execution_observability_exports_stable_identity_and_replay_safe_service_spans()
-> Result<()> {
    // Pins: the real Execution/start -> Session activation -> ExecutionRun -> ExecutionTask path
    // exports one stable run identity on every durable hop without putting attempt-local trace
    // headers into replayed Restate commands.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": {
                "content": "execution observability synthesis complete",
                "tool_calls": []
            }
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let capture = fixture.otlp_capture()?;
    capture.clear().await;
    let test = fixture.isolated().await;
    let session_id = test.create_session("execution-observability").await?;
    let session = test.client().get_session(session_id).await?;
    let objective = "return the observable durable execution value";
    let originating_user_sequence_num = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: objective.to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let planning: ExecutionPlanningContextResponse = test
        .client()
        .post_call(
            "/Execution/planning_context",
            &ExecutionPlanningContextRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                requested_template: None,
            },
        )
        .await?;
    let compiled = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: objective.to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "return the exact observable value".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "output-schema".to_string(),
                description: "terminal output matches the declared schema".to_string(),
                requirement_ids: vec!["result".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object", "additionalProperties": false}),
            output_schema: json!({
                "type": "object",
                "properties": {"value": {"const": "observable"}},
                "required": ["value"],
                "additionalProperties": false
            }),
            nodes: vec![ExecutionNode {
                id: "observable-output".to_string(),
                requirement_ids: vec!["result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({
                    "type": "object",
                    "properties": {"value": {"const": "observable"}},
                    "required": ["value"],
                    "additionalProperties": false
                }),
                operation: ExecutionOperation::Output {
                    value: json!({"value": "observable"}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: planning.snapshot.catalog.clone(),
        authorization: planning.snapshot.authorization.clone(),
        approved_budget: planning.snapshot.budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .context("compile observable output-only execution")?;
    let source_provenance: ExecutionSourceProvenance =
        crate::test_source_provenance(&compiled.plan.plan_hash.to_string());
    let started: ExecutionStartResponse = test
        .client()
        .post_call(
            "/Execution/start",
            &ExecutionStartRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                planning_context_uid: planning.planning_context_uid,
                planning_context_hash: planning.planning_context_hash,
                idempotency_key: Some(format!("execution-observability-{session_id}")),
                compiled,
                run_input: json!({}),
                source_provenance,
            },
        )
        .await
        .context("start observable execution through the real service")?;
    assert!(started.created);
    assert!(!started.confirmation_required);

    let run_request = ExecutionRunRequest {
        tenant_id: session.tenant_id,
        contact_id: None,
        session_id,
        run_uid: started.run.run_uid,
    };
    let terminal = await_execution_terminal(test.client(), &run_request).await?;
    assert_eq!(terminal.output, Some(json!({"value": "observable"})));
    let listed_tasks = list_execution_tasks(test.client(), run_request.clone()).await?;
    assert!(listed_tasks.next_cursor.is_none());
    let [listed_task] = listed_tasks.tasks.as_slice() else {
        anyhow::bail!(
            "observable execution should persist exactly one task: {:?}",
            listed_tasks.tasks
        );
    };

    let repository = ExecutionRepository::new(
        sqlx::PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect execution observability repository")?,
    );
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let persisted_run = repository
        .load_run(scope, started.run.run_uid)
        .await?
        .context("observable execution run should remain persisted")?;
    let persisted_task = repository
        .load_task(scope, persisted_run.run_uid, listed_task.task_id)
        .await?
        .context("observable execution task should remain persisted")?;
    assert_eq!(persisted_task.node_id, listed_task.node_id);

    let eval_snapshot = collect_execution_eval_snapshot(
        &repository,
        scope,
        &fixture.postgres_url,
        test.client(),
        &run_request,
        None,
    )
    .await?;
    assert_eq!(eval_snapshot.run.run_uid, persisted_run.run_uid);
    assert_eq!(eval_snapshot.run.status, persisted_run.status);
    assert_eq!(eval_snapshot.run.terminal_output, persisted_run.output);
    assert_eq!(eval_snapshot.run.terminal_gaps, persisted_run.terminal_gaps);
    assert_eq!(
        eval_snapshot.run.budget_ledger.limit,
        persisted_run.approved_budget
    );
    assert_eq!(
        eval_snapshot.run.budget_ledger.reserved,
        persisted_run.reserved
    );
    assert_eq!(
        eval_snapshot.run.budget_ledger.consumed,
        persisted_run.consumed
    );
    assert_eq!(eval_snapshot.run.progress.total_tasks, 1);
    assert_eq!(eval_snapshot.run.progress.completed_tasks, 1);
    assert_eq!(eval_snapshot.tasks.len(), 1);
    assert_eq!(eval_snapshot.tasks[0].task_id, persisted_task.task_id);
    assert_eq!(eval_snapshot.tasks[0].status, persisted_task.status);
    assert_eq!(
        eval_snapshot.planning_audits.len(),
        planning_audits(&fixture.postgres_url, session_id)
            .await?
            .len()
    );
    let events = raw_events(test.client(), session_id).await?;
    assert_eq!(
        eval_snapshot.harness.session_events.run_started,
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ExecutionRunStarted(_)))
            .count() as u64
    );
    assert_eq!(
        eval_snapshot.harness.session_events.progress,
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ExecutionProgress(_)))
            .count() as u64
    );
    assert_eq!(eval_snapshot.harness.session_events.terminal, 1);
    assert_eq!(eval_snapshot.harness.session_events.raw_task_output, 0);
    let serialized = serde_json::to_value(&eval_snapshot)?;
    let task = serialized["tasks"][0]
        .as_object()
        .context("serialized eval task must be an object")?;
    assert!(!task.contains_key("input"));
    assert!(!task.contains_key("output"));
    assert!(!task.contains_key("error"));
    assert!(!task.contains_key("citations"));

    let run_uid = persisted_run.run_uid.to_string();
    let task_id = persisted_task.task_id.to_string();
    let plan_hash = persisted_run.active_plan_hash.to_string();
    let plan_revision = persisted_task.plan_revision.to_string();
    let run_span = capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("ExecutionRun")
                && span.attribute("restate.handler") == Some("run")
                && span.attribute("moa.execution.run_uid") == Some(run_uid.as_str())
        })
        .await
        .context("wait for exported ExecutionRun handler span")?;
    let task_span = capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("ExecutionTask")
                && span.attribute("restate.handler") == Some("run")
                && span.attribute("moa.execution.run_uid") == Some(run_uid.as_str())
                && span.attribute("moa.execution.task_id") == Some(task_id.as_str())
        })
        .await
        .context("wait for exported ExecutionTask handler span")?;
    let activation_span = capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("Session")
                && span.attribute("restate.handler") == Some("execution_run_started")
                && span.attribute("moa.execution.run_uid") == Some(run_uid.as_str())
        })
        .await
        .context("wait for exported Session/execution_run_started handler span")?;
    let service_span = capture
        .wait_for_span(SERVICE_TIMEOUT, |span| {
            span.attribute("restate.service") == Some("Execution")
                && span.attribute("restate.handler") == Some("start")
                && span.attribute("moa.execution.run_uid") == Some(run_uid.as_str())
        })
        .await
        .context("wait for exported Execution/start handler span")?;

    assert_eq!(
        run_span.attribute("moa.execution.run_uid"),
        Some(run_uid.as_str())
    );
    assert_eq!(
        task_span.attribute("moa.execution.run_uid"),
        Some(run_uid.as_str())
    );
    assert_eq!(
        task_span.attribute("moa.execution.task_id"),
        Some(task_id.as_str())
    );
    assert_eq!(
        task_span.attribute("moa.execution.node_id"),
        Some(persisted_task.node_id.as_str())
    );
    assert_eq!(
        task_span.attribute("moa.execution.plan_hash"),
        Some(plan_hash.as_str())
    );
    assert_eq!(
        task_span.attribute("moa.execution.plan_revision"),
        Some(plan_revision.as_str())
    );
    assert_eq!(
        service_span.attribute("moa.execution.run_uid"),
        Some(run_uid.as_str())
    );
    assert_eq!(
        activation_span.attribute("moa.execution.run_uid"),
        Some(run_uid.as_str())
    );

    let indexed_span = capture
        .span_by_run_uid(&run_uid)
        .await
        .context("fixture should index a real exported span by execution run UID")?;
    assert_eq!(
        indexed_span.attribute("moa.execution.run_uid"),
        Some(run_uid.as_str())
    );
    for span in [
        &service_span,
        &activation_span,
        &run_span,
        &task_span,
        &indexed_span,
    ] {
        assert_eq!(
            span.resource_attribute("service.name"),
            Some(capture.resource_name())
        );
        assert_eq!(
            span.resource_attribute("deployment.environment"),
            Some("test")
        );
        assert_eq!(
            span.resource_attribute("service.version"),
            Some(capture.resource_name())
        );
        assert_eq!(span.scope_name(), capture.resource_name());
    }
    Ok(())
}
