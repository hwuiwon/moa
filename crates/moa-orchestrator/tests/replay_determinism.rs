//! Replay-determinism coverage for Restate workflow durable steps.

// NONDETERMINISM AUDIT
//
// Consolidate:
// - `Instant::now` and `Utc::now` were previously consulted directly in `Consolidate::run`.
//   They now live inside the journaled `build_consolidate_report` durable step, so Restate
//   captures the first-run report and replays that value instead of consulting live time.
// - `Uuid::now_v7` and `Utc::now` in `record_memory_learning` are inside the journaled
//   `record_memory_learning` durable step. The current graph no-op report skips that step, but
//   the nondeterministic sources are still behind `ctx.run(...)` when it becomes active.
//

mod support;

use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use moa_core::WorkspaceId;
use moa_orchestrator::workflows::consolidate::{
    ConsolidateDurableSteps, ConsolidateReport, ConsolidateRequest, run_consolidate_workflow,
};
use restate_sdk::prelude::HandlerError;
use serde::Serialize;
use serde_json::json;
use support::durable_step_recorder::{Recorder, assert_traces_identical};
use support::fake_clock::FakeClock;

#[derive(Debug, Clone, Serialize)]
struct WorkspaceStateFixture {
    workspace_id: String,
    graph_nodes: Vec<GraphNodeFixture>,
    pending_changes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct GraphNodeFixture {
    uid: String,
    label: String,
    summary: String,
}

struct RecordedConsolidateSteps<'a> {
    recorder: &'a mut Recorder,
    clock: FakeClock,
    duration_ms: u64,
}

#[async_trait]
impl ConsolidateDurableSteps for RecordedConsolidateSteps<'_> {
    async fn mark_consolidation_started(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<(), HandlerError> {
        self.recorder.invoke(
            "Workspace",
            "mark_consolidation_started",
            &json!({
                "key": request.workspace_id.to_string(),
                "request": request.target_date,
            }),
            || (),
        );
        Ok(())
    }

    async fn build_consolidate_report(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<ConsolidateReport, HandlerError> {
        Ok(self.recorder.run("build_consolidate_report", request, || {
            ConsolidateReport::graph_noop(
                request.workspace_id.clone(),
                request.target_date,
                self.clock.now(),
                self.duration_ms,
            )
        }))
    }

    async fn record_memory_learning(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        if report.records_updated == 0
            && report.records_deleted == 0
            && report.relative_dates_normalized == 0
            && report.contradictions_resolved == 0
            && report.confidence_decayed == 0
            && report.errors.is_empty()
        {
            return Ok(());
        }
        self.recorder.run(
            "record_memory_learning",
            report,
            || json!({"recorded": true}),
        );
        Ok(())
    }

    async fn consolidation_completed(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        self.recorder.invoke(
            "Workspace",
            "consolidation_completed",
            &json!({
                "key": report.workspace_id.to_string(),
                "request": report,
            }),
            || (),
        );
        Ok(())
    }
}

#[tokio::test]
async fn consolidate_workflow_first_run_and_replay_emit_identical_durable_steps_for_minimal_input()
{
    let request = ConsolidateRequest {
        workspace_id: WorkspaceId::new("workspace-minimal"),
        target_date: chrono::NaiveDate::from_ymd_opt(2026, 5, 7).expect("valid target date"),
    };
    let clock = fixed_clock();

    let trace1 = run_consolidate_trace(Recorder::recording(), request.clone(), clock.clone()).await;
    clock.advance(Duration::hours(6));
    let trace2 = run_consolidate_trace(Recorder::replaying(trace1.clone()), request, clock).await;

    assert_traces_identical(&trace1, &trace2);
}

#[tokio::test]
async fn consolidate_workflow_replay_with_realistic_workspace_state_emits_identical_steps() {
    let fixture = realistic_workspace_fixture();
    assert_eq!(fixture.graph_nodes.len(), 16);
    assert_eq!(fixture.pending_changes.len(), 6);
    let request = ConsolidateRequest {
        workspace_id: WorkspaceId::new(fixture.workspace_id),
        target_date: chrono::NaiveDate::from_ymd_opt(2026, 5, 7).expect("valid target date"),
    };
    let clock = fixed_clock();

    let trace1 = run_consolidate_trace(Recorder::recording(), request.clone(), clock.clone()).await;
    let trace2 = run_consolidate_trace(Recorder::replaying(trace1.clone()), request, clock).await;

    assert_traces_identical(&trace1, &trace2);
}

async fn run_consolidate_trace(
    recorder: Recorder,
    request: ConsolidateRequest,
    clock: FakeClock,
) -> Vec<support::durable_step_recorder::DurableStep> {
    let mut recorder = recorder;
    let mut steps = RecordedConsolidateSteps {
        recorder: &mut recorder,
        clock,
        duration_ms: 250,
    };
    run_consolidate_workflow(&mut steps, request)
        .await
        .expect("consolidate workflow should succeed");
    recorder.finish()
}

fn fixed_clock() -> FakeClock {
    FakeClock::new(
        Utc.with_ymd_and_hms(2026, 5, 7, 12, 0, 0)
            .single()
            .expect("valid fixed time"),
    )
}

fn realistic_workspace_fixture() -> WorkspaceStateFixture {
    WorkspaceStateFixture {
        workspace_id: "workspace-realistic".to_string(),
        graph_nodes: (0..16)
            .map(|index| GraphNodeFixture {
                uid: format!("node-{index:02}"),
                label: if index % 3 == 0 {
                    "Decision".to_string()
                } else {
                    "Fact".to_string()
                },
                summary: format!("Graph memory summary {index}"),
            })
            .collect(),
        pending_changes: vec![
            "normalize relative date in deploy note".to_string(),
            "merge duplicate release fact".to_string(),
            "decay stale confidence score".to_string(),
            "resolve contradictory rollback note".to_string(),
            "drop orphaned scratch node".to_string(),
            "refresh workspace summary".to_string(),
        ],
    }
}
