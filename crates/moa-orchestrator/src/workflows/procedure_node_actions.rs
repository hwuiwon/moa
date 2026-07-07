//! Adapters for executing effectful procedure nodes through existing services.

use std::time::Duration;

use moa_artifacts::reference::ArtifactRef;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::memory::{MemoryIngestDocument, MemoryIngestRequest, MemorySearchRequest};
use moa_core::wire::turn::{QueueMessageRequest, TurnOutcome, TurnOutcomeKind};
use moa_core::{
    ActionPolicyEffect, ContactId, DelegationTool, SessionActorRef, SessionId, SessionMeta,
    SessionStatus, SpawnWorkerInput, TenantId, ToolCallId, ToolCallRequest, ToolInvocation,
    ToolOutput, WaitWorkerInput,
};
use moa_skills::procedure::interpreter::ProcedureNodeRequest;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::delegation::{DelegationParent, execute_delegation_tool, storage_user_id};
use crate::objects::session::{
    AttachSessionTurnWaiterInput, RemoveSessionTurnWaiterInput, SessionClient,
};
use crate::restate_identity::with_identity_headers;
use crate::services::action_policy::{ActionPolicyClient, PrepareActionReviewRequest};
use crate::services::memory::MemoryClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::services::tool_executor::ToolExecutorClient;

const AGENT_NODE_WAIT_TIMEOUT: Duration = Duration::from_secs(180);
const WORKER_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Runtime context needed to execute one procedure side-effect node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureNodeActionContext {
    /// Tenant that owns the procedure run.
    pub tenant_id: TenantId,
    /// Durable procedure run identifier.
    pub run_uid: Uuid,
    /// Stable procedure node identifier.
    pub node_id: String,
    /// Optional owning session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Identity that authorized the procedure run.
    pub identity: Identity,
    /// Restate promise key that resolves when the procedure run is cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_promise_key: Option<String>,
}

/// Result of attempting one governed procedure side effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcedureNodeActionOutcome {
    /// The node executed and produced procedure-state output.
    Completed {
        /// Output to store under the procedure node id.
        output: Value,
    },
    /// The node failed without a pending review.
    Failed {
        /// Failure reason.
        error: String,
    },
    /// The procedure run was cancelled before the node produced an output.
    Cancelled {
        /// Cancellation reason.
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentNodeTurnWait {
    Outcome(TurnOutcome),
    Cancelled(String),
}

async fn procedure_cancel_requested(
    ctx: &WorkflowContext<'_>,
    action_context: &ProcedureNodeActionContext,
) -> Result<Option<String>, HandlerError> {
    let Some(promise_key) = &action_context.cancel_promise_key else {
        return Ok(None);
    };
    ctx.peek_promise::<String>(promise_key)
        .await
        .map_err(HandlerError::from)
}

/// Executes a procedure action/tool node through the same policy and tool services as agent turns.
pub async fn execute_procedure_node_action(
    ctx: &WorkflowContext<'_>,
    action_context: ProcedureNodeActionContext,
    request: ProcedureNodeRequest,
) -> Result<ProcedureNodeActionOutcome, HandlerError> {
    if let Some(reason) = procedure_cancel_requested(ctx, &action_context).await? {
        return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
    }

    match &request {
        ProcedureNodeRequest::Agent {
            input, max_turns, ..
        } => {
            return execute_agent_node(ctx, action_context, input, *max_turns).await;
        }
        ProcedureNodeRequest::Worker {
            input, max_turns, ..
        } => {
            return execute_worker_node(ctx, action_context, input, *max_turns).await;
        }
        ProcedureNodeRequest::MemoryRead { input, .. } => {
            return execute_memory_read_node(ctx, action_context, input).await;
        }
        ProcedureNodeRequest::MemoryWrite { input, .. } => {
            return execute_memory_write_node(ctx, action_context, input).await;
        }
        _ => {}
    }

    let invocation = match invocation_from_request(&request) {
        Ok(invocation) => invocation,
        Err(error) => return Ok(ProcedureNodeActionOutcome::Failed { error }),
    };
    let tool_call_id = stable_tool_call_id(action_context.run_uid, &action_context.node_id);
    let idempotency_key = Some(format!(
        "procedure:{}:{}",
        action_context.run_uid, action_context.node_id
    ));
    let session = procedure_session_meta(ctx, &action_context).await?;
    let prepared_action = ctx
        .service_client::<ActionPolicyClient>()
        .prepare_action_review(Json(PrepareActionReviewRequest {
            session: session.clone(),
            invocation: invocation.clone(),
            review_id: stable_review_id(action_context.run_uid, &action_context.node_id),
            tool_call_id,
            worker_id: None,
            origin_kind: Some("procedure".to_string()),
            origin_id: Some(action_context.run_uid.to_string()),
            origin_step_id: Some(action_context.node_id.clone()),
            idempotency_key: idempotency_key.clone(),
        }))
        .call()
        .await?
        .into_inner();

    if matches!(prepared_action.effect, ActionPolicyEffect::Deny) {
        let reason = prepared_action
            .reason
            .unwrap_or_else(|| "denied by action policy".to_string());
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: format!("Tool {} denied by action policy: {reason}", invocation.name),
        });
    }

    let tool_request = ToolCallRequest {
        tool_call_id,
        provider_tool_use_id: invocation.id.clone(),
        tool_name: invocation.name.clone(),
        input: invocation.input.clone(),
        active_canary: None,
        session_id: action_context.session_id,
        tenant_id: action_context.tenant_id,
        user_id: storage_user_id(&session),
        idempotency_key,
        trusted_sandbox_manifest: None,
        worker_id: None,
    };

    if matches!(prepared_action.effect, ActionPolicyEffect::AdminReview) {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: format!(
                "procedure action `{}` requires tenant admin review; add an explicit procedure review node before this action or update the action policy",
                invocation.name
            ),
        });
    }

    if let Some(reason) = procedure_cancel_requested(ctx, &action_context).await? {
        return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
    }

    let output = ctx
        .service_client::<ToolExecutorClient>()
        .execute(Json::from(tool_request))
        .call()
        .await?
        .into_inner();
    if output.is_error {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: output.to_text(),
        });
    }

    Ok(ProcedureNodeActionOutcome::Completed {
        output: tool_output_value(output),
    })
}

async fn execute_agent_node(
    ctx: &WorkflowContext<'_>,
    action_context: ProcedureNodeActionContext,
    input: &Value,
    max_turns: Option<u32>,
) -> Result<ProcedureNodeActionOutcome, HandlerError> {
    let Some(session_id) = action_context.session_id else {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: "procedure agent node requires an associated session_id".to_string(),
        });
    };
    if let Some(reason) = procedure_cancel_requested(ctx, &action_context).await? {
        return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
    }
    if matches!(max_turns, Some(0)) {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: "procedure agent node max_turns must be at least 1".to_string(),
        });
    }
    let user_message = match prompt_from_input(input) {
        Some(prompt) => prompt,
        None => {
            return Ok(ProcedureNodeActionOutcome::Failed {
                error:
                    "procedure agent node requires input.instruction, input.prompt, or input.message"
                        .to_string(),
            });
        }
    };
    let response = with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .queue_message(Json::from(QueueMessageRequest {
                user_message,
                attachments: Vec::new(),
                model: input
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                contact: None,
                max_turns,
            })),
        &action_context.identity,
    )
    .call()
    .await?
    .into_inner();
    let Some(turn_id) = response.started_turn_id else {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: "procedure agent node was queued behind an active session turn".to_string(),
        });
    };

    let outcome =
        match wait_for_agent_node_turn(ctx, &action_context, session_id, turn_id.clone()).await? {
            AgentNodeTurnWait::Outcome(outcome) => outcome,
            AgentNodeTurnWait::Cancelled(reason) => {
                return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
            }
        };
    if outcome.kind == TurnOutcomeKind::Completed {
        Ok(ProcedureNodeActionOutcome::Completed {
            output: json!({
                "turn_id": outcome.turn_id,
                "message": outcome.message,
                "max_turns": max_turns,
            }),
        })
    } else {
        Ok(ProcedureNodeActionOutcome::Failed {
            error: outcome.message,
        })
    }
}

async fn wait_for_agent_node_turn(
    ctx: &WorkflowContext<'_>,
    action_context: &ProcedureNodeActionContext,
    session_id: SessionId,
    turn_id: String,
) -> Result<AgentNodeTurnWait, HandlerError> {
    let (awakeable_id, completion) = ctx.awakeable::<String>();
    let attached = ctx
        .object_client::<SessionClient>(session_id.to_string())
        .attach_turn_waiter(Json::from(AttachSessionTurnWaiterInput {
            turn_id: turn_id.clone(),
            awakeable_id: awakeable_id.clone(),
        }))
        .call()
        .await?
        .into_inner();
    if let Some(outcome) = attached.outcome {
        return Ok(AgentNodeTurnWait::Outcome(outcome));
    }

    if let Some(cancel_promise_key) = action_context.cancel_promise_key.as_deref() {
        return restate_sdk::select! {
            outcome = completion => {
                parse_turn_outcome(&outcome?).map(AgentNodeTurnWait::Outcome)
            },
            reason = ctx.promise::<String>(cancel_promise_key) => {
                remove_agent_node_turn_waiter(ctx, session_id, turn_id.clone(), awakeable_id).await?;
                Ok(AgentNodeTurnWait::Cancelled(reason?))
            },
            _ = ctx.sleep(AGENT_NODE_WAIT_TIMEOUT) => {
                remove_agent_node_turn_waiter(ctx, session_id, turn_id.clone(), awakeable_id).await?;
                Err(TerminalError::new(format!(
                    "procedure agent node timed out waiting for turn {turn_id}"
                )).into())
            }
        };
    }

    restate_sdk::select! {
        outcome = completion => parse_turn_outcome(&outcome?),
        _ = ctx.sleep(AGENT_NODE_WAIT_TIMEOUT) => {
            remove_agent_node_turn_waiter(ctx, session_id, turn_id.clone(), awakeable_id).await?;
            Err(TerminalError::new(format!(
                "procedure agent node timed out waiting for turn {turn_id}"
            )).into())
        }
    }
    .map(AgentNodeTurnWait::Outcome)
}

async fn remove_agent_node_turn_waiter(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: String,
    awakeable_id: String,
) -> Result<(), HandlerError> {
    ctx.object_client::<SessionClient>(session_id.to_string())
        .remove_turn_waiter(Json::from(RemoveSessionTurnWaiterInput {
            turn_id,
            awakeable_id,
        }))
        .call()
        .await?;
    Ok(())
}

fn parse_turn_outcome(raw: &str) -> Result<TurnOutcome, HandlerError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize procedure agent turn outcome: {error}"
        ))
        .into()
    })
}

async fn execute_worker_node(
    ctx: &WorkflowContext<'_>,
    action_context: ProcedureNodeActionContext,
    input: &Value,
    max_turns: Option<u32>,
) -> Result<ProcedureNodeActionOutcome, HandlerError> {
    let Some(session_id) = action_context.session_id else {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: "procedure worker node requires an associated session_id".to_string(),
        });
    };
    if let Some(reason) = procedure_cancel_requested(ctx, &action_context).await? {
        return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
    }
    if matches!(max_turns, Some(0)) {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: "procedure worker node max_turns must be at least 1".to_string(),
        });
    }
    let mut spawn_input = match spawn_input_from_node(input) {
        Ok(input) => input,
        Err(error) => return Ok(ProcedureNodeActionOutcome::Failed { error }),
    };
    spawn_input.max_turns = max_turns;
    let meta = with_identity_headers(
        ctx.service_client::<RestateSessionStoreClient>()
            .get_session(Json(session_id)),
        &action_context.identity,
    )
    .call()
    .await?
    .into_inner();
    let parent = DelegationParent::RootSession {
        session_id,
        meta: &meta,
    };
    let spawn_output = match execute_delegation_tool(
        ctx,
        parent,
        DelegationTool::Spawn(spawn_input),
        None,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            return Ok(ProcedureNodeActionOutcome::Failed {
                error: format!("{error:?}"),
            });
        }
    };
    let Some(spawn) = structured_output::<moa_core::SpawnWorkerOutput>(&spawn_output) else {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: "procedure worker node spawn returned no structured output".to_string(),
        });
    };
    if let Some(reason) = procedure_cancel_requested(ctx, &action_context).await? {
        return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
    }
    let wait_timeout_ms = input
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(WORKER_WAIT_TIMEOUT_MS);
    let wait_output = match execute_delegation_tool(
        ctx,
        parent,
        DelegationTool::Wait(WaitWorkerInput {
            worker_id: spawn.worker_id.clone(),
            timeout_ms: wait_timeout_ms,
        }),
        None,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            return Ok(ProcedureNodeActionOutcome::Failed {
                error: format!("{error:?}"),
            });
        }
    };
    let Some(wait) = structured_output::<moa_core::WaitWorkerOutput>(&wait_output) else {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: "procedure worker node wait returned no structured output".to_string(),
        });
    };
    if wait.timed_out {
        return Ok(ProcedureNodeActionOutcome::Failed {
            error: format!(
                "procedure worker node timed out waiting for {}",
                wait.worker_id
            ),
        });
    }
    Ok(ProcedureNodeActionOutcome::Completed {
        output: json!({
            "spawn": spawn,
            "wait": wait,
            "max_turns": max_turns,
        }),
    })
}

async fn execute_memory_read_node(
    ctx: &WorkflowContext<'_>,
    action_context: ProcedureNodeActionContext,
    input: &Value,
) -> Result<ProcedureNodeActionOutcome, HandlerError> {
    let request = match memory_search_request_from_node(&action_context, input) {
        Ok(request) => request,
        Err(error) => return Ok(ProcedureNodeActionOutcome::Failed { error }),
    };
    if let Some(reason) = procedure_cancel_requested(ctx, &action_context).await? {
        return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
    }
    let contact_id = request.contact_id;
    let response = with_identity_headers(
        ctx.service_client::<MemoryClient>()
            .search(Json(request.clone())),
        &action_context.identity,
    )
    .call()
    .await?
    .into_inner();

    Ok(ProcedureNodeActionOutcome::Completed {
        output: json!({
            "query": response.query,
            "contact_id": contact_id,
            "hits": response.hits,
        }),
    })
}

async fn execute_memory_write_node(
    ctx: &WorkflowContext<'_>,
    action_context: ProcedureNodeActionContext,
    input: &Value,
) -> Result<ProcedureNodeActionOutcome, HandlerError> {
    let request = match memory_ingest_request_from_node(&action_context, input) {
        Ok(request) => request,
        Err(error) => return Ok(ProcedureNodeActionOutcome::Failed { error }),
    };
    if let Some(reason) = procedure_cancel_requested(ctx, &action_context).await? {
        return Ok(ProcedureNodeActionOutcome::Cancelled { reason });
    }
    let contact_id = request.contact_id;
    let response = with_identity_headers(
        ctx.service_client::<MemoryClient>()
            .ingest_documents(Json(request)),
        &action_context.identity,
    )
    .call()
    .await?
    .into_inner();

    Ok(ProcedureNodeActionOutcome::Completed {
        output: json!({
            "tenant_id": response.tenant_id,
            "contact_id": contact_id,
            "results": response.results,
        }),
    })
}

fn memory_search_request_from_node(
    context: &ProcedureNodeActionContext,
    input: &Value,
) -> Result<MemorySearchRequest, String> {
    let query = required_string(
        input,
        &["query", "prompt", "question"],
        "procedure memory_read node requires input.query, input.prompt, or input.question",
    )?;
    Ok(MemorySearchRequest {
        tenant_id: context.tenant_id,
        contact_id: contact_id_from_input_or_identity(input, &context.identity)?,
        query,
        limit: memory_limit(input),
        label_filter: string_array_field(input, "label_filter")?,
        max_pii_class: optional_string_field(input, "max_pii_class")?,
        use_reranker: input
            .get("use_reranker")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn memory_ingest_request_from_node(
    context: &ProcedureNodeActionContext,
    input: &Value,
) -> Result<MemoryIngestRequest, String> {
    Ok(MemoryIngestRequest {
        tenant_id: context.tenant_id,
        contact_id: contact_id_from_input_or_identity(input, &context.identity)?,
        documents: memory_documents_from_node(context, input)?,
    })
}

fn memory_documents_from_node(
    context: &ProcedureNodeActionContext,
    input: &Value,
) -> Result<Vec<MemoryIngestDocument>, String> {
    if let Some(documents) = input.get("documents") {
        let mut documents = serde_json::from_value::<Vec<MemoryIngestDocument>>(documents.clone())
            .map_err(|error| {
                format!("procedure memory_write input.documents is invalid: {error}")
            })?;
        if documents.is_empty() {
            return Err("procedure memory_write node requires at least one document".to_string());
        }
        for (index, document) in documents.iter_mut().enumerate() {
            document.metadata =
                procedure_memory_metadata(document.metadata.clone(), context, index)?;
        }
        return Ok(documents);
    }

    let content = required_string(
        input,
        &["content", "text", "fact"],
        "procedure memory_write node requires input.content, input.text, input.fact, or input.documents",
    )?;
    let source_name = optional_string_field(input, "source_name")?
        .unwrap_or_else(|| format!("procedure:{}:{}", context.run_uid, context.node_id));
    let source_uri = optional_string_field(input, "source_uri")?;
    let metadata = input.get("metadata").cloned().unwrap_or_else(|| json!({}));

    Ok(vec![MemoryIngestDocument {
        source_name,
        content,
        source_uri,
        metadata: procedure_memory_metadata(metadata, context, 0)?,
    }])
}

fn procedure_memory_metadata(
    metadata: Value,
    context: &ProcedureNodeActionContext,
    index: usize,
) -> Result<Value, String> {
    let mut map = match metadata {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        _ => return Err("procedure memory_write metadata must be an object".to_string()),
    };
    map.insert("procedure_run_uid".to_string(), json!(context.run_uid));
    map.insert("procedure_node_id".to_string(), json!(context.node_id));
    map.insert("procedure_document_index".to_string(), json!(index));
    Ok(Value::Object(map))
}

fn contact_id_from_input_or_identity(
    input: &Value,
    identity: &Identity,
) -> Result<Option<ContactId>, String> {
    if let Some(value) = input.get("contact_id")
        && !value.is_null()
    {
        return parse_contact_id(value).map(Some);
    }
    if identity.identity_type == IdentityType::Contact {
        return Ok(Some(ContactId(identity.id)));
    }
    Ok(None)
}

fn parse_contact_id(value: &Value) -> Result<ContactId, String> {
    let Some(value) = value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err("procedure memory node input.contact_id must be a UUID string".to_string());
    };
    Uuid::parse_str(value)
        .map(ContactId)
        .map_err(|error| format!("procedure memory node input.contact_id is invalid: {error}"))
}

fn memory_limit(input: &Value) -> u32 {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(8)
        .clamp(1, u64::from(u32::MAX)) as u32
}

fn string_array_field(input: &Value, field: &str) -> Result<Vec<String>, String> {
    let Some(value) = input.get(field) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(format!(
            "procedure memory node input.{field} must be an array"
        ));
    };
    values
        .iter()
        .map(|value| {
            value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                format!("procedure memory node input.{field} entries must be strings")
            })
        })
        .collect()
}

fn optional_string_field(input: &Value, field: &str) -> Result<Option<String>, String> {
    let Some(value) = input.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Some(value.to_string()))
        .ok_or_else(|| format!("procedure memory node input.{field} must be a string"))
}

fn required_string(input: &Value, fields: &[&str], message: &str) -> Result<String, String> {
    fields
        .iter()
        .find_map(|field| input.get(*field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| message.to_string())
}

fn invocation_from_request(request: &ProcedureNodeRequest) -> Result<ToolInvocation, String> {
    let (node_id, artifact_ref, tool_refs, input) = match request {
        ProcedureNodeRequest::Action {
            node_id,
            artifact_ref,
            input,
        } => (
            node_id,
            artifact_ref.as_ref(),
            Vec::<ArtifactRef>::new(),
            input,
        ),
        ProcedureNodeRequest::Tool {
            node_id,
            tool_refs,
            input,
        } => (node_id, None, tool_refs.clone(), input),
        ProcedureNodeRequest::SkillAction {
            node_id,
            artifact_ref,
            input,
        } => (
            node_id,
            artifact_ref.as_ref(),
            Vec::<ArtifactRef>::new(),
            input,
        ),
        _ => {
            return Err("procedure node request is not an executable action node".to_string());
        }
    };
    let tool_name = tool_name_from_input(input)
        .or_else(|| tool_name_from_refs(&tool_refs))
        .or_else(|| artifact_ref.and_then(tool_name_from_artifact_ref))
        .ok_or_else(|| format!("procedure node `{node_id}` did not specify a tool name"))?;
    Ok(ToolInvocation {
        id: Some(format!("procedure:{node_id}")),
        name: tool_name,
        input: tool_input(input),
    })
}

fn prompt_from_input(input: &Value) -> Option<String> {
    input
        .get("instruction")
        .or_else(|| input.get("prompt"))
        .or_else(|| input.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn spawn_input_from_node(input: &Value) -> Result<SpawnWorkerInput, String> {
    let task = input
        .get("task")
        .or_else(|| input.get("instruction"))
        .or_else(|| input.get("prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "procedure worker node requires input.task, input.instruction, or input.prompt"
                .to_string()
        })?
        .to_string();
    let tool_subset = input
        .get("tool_subset")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let budget_tokens = input
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(moa_core::default_worker_budget_tokens);
    Ok(SpawnWorkerInput {
        task,
        tool_subset,
        budget_tokens,
        max_turns: None,
    })
}

fn structured_output<T>(output: &ToolOutput) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    output
        .structured
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn tool_name_from_refs(refs: &[ArtifactRef]) -> Option<String> {
    let mut names = refs.iter().filter_map(tool_name_from_artifact_ref);
    let name = names.next()?;
    if names.next().is_some() {
        return None;
    }
    Some(name)
}

fn tool_name_from_artifact_ref(reference: &ArtifactRef) -> Option<String> {
    match reference {
        ArtifactRef::Tool { name } => Some(name.clone()),
        ArtifactRef::Action { connector, action } => Some(format!("{connector}.{action}")),
        ArtifactRef::Artifact { .. } => None,
    }
}

fn tool_name_from_input(input: &Value) -> Option<String> {
    input
        .get("tool_name")
        .or_else(|| input.get("tool"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn tool_input(input: &Value) -> Value {
    if let Some(tool_input) = input.get("input") {
        return tool_input.clone();
    }
    if input.get("tool_name").is_none() && input.get("tool").is_none() {
        return input.clone();
    }
    let Some(object) = input.as_object() else {
        return input.clone();
    };
    let mut payload = Map::new();
    for (key, value) in object {
        if key != "tool_name" && key != "tool" {
            payload.insert(key.clone(), value.clone());
        }
    }
    Value::Object(payload)
}

/// Loads the session metadata that governs a procedure node's tool execution.
///
/// When the procedure run is linked to a session, this loads the real stored
/// [`SessionMeta`] so action-policy evaluation and event attribution use the
/// session's true contact, actor, model, and agent context — matching the
/// governed tool flow. Session-less runs (for example API-started procedures
/// with no linked session) fall back to a synthetic meta derived from the
/// authorizing identity.
async fn procedure_session_meta(
    ctx: &WorkflowContext<'_>,
    context: &ProcedureNodeActionContext,
) -> Result<SessionMeta, HandlerError> {
    let Some(session_id) = context.session_id else {
        return Ok(synthetic_procedure_session_meta(context));
    };
    let store = OrchestratorCtx::current_session_store();
    Ok(ctx
        .run(|| async move {
            store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("procedure_load_session_meta")
        .await?
        .into_inner())
}

/// Builds a synthetic session meta for a session-less procedure run.
///
/// Session-less runs have no stored session to read, so tool attribution falls
/// back to the identity that authorized the procedure run.
fn synthetic_procedure_session_meta(context: &ProcedureNodeActionContext) -> SessionMeta {
    SessionMeta {
        id: context.session_id.unwrap_or_default(),
        tenant_id: context.tenant_id,
        status: SessionStatus::Running,
        created_by: Some(SessionActorRef::Identity {
            id: context.identity.id,
        }),
        agent_context: None,
        ..SessionMeta::default()
    }
}

fn tool_output_value(output: ToolOutput) -> Value {
    serde_json::to_value(&output).unwrap_or_else(|_| {
        json!({
            "content": output.to_text(),
            "is_error": output.is_error,
            "duration_ms": output.duration.as_millis() as u64,
        })
    })
}

fn stable_tool_call_id(run_uid: Uuid, node_id: &str) -> ToolCallId {
    ToolCallId(stable_uuid(b"moa.procedure.tool_call.v1", run_uid, node_id))
}

fn stable_review_id(run_uid: Uuid, node_id: &str) -> Uuid {
    stable_uuid(b"moa.procedure.review.v1", run_uid, node_id)
}

fn stable_uuid(domain: &[u8], run_uid: Uuid, node_id: &str) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(run_uid.as_bytes());
    hasher.update(&(node_id.len() as u64).to_be_bytes());
    hasher.update(node_id.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::time::{Duration, Instant};

    use moa_artifacts::reference::ArtifactRef;
    use moa_skills::procedure::interpreter::ProcedureNodeRequest;
    use serde_json::json;

    use crate::objects::session::AttachSessionTurnWaiterOutput;

    use super::*;

    async fn await_agent_turn_after_session_waiter(
        attached: AttachSessionTurnWaiterOutput,
        completion: impl Future<Output = Result<String, TerminalError>>,
    ) -> Result<TurnOutcome, HandlerError> {
        match await_agent_turn_or_cancel_after_session_waiter(
            attached,
            completion,
            std::future::pending(),
        )
        .await?
        {
            AgentNodeTurnWait::Outcome(outcome) => Ok(outcome),
            AgentNodeTurnWait::Cancelled(reason) => Err(TerminalError::new(format!(
                "unexpected cancellation while waiting for agent turn: {reason}"
            ))
            .into()),
        }
    }

    async fn await_agent_turn_or_cancel_after_session_waiter(
        attached: AttachSessionTurnWaiterOutput,
        completion: impl Future<Output = Result<String, TerminalError>>,
        cancellation: impl Future<Output = Result<String, TerminalError>>,
    ) -> Result<AgentNodeTurnWait, HandlerError> {
        if let Some(outcome) = attached.outcome {
            return Ok(AgentNodeTurnWait::Outcome(outcome));
        }
        tokio::select! {
            raw = completion => parse_turn_outcome(&raw?).map(AgentNodeTurnWait::Outcome),
            reason = cancellation => Ok(AgentNodeTurnWait::Cancelled(reason?)),
        }
    }

    #[test]
    fn tool_node_uses_single_tool_ref_and_node_input() {
        // Pins: UI-authored tool nodes keep tool identity in graph refs and payload in node input.
        let invocation = invocation_from_request(&ProcedureNodeRequest::Tool {
            node_id: "read".to_string(),
            tool_refs: vec![ArtifactRef::tool("file_read")],
            input: json!({ "path": "README.md" }),
        })
        .expect("tool invocation should resolve");

        assert_eq!(invocation.name, "file_read");
        assert_eq!(invocation.input, json!({ "path": "README.md" }));
    }

    #[test]
    fn tool_node_can_use_explicit_input_tool_name() {
        // Pins: imported procedure fixtures can encode tool identity directly in node input.
        let invocation = invocation_from_request(&ProcedureNodeRequest::Tool {
            node_id: "shell".to_string(),
            tool_refs: Vec::new(),
            input: json!({
                "tool_name": "bash",
                "input": { "cmd": "printf hi" }
            }),
        })
        .expect("tool invocation should resolve");

        assert_eq!(invocation.name, "bash");
        assert_eq!(invocation.input, json!({ "cmd": "printf hi" }));
    }

    #[test]
    fn stable_ids_are_deterministic_per_run_and_node() {
        // Pins: procedure side effects use stable ids across Restate replays.
        let run_uid = Uuid::now_v7();

        assert_eq!(
            stable_tool_call_id(run_uid, "node-a"),
            stable_tool_call_id(run_uid, "node-a")
        );
        assert_ne!(
            stable_tool_call_id(run_uid, "node-a"),
            stable_tool_call_id(run_uid, "node-b")
        );
    }

    #[test]
    fn agent_prompt_uses_instruction_prompt_or_message() {
        // Pins: agent nodes expose one clear user-message field for future UI editors.
        assert_eq!(
            prompt_from_input(&json!({ "instruction": "Check the order" })).as_deref(),
            Some("Check the order")
        );
        assert_eq!(
            prompt_from_input(&json!({ "prompt": "Review the ticket" })).as_deref(),
            Some("Review the ticket")
        );
        assert_eq!(prompt_from_input(&json!({ "instruction": " " })), None);
    }

    #[tokio::test]
    async fn agent_node_turn_completion_awakeable_resolves_before_legacy_poll_interval_offline() {
        // Pins: procedure agent nodes wait on the session turn signal, not a 1s snapshot poll.
        let outcome = TurnOutcome {
            turn_id: "turn-procedure-node".to_string(),
            kind: TurnOutcomeKind::Completed,
            message: "agent node completed".to_string(),
        };
        let raw = serde_json::to_string(&outcome).expect("turn outcome serializes");
        let started = Instant::now();

        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(100),
                await_agent_turn_after_session_waiter(
                    AttachSessionTurnWaiterOutput { outcome: None },
                    async { Ok::<_, TerminalError>(raw) },
                ),
            )
            .await
            .expect("completion signal should resolve well below the old 1s polling interval")
            .expect("turn outcome signal should parse"),
            outcome
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "agent turn completion path waited like the old polling loop"
        );
    }

    #[tokio::test]
    async fn agent_node_turn_wait_observes_cancellation_before_timeout_offline() {
        // Pins: procedure agent nodes race cancellation instead of waiting for the 180s timeout.
        let started = Instant::now();

        let wait = tokio::time::timeout(
            Duration::from_millis(100),
            await_agent_turn_or_cancel_after_session_waiter(
                AttachSessionTurnWaiterOutput { outcome: None },
                std::future::pending(),
                async { Ok::<_, TerminalError>("operator cancelled procedure".to_string()) },
            ),
        )
        .await
        .expect("cancellation should win without waiting for the agent-node timeout")
        .expect("cancellation wait should resolve");

        assert_eq!(
            wait,
            AgentNodeTurnWait::Cancelled("operator cancelled procedure".to_string())
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "agent turn cancellation path waited like the full timeout"
        );
    }

    #[test]
    fn worker_input_preserves_task_budget_and_tools() {
        // Pins: worker procedure nodes feed the existing delegation validator shape.
        let input = spawn_input_from_node(&json!({
            "task": "Investigate refunds",
            "tool_subset": ["file_read", "grep"],
            "budget_tokens": 12000
        }))
        .expect("spawn input should parse");

        assert_eq!(input.task, "Investigate refunds");
        assert_eq!(input.tool_subset, vec!["file_read", "grep"]);
        assert_eq!(input.budget_tokens, 12000);
    }

    #[test]
    fn memory_read_defaults_contact_identity_to_contact_scope() {
        // Pins: contact-triggered procedure memory reads never silently inherit tenant memory.
        let contact_uuid =
            Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("contact id");
        let context = procedure_action_context(Identity {
            identity_type: IdentityType::Contact,
            id: contact_uuid,
            tenant_id: TenantId::new(),
            api_key_id: None,
            acting_on_behalf_of: None,
        });

        let request = memory_search_request_from_node(
            &context,
            &json!({
                "query": "preferred support channel",
                "limit": 0,
                "label_filter": ["Fact"],
                "max_pii_class": "restricted",
                "use_reranker": true
            }),
        )
        .expect("memory read request should parse");

        assert_eq!(request.contact_id, Some(ContactId(contact_uuid)));
        assert_eq!(request.query, "preferred support channel");
        assert_eq!(request.limit, 1);
        assert_eq!(request.label_filter, vec!["Fact"]);
        assert_eq!(request.max_pii_class.as_deref(), Some("restricted"));
        assert!(request.use_reranker);
    }

    #[test]
    fn memory_write_stamps_procedure_provenance_metadata() {
        // Pins: procedure memory writes preserve reviewable node/run provenance in graph ingestion metadata.
        let context = procedure_action_context(Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::now_v7(),
            tenant_id: TenantId::new(),
            api_key_id: None,
            acting_on_behalf_of: None,
        });

        let request = memory_ingest_request_from_node(
            &context,
            &json!({
                "contact_id": "22222222-2222-2222-2222-222222222222",
                "content": "The customer prefers email updates.",
                "source_name": "handoff note",
                "metadata": {
                    "source": "procedure-test"
                }
            }),
        )
        .expect("memory write request should parse");

        assert_eq!(
            request.contact_id,
            Some(ContactId(
                Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("contact id")
            ))
        );
        assert_eq!(request.documents.len(), 1);
        assert_eq!(request.documents[0].source_name, "handoff note");
        assert_eq!(
            request.documents[0].metadata["source"],
            json!("procedure-test")
        );
        assert_eq!(
            request.documents[0].metadata["procedure_run_uid"],
            json!(context.run_uid)
        );
        assert_eq!(
            request.documents[0].metadata["procedure_node_id"],
            json!("memory")
        );
        assert_eq!(
            request.documents[0].metadata["procedure_document_index"],
            json!(0)
        );
    }

    #[test]
    fn memory_write_requires_content_or_documents() {
        // Pins: memory_write nodes fail closed rather than writing empty graph-memory records.
        let context = procedure_action_context(Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::now_v7(),
            tenant_id: TenantId::new(),
            api_key_id: None,
            acting_on_behalf_of: None,
        });

        let error = memory_ingest_request_from_node(&context, &json!({ "metadata": {} }))
            .expect_err("empty memory write input should fail");

        assert!(error.contains("requires input.content"));
    }

    #[test]
    fn session_less_procedure_uses_synthetic_meta_and_identity_attribution() {
        // Pins: an API-started procedure with no linked session falls back to a
        // synthetic session meta and attributes governed tool calls to the
        // authorizing identity, not the tenant.
        let identity_id = Uuid::now_v7();
        let tenant_id = TenantId::new();
        let context = procedure_action_context(Identity {
            identity_type: IdentityType::Operator,
            id: identity_id,
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        });
        assert!(
            context.session_id.is_none(),
            "fallback path requires a session-less run"
        );

        let meta = synthetic_procedure_session_meta(&context);
        assert_eq!(meta.tenant_id, tenant_id);
        assert_eq!(
            meta.created_by,
            Some(SessionActorRef::Identity { id: identity_id })
        );
        assert_eq!(
            storage_user_id(&meta),
            moa_core::UserId::new(format!("identity:{identity_id}"))
        );
    }

    fn procedure_action_context(identity: Identity) -> ProcedureNodeActionContext {
        ProcedureNodeActionContext {
            tenant_id: identity.tenant_id,
            run_uid: Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
                .expect("procedure run id"),
            node_id: "memory".to_string(),
            session_id: None,
            identity,
            cancel_promise_key: None,
        }
    }
}
