//! Durable Restate facade over the configured tool router.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use moa_core::traits::SessionRepository;
use moa_core::wire::session_store::AppendEventRequest;
use moa_core::wire::tools::{ToolDescriptor, tool_descriptor};
use moa_core::{
    ClaimCheck, Event, EventRecord, EventType, IdempotencyClass, MoaError, SandboxFile, SessionId,
    SessionMeta, SessionStatus, TenantId, ToolCallId, ToolCallRequest, ToolDefinition,
    ToolFailureClass, ToolInvocation, ToolOutput, TrustedSandboxFileEntry,
    TrustedSandboxFileManifestPayload, TrustedSandboxFileManifestRef, classify_tool_error,
};
use moa_hands::ToolRouter;
use moa_memory_ingest::{execute_memory_tool, is_fast_memory_tool};
use moa_observability::record_tool_idempotency_scan;
use moa_security::{ToolInputCanaryScreening, screen_tool_input_for_canary};
use restate_sdk::prelude::*;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::services::session_store::RestateSessionStoreClient;
use crate::turn::util::{blocked_canary_message, blocked_canary_tool_output};
use crate::workflows::errors::moa_error_to_handler_error;
use moa_observability::restate_observability::annotate_restate_handler_span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Restate service surface for durable tool execution.
#[restate_sdk::service]
pub trait ToolExecutor {
    /// Executes one tool call through the configured router.
    async fn execute(request: Json<ToolCallRequest>) -> Result<Json<ToolOutput>, HandlerError>;

    /// Lists the currently registered tools for the requested tenant.
    async fn list_tools(
        tenant_id: Json<TenantId>,
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
    trusted_manifest_store: Arc<dyn TrustedSandboxFileManifestStore>,
}

impl ToolExecutorImpl {
    /// Creates a new Restate tool executor over a shared router.
    #[must_use]
    pub fn new(router: Arc<ToolRouter>) -> Self {
        Self {
            router,
            trusted_manifest_store: Arc::new(SessionStoreTrustedSandboxFileManifestStore),
        }
    }

    /// Overrides the trusted sandbox file manifest store.
    #[must_use]
    pub fn with_trusted_manifest_store(
        mut self,
        trusted_manifest_store: Arc<dyn TrustedSandboxFileManifestStore>,
    ) -> Self {
        self.trusted_manifest_store = trusted_manifest_store;
        self
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
        let trusted_sandbox_files = self.trusted_sandbox_files_for_request(request).await?;
        self.router
            .set_trusted_sandbox_files(session, trusted_sandbox_files)
            .await;
        let (_hand_id, output) = self
            .router
            .execute_authorized_with_recovery(session, &invocation)
            .await?;
        Ok(output)
    }

    async fn trusted_sandbox_files_for_request(
        &self,
        request: &ToolCallRequest,
    ) -> moa_core::Result<Vec<SandboxFile>> {
        let Some(manifest) = request.trusted_sandbox_manifest.as_ref() else {
            return Ok(Vec::new());
        };
        let session_id = request.session_id.ok_or_else(|| {
            MoaError::ValidationError(format!(
                "tool {} supplied trusted_sandbox_manifest without session_id",
                request.tool_name
            ))
        })?;
        self.trusted_manifest_store.load(session_id, manifest).await
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

/// Durable loader for trusted sandbox file manifests referenced by tool requests.
#[async_trait]
pub trait TrustedSandboxFileManifestStore: Send + Sync {
    /// Loads and validates files for a session-scoped manifest reference.
    async fn load(
        &self,
        session_id: SessionId,
        manifest: &TrustedSandboxFileManifestRef,
    ) -> moa_core::Result<Vec<SandboxFile>>;
}

#[derive(Clone, Copy)]
struct SessionStoreTrustedSandboxFileManifestStore;

#[async_trait]
impl TrustedSandboxFileManifestStore for SessionStoreTrustedSandboxFileManifestStore {
    async fn load(
        &self,
        session_id: SessionId,
        manifest: &TrustedSandboxFileManifestRef,
    ) -> moa_core::Result<Vec<SandboxFile>> {
        let store = OrchestratorCtx::current_session_store();
        load_trusted_sandbox_manifest_from_store(store.as_ref(), session_id, manifest).await
    }
}

impl ToolExecutor for ToolExecutorImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal session and sub-agent workflows admit callers before invoking tool execution.
    async fn execute(
        &self,
        ctx: Context<'_>,
        request: Json<ToolCallRequest>,
    ) -> Result<Json<ToolOutput>, HandlerError> {
        annotate_restate_handler_span("ToolExecutor", "execute");
        let request = request.into_inner();
        let session = resolve_session(&ctx, &request).await?;
        annotate_tool_execution_span(&session, &request);

        let serialized_input = serde_json::to_string(&request.input)
            .map_err(|error| moa_error_to_handler_error(error.into()))?;
        if matches!(
            screen_tool_input_for_canary(request.active_canary.as_deref(), &serialized_input),
            ToolInputCanaryScreening::Blocked(_)
        ) {
            if !prior_tool_call_event_exists(&ctx, &session, &request).await? {
                append_tool_call_event(&ctx, &request).await?;
            }
            append_tool_canary_block_events(&ctx, &request).await?;
            return Ok(Json::from(blocked_canary_tool_output(&request.tool_name)));
        }

        if !prior_tool_call_event_exists(&ctx, &session, &request).await? {
            append_tool_call_event(&ctx, &request).await?;
        }

        if let Some(output) = agent_tool_policy_denied_output(&session, &request) {
            append_agent_tool_policy_denied_event(&ctx, &request, &output).await?;
            return Ok(Json::from(output));
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
        ) && prior_non_idempotent_result_exists(&ctx, &session, &request).await?
        {
            return Err(TerminalError::new(format!(
                "refusing to re-execute non-idempotent tool {} (tool_call_id={}) because a prior result already exists",
                request.tool_name, request.tool_call_id
            ))
            .into());
        }

        let run_plan =
            build_tool_run_plan(&definition, &request).map_err(moa_error_to_handler_error)?;
        let request_for_run = request.clone();
        let session_for_run = session.clone();
        let service = self.clone();

        let output = match ctx
            .run(|| async move {
                service
                    .execute_buffered(&session_for_run, &request_for_run)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
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

    #[tracing::instrument(skip(self, _ctx, tenant_id))]
    // SAFETY: Returns informational tool descriptors; the tenant id only scopes descriptor listing.
    async fn list_tools(
        &self,
        _ctx: Context<'_>,
        tenant_id: Json<TenantId>,
    ) -> Result<Json<Vec<ToolDescriptor>>, HandlerError> {
        annotate_restate_handler_span("ToolExecutor", "list_tools");
        let _tenant_id = tenant_id.into_inner();
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

#[cfg(test)]
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

fn agent_tool_policy_denied_output(
    session: &SessionMeta,
    request: &ToolCallRequest,
) -> Option<ToolOutput> {
    let agent_context = session.agent_context.as_ref()?;
    match agent_context.allows_tool(&request.tool_name) {
        Ok(true) => None,
        Ok(false) => Some(ToolOutput::error(
            format!(
                "tool {} denied by agent policy {} for {}",
                request.tool_name, agent_context.policy_hash, agent_context.definition_ref
            ),
            Duration::ZERO,
        )),
        Err(error) => Some(ToolOutput::error(
            format!(
                "tool {} denied because agent policy {} for {} could not be parsed: {error}",
                request.tool_name, agent_context.policy_hash, agent_context.definition_ref
            ),
            Duration::ZERO,
        )),
    }
}

fn annotate_tool_execution_span(session: &SessionMeta, request: &ToolCallRequest) {
    let span = tracing::Span::current();
    span.set_attribute("moa.session.id", session.id.to_string());
    span.set_attribute("moa.tenant.id", session.tenant_id.to_string());
    span.set_attribute("moa.tool.name", request.tool_name.clone());
    if let Some(contact) = session.contact.as_ref() {
        span.set_attribute("moa.contact.id", contact.contact_id.to_string());
        span.set_attribute("moa.contact.state", contact.state.as_str().to_string());
    }
}

async fn load_trusted_sandbox_manifest_from_store(
    store: &(dyn SessionRepository + '_),
    session_id: SessionId,
    manifest: &TrustedSandboxFileManifestRef,
) -> moa_core::Result<Vec<SandboxFile>> {
    let claim_check = ClaimCheck {
        blob_id: manifest.blob_id.clone(),
        size: manifest.size,
        preview: String::new(),
    };
    let payload = store.load_text_artifact(session_id, &claim_check).await?;
    trusted_sandbox_files_from_manifest_payload(manifest, &payload)
}

/// Validates and decodes a trusted sandbox file manifest payload.
pub fn trusted_sandbox_files_from_manifest_payload(
    manifest: &TrustedSandboxFileManifestRef,
    payload: &str,
) -> moa_core::Result<Vec<SandboxFile>> {
    let actual_manifest_hash = sha256_hex(payload.as_bytes());
    if actual_manifest_hash != manifest.manifest_sha256 {
        return Err(MoaError::StorageError(format!(
            "trusted sandbox file manifest {} hash mismatch",
            manifest.blob_id
        )));
    }
    let payload: TrustedSandboxFileManifestPayload =
        serde_json::from_str(payload).map_err(|error| {
            MoaError::StorageError(format!(
                "trusted sandbox file manifest {} could not be decoded: {error}",
                manifest.blob_id
            ))
        })?;
    validate_trusted_sandbox_manifest_files(manifest, &payload.files)?;
    Ok(payload.files)
}

fn validate_trusted_sandbox_manifest_files(
    manifest: &TrustedSandboxFileManifestRef,
    files: &[SandboxFile],
) -> moa_core::Result<()> {
    if manifest.files.len() != files.len() {
        return Err(MoaError::StorageError(format!(
            "trusted sandbox file manifest {} expected {} files but loaded {}",
            manifest.blob_id,
            manifest.files.len(),
            files.len()
        )));
    }
    for (expected, file) in manifest.files.iter().zip(files) {
        let actual = TrustedSandboxFileEntry {
            path: file.path.clone(),
            content_sha256: sha256_hex(&file.content),
            size: file.content.len(),
            executable: file.executable,
        };
        if &actual != expected {
            return Err(MoaError::StorageError(format!(
                "trusted sandbox file manifest {} entry mismatch for {}",
                manifest.blob_id, expected.path
            )));
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

async fn resolve_session(
    ctx: &Context<'_>,
    request: &ToolCallRequest,
) -> Result<SessionMeta, HandlerError> {
    if let Some(session_id) = request.session_id {
        let store = OrchestratorCtx::current_session_store();
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
        id: synthetic_session_id(request.tenant_id),
        tenant_id: request.tenant_id,
        status: SessionStatus::Running,
        ..SessionMeta::default()
    })
}

fn synthetic_session_id(tenant_id: TenantId) -> SessionId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.orchestrator.synthetic_session.v1");
    update_len_prefixed(&mut hasher, tenant_id.to_string().as_bytes());
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
    session: &SessionMeta,
    request: &ToolCallRequest,
) -> Result<bool, HandlerError> {
    let session_id = request.session_id.ok_or_else(|| {
        moa_error_to_handler_error(MoaError::ValidationError(format!(
            "tool {} requires session_id because it is non-idempotent",
            request.tool_name
        )))
    })?;
    let store = OrchestratorCtx::current_session_store();
    let storage_partition_id = storage_partition_id_for_session(session);
    let tool_call_id = request.tool_call_id;
    let scan_started = Instant::now();
    let exists = ctx
        .run(|| async move {
            store
                .tool_event_exists(
                    &storage_partition_id,
                    session_id,
                    EventType::ToolResult,
                    tool_call_id,
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("tool_executor_tool_result_exists")
        .await?
        .into_inner();
    record_tool_idempotency_scan("ToolResult", 0, scan_started.elapsed());
    Ok(exists)
}

async fn prior_tool_call_event_exists(
    ctx: &Context<'_>,
    session: &SessionMeta,
    request: &ToolCallRequest,
) -> Result<bool, HandlerError> {
    let Some(session_id) = request.session_id else {
        return Ok(false);
    };

    let store = OrchestratorCtx::current_session_store();
    let storage_partition_id = storage_partition_id_for_session(session);
    let tool_call_id = request.tool_call_id;
    let scan_started = Instant::now();
    let exists = ctx
        .run(|| async move {
            store
                .tool_event_exists(
                    &storage_partition_id,
                    session_id,
                    EventType::ToolCall,
                    tool_call_id,
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("tool_executor_tool_call_exists")
        .await?
        .into_inner();
    record_tool_idempotency_scan("ToolCall", 0, scan_started.elapsed());
    Ok(exists)
}

fn storage_partition_id_for_session(session: &SessionMeta) -> moa_core::StoragePartitionId {
    moa_core::StoragePartitionId::for_tenant(session.tenant_id)
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

async fn append_agent_tool_policy_denied_event(
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
            event: Event::ToolError {
                tool_id: request.tool_call_id,
                provider_tool_use_id: request.provider_tool_use_id.clone(),
                tool_name: request.tool_name.clone(),
                error: output.to_text(),
                retryable: false,
            },
        }))
        .call()
        .await?;

    Ok(())
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::{
        AgentContext, AgentPolicySnapshot, AgentToolPolicy, AgentToolPolicyMode, Event,
        EventRecord, EventType, HandHandle, HandProvider, HandSpec, HandStatus, IdempotencyClass,
        RiskLevel, SandboxFile, SandboxTier, SessionId, SessionMeta, TenantId, ToolCallId,
        ToolCallRequest, ToolDiffStrategy, ToolInputShape, ToolOutput, ToolPolicySpec,
        TrustedSandboxFileEntry, TrustedSandboxFileManifestRef, UserId,
    };
    use moa_hands::{ToolRegistry, ToolRouter};
    use uuid::Uuid;

    use super::{
        ToolExecutorImpl, TrustedSandboxFileManifestStore, agent_tool_policy_denied_output,
        blocked_canary_tool_output, has_prior_tool_call_event, synthetic_session_id,
    };

    #[derive(Default)]
    struct InstallingProvider {
        installed_files: Mutex<Vec<SandboxFile>>,
    }

    impl InstallingProvider {
        fn installed_files(&self) -> Vec<SandboxFile> {
            self.installed_files
                .lock()
                .expect("lock installed files")
                .clone()
        }
    }

    #[async_trait]
    impl HandProvider for InstallingProvider {
        fn provider_name(&self) -> &str {
            "install-provider"
        }

        async fn provision(&self, _spec: HandSpec) -> moa_core::Result<HandHandle> {
            Ok(HandHandle::docker("install-provider-1"))
        }

        async fn execute(
            &self,
            _handle: &HandHandle,
            _tool: &str,
            _input: &str,
        ) -> moa_core::Result<ToolOutput> {
            Ok(ToolOutput::text("ok", Duration::from_millis(1)))
        }

        async fn install_files(
            &self,
            _handle: &HandHandle,
            files: &[SandboxFile],
        ) -> moa_core::Result<()> {
            *self.installed_files.lock().expect("lock installed files") = files.to_vec();
            Ok(())
        }

        async fn status(&self, _handle: &HandHandle) -> moa_core::Result<HandStatus> {
            Ok(HandStatus::Running)
        }

        async fn pause(&self, _handle: &HandHandle) -> moa_core::Result<()> {
            Ok(())
        }

        async fn resume(&self, _handle: &HandHandle) -> moa_core::Result<()> {
            Ok(())
        }

        async fn destroy(&self, _handle: &HandHandle) -> moa_core::Result<()> {
            Ok(())
        }
    }

    struct StaticTrustedManifestStore {
        files: Vec<SandboxFile>,
    }

    #[async_trait]
    impl TrustedSandboxFileManifestStore for StaticTrustedManifestStore {
        async fn load(
            &self,
            _session_id: SessionId,
            _manifest: &TrustedSandboxFileManifestRef,
        ) -> moa_core::Result<Vec<SandboxFile>> {
            Ok(self.files.clone())
        }
    }

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
        let session_id = synthetic_session_id(TenantId::from(Uuid::from_u128(1)));

        assert_eq!(
            session_id.0.to_string(),
            "be49b430-9b14-407d-9e03-1e2a81dc8d8c"
        );
        assert_eq!(session_id.0.get_version_num(), 4);
        assert_eq!(session_id.0.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn canary_block_output_is_terminal_tool_error() {
        // Pins: ToolExecutor reports blocked canary input as a tool error before backend execution.
        let output = blocked_canary_tool_output("bash");

        assert!(output.is_error);
        assert_eq!(
            output.to_text(),
            "Tool bash blocked because it leaked a protected canary token."
        );
        assert_eq!(output.duration, std::time::Duration::ZERO);
    }

    #[test]
    fn agent_tool_policy_denies_unlisted_tools() {
        // Pins: persisted agent policy snapshots are enforced before router execution.
        let session = SessionMeta {
            agent_context: Some(agent_context_with_allowlist(&["file_read"])),
            ..SessionMeta::default()
        };
        let denied = agent_tool_policy_denied_output(&session, &tool_request("bash"))
            .expect("bash should be denied by allowlist policy");
        assert!(denied.is_error);
        assert_eq!(
            denied.to_text(),
            "tool bash denied by agent policy policy-hash for agent://support"
        );

        assert!(agent_tool_policy_denied_output(&session, &tool_request("file_read")).is_none());
    }

    fn agent_context_with_allowlist(tools: &[&str]) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            instructions: vec!["stay in scope".to_string()],
            tool_policy: AgentToolPolicy {
                mode: AgentToolPolicyMode::Allowlist,
                tools: tools.iter().map(|tool| (*tool).to_string()).collect(),
                denied_tools: Vec::new(),
            },
            revision_lock: None,
            ..AgentPolicySnapshot::default()
        };
        AgentContext {
            agent_id: None,
            installation_uid: Some(Uuid::now_v7()),
            deployment_uid: Some(Uuid::now_v7()),
            definition_ref: "agent://support".to_string(),
            revision_uid: Uuid::now_v7(),
            policy_hash: "policy-hash".to_string(),
            display_name: "Support".to_string(),
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
        }
    }

    fn tool_request(tool_name: &str) -> ToolCallRequest {
        ToolCallRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_use_id: Some("toolu_policy".to_string()),
            tool_name: tool_name.to_string(),
            input: serde_json::json!({}),
            active_canary: None,
            session_id: None,
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            user_id: UserId::new("user-1"),
            idempotency_key: None,
            trusted_sandbox_manifest: None,
        }
    }

    #[tokio::test]
    async fn execute_buffered_installs_files_from_durable_request_manifest() {
        // Pins: ToolExecutor does not rely on trusted-file state from the turn-loop router.
        let provider = Arc::new(InstallingProvider::default());
        let mut registry = ToolRegistry::default_local();
        registry.register_hand(
            "bash",
            "test shell command",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }),
            ToolPolicySpec {
                risk_level: RiskLevel::High,
                default_effect: moa_core::ActionPolicyEffect::Allow,
                action_class: moa_core::ActionClass::CommandExecution,
                input_shape: ToolInputShape::Json,
                diff_strategy: ToolDiffStrategy::None,
            },
            IdempotencyClass::Idempotent,
        );
        registry.retarget_hand_tools(provider.provider_name(), SandboxTier::Container);
        registry.retain_only(["bash"]);
        let provider_trait: Arc<dyn HandProvider> = provider.clone();
        let mut providers = HashMap::new();
        providers.insert(provider_trait.provider_name().to_string(), provider_trait);
        let files = vec![SandboxFile {
            path: ".moa/skills/test/SKILL.md".to_string(),
            content: b"use this skill".to_vec(),
            executable: false,
        }];
        let manifest = TrustedSandboxFileManifestRef {
            blob_id: "session-blob-1".to_string(),
            size: 128,
            manifest_sha256: "manifest-sha256".to_string(),
            files: vec![TrustedSandboxFileEntry {
                path: ".moa/skills/test/SKILL.md".to_string(),
                content_sha256: "content-sha256".to_string(),
                size: b"use this skill".len(),
                executable: false,
            }],
        };
        let executor = ToolExecutorImpl::new(Arc::new(ToolRouter::new(registry, providers)))
            .with_trusted_manifest_store(Arc::new(StaticTrustedManifestStore {
                files: files.clone(),
            }));
        let request = ToolCallRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_use_id: Some("provider-tool-use".to_string()),
            tool_name: "bash".to_string(),
            input: serde_json::json!({"cmd": "cat .moa/skills/test/SKILL.md"}),
            active_canary: None,
            session_id: Some(SessionId::new()),
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            user_id: UserId::new("user-1"),
            idempotency_key: None,
            trusted_sandbox_manifest: Some(manifest),
        };

        let output = executor
            .execute_buffered(&SessionMeta::default(), &request)
            .await
            .expect("tool execution should use request manifest");

        assert!(!output.is_error);
        assert_eq!(provider.installed_files(), files);
    }
}
