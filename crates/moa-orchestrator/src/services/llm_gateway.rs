//! Durable Restate facade over configured LLM providers.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_config::SessionLimitsConfig;
use moa_core::{
    error::MoaError,
    events::Event,
    traits::{LLMProvider, RuntimeCacheStore},
    types::completion::CompletionRequest,
    types::completion::CompletionResponse,
    types::completion::DEFER_BRAIN_RESPONSE_METADATA_KEY,
    types::completion::StopReason,
    types::completion::TokenUsage,
    types::contact::ContactId,
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::TenantId,
    types::model::TokenPricing,
    types::observability::genai_operation_name,
    types::observability::genai_provider_name,
    types::provider::ModelTier,
    types::resource::DeadlineGuard,
    types::resource::ResourceAmounts,
    types::resource::ResourceBudget,
};
use moa_memory_ingest::{SessionTurn, ingestion_object_key, turn_transcript};
use moa_observability::record_llm_cost_cents;
use moa_providers::{CancellableLLMProvider, ProviderRegistry};
use moa_wire::session_store::AppendEventRequest;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::objects::ingestion::IngestionVOClient;
use crate::services::narration::NarrateSessionRequest;
use crate::services::session_store::RestateSessionStoreClient;
use crate::workflows::errors::moa_error_to_handler_error;
use moa_observability::restate_observability::annotate_restate_handler_span;

const COMPLETION_OWNER_METADATA_KEY: &str = "moa.llm_completion_owner";
const COMPLETION_CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(250);
const COMPLETION_CANCELLATION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const COMPLETION_CANCELLATION_KEY_DOMAIN: &str = "moa:llm-completion-cancel:v1";
const COMPLETION_CANCELLATION_VALUE: &[u8] = b"cancelled";
const SESSION_ID_METADATA_KEY: &str = "_moa.session_id";
const TENANT_ID_METADATA_KEY: &str = "_moa.tenant_id";
const CONTACT_ID_METADATA_KEY: &str = "_moa.contact_id";

/// Durable workflow kind that owns one cancellable LLM completion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LLMCompletionOwnerKind {
    /// A root session turn workflow.
    RootTurn,
    /// A worker turn workflow.
    WorkerTurn,
    /// A durable execution run and every task it owns.
    ExecutionRun,
}

impl LLMCompletionOwnerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::RootTurn => "root_turn",
            Self::WorkerTurn => "worker_turn",
            Self::ExecutionRun => "execution_run",
        }
    }
}

/// Stable workflow owner carried by the private LLM cancellation service boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LLMCompletionOwner {
    kind: LLMCompletionOwnerKind,
    workflow_key: String,
}

impl LLMCompletionOwner {
    /// Creates the owner for one root turn workflow.
    pub(crate) fn root_turn(workflow_key: impl Into<String>) -> Self {
        Self {
            kind: LLMCompletionOwnerKind::RootTurn,
            workflow_key: workflow_key.into(),
        }
    }

    /// Creates the owner for one worker turn workflow.
    pub(crate) fn worker_turn(workflow_key: impl Into<String>) -> Self {
        Self {
            kind: LLMCompletionOwnerKind::WorkerTurn,
            workflow_key: workflow_key.into(),
        }
    }

    /// Creates the owner for one durable execution run and all of its task workflows.
    pub(crate) fn execution_run(workflow_key: impl Into<String>) -> Self {
        Self {
            kind: LLMCompletionOwnerKind::ExecutionRun,
            workflow_key: workflow_key.into(),
        }
    }

    fn cancellation_key(&self) -> Result<String, HandlerError> {
        if self.workflow_key.trim().is_empty() {
            return Err(TerminalError::new_with_code(
                422,
                "LLM completion owner workflow key must not be empty",
            )
            .into());
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"moa.llm-completion-owner.v1\0");
        hasher.update(self.kind.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(self.workflow_key.as_bytes());
        Ok(format!(
            "{COMPLETION_CANCELLATION_KEY_DOMAIN}:{}:{}",
            self.kind.as_str(),
            hasher.finalize().to_hex()
        ))
    }
}

/// Restate service surface for journaled LLM completions.
#[restate_sdk::service]
pub trait LLMGateway {
    /// Executes one buffered completion through the configured provider.
    async fn complete(
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError>;

    /// Executes one buffered completion inside a caller-admitted resource slice.
    async fn complete_bounded(
        request: Json<BoundedCompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError>;

    /// Fences provider I/O owned by one cancelled workflow.
    async fn cancel_owner(request: Json<LLMCompletionOwner>) -> Result<(), HandlerError>;

    /// Produces at most one durable progress narration for a session.
    ///
    /// Invoked as a detached job by the per-session narration tick. Hosted on
    /// this service to reuse its provider registry and avoid a new Restate
    /// binding; the narration logic lives in [`crate::services::narration`].
    async fn narrate_session(request: Json<NarrateSessionRequest>) -> Result<(), HandlerError>;
}

/// Completion request plus the downward-only resource slice admitted for it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BoundedCompletionRequest {
    /// Provider completion request.
    pub request: CompletionRequest,
    /// Deadline and metered allowance this dispatch may consume.
    pub budget: ResourceBudget,
}

/// The request metadata needed after the provider-owned request has completed.
///
/// The durable gateway receives the owned DTO and moves it into canonical shared
/// provider storage exactly once. This compact snapshot keeps persistence
/// independent of a second full [`CompletionRequest`] clone. Raw metadata values
/// are retained so validation and malformed-value warnings keep the same
/// persistence-gated timing as the request-based helpers.
#[derive(Debug)]
struct CompletionAuditContext {
    defer_brain_response: bool,
    session_id: Option<Value>,
    tenant_id: Option<Value>,
    contact_id: Option<Value>,
    user_turn: Option<Value>,
    memory_write_barrier: Option<Value>,
}

impl CompletionAuditContext {
    /// Captures only the metadata consumed by gateway persistence and ingestion.
    fn from_request(request: &CompletionRequest) -> Self {
        Self {
            defer_brain_response: should_defer_brain_response(request),
            session_id: request.metadata.get(SESSION_ID_METADATA_KEY).cloned(),
            tenant_id: request.metadata.get(TENANT_ID_METADATA_KEY).cloned(),
            contact_id: request.metadata.get(CONTACT_ID_METADATA_KEY).cloned(),
            user_turn: request.metadata.get(USER_TURN_METADATA_KEY).cloned(),
            memory_write_barrier: request
                .metadata
                .get(MEMORY_WRITE_BARRIER_METADATA_KEY)
                .cloned(),
        }
    }

    /// Parses the session identity using the same malformed-metadata handling as the request path.
    fn session_id(&self) -> Option<SessionId> {
        session_id_from_metadata(self.session_id.as_ref())
    }

    /// Parses the tenant/contact turn scope using the same request metadata rules.
    fn turn_scope(&self) -> Option<(TenantId, ContactId)> {
        let tenant_id =
            uuid_metadata_value(self.tenant_id.as_ref(), TENANT_ID_METADATA_KEY).map(TenantId)?;
        let contact_id = uuid_metadata_value(self.contact_id.as_ref(), CONTACT_ID_METADATA_KEY)
            .map(ContactId)?;
        Some((tenant_id, contact_id))
    }

    /// Returns the durable user-turn text when its metadata is a string.
    fn user_turn(&self) -> Option<&str> {
        string_metadata_value(self.user_turn.as_ref(), USER_TURN_METADATA_KEY)
    }

    /// Parses the optional validated memory-write barrier.
    ///
    /// An invalid present barrier remains an error so memory ingestion is
    /// skipped after provider dispatch, matching
    /// [`session_turn_from_completion_request`].
    fn memory_write_barrier(
        &self,
    ) -> moa_core::error::Result<Option<moa_core::types::memory::InformationBarrierId>> {
        string_metadata_value(
            self.memory_write_barrier.as_ref(),
            MEMORY_WRITE_BARRIER_METADATA_KEY,
        )
        .map(moa_core::types::memory::InformationBarrierId::parse)
        .transpose()
    }
}

/// Concrete Restate service implementation backed by configured providers.
#[derive(Clone)]
pub struct LLMGatewayImpl {
    providers: Arc<ProviderRegistry>,
    session_limits: Option<SessionLimitsConfig>,
    runtime_cache: Option<Arc<dyn RuntimeCacheStore>>,
}

impl LLMGatewayImpl {
    /// Creates a new Restate LLM gateway over a shared provider registry.
    #[must_use]
    pub fn new(providers: Arc<ProviderRegistry>) -> Self {
        Self {
            providers,
            session_limits: None,
            runtime_cache: None,
        }
    }

    /// Supplies the session limits used by progress narration.
    #[must_use]
    pub fn with_session_limits(mut self, session_limits: SessionLimitsConfig) -> Self {
        self.session_limits = Some(session_limits);
        self
    }

    /// Supplies the shared runtime cache used to fence cancelled provider I/O.
    #[must_use]
    pub fn with_runtime_cache(mut self, runtime_cache: Arc<dyn RuntimeCacheStore>) -> Self {
        self.runtime_cache = Some(runtime_cache);
        self
    }

    /// Executes one completion directly and buffers the full provider response.
    pub async fn complete_buffered(
        &self,
        request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionResponse> {
        self.complete_buffered_with_budget(request, ResourceBudget::UNBOUNDED)
            .await
    }

    /// Executes one completion while enforcing the caller's provider-side budget.
    pub async fn complete_buffered_with_budget(
        &self,
        request: CompletionRequest,
        budget: ResourceBudget,
    ) -> moa_core::error::Result<CompletionResponse> {
        let requested_model = request.model.as_ref().map(ModelId::as_str);
        let (provider_id, model) = self.providers.resolve_provider_id(requested_model)?;
        let resolved = self.providers.provider_for_id(provider_id, &model)?;
        let mut request = bound_completion_request(request, budget)?;
        request.model = Some(resolved.model.clone());

        let provider = CancellableLLMProvider::new(
            resolved.provider,
            DeadlineGuard::from_budget(CancellationToken::new(), budget),
        );
        let stream = provider.complete(request.into_shared()).await?;
        let response = stream.collect().await?;
        admit_completion_usage(&response, budget)?;
        Ok(response)
    }

    async fn complete_buffered_with_owner_fence(
        &self,
        request: CompletionRequest,
        budget: ResourceBudget,
        owner: Option<LLMCompletionOwner>,
    ) -> Result<CompletionResponse, HandlerError> {
        let Some(owner) = owner else {
            return self
                .complete_buffered_with_budget(request, budget)
                .await
                .map_err(moa_error_to_handler_error);
        };
        let runtime_cache = self.runtime_cache.clone().ok_or_else(|| {
            TerminalError::new("owned LLM completion omitted its shared runtime cache")
        })?;
        let (_, cancelled_model) = self
            .providers
            .resolve_provider_id(request.model.as_ref().map(ModelId::as_str))
            .map_err(moa_error_to_handler_error)?;
        if completion_owner_is_cancelled(runtime_cache.as_ref(), &owner).await? {
            return Ok(cancelled_completion_response(cancelled_model));
        }

        let completion = self.complete_buffered_with_budget(request, budget);
        let cancellation = wait_for_completion_owner_cancellation(runtime_cache, owner);
        tokio::pin!(completion);
        tokio::pin!(cancellation);
        tokio::select! {
            biased;
            response = &mut completion => response.map_err(moa_error_to_handler_error),
            result = &mut cancellation => {
                result?;
                Ok(cancelled_completion_response(cancelled_model))
            },
        }
    }
}

/// Adds a typed workflow owner to one internal completion request.
pub(crate) fn attach_completion_owner(request: &mut CompletionRequest, owner: &LLMCompletionOwner) {
    request.metadata.insert(
        COMPLETION_OWNER_METADATA_KEY.to_string(),
        serde_json::json!(owner),
    );
}

/// Closed logical coordinates for paid provider actions invoked from durable workflows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LLMCompletionAction {
    /// One execution-route classifier attempt.
    ExecutionRouting { attempt: usize },
    /// One initial durable-plan generation or repair attempt.
    InitialPlanning { attempt: usize },
    /// The single root-turn input guardrail evaluation.
    RootInputGuardrail,
    /// One root model-loop turn.
    RootModel { turn: usize },
    /// One root output guardrail evaluation for a model-loop turn.
    RootOutputGuardrail { turn: usize },
    /// One worker model-loop turn.
    WorkerModel { turn: usize },
    /// One execution-task model-loop turn.
    ExecutionTaskModel { turn: u32 },
    /// One execution amendment generation or repair attempt.
    ExecutionAmendment {
        run_uid: Uuid,
        plan_revision: u64,
        attempt: usize,
    },
    /// One behavior-lab simulator turn.
    ExperimentSimulator { trial_uid: Uuid, turn: u32 },
}

impl LLMCompletionAction {
    fn coordinate(self) -> String {
        match self {
            Self::ExecutionRouting { attempt } => format!("execution-routing:{attempt}"),
            Self::InitialPlanning { attempt } => format!("initial-planning:{attempt}"),
            Self::RootInputGuardrail => "root-input-guardrail".to_string(),
            Self::RootModel { turn } => format!("root-model:{turn}"),
            Self::RootOutputGuardrail { turn } => format!("root-output-guardrail:{turn}"),
            Self::WorkerModel { turn } => format!("worker-model:{turn}"),
            Self::ExecutionTaskModel { turn } => format!("execution-task-model:{turn}"),
            Self::ExecutionAmendment {
                run_uid,
                plan_revision,
                attempt,
            } => format!("execution-amendment:{run_uid}:{plan_revision}:{attempt}"),
            Self::ExperimentSimulator { trial_uid, turn } => {
                format!("experiment-simulator:{trial_uid}:{turn}")
            }
        }
    }
}

/// Returns the stable, versioned Restate idempotency key for one workflow-owned provider action.
pub(crate) fn completion_idempotency_key(
    caller_invocation_id: &str,
    action: LLMCompletionAction,
) -> String {
    format!(
        "moa:llm-completion:v1:{caller_invocation_id}:{}",
        action.coordinate()
    )
}

fn take_completion_owner(
    request: &mut CompletionRequest,
) -> Result<Option<LLMCompletionOwner>, HandlerError> {
    let owner: Option<LLMCompletionOwner> = request
        .metadata
        .remove(COMPLETION_OWNER_METADATA_KEY)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| {
            HandlerError::from(TerminalError::new_with_code(
                422,
                format!("invalid LLM completion owner metadata: {error}"),
            ))
        })?;
    if let Some(owner) = owner.as_ref() {
        owner.cancellation_key()?;
    }
    Ok(owner)
}

/// Durably fences all provider I/O owned by one workflow.
pub(crate) async fn cancel_completion_owner(
    ctx: &SharedWorkflowContext<'_>,
    owner: LLMCompletionOwner,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<LLMGatewayClient>()
            .cancel_owner(Json::from(owner)),
    )
    .call()
    .await?;
    Ok(())
}

/// Durably fences an owner from an ordinary Restate service handler.
pub(crate) async fn cancel_completion_owner_from_service(
    ctx: &Context<'_>,
    owner: LLMCompletionOwner,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<LLMGatewayClient>()
            .cancel_owner(Json::from(owner)),
    )
    .call()
    .await?;
    Ok(())
}

/// Durably fences an owner from its keyed execution-run workflow.
pub(crate) async fn cancel_completion_owner_from_workflow(
    ctx: &WorkflowContext<'_>,
    owner: LLMCompletionOwner,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<LLMGatewayClient>()
            .cancel_owner(Json::from(owner)),
    )
    .call()
    .await?;
    Ok(())
}

async fn wait_for_completion_owner_cancellation(
    runtime_cache: Arc<dyn RuntimeCacheStore>,
    owner: LLMCompletionOwner,
) -> Result<(), HandlerError> {
    loop {
        tokio::time::sleep(COMPLETION_CANCELLATION_POLL_INTERVAL).await;
        if completion_owner_is_cancelled(runtime_cache.as_ref(), &owner).await? {
            return Ok(());
        }
    }
}

async fn completion_owner_is_cancelled(
    runtime_cache: &dyn RuntimeCacheStore,
    owner: &LLMCompletionOwner,
) -> Result<bool, HandlerError> {
    runtime_cache
        .get(&owner.cancellation_key()?)
        .await
        .map(|value| value.is_some())
        .map_err(moa_error_to_handler_error)
}

fn cancelled_completion_response(model: ModelId) -> CompletionResponse {
    CompletionResponse {
        text: String::new(),
        content: Vec::new(),
        stop_reason: StopReason::Cancelled,
        model,
        usage: TokenUsage::default(),
        duration_ms: 0,
        thought_signature: None,
    }
}

fn bound_completion_request(
    mut request: CompletionRequest,
    budget: ResourceBudget,
) -> moa_core::error::Result<CompletionRequest> {
    let Some(remaining) = budget.remaining else {
        return Ok(request);
    };
    if remaining.model_calls == 0 {
        return Err(MoaError::BudgetExhausted(
            "model dispatch refused: no model calls remain".to_string(),
        ));
    }
    if remaining.tokens == 0 {
        return Err(MoaError::BudgetExhausted(
            "model dispatch refused: no tokens remain".to_string(),
        ));
    }
    let token_cap = usize::try_from(remaining.tokens).unwrap_or(usize::MAX);
    request.max_output_tokens = Some(
        request
            .max_output_tokens
            .map_or(token_cap, |configured| configured.min(token_cap)),
    );
    Ok(request)
}

fn admit_completion_usage(
    response: &CompletionResponse,
    budget: ResourceBudget,
) -> moa_core::error::Result<()> {
    let usage = response.token_usage();
    let actual = ResourceAmounts {
        cost_micro_usd: compute_cost_micros(response.model.as_str(), usage),
        tokens: usage
            .total_input_tokens()
            .saturating_add(usage.output_tokens) as u64,
        turns: 0,
        model_calls: 1,
        tool_calls: 0,
    };
    if let Some(kind) = budget.first_exceeding(&actual) {
        return Err(MoaError::BudgetExhausted(format!(
            "model completion exceeded its admitted {kind} allowance"
        )));
    }
    Ok(())
}

async fn record_completion(
    ctx: &Context<'_>,
    audit_context: CompletionAuditContext,
    response: CompletionResponse,
    provider_id: &str,
) -> Result<Json<CompletionResponse>, HandlerError> {
    let usage = response.token_usage();
    let cost_cents = compute_cost_cents(response.model.as_str(), usage);
    let finish_reason = match &response.stop_reason {
        moa_core::types::completion::StopReason::EndTurn => "end_turn",
        moa_core::types::completion::StopReason::MaxTokens => "max_tokens",
        moa_core::types::completion::StopReason::ToolUse => "tool_use",
        moa_core::types::completion::StopReason::Cancelled => "cancelled",
        moa_core::types::completion::StopReason::Other(_) => "other",
    };
    let provider_name = genai_provider_name(provider_id);
    let operation_name = genai_operation_name(provider_id);
    let span = tracing::Span::current();
    span.set_attribute("gen_ai.operation.name", operation_name);
    span.set_attribute("gen_ai.provider.name", provider_name.to_string());
    span.set_attribute("gen_ai.request.model", response.model.to_string());
    span.set_attribute("gen_ai.response.model", response.model.to_string());
    span.set_attribute("gen_ai.response.finish_reasons", finish_reason.to_string());
    span.set_attribute(
        "gen_ai.usage.input_tokens",
        usage.total_input_tokens() as i64,
    );
    span.set_attribute("gen_ai.usage.output_tokens", usage.output_tokens as i64);
    if usage.input_tokens_cache_read > 0 {
        span.set_attribute(
            "gen_ai.usage.cache_read.input_tokens",
            usage.input_tokens_cache_read as i64,
        );
    }
    if usage.input_tokens_cache_write > 0 {
        span.set_attribute(
            "gen_ai.usage.cache_creation.input_tokens",
            usage.input_tokens_cache_write as i64,
        );
    }
    record_llm_cost_cents(provider_id, response.model.as_str(), u64::from(cost_cents));

    if should_persist_brain_response(&audit_context, &response)
        && let Some(session_id) = audit_context.session_id()
    {
        let event = Event::BrainResponse {
            text: response.text.clone(),
            thought_signature: response.thought_signature.clone(),
            model: response.model.clone(),
            model_tier: ModelTier::Main,
            input_tokens_uncached: usage.input_tokens_uncached,
            input_tokens_cache_write: usage.input_tokens_cache_write,
            input_tokens_cache_read: usage.input_tokens_cache_read,
            output_tokens: usage.output_tokens,
            cost_cents,
            duration_ms: response.duration_ms,
            llm_ttft_ms: None,
        };

        let appended = crate::restate_identity::replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event,
                    dedupe_key: None,
                })),
        )
        .call()
        .await?
        .into_inner();

        if let Some(turn) = session_turn_from_audit_context(
            &audit_context,
            session_id,
            appended.sequence_num,
            appended.timestamp,
        ) {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<IngestionVOClient>(ingestion_object_key(&turn))
                    .ingest_turn(Json(turn)),
            )
            .send();
        }
    }

    Ok(Json::from(response))
}

impl LLMGateway for LLMGatewayImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal workflow and eval-runner callers admit session or tenant access before requesting provider completion.
    async fn complete(
        &self,
        ctx: Context<'_>,
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError> {
        let mut request = request.into_inner();
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("LLMGateway", "complete");
        let completion_owner = take_completion_owner(&mut request)?;
        let (provider_id, _) = self
            .providers
            .resolve_provider_id(request.model.as_ref().map(ModelId::as_str))
            .map_err(moa_error_to_handler_error)?;
        let audit_context = CompletionAuditContext::from_request(&request);
        let service = self.clone();
        let response = ctx
            .run(|| async move {
                service
                    .complete_buffered_with_owner_fence(
                        request,
                        ResourceBudget::UNBOUNDED,
                        completion_owner,
                    )
                    .await
                    .map(Json::from)
            })
            .name("llm_complete")
            .retry_policy(llm_run_retry_policy())
            .await?
            .into_inner();
        record_completion(&ctx, audit_context, response, provider_id.as_str()).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal workflow callers admit the resource slice and session or tenant access before requesting provider completion.
    async fn complete_bounded(
        &self,
        ctx: Context<'_>,
        request: Json<BoundedCompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError> {
        let BoundedCompletionRequest {
            mut request,
            budget,
        } = request.into_inner();
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("LLMGateway", "complete_bounded");
        let completion_owner = take_completion_owner(&mut request)?;
        let (provider_id, _) = self
            .providers
            .resolve_provider_id(request.model.as_ref().map(ModelId::as_str))
            .map_err(moa_error_to_handler_error)?;
        let audit_context = CompletionAuditContext::from_request(&request);
        let service = self.clone();
        let response = ctx
            .run(|| async move {
                service
                    .complete_buffered_with_owner_fence(request, budget, completion_owner)
                    .await
                    .map(Json::from)
            })
            .name("llm_complete_bounded")
            .retry_policy(llm_run_retry_policy())
            .await?
            .into_inner();
        record_completion(&ctx, audit_context, response, provider_id.as_str()).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal workflow cancellation handlers call this ingress-private service with their own workflow key.
    async fn cancel_owner(
        &self,
        ctx: Context<'_>,
        request: Json<LLMCompletionOwner>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("LLMGateway", "cancel_owner");
        let key = request.into_inner().cancellation_key()?;
        let runtime_cache = self.runtime_cache.clone().ok_or_else(|| {
            TerminalError::new("LLM completion cancellation omitted its shared runtime cache")
        })?;
        ctx.run(|| async move {
            runtime_cache
                .set(
                    &key,
                    COMPLETION_CANCELLATION_VALUE.to_vec(),
                    COMPLETION_CANCELLATION_TTL,
                )
                .await
                .map_err(moa_error_to_handler_error)
        })
        .name("llm_cancel_owner")
        .await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: dispatched as a detached job by the per-session narration tick, which forwards the session participant identity used to authorize the gated progress read.
    async fn narrate_session(
        &self,
        ctx: Context<'_>,
        request: Json<NarrateSessionRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("LLMGateway", "narrate_session");
        crate::services::narration::run_narration_job(
            &ctx,
            self,
            self.session_limits.as_ref(),
            request.into_inner(),
        )
        .await
    }
}

/// Returns whether a completion request defers visible `BrainResponse` persistence to its caller.
#[must_use]
pub fn should_defer_brain_response(request: &CompletionRequest) -> bool {
    matches!(
        request.metadata.get(DEFER_BRAIN_RESPONSE_METADATA_KEY),
        Some(Value::Bool(true))
    )
}

fn should_persist_brain_response(
    audit_context: &CompletionAuditContext,
    response: &CompletionResponse,
) -> bool {
    response.stop_reason != StopReason::Cancelled && !audit_context.defer_brain_response
}

/// Computes the normalized completion cost in cents for one model response.
///
/// Resolves the model's pricing catalog entry and defers to the single
/// canonical formula in [`moa_core::types::model::TokenPricing::cost_cents`].
#[must_use]
pub fn compute_cost_cents(model: &str, usage: TokenUsage) -> u32 {
    let pricing = moa_providers::pricing_for_model(model).unwrap_or_else(zero_token_pricing);
    pricing.cost_cents(&usage)
}

/// Computes the completion cost in micros of USD for one model response.
///
/// Resolves the model's pricing catalog entry and defers to the single
/// canonical formula in [`moa_core::types::model::TokenPricing::cost_micros`],
/// which keeps sub-cent precision so lineage records the true cost of small
/// turns instead of rounding them to zero.
#[must_use]
pub fn compute_cost_micros(model: &str, usage: TokenUsage) -> u64 {
    let pricing = moa_providers::pricing_for_model(model).unwrap_or_else(zero_token_pricing);
    pricing.cost_micros(&usage)
}

fn zero_token_pricing() -> TokenPricing {
    TokenPricing {
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
        cached_input_per_mtok: None,
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
    }
}

fn llm_run_retry_policy() -> RunRetryPolicy {
    // Provider adapters own retry (three retries, four attempts per candidate)
    // and failover. Restate gets one attempt so it cannot multiply paid calls;
    // the total bound is `configured_candidates * 4`.
    RunRetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(30))
        .max_attempts(1)
}

fn session_id_from_metadata(session_value: Option<&Value>) -> Option<SessionId> {
    let session_value = session_value?;
    match session_value {
        Value::String(raw) => parse_session_id(raw),
        other => {
            tracing::warn!(
                metadata = %other,
                "ignoring non-string _moa.session_id metadata"
            );
            None
        }
    }
}

fn turn_scope_from_request(request: &CompletionRequest) -> Option<(TenantId, ContactId)> {
    let tenant_id = uuid_metadata_value(
        request.metadata.get(TENANT_ID_METADATA_KEY),
        TENANT_ID_METADATA_KEY,
    )
    .map(TenantId)?;
    let contact_id = uuid_metadata_value(
        request.metadata.get(CONTACT_ID_METADATA_KEY),
        CONTACT_ID_METADATA_KEY,
    )
    .map(ContactId)?;
    Some((tenant_id, contact_id))
}

/// Metadata key carrying the active turn's durable user-message text.
///
/// Turn compilation stamps this from the session event log. Memory ingestion
/// reads it instead of the compiled provider messages so injected context
/// (memory reminders, digests, planning hints) and replayed history never
/// re-enter fact extraction, and requests without a durable user turn (worker
/// sub-requests, internal jobs) never write conversational memory.
pub(crate) const USER_TURN_METADATA_KEY: &str = "_moa.user_turn";
/// Metadata key carrying the pinned agent policy's validated memory write barrier.
pub(crate) const MEMORY_WRITE_BARRIER_METADATA_KEY: &str = "_moa.memory_write_barrier";

pub(crate) fn session_turn_from_completion_request(
    request: &CompletionRequest,
    session_id: SessionId,
    turn_seq: u64,
    finalized_at: DateTime<Utc>,
) -> Option<SessionTurn> {
    let (tenant_id, contact_id) = turn_scope_from_request(request)?;
    let transcript = turn_transcript(string_metadata(request, USER_TURN_METADATA_KEY)?);
    if transcript.trim().is_empty() {
        return None;
    }
    Some(SessionTurn {
        tenant_id,
        contact_id: Some(contact_id),
        session_id,
        turn_seq,
        dominant_pii_class: dominant_pii_class_hint(&transcript).to_string(),
        transcript,
        finalized_at,
        barrier: string_metadata(request, MEMORY_WRITE_BARRIER_METADATA_KEY)
            .map(moa_core::types::memory::InformationBarrierId::parse)
            .transpose()
            .ok()?,
    })
}

fn session_turn_from_audit_context(
    audit_context: &CompletionAuditContext,
    session_id: SessionId,
    turn_seq: u64,
    finalized_at: DateTime<Utc>,
) -> Option<SessionTurn> {
    let (tenant_id, contact_id) = audit_context.turn_scope()?;
    let transcript = turn_transcript(audit_context.user_turn()?);
    if transcript.trim().is_empty() {
        return None;
    }
    Some(SessionTurn {
        tenant_id,
        contact_id: Some(contact_id),
        session_id,
        turn_seq,
        dominant_pii_class: dominant_pii_class_hint(&transcript).to_string(),
        transcript,
        finalized_at,
        barrier: audit_context.memory_write_barrier().ok()?,
    })
}

fn string_metadata<'a>(request: &'a CompletionRequest, key: &str) -> Option<&'a str> {
    string_metadata_value(request.metadata.get(key), key)
}

fn string_metadata_value<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    match value? {
        Value::String(raw) => Some(raw.as_str()),
        other => {
            tracing::warn!(metadata = %other, key, "ignoring non-string request metadata");
            None
        }
    }
}

fn uuid_metadata_value(value: Option<&Value>, key: &str) -> Option<Uuid> {
    let raw = string_metadata_value(value, key)?;
    match Uuid::parse_str(raw) {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(metadata = raw, key, error = %error, "ignoring invalid UUID metadata");
            None
        }
    }
}

fn dominant_pii_class_hint(transcript: &str) -> &'static str {
    let lower = transcript.to_ascii_lowercase();
    if lower.contains("ssn") || lower.contains("medical record") || lower.contains("government id")
    {
        "phi"
    } else if lower.contains("secret") || lower.contains("sk-") || lower.contains("account number")
    {
        "restricted"
    } else if transcript.contains('@') || lower.contains("phone") || lower.contains("address") {
        "pii"
    } else {
        "none"
    }
}

fn parse_session_id(raw: &str) -> Option<SessionId> {
    match Uuid::parse_str(raw) {
        Ok(uuid) => Some(SessionId(uuid)),
        Err(error) => {
            tracing::warn!(session_id = raw, error = %error, "ignoring invalid _moa.session_id metadata");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{DateTime, Utc};
    use moa_core::{
        error::MoaError,
        types::completion::CompletionRequest,
        types::completion::StopReason,
        types::contact::ContactId,
        types::context::ContextMessage,
        types::identifiers::ModelId,
        types::identifiers::SessionId,
        types::identifiers::TenantId,
        types::resource::{ResourceAmounts, ResourceBudget},
    };
    use serde_json::json;

    use super::{
        COMPLETION_OWNER_METADATA_KEY, CompletionAuditContext, LLMCompletionOwner,
        MEMORY_WRITE_BARRIER_METADATA_KEY, USER_TURN_METADATA_KEY, attach_completion_owner,
        bound_completion_request, cancelled_completion_response, session_turn_from_audit_context,
        session_turn_from_completion_request, should_persist_brain_response, take_completion_owner,
    };

    #[test]
    fn completion_owner_is_typed_stripped_and_zero_usage_offline() {
        // Pins: the internal workflow owner never leaks into a provider request,
        // and a fenced completion cannot contribute content or billable usage.
        let owner = LLMCompletionOwner::execution_run(uuid::Uuid::from_u128(41).to_string());
        let mut request = CompletionRequest::new("cancel me");
        attach_completion_owner(&mut request, &owner);

        assert_eq!(
            take_completion_owner(&mut request).expect("typed completion owner should decode"),
            Some(owner)
        );
        assert!(!request.metadata.contains_key(COMPLETION_OWNER_METADATA_KEY));

        let cancelled = cancelled_completion_response(ModelId::new("cancelled-test"));
        let audit_context = CompletionAuditContext::from_request(&request);
        assert_eq!(cancelled.stop_reason, StopReason::Cancelled);
        assert!(cancelled.text.is_empty());
        assert!(cancelled.content.is_empty());
        assert_eq!(cancelled.usage, Default::default());
        assert!(
            !should_persist_brain_response(&audit_context, &cancelled),
            "cancelled completions must not append an empty visible response or ingest a turn"
        );
    }

    #[test]
    fn completion_owner_cache_keys_are_kind_scoped_and_hashed_offline() {
        // Pins: equal workflow keys in different workflow namespaces never share
        // a cancellation fence, and raw owner keys are not exposed to Valkey.
        let raw_key = "tenant-sensitive-owner";
        let root = LLMCompletionOwner::root_turn(raw_key)
            .cancellation_key()
            .expect("root owner should produce a cache key");
        let worker = LLMCompletionOwner::worker_turn(raw_key)
            .cancellation_key()
            .expect("worker owner should produce a cache key");
        let run = LLMCompletionOwner::execution_run(raw_key)
            .cancellation_key()
            .expect("execution run owner should produce a cache key");

        assert_ne!(root, worker);
        assert_ne!(worker, run);
        assert_ne!(root, run);
        assert!(!root.contains(raw_key));
        assert!(!worker.contains(raw_key));
        assert!(!run.contains(raw_key));
    }

    #[test]
    fn bounded_completion_clamps_output_and_refuses_an_exhausted_call_offline() {
        // Pins: the budget is enforced inside LLMGateway, after the Restate hop and
        // before provider dispatch, so a child cannot silently regain provider defaults.
        let request = CompletionRequest {
            max_output_tokens: Some(8_192),
            ..CompletionRequest::new("bounded")
        };
        let bounded = bound_completion_request(
            request.clone(),
            ResourceBudget::new(
                None,
                Some(ResourceAmounts {
                    cost_micro_usd: 1_000,
                    tokens: 256,
                    turns: 0,
                    model_calls: 1,
                    tool_calls: 0,
                }),
            ),
        )
        .expect("one admitted model call should be accepted");
        assert_eq!(bounded.max_output_tokens, Some(256));

        let error = bound_completion_request(
            request,
            ResourceBudget::new(
                None,
                Some(ResourceAmounts {
                    tokens: 256,
                    ..ResourceAmounts::ZERO
                }),
            ),
        )
        .expect_err("zero remaining model calls must fail before provider dispatch");
        assert!(matches!(error, MoaError::BudgetExhausted(_)));
    }

    #[test]
    fn session_turn_contact_id_matches_turn_author() {
        // Pins: finalized LLM turns stamp memory ingestion with the current request's contact metadata.
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let contact_id = ContactId::new();
        let finalized_at = DateTime::parse_from_rfc3339("2026-05-07T12:00:00Z")
            .expect("test timestamp parses")
            .with_timezone(&Utc);
        let mut metadata = HashMap::new();
        metadata.insert("_moa.session_id".to_string(), json!(session_id.to_string()));
        metadata.insert("_moa.tenant_id".to_string(), json!(tenant_id.to_string()));
        metadata.insert("_moa.contact_id".to_string(), json!(contact_id.to_string()));
        metadata.insert(
            USER_TURN_METADATA_KEY.to_string(),
            json!("My email is user-alpha@example.com."),
        );
        metadata.insert(
            MEMORY_WRITE_BARRIER_METADATA_KEY.to_string(),
            json!("deal-alpha"),
        );
        let request = CompletionRequest {
            model: None,
            messages: vec![
                ContextMessage::system("system policy"),
                ContextMessage::assistant("prior assistant answer"),
                ContextMessage::user("history user turn from a previous exchange"),
                ContextMessage::user(
                    "<memory-reminder>\nuser-alpha deploys to us-east-1\n</memory-reminder>",
                ),
                ContextMessage::user("My email is user-alpha@example.com."),
            ],
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            response_format: None,
            native_web_search: Default::default(),
            metadata,
        };

        let audit_context = CompletionAuditContext::from_request(&request);
        let turn = session_turn_from_completion_request(&request, session_id, 42, finalized_at)
            .expect("request metadata should produce an ingestable turn");
        let audit_turn =
            session_turn_from_audit_context(&audit_context, session_id, 42, finalized_at)
                .expect("audit metadata should produce the same ingestable turn");

        assert_eq!(audit_turn, turn);

        assert_eq!(turn.tenant_id, tenant_id);
        assert_eq!(turn.contact_id, Some(contact_id));
        assert_eq!(turn.session_id, session_id);
        assert_eq!(turn.turn_seq, 42);
        assert_eq!(turn.finalized_at, finalized_at);
        assert_eq!(turn.dominant_pii_class, "pii");
        assert_eq!(
            turn.barrier.as_ref().map(|barrier| barrier.as_str()),
            Some("deal-alpha")
        );
        assert_eq!(turn.transcript, "user: My email is user-alpha@example.com.");
        assert!(!turn.transcript.contains("system policy"));
        assert!(!turn.transcript.contains("prior assistant answer"));
        // Pins the feedback-loop fix: injected memory reminders and replayed
        // history in the compiled request never reach the ingestion transcript.
        assert!(!turn.transcript.contains("memory-reminder"));
        assert!(!turn.transcript.contains("us-east-1"));
        assert!(!turn.transcript.contains("previous exchange"));
    }

    #[test]
    fn session_turn_requires_a_durable_user_turn() {
        // Pins: requests without user-turn metadata (worker sub-requests,
        // internal jobs) never write conversational memory, even when their
        // compiled messages contain user-role content.
        let session_id = SessionId::new();
        let mut metadata = HashMap::new();
        metadata.insert("_moa.session_id".to_string(), json!(session_id.to_string()));
        metadata.insert(
            "_moa.tenant_id".to_string(),
            json!(TenantId::new().to_string()),
        );
        metadata.insert(
            "_moa.contact_id".to_string(),
            json!(ContactId::new().to_string()),
        );
        let request = CompletionRequest {
            model: None,
            messages: vec![ContextMessage::user("worker task prompt")],
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            response_format: None,
            native_web_search: Default::default(),
            metadata,
        };

        assert!(
            session_turn_from_completion_request(&request, session_id, 7, Utc::now()).is_none()
        );
    }

    #[test]
    fn completion_audit_context_skips_ingestion_for_malformed_barrier() {
        // Pins: moving only persistence metadata out of the request retains the
        // same ingestion skip after dispatch when a memory barrier is malformed.
        let session_id = SessionId::new();
        let mut metadata = HashMap::new();
        metadata.insert(
            "_moa.tenant_id".to_string(),
            json!(TenantId::new().to_string()),
        );
        metadata.insert(
            "_moa.contact_id".to_string(),
            json!(ContactId::new().to_string()),
        );
        metadata.insert(USER_TURN_METADATA_KEY.to_string(), json!("hello"));
        metadata.insert(MEMORY_WRITE_BARRIER_METADATA_KEY.to_string(), json!(""));
        let request = CompletionRequest {
            metadata,
            ..CompletionRequest::new("hello")
        };
        let audit_context = CompletionAuditContext::from_request(&request);
        let finalized_at = Utc::now();

        assert_eq!(
            session_turn_from_audit_context(&audit_context, session_id, 7, finalized_at),
            session_turn_from_completion_request(&request, session_id, 7, finalized_at)
        );
        assert!(
            session_turn_from_audit_context(&audit_context, session_id, 7, finalized_at).is_none()
        );
    }
}
