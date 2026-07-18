//! Typed execution-eval assertions shared by deterministic service scenarios.

use anyhow::{Context, Result};
use moa_core::{
    events::Event,
    types::execution_planning::{ExecutionRouteKind, ExecutionStrategy},
};
use moa_eval::execution::{ExecutionEvalCaseResultV1, ExecutionInvariantSpecV1};
use moa_execution::{
    repository::{ExecutionRepository, ExecutionScope},
    wire::ExecutionRunRequest,
};
use moa_test_support::{FixtureCapabilityController, OrchestratorTestFixture, TestApiClient};

use crate::execution_execution_support::{
    assertions::{assert_initial_route, assert_no_execution_lifecycle_events},
    evaluation::{collect_execution_eval_snapshot, collect_repository_execution_eval_snapshot},
};

/// Collects one service snapshot, evaluates typed invariants, and hard-fails any violation.
pub(crate) async fn assert_execution_eval_case(
    fixture: &OrchestratorTestFixture,
    client: &TestApiClient,
    request: &ExecutionRunRequest,
    capability_controller: Option<&FixtureCapabilityController>,
    case_id: &str,
    specs: &[ExecutionInvariantSpecV1],
) -> Result<ExecutionEvalCaseResultV1> {
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect execution service eval collector")?;
    let repository = ExecutionRepository::new(pool.clone());
    let scope = match request.contact_id {
        Some(contact_id) => ExecutionScope::Contact {
            tenant_id: request.tenant_id,
            contact_id,
        },
        None => ExecutionScope::Tenant {
            tenant_id: request.tenant_id,
        },
    };
    let snapshot = collect_execution_eval_snapshot(
        &repository,
        scope,
        &fixture.postgres_url,
        client,
        request,
        capability_controller,
    )
    .await?;
    pool.close().await;
    let result = ExecutionEvalCaseResultV1::evaluate(case_id, &snapshot, specs, 0)?;
    if !result.passed {
        anyhow::bail!("execution eval case `{case_id}` failed: {result:#?}");
    }
    Ok(result)
}

/// Evaluates typed repository invariants for a service test that intentionally bypasses Session.
pub(crate) async fn assert_repository_execution_eval_case(
    fixture: &OrchestratorTestFixture,
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    request: &ExecutionRunRequest,
    case_id: &str,
    specs: &[ExecutionInvariantSpecV1],
) -> Result<ExecutionEvalCaseResultV1> {
    let snapshot = collect_repository_execution_eval_snapshot(
        repository,
        scope,
        &fixture.postgres_url,
        request.session_id,
        request.run_uid,
    )
    .await?;
    let result = ExecutionEvalCaseResultV1::evaluate(case_id, &snapshot, specs, 0)?;
    if !result.passed {
        anyhow::bail!("repository execution eval case `{case_id}` failed: {result:#?}");
    }
    Ok(result)
}

/// Pins a non-Durable route to one typed route audit and zero execution lifecycle events.
pub(crate) fn assert_non_durable_eval(
    audits: &[moa_core::types::execution_planning::ExecutionPlanningAuditEnvelopeV1],
    events: &[moa_core::types::events_stream::EventRecord],
    decision: ExecutionRouteKind,
    strategy: Option<ExecutionStrategy>,
    reason: moa_core::types::execution_planning::ExecutionRouteReason,
) {
    assert!(matches!(
        (decision, strategy),
        (ExecutionRouteKind::Respond, None)
            | (ExecutionRouteKind::Execute, Some(ExecutionStrategy::Inline))
    ));
    assert_initial_route(audits, decision, strategy, reason);
    assert_no_execution_lifecycle_events(events);
    assert!(events.iter().all(|record| {
        !matches!(
            record.event,
            Event::ExecutionRunStarted(_)
                | Event::ExecutionProgress(_)
                | Event::ExecutionInputRequired(_)
                | Event::ExecutionCompleted(_)
                | Event::ExecutionFailed { .. }
                | Event::ExecutionCancelled(_)
                | Event::ExecutionSynthesisRequested(_)
        )
    }));
}
