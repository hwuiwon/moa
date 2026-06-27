// Review and signal workflow-node support.

use moa_core::wire::workflows::{
    WorkflowCancelRequest, WorkflowCancelResponse, WorkflowReviewDecisionKind,
    WorkflowReviewDecisionRequest, WorkflowReviewDecisionResponse, WorkflowSignalRequest,
    WorkflowSignalResponse,
};

use crate::support::restate_runtime::grant_tenant_operator;

async fn decide_workflow_review(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<WorkflowReviewDecisionResponse> {
    let request = WorkflowReviewDecisionRequest {
        tenant_id,
        run_id,
        node_id: Some("gate".to_string()),
        decision: WorkflowReviewDecisionKind::Approved,
        reason: Some("approved in workflow review e2e".to_string()),
        output: Some(json!({ "approved": true })),
    };
    post_json_with_identity(
        client,
        ingress,
        "Workflows",
        "decide_review",
        identity,
        &request,
    )
    .await?
    .json::<WorkflowReviewDecisionResponse>()
    .await
    .context("deserialize workflow review decision response")
}

async fn signal_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<WorkflowSignalResponse> {
    let request = WorkflowSignalRequest {
        tenant_id,
        run_id,
        node_id: Some("signal".to_string()),
        signal_name: Some("ticket_ready".to_string()),
        payload: json!({ "ticket": "T-123" }),
    };
    post_json_with_identity(client, ingress, "Workflows", "signal", identity, &request)
        .await?
        .json::<WorkflowSignalResponse>()
        .await
        .context("deserialize workflow signal response")
}

async fn cancel_workflow(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<WorkflowCancelResponse> {
    let request = WorkflowCancelRequest {
        tenant_id,
        run_id,
        reason: Some("cancelled in workflow e2e".to_string()),
    };
    post_json_with_identity(client, ingress, "Workflows", "cancel", identity, &request)
        .await?
        .json::<WorkflowCancelResponse>()
        .await
        .context("deserialize workflow cancel response")
}

async fn wait_for_workflow_node_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
    expected: WorkflowNodeStatusExpectation<'_>,
) -> Result<WorkflowRunStatus> {
    let request = WorkflowStatusRequest { tenant_id, run_id };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Workflows", "status", identity, &request)
                .await?
                .json::<WorkflowRunStatus>()
                .await
                .context("deserialize workflow status response")?;
        let node_matches = status
            .node_runs
            .iter()
            .any(|node_run| {
                node_run.node_id == expected.node_id
                    && node_run.status == expected.expected_node_status
            });
        if status.status == expected.expected_run_status && node_matches {
            return Ok(status);
        }
        if status.status == "failed" {
            bail!(
                "workflow run failed before reaching {}: {status:?}",
                expected.expected_run_status
            );
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for workflow run {run_id} to reach {} with {}={}; last status: {last_status:?}",
        expected.expected_run_status,
        expected.node_id,
        expected.expected_node_status
    )
}

#[derive(Debug, Clone, Copy)]
struct WorkflowNodeStatusExpectation<'a> {
    expected_run_status: &'a str,
    node_id: &'a str,
    expected_node_status: &'a str,
}

fn review_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: review-gated-workflow
  description: Workflow that pauses on an explicit review node.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: gate
        kind: review
        input:
          prompt: Approve before completing the workflow.
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        input:
          reviewed: true
        ui:
          x: 520
          y: 120
    edges:
      - id: start-gate
        from: start
        to: gate
      - id: gate-done
        from: gate
        to: done
    ui:
      layout: dagre
"#
}

fn wait_signal_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: signal-gated-workflow
  description: Workflow that pauses until an external signal arrives.
  tags:
    - test
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
        ui:
          x: 80
          y: 120
      - id: signal
        kind: wait_signal
        input:
          name: ticket_ready
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-signal
        from: start
        to: signal
      - id: signal-done
        from: signal
        to: done
    ui:
      layout: dagre
"#
}
