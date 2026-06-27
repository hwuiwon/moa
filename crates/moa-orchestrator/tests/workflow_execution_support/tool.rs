// Tool, agent, and sub-agent workflow-node support.

use std::fs;

use moa_core::{Event, EventRange, EventRecord, ModelId, SessionId, SubAgentChildRef};

use crate::support::restate_runtime::grant_session_participant;
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, test_session_meta,
};

async fn start_workflow_with_session(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    workflow_ref: &str,
    input: Value,
    session_id: Option<SessionId>,
) -> Result<WorkflowRunResponse> {
    let request = WorkflowRunRequest {
        tenant_id,
        workflow_ref: workflow_ref.to_string(),
        input,
        session_id,
        idempotency_key: Some(format!("workflow-{}", Uuid::now_v7())),
    };
    post_json_with_identity(client, ingress, "Workflows", "run", identity, &request)
        .await?
        .json::<WorkflowRunResponse>()
        .await
        .context("deserialize workflow run response")
}

async fn create_session(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    meta: &moa_core::SessionMeta,
) -> Result<SessionId> {
    let create_request = client.post(service_url(ingress, "SessionStore", "create_session"));
    let session_id = with_identity(create_request, identity)
        .json(meta)
        .send()
        .await
        .context("create session via Restate ingress")?
        .error_for_status()
        .context("create_session should succeed")?
        .json::<SessionId>()
        .await
        .context("deserialize create_session response")?;
    grant_session_participant(identity, session_id).await?;

    client
        .post(service_url(ingress, "SessionStore", "init_session_vo"))
        .json(&init_session_vo_request(session_id, meta.clone()))
        .send()
        .await
        .context("initialize session VO state")?
        .error_for_status()
        .context("init_session_vo should succeed")?;

    Ok(session_id)
}

async fn seed_active_session_children(
    client: &reqwest::Client,
    ingress: &str,
    session_id: SessionId,
) -> Result<()> {
    for index in 0..4 {
        let child = SubAgentChildRef {
            id: format!("{session_id}-active-child-{index}"),
            task_hash: format!("active-hash-{index}"),
            budget_tokens: 256,
            terminal: None,
        };
        client
            .post(object_url(ingress, "Session", session_id, "register_child"))
            .json(&child)
            .send()
            .await
            .context("seed active session child")?
            .error_for_status()
            .context("register_child should accept seeded active child")?;
    }
    Ok(())
}

async fn fetch_events(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    post_json_with_identity(
        client,
        ingress,
        "SessionStore",
        "get_events",
        identity,
        &get_events_request(session_id, EventRange::all()),
    )
    .await?
    .json::<Vec<EventRecord>>()
    .await
    .context("deserialize session events")
}

fn object_url(ingress: &str, service: &str, object_id: SessionId, handler: &str) -> String {
    format!(
        "{}/{service}/{object_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

fn tool_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: tool-search-workflow
  description: Workflow that executes one idempotent tool node.
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
      - id: search
        kind: tool
        tool_refs:
          - tool://file_search
        input:
          pattern: "*"
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-search
        from: start
        to: search
      - id: search-done
        from: search
        to: done
    ui:
      layout: dagre
"#
}

fn agent_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: agent-adapter-workflow
  description: Workflow that adapts one deterministic graph node into a session turn.
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
      - id: agent
        kind: agent
        max_turns: 1
        input:
          prompt: Summarize the deterministic skill adapter status.
          model: scripted-loadtest
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-agent
        from: start
        to: agent
      - id: agent-done
        from: agent
        to: done
    ui:
      layout: dagre
"#
}

fn sub_agent_workflow_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: sub-agent-fanout-workflow
  description: Workflow that adapts one deterministic graph node into sub-agent delegation.
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
      - id: delegate
        kind: sub_agent
        max_turns: 1
        input:
          task: Inspect whether this workflow node respects existing delegation fan-out limits.
          task_name: fanout-check
          tool_subset: []
          budget_tokens: 256
          timeout_ms: 0
        ui:
          x: 280
          y: 120
      - id: done
        kind: end
        ui:
          x: 520
          y: 120
    edges:
      - id: start-delegate
        from: start
        to: delegate
      - id: delegate-done
        from: delegate
        to: done
    ui:
      layout: dagre
"#
}

fn write_scripted_agent_fixture(path: &Path, final_text: &str) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": final_text,
                "tool_calls": []
            }
        }
    });
    let body = serde_json::to_vec_pretty(&fixture).context("serialize scripted agent fixture")?;
    fs::write(path, body).context("write scripted agent fixture")
}
