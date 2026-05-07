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
// IntentDiscovery:
// - The Postgres query in `load_undefined_segments` uses database time and ordering; it is inside
//   the journaled `load_undefined_segments` durable step, so the segment list is replayed.
// - The LLM response from `LLMGateway.complete` is a durable service invocation; the workflow
//   only computes deterministic prompt bytes before the call.
// - `Uuid::now_v7`, `Utc::now`, embedding calls, and Postgres writes in
//   `persist_discovered_intents` are inside the journaled `persist_discovered_intents` step.
// - `HashSet` is used only for existing-label membership checks; no durable step order depends
//   on iterating it.
// - Workflow config is read from the installed `OrchestratorCtx` before durable calls. The replay
//   tests pass an explicit config snapshot to the extracted workflow body so config drift would
//   show up as changed durable inputs in this harness.

mod support;

use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use moa_core::{CompletionRequest, IntentSource, IntentStatus, ModelId, TenantIntent, WorkspaceId};
use moa_orchestrator::workflows::consolidate::{
    ConsolidateDurableSteps, ConsolidateReport, ConsolidateRequest, run_consolidate_workflow,
};
use moa_orchestrator::workflows::intent_discovery::{
    DiscoveredCluster, DiscoverySegment, IntentDiscoveryDurableSteps, IntentDiscoveryRequest,
    IntentDiscoveryWorkflowConfig, PersistDiscoveredIntentsRequest, run_intent_discovery_workflow,
};
use restate_sdk::prelude::HandlerError;
use serde::Serialize;
use serde_json::json;
use support::durable_step_recorder::{Recorder, assert_traces_identical};
use support::fake_clock::FakeClock;
use uuid::Uuid;

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

struct RecordedIntentDiscoverySteps<'a> {
    recorder: &'a mut Recorder,
    segments: Vec<DiscoverySegment>,
    llm_response: String,
}

#[async_trait]
impl IntentDiscoveryDurableSteps for RecordedIntentDiscoverySteps<'_> {
    async fn load_undefined_segments(
        &mut self,
        tenant_id: &str,
        window_days: u64,
        limit: usize,
    ) -> Result<Vec<DiscoverySegment>, HandlerError> {
        Ok(self.recorder.run(
            "load_undefined_segments",
            &json!({
                "tenant_id": tenant_id,
                "window_days": window_days,
                "limit": limit,
            }),
            || self.segments.clone(),
        ))
    }

    async fn complete_discovery_prompt(
        &mut self,
        request: CompletionRequest,
    ) -> Result<String, HandlerError> {
        Ok(self
            .recorder
            .invoke("LLMGateway", "complete", &request, || {
                self.llm_response.clone()
            }))
    }

    async fn persist_discovered_intents(
        &mut self,
        request: PersistDiscoveredIntentsRequest,
    ) -> Result<Vec<TenantIntent>, HandlerError> {
        Ok(self
            .recorder
            .run("persist_discovered_intents", &request, || {
                build_proposed_intents(&request)
            }))
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

#[tokio::test]
async fn intent_discovery_workflow_first_run_and_replay_emit_identical_durable_steps() {
    let config = intent_config();
    let request = IntentDiscoveryRequest {
        tenant_id: "tenant-replay".to_string(),
    };
    let segments = discovery_segments(5);
    let llm_response = cluster_response("Deployment Debugging", &[0, 1, 2]);

    let trace1 = run_intent_trace(
        Recorder::recording(),
        &config,
        request.clone(),
        segments.clone(),
        llm_response.clone(),
    )
    .await;
    let trace2 = run_intent_trace(
        Recorder::replaying(trace1.clone()),
        &config,
        request,
        segments,
        llm_response,
    )
    .await;

    assert_traces_identical(&trace1, &trace2);
}

#[tokio::test]
async fn intent_discovery_workflow_replay_after_clock_advance_does_not_change_step_outputs() {
    let clock = fixed_clock();
    let config = intent_config();
    let request = IntentDiscoveryRequest {
        tenant_id: "tenant-clock-replay".to_string(),
    };
    let segments = discovery_segments(6);
    let llm_response = cluster_response("Incident Triage", &[0, 1, 2, 3]);

    let trace1 = run_intent_trace(
        Recorder::recording(),
        &config,
        request.clone(),
        segments.clone(),
        llm_response.clone(),
    )
    .await;
    clock.advance(Duration::days(3));
    let trace2 = run_intent_trace(
        Recorder::replaying(trace1.clone()),
        &config,
        request,
        segments,
        llm_response,
    )
    .await;

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

async fn run_intent_trace(
    recorder: Recorder,
    config: &IntentDiscoveryWorkflowConfig,
    request: IntentDiscoveryRequest,
    segments: Vec<DiscoverySegment>,
    llm_response: String,
) -> Vec<support::durable_step_recorder::DurableStep> {
    let mut recorder = recorder;
    let mut steps = RecordedIntentDiscoverySteps {
        recorder: &mut recorder,
        segments,
        llm_response,
    };
    run_intent_discovery_workflow(&mut steps, config, request)
        .await
        .expect("intent discovery workflow should succeed");
    recorder.finish()
}

fn fixed_clock() -> FakeClock {
    FakeClock::new(
        Utc.with_ymd_and_hms(2026, 5, 7, 12, 0, 0)
            .single()
            .expect("valid fixed time"),
    )
}

fn intent_config() -> IntentDiscoveryWorkflowConfig {
    IntentDiscoveryWorkflowConfig {
        enabled: true,
        discovery_window_days: 14,
        min_segments_for_discovery: 3,
        min_cluster_size: 3,
        model_id: ModelId::new("claude-haiku-4"),
    }
}

fn discovery_segments(count: usize) -> Vec<DiscoverySegment> {
    (0..count)
        .map(|index| DiscoverySegment {
            id: deterministic_uuid(index as u128 + 1),
            text: format!("Investigate failed deployment signal {index}"),
        })
        .collect()
}

fn cluster_response(label: &str, member_indices: &[usize]) -> String {
    serde_json::to_string(&vec![DiscoveredCluster {
        label: label.to_string(),
        description: Some(format!("Resolve recurring {label} work")),
        example_queries: vec![
            "debug deployment failure".to_string(),
            "triage release incident".to_string(),
            "fix rollout regression".to_string(),
        ],
        member_indices: member_indices.to_vec(),
        confidence: None,
    }])
    .expect("serialize cluster response")
}

fn build_proposed_intents(request: &PersistDiscoveredIntentsRequest) -> Vec<TenantIntent> {
    request
        .clusters
        .iter()
        .enumerate()
        .filter_map(|(cluster_index, cluster)| {
            let member_count = cluster
                .member_indices
                .iter()
                .filter(|index| request.segments.get(**index).is_some())
                .count();
            if member_count < request.min_cluster_size {
                return None;
            }
            Some(TenantIntent {
                id: deterministic_uuid(1_000 + cluster_index as u128),
                tenant_id: request.tenant_id.clone(),
                label: cluster.label.trim().to_string(),
                description: cluster.description.clone(),
                status: IntentStatus::Proposed,
                source: IntentSource::Discovered,
                catalog_ref: None,
                example_queries: cluster.example_queries.clone(),
                embedding: None,
                segment_count: member_count as u32,
                resolution_rate: None,
            })
        })
        .collect()
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

fn deterministic_uuid(offset: u128) -> Uuid {
    Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0000 + offset)
}
