// Review and signal procedure-node support.

use moa_core::wire::procedures::{
    ProcedureCancelRequest, ProcedureCancelResponse, ProcedureReviewDecisionKind,
    ProcedureReviewDecisionRequest, ProcedureReviewDecisionResponse, ProcedureSignalRequest,
    ProcedureSignalResponse,
};

use crate::support::restate_runtime::grant_tenant_operator;

async fn decide_procedure_review(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<ProcedureReviewDecisionResponse> {
    let request = ProcedureReviewDecisionRequest {
        tenant_id,
        run_id,
        node_id: Some("gate".to_string()),
        decision: ProcedureReviewDecisionKind::Approved,
        reason: Some("approved in procedure review e2e".to_string()),
        output: Some(json!({ "approved": true })),
    };
    post_json_with_identity(
        client,
        ingress,
        "Skills",
        "decide_review",
        identity,
        &request,
    )
    .await?
    .json::<ProcedureReviewDecisionResponse>()
    .await
    .context("deserialize procedure review decision response")
}

async fn signal_procedure(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<ProcedureSignalResponse> {
    let request = ProcedureSignalRequest {
        tenant_id,
        run_id,
        node_id: Some("signal".to_string()),
        signal_name: Some("ticket_ready".to_string()),
        payload: json!({ "ticket": "T-123" }),
    };
    post_json_with_identity(client, ingress, "Skills", "signal", identity, &request)
        .await?
        .json::<ProcedureSignalResponse>()
        .await
        .context("deserialize procedure signal response")
}

async fn cancel_procedure(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<ProcedureCancelResponse> {
    let request = ProcedureCancelRequest {
        tenant_id,
        run_id,
        reason: Some("cancelled in procedure e2e".to_string()),
    };
    post_json_with_identity(client, ingress, "Skills", "cancel", identity, &request)
        .await?
        .json::<ProcedureCancelResponse>()
        .await
        .context("deserialize procedure cancel response")
}

async fn wait_for_procedure_node_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    run_id: Uuid,
    expected: ProcedureNodeStatusExpectation<'_>,
) -> Result<ProcedureRunStatus> {
    let request = ProcedureStatusRequest { tenant_id, run_id };
    let mut last_status = None;
    for _attempt in 0..60 {
        let status =
            post_json_with_identity(client, ingress, "Skills", "status", identity, &request)
                .await?
                .json::<ProcedureRunStatus>()
                .await
                .context("deserialize procedure status response")?;
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
                "procedure run failed before reaching {}: {status:?}",
                expected.expected_run_status
            );
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for procedure run {run_id} to reach {} with {}={}; last status: {last_status:?}",
        expected.expected_run_status,
        expected.node_id,
        expected.expected_node_status
    )
}

#[derive(Debug, Clone, Copy)]
struct ProcedureNodeStatusExpectation<'a> {
    expected_run_status: &'a str,
    node_id: &'a str,
    expected_node_status: &'a str,
}

fn review_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: review-gated-procedure
  description: Procedure that pauses on an explicit review node.
  tags:
    - test
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
      nodes:
        - id: start
          kind: start
          ui:
            x: 80
            y: 120
        - id: gate
          kind: review
          input:
            prompt: Approve before completing the procedure.
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

fn wait_signal_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: signal-gated-procedure
  description: Procedure that pauses until an external signal arrives.
  tags:
    - test
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
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
