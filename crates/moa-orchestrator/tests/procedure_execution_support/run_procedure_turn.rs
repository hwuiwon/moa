// Model-turn support for the agent `run_procedure` tool e2e.
//
// Included alongside `common.rs`, so this file reuses that module's imports and
// helpers (`Result`, `Context`, `bail`, `Duration`, `Value`, `json`, `Uuid`,
// `Identity`, `TenantId`, `with_identity`, `sleep`, `post_json_with_identity`,
// `service_url`, ...). It only declares the pieces `common.rs` and `tool.rs` do
// not: pinning a procedure-capable skill onto a session, driving one scripted
// model turn through `Session/start_turn`, and reading the `run_procedure`
// tool call/result back out of the durable event log.

use std::fs;
use std::time::Instant;

use moa_core::wire::turn::{StartTurnRequest, StartTurnResponse};
use moa_core::{
    types::action_policy::ActionPolicyEffect, types::agent::AgentContext, types::agent::AgentKnowledgePolicy, types::agent::AgentKnowledgeScopeMode,
    types::agent::AgentPolicySnapshot, types::agent::AgentSkillPolicy, types::agent::AgentSkillPolicyMode, events::Event, types::events_stream::EventRange, types::events_stream::EventRecord,
    types::identifiers::ModelId, types::contact::SessionActorRef, types::identifiers::SessionId, types::session::SessionMeta, types::session::SessionStatus, types::tools::ToolContent, types::tools::ToolOutput,
};

use crate::support::restate_runtime::grant_session_participant;
use crate::support::session_store_service::{get_events_request, init_session_vo_request};

/// Model id served by the scripted provider override (see `scripted_capabilities`).
const SCRIPTED_MODEL: &str = "scripted-loadtest";

/// The `run_procedure` tool name the agent emits and the governed path routes.
const RUN_PROCEDURE_TOOL: &str = "run_procedure";

/// Builds a session meta that pins exactly one procedure-capable skill for the
/// turn and disables graph-memory retrieval.
///
/// Pinning (mode `Pinned` + `max_visible = 1`) guarantees `pinned_skill_ref` is
/// the only selected skill, so its `run_procedure`/`procedure_status` tool
/// schemas are injected and its `skill://` ref is the sole member of the turn's
/// procedure-capable set. Disabling knowledge keeps the turn's only provider
/// calls the agent-loop calls, so the FIFO scripted responses stay aligned.
/// `created_by` is a concrete identity so the agent-initiated run is authorized
/// by a known session owner rather than rejected as anonymous.
fn pinned_procedure_session_meta(
    tenant_id: TenantId,
    identity: &Identity,
    pinned_skill_ref: &str,
) -> SessionMeta {
    SessionMeta {
        tenant_id,
        model: ModelId::new(SCRIPTED_MODEL),
        created_by: Some(SessionActorRef::Identity { id: identity.id }),
        agent_context: Some(pinned_procedure_agent_context(pinned_skill_ref)),
        ..SessionMeta::default()
    }
}

/// Builds the agent context whose policy snapshot pins one procedure skill and
/// disables graph-memory retrieval.
fn pinned_procedure_agent_context(pinned_skill_ref: &str) -> AgentContext {
    let snapshot = AgentPolicySnapshot {
        knowledge_policy: AgentKnowledgePolicy {
            mode: AgentKnowledgeScopeMode::Disabled,
            ..AgentKnowledgePolicy::default()
        },
        skill_policy: AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Pinned,
            refs: vec![pinned_skill_ref.to_string()],
            max_visible: Some(1),
        },
        ..AgentPolicySnapshot::default()
    };
    let mut context = AgentContext::system_default();
    context.policy_snapshot = json!(snapshot);
    context
}

/// Creates and initializes a session for `meta`, granting the identity direct
/// participation so it can drive `Session/start_turn`.
async fn create_turn_session(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    meta: &SessionMeta,
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

/// Builds a keyed Restate virtual-object URL.
fn session_object_url(ingress: &str, session_id: SessionId, handler: &str) -> String {
    format!(
        "{}/restate/call/Session/{session_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

/// Starts one turn for `user_message` and returns the started turn id.
async fn start_turn(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
    user_message: &str,
) -> Result<String> {
    let request = client.post(session_object_url(ingress, session_id, "start_turn"));
    let response = with_identity(request, identity)
        .json(&StartTurnRequest {
            user_message: user_message.to_string(),
            attachments: Vec::new(),
            model: None,
            contact: None,
            max_turns: Some(6),
        })
        .send()
        .await
        .context("send Session/start_turn")?
        .error_for_status()
        .context("Session/start_turn should succeed")?
        .json::<StartTurnResponse>()
        .await
        .context("deserialize Session/start_turn response")?;
    assert!(!response.queued, "a fresh session should start immediately");
    response
        .turn_id
        .context("start_turn on an idle session should begin a turn immediately")
}

/// Polls `Session/status` until the session leaves `Running`/`Created`, meaning
/// the user message (including the scripted tool call and final response) has
/// fully settled.
async fn wait_for_session_settled(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
    timeout: Duration,
) -> Result<SessionStatus> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        let status = with_identity(
            client.post(session_object_url(ingress, session_id, "status")),
            identity,
        )
        .send()
        .await
        .context("call Session/status")?
        .error_for_status()
        .context("Session/status should succeed")?
        .json::<SessionStatus>()
        .await
        .context("deserialize Session/status")?;
        if !matches!(status, SessionStatus::Created | SessionStatus::Running) {
            return Ok(status);
        }
        last = Some(status);
        sleep(Duration::from_millis(500)).await;
    }

    bail!("session {session_id} did not settle within {timeout:?}; last status: {last:?}")
}

/// Fetches the full durable event log for `session_id`.
async fn fetch_session_events(
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

/// A trivial `start -> end` procedure skill that reaches a terminal state
/// immediately with no node actions.
fn trivial_procedure_skill_source(name: &str, description: &str) -> String {
    format!(
        r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: {name}
  description: {description}
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
        - id: done
          kind: end
          ui:
            x: 280
            y: 120
      edges:
        - id: start-done
          from: start
          to: done
      ui:
        layout: dagre
"#
    )
}

/// Writes a scripted provider fixture whose first agent-loop response emits one
/// `run_procedure` tool call for `target_skill`, and whose second response ends
/// the turn with `final_text`.
///
/// The FIFO `responses` queue drives the two agent-loop calls in order; `default`
/// backstops any later call (e.g. post-turn narration) with the same end-turn
/// text. The turn's only provider calls are the agent-loop calls (graph memory is
/// disabled and skill ranking is keyword-based), so the queue stays aligned.
fn write_run_procedure_script(path: &Path, target_skill: &str, final_text: &str) -> Result<()> {
    let fixture = json!({
        "default": { "completion": { "content": final_text, "tool_calls": [] } },
        "responses": [
            {
                "completion": {
                    "content": "",
                    "tool_calls": [
                        {
                            "name": RUN_PROCEDURE_TOOL,
                            "id": "run-procedure-call-1",
                            "input": { "skill": target_skill, "input": {} }
                        }
                    ]
                }
            },
            { "completion": { "content": final_text, "tool_calls": [] } }
        ]
    });
    let body =
        serde_json::to_vec_pretty(&fixture).context("serialize run_procedure scripted fixture")?;
    fs::write(path, body).context("write run_procedure scripted fixture")
}

/// Request payload for `ActionPolicy/upsert_rule`, mirroring the service wire
/// shape without importing the crate-internal type.
#[derive(serde::Serialize)]
struct RunProcedureRuleRequest {
    tenant_id: TenantId,
    tool_name: String,
    pattern: String,
    effect: ActionPolicyEffect,
    reason: Option<String>,
}

/// Seeds a tenant action-policy rule for the `run_procedure` tool.
async fn upsert_run_procedure_rule(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    tenant_id: TenantId,
    effect: ActionPolicyEffect,
) -> Result<()> {
    post_json_with_identity(
        client,
        ingress,
        "ActionPolicy",
        "upsert_rule",
        identity,
        &RunProcedureRuleRequest {
            tenant_id,
            tool_name: RUN_PROCEDURE_TOOL.to_string(),
            pattern: "*".to_string(),
            effect,
            reason: Some("run_procedure tool e2e rule".to_string()),
        },
    )
    .await?;
    Ok(())
}

/// Returns the `skill` argument of the single `run_procedure` tool call in the
/// event log, if the model emitted one.
fn run_procedure_call_skill(events: &[EventRecord]) -> Option<String> {
    events.iter().find_map(|record| match &record.event {
        Event::ToolCall {
            tool_name, input, ..
        } if tool_name == RUN_PROCEDURE_TOOL => input
            .get("skill")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        _ => None,
    })
}

/// Returns the output and success flag of the `run_procedure` tool result whose
/// call/result pair is in the event log.
fn run_procedure_tool_result(events: &[EventRecord]) -> Option<(&ToolOutput, bool)> {
    let tool_id = events.iter().find_map(|record| match &record.event {
        Event::ToolCall {
            tool_id, tool_name, ..
        } if tool_name == RUN_PROCEDURE_TOOL => Some(*tool_id),
        _ => None,
    })?;
    events.iter().find_map(|record| match &record.event {
        Event::ToolResult {
            tool_id: id,
            output,
            success,
            ..
        } if *id == tool_id => Some((output, *success)),
        _ => None,
    })
}

/// Extracts the `run_id` a successful `run_procedure` output carries in its JSON
/// content block. Rejected or denied outputs carry no such block, so this
/// returns `None` and doubles as the "no run was created" signal.
fn run_id_from_output(output: &ToolOutput) -> Option<Uuid> {
    output.content.iter().find_map(|content| match content {
        ToolContent::Json { data } => data
            .get("run_id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok()),
        ToolContent::Text { .. } => None,
    })
}

/// Returns whether the event log ends the turn with a final assistant response
/// carrying `text`.
fn has_final_brain_response(events: &[EventRecord], text: &str) -> bool {
    events.iter().any(|record| {
        matches!(&record.event, Event::BrainResponse { text: actual, .. } if actual == text)
    })
}

/// Renders the durable event log compactly for failure diagnostics, surfacing the
/// `Error` events that carry a failed turn's reason (the fixture nulls the
/// orchestrator process logs, so the session log is the turn-failure source).
fn describe_events(events: &[EventRecord]) -> String {
    events
        .iter()
        .map(|record| {
            let seq = record.sequence_num;
            match &record.event {
                Event::UserMessage { text, .. } => {
                    format!("#{seq} UserMessage {}", truncate(text))
                }
                Event::BrainResponse { text, .. } => {
                    format!("#{seq} BrainResponse {}", truncate(text))
                }
                Event::ToolCall {
                    tool_name,
                    tool_id,
                    input,
                    ..
                } => format!(
                    "#{seq} ToolCall {tool_name} {tool_id} {}",
                    truncate(&input.to_string())
                ),
                Event::ToolResult {
                    tool_id,
                    success,
                    output,
                    ..
                } => format!(
                    "#{seq} ToolResult {tool_id} success={success} {}",
                    truncate(&output.to_text())
                ),
                Event::Error {
                    message,
                    recoverable,
                } => format!(
                    "#{seq} Error recoverable={recoverable} {}",
                    truncate(message)
                ),
                _ => format!("#{seq} {:?}", record.event_type),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapses whitespace and caps length so diagnostic dumps stay readable.
fn truncate(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 200 {
        format!("{}...", one_line.chars().take(200).collect::<String>())
    } else {
        one_line
    }
}
