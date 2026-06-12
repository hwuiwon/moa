//! Durable Restate façade over the workspace tool router.

use std::sync::Arc;
use std::time::{Duration, Instant};

pub use moa_core::wire::ToolDescriptor;
use moa_core::wire::tool_descriptor;
use moa_core::{
    Event, EventRange, EventRecord, EventType, IdempotencyClass, MoaError, SessionId, SessionMeta,
    SessionStatus, SessionStore as _, ToolCallId, ToolCallRequest, ToolDefinition,
    ToolFailureClass, ToolInvocation, ToolOutput, classify_tool_error,
    record_tool_idempotency_scan,
};
use moa_hands::ToolRouter;
use moa_memory_ingest::{execute_memory_tool, is_fast_memory_tool};
use moa_security::{ToolInputCanaryScreening, screen_tool_input_for_canary};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::services::session_store::{AppendEventRequest, RestateSessionStoreClient};
use moa_core::restate_observability::annotate_restate_handler_span;

/// Restate service surface for durable tool execution.
#[restate_sdk::service]
pub trait ToolExecutor {
    /// Executes one tool call through the configured router.
    async fn execute(request: Json<ToolCallRequest>) -> Result<Json<ToolOutput>, HandlerError>;

    /// Lists the currently registered tools for the requested workspace.
    async fn list_tools(
        workspace_id: Json<moa_core::WorkspaceId>,
    ) -> Result<Json<Vec<ToolDescriptor>>, HandlerError>;
}

/// Derived `ctx.run()` plan for one tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRunPlan {
    /// Stable run-operation name recorded in the Restate journal.
    pub name: String,
    /// Maximum number of attempts allowed for the underlying `ctx.run()` closure.
    pub max_attempts: u32,
}

/// Concrete Restate service implementation backed by a shared `ToolRouter`.
#[derive(Clone)]
pub struct ToolExecutorImpl {
    router: Arc<ToolRouter>,
}

impl ToolExecutorImpl {
    /// Creates a new Restate tool executor over a shared router.
    #[must_use]
    pub fn new(router: Arc<ToolRouter>) -> Self {
        Self { router }
    }

    async fn execute_buffered(
        &self,
        session: &SessionMeta,
        request: &ToolCallRequest,
    ) -> moa_core::Result<ToolOutput> {
        if is_fast_memory_tool(&request.tool_name) {
            return execute_memory_tool(session, &request.tool_name, &request.input).await;
        }

        let invocation = ToolInvocation {
            id: request.provider_tool_use_id.clone(),
            name: request.tool_name.clone(),
            input: request.input.clone(),
        };
        let (_hand_id, output) = self
            .router
            .execute_authorized_with_recovery(session, &invocation)
            .await?;
        Ok(output)
    }

    /// Returns the registered tool descriptors in stable name order.
    pub fn list_descriptors(&self) -> Vec<ToolDescriptor> {
        self.router
            .tool_definitions()
            .into_iter()
            .map(tool_descriptor)
            .collect()
    }
}

impl ToolExecutor for ToolExecutorImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn execute(
        &self,
        ctx: Context<'_>,
        request: Json<ToolCallRequest>,
    ) -> Result<Json<ToolOutput>, HandlerError> {
        annotate_restate_handler_span("ToolExecutor", "execute");
        let request = request.into_inner();
        let session = resolve_session(&ctx, &request).await?;

        let serialized_input = serde_json::to_string(&request.input)
            .map_err(|error| to_handler_error(error.into()))?;
        if matches!(
            screen_tool_input_for_canary(request.active_canary.as_deref(), &serialized_input),
            ToolInputCanaryScreening::Blocked(_)
        ) {
            if !prior_tool_call_event_exists(&ctx, &request).await? {
                append_tool_call_event(&ctx, &request).await?;
            }
            append_tool_canary_block_events(&ctx, &request).await?;
            return Ok(Json::from(blocked_canary_output(&request.tool_name)));
        }

        if !prior_tool_call_event_exists(&ctx, &request).await? {
            append_tool_call_event(&ctx, &request).await?;
        }

        let definition = match self.router.tool_definition(&request.tool_name) {
            Some(definition) => definition,
            None => {
                let output = ToolOutput::from(ToolFailureClass::Fatal {
                    reason: format!("unknown tool: {}", request.tool_name),
                });
                append_tool_result_event(&ctx, &request, &output).await?;
                return Ok(Json::from(output));
            }
        };
        if let Err(error) = validate_request(&definition, &request) {
            let output = ToolOutput::from(classify_tool_error(&error, 0));
            append_tool_result_event(&ctx, &request, &output).await?;
            return Ok(Json::from(output));
        }

        if matches!(
            definition.idempotency_class,
            IdempotencyClass::NonIdempotent
        ) && prior_non_idempotent_result_exists(&ctx, &request).await?
        {
            return Err(TerminalError::new(format!(
                "refusing to re-execute non-idempotent tool {} (tool_call_id={}) because a prior result already exists",
                request.tool_name, request.tool_call_id
            ))
            .into());
        }

        let run_plan = build_tool_run_plan(&definition, &request).map_err(to_handler_error)?;
        let request_for_run = request.clone();
        let session_for_run = session.clone();
        let service = self.clone();

        let output = match ctx
            .run(|| async move {
                service
                    .execute_buffered(&session_for_run, &request_for_run)
                    .await
                    .map(Json::from)
                    .map_err(to_handler_error)
            })
            .name(run_plan.name)
            .retry_policy(tool_run_retry_policy(definition.idempotency_class))
            .await
        {
            Ok(result) => result.into_inner(),
            Err(error) => {
                append_tool_error_event(&ctx, &request, &definition, error.to_string()).await?;
                return Err(error.into());
            }
        };

        append_tool_result_event(&ctx, &request, &output).await?;

        Ok(Json::from(output))
    }

    #[tracing::instrument(skip(self, _ctx, workspace_id))]
    async fn list_tools(
        &self,
        _ctx: Context<'_>,
        workspace_id: Json<moa_core::WorkspaceId>,
    ) -> Result<Json<Vec<ToolDescriptor>>, HandlerError> {
        annotate_restate_handler_span("ToolExecutor", "list_tools");
        let _workspace_id = workspace_id.into_inner();
        Ok(Json::from(self.list_descriptors()))
    }
}

/// Builds the stable `ctx.run()` name for one tool call.
pub fn tool_run_name(
    definition: &ToolDefinition,
    request: &ToolCallRequest,
) -> moa_core::Result<String> {
    match definition.idempotency_class {
        IdempotencyClass::Idempotent => Ok(format!(
            "tool_execute:idempotent:{}:{}",
            request.tool_name, request.tool_call_id
        )),
        IdempotencyClass::IdempotentWithKey => {
            let key = request
                .idempotency_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    MoaError::ValidationError(format!(
                        "tool {} requires idempotency_key",
                        request.tool_name
                    ))
                })?;
            Ok(format!(
                "tool_execute:keyed:{}:{}:{}",
                request.tool_name, request.tool_call_id, key
            ))
        }
        IdempotencyClass::NonIdempotent => Ok(format!(
            "tool_execute:non_idempotent:{}:{}",
            request.tool_name, request.tool_call_id
        )),
    }
}

/// Builds the derived `ctx.run()` plan for one tool call.
pub fn build_tool_run_plan(
    definition: &ToolDefinition,
    request: &ToolCallRequest,
) -> moa_core::Result<ToolRunPlan> {
    Ok(ToolRunPlan {
        name: tool_run_name(definition, request)?,
        max_attempts: retry_max_attempts_for(definition.idempotency_class),
    })
}

/// Returns whether the given event slice already contains a terminal tool result for the call id.
pub fn has_prior_non_idempotent_result(events: &[EventRecord], tool_call_id: ToolCallId) -> bool {
    events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolResult { tool_id, .. } if *tool_id == tool_call_id
        )
    })
}

fn has_prior_tool_call_event(events: &[EventRecord], tool_call_id: ToolCallId) -> bool {
    events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolCall { tool_id, .. } if *tool_id == tool_call_id
        )
    })
}

fn validate_request(
    definition: &ToolDefinition,
    request: &ToolCallRequest,
) -> moa_core::Result<()> {
    if matches!(
        definition.idempotency_class,
        IdempotencyClass::IdempotentWithKey
    ) && request
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(MoaError::ValidationError(format!(
            "tool {} requires idempotency_key",
            request.tool_name
        )));
    }

    if matches!(
        definition.idempotency_class,
        IdempotencyClass::NonIdempotent
    ) && request.session_id.is_none()
    {
        return Err(MoaError::ValidationError(format!(
            "tool {} requires session_id because it is non-idempotent",
            request.tool_name
        )));
    }

    Ok(())
}

async fn resolve_session(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
) -> Result<SessionMeta, HandlerError> {
    if let Some(session_id) = request.session_id {
        let store = OrchestratorCtx::current().session_store.clone();
        return Ok(ctx
            .run(|| async move {
                store
                    .get_session(session_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("tool_executor_get_session")
            .await?
            .into_inner());
    }

    Ok(SessionMeta {
        id: synthetic_session_id(&request.workspace_id),
        workspace_id: request.workspace_id.clone(),
        user_id: request.user_id.clone(),
        status: SessionStatus::Running,
        ..SessionMeta::default()
    })
}

fn synthetic_session_id(workspace_id: &moa_core::WorkspaceId) -> SessionId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.orchestrator.synthetic_session.v1");
    update_len_prefixed(&mut hasher, workspace_id.as_str().as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    SessionId(Uuid::from_bytes(bytes))
}

fn update_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

async fn prior_non_idempotent_result_exists(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
) -> Result<bool, HandlerError> {
    let session_id = request.session_id.ok_or_else(|| {
        to_handler_error(MoaError::ValidationError(format!(
            "tool {} requires session_id because it is non-idempotent",
            request.tool_name
        )))
    })?;
    let store = OrchestratorCtx::current().session_store.clone();
    let scan_started = Instant::now();
    let events = ctx
        .run(|| async move {
            store
                .get_events(
                    session_id,
                    EventRange {
                        from_seq: None,
                        to_seq: None,
                        event_types: Some(vec![EventType::ToolResult]),
                        limit: None,
                    },
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("tool_executor_get_prior_tool_results")
        .await?
        .into_inner();
    record_tool_idempotency_scan("ToolResult", events.len() as u64, scan_started.elapsed());
    Ok(has_prior_non_idempotent_result(
        &events,
        request.tool_call_id,
    ))
}

async fn prior_tool_call_event_exists(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
) -> Result<bool, HandlerError> {
    let Some(session_id) = request.session_id else {
        return Ok(false);
    };

    let store = OrchestratorCtx::current().session_store.clone();
    let scan_started = Instant::now();
    let events = ctx
        .run(|| async move {
            store
                .get_events(
                    session_id,
                    EventRange {
                        from_seq: None,
                        to_seq: None,
                        event_types: Some(vec![EventType::ToolCall]),
                        limit: None,
                    },
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("tool_executor_get_prior_tool_calls")
        .await?
        .into_inner();
    record_tool_idempotency_scan("ToolCall", events.len() as u64, scan_started.elapsed());
    Ok(has_prior_tool_call_event(&events, request.tool_call_id))
}

async fn append_tool_call_event(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
) -> Result<(), HandlerError> {
    let Some(session_id) = request.session_id else {
        return Ok(());
    };

    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::ToolCall {
                tool_id: request.tool_call_id,
                provider_tool_use_id: request.provider_tool_use_id.clone(),
                provider_thought_signature: None,
                tool_name: request.tool_name.clone(),
                input: request.input.clone(),
                hand_id: None,
            },
        }))
        .call()
        .await?;

    Ok(())
}

async fn append_tool_result_event(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    let Some(session_id) = request.session_id else {
        return Ok(());
    };

    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::ToolResult {
                tool_id: request.tool_call_id,
                provider_tool_use_id: request.provider_tool_use_id.clone(),
                output: output.clone(),
                original_output_tokens: output.original_output_tokens,
                success: !output.is_error,
                duration_ms: output.duration.as_millis() as u64,
            },
        }))
        .call()
        .await?;

    Ok(())
}

async fn append_tool_error_event(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
    definition: &ToolDefinition,
    error: String,
) -> Result<(), HandlerError> {
    let Some(session_id) = request.session_id else {
        return Ok(());
    };

    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::ToolError {
                tool_id: request.tool_call_id,
                provider_tool_use_id: request.provider_tool_use_id.clone(),
                tool_name: request.tool_name.clone(),
                error,
                retryable: !matches!(
                    definition.idempotency_class,
                    IdempotencyClass::NonIdempotent
                ),
            },
        }))
        .call()
        .await?;

    Ok(())
}

async fn append_tool_canary_block_events(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
) -> Result<(), HandlerError> {
    let Some(session_id) = request.session_id else {
        return Ok(());
    };

    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::Warning {
                message: format!(
                    "blocked tool {} because the active canary leaked into tool input",
                    request.tool_name
                ),
            },
        }))
        .call()
        .await?;

    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::ToolError {
                tool_id: request.tool_call_id,
                provider_tool_use_id: request.provider_tool_use_id.clone(),
                tool_name: request.tool_name.clone(),
                error: blocked_canary_message(&request.tool_name),
                retryable: false,
            },
        }))
        .call()
        .await?;

    Ok(())
}

fn blocked_canary_output(tool_name: &str) -> ToolOutput {
    ToolOutput::error(blocked_canary_message(tool_name), Duration::ZERO)
}

fn blocked_canary_message(tool_name: &str) -> String {
    format!("tool {tool_name} blocked because it leaked a protected canary token")
}

fn tool_run_retry_policy(idempotency_class: IdempotencyClass) -> RunRetryPolicy {
    let max_attempts = retry_max_attempts_for(idempotency_class);
    match idempotency_class {
        IdempotencyClass::Idempotent | IdempotencyClass::IdempotentWithKey => RunRetryPolicy::new()
            .initial_delay(Duration::from_millis(500))
            .exponentiation_factor(2.0)
            .max_delay(Duration::from_secs(5))
            .max_attempts(max_attempts),
        IdempotencyClass::NonIdempotent => RunRetryPolicy::new().max_attempts(max_attempts),
    }
}

fn retry_max_attempts_for(idempotency_class: IdempotencyClass) -> u32 {
    let _ = idempotency_class;
    1
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{Event, EventRecord, EventType, ToolCallId, WorkspaceId};
    use uuid::Uuid;

    use super::{blocked_canary_output, has_prior_tool_call_event, synthetic_session_id};

    fn tool_call_record(tool_call_id: ToolCallId) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: moa_core::SessionId::new(),
            sequence_num: 0,
            event_type: EventType::ToolCall,
            event: Event::ToolCall {
                tool_id: tool_call_id,
                provider_tool_use_id: Some("toolu_existing".to_string()),
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({ "cmd": "printf existing" }),
                hand_id: None,
            },
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    #[test]
    fn prior_tool_call_lookup_matches_tool_call_id() {
        let existing = ToolCallId::new();
        let events = vec![tool_call_record(existing)];

        assert!(has_prior_tool_call_event(&events, existing));
        assert!(!has_prior_tool_call_event(&events, ToolCallId::new()));
    }

    #[test]
    fn synthetic_session_id_is_domain_stable_uuid() {
        let session_id = synthetic_session_id(&WorkspaceId::new("workspace-1"));

        assert_eq!(
            session_id.0.to_string(),
            "d69e7597-867f-4211-925d-8343bafa1617"
        );
        assert_eq!(session_id.0.get_version_num(), 4);
        assert_eq!(session_id.0.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn canary_block_output_is_terminal_tool_error() {
        // Pins: ToolExecutor reports blocked canary input as a tool error before backend execution.
        let output = blocked_canary_output("bash");

        assert!(output.is_error);
        assert_eq!(
            output.to_text(),
            "tool bash blocked because it leaked a protected canary token"
        );
        assert_eq!(output.duration, std::time::Duration::ZERO);
    }
}
