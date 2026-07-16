//! Durable Restate facade over configured LLM providers.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::config::SessionLimitsConfig;
use moa_core::wire::session_store::AppendEventRequest;
use moa_core::{
    events::Event, types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::DEFER_BRAIN_RESPONSE_METADATA_KEY, types::completion::TokenUsage,
    types::contact::ContactId, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::model::TokenPricing,
    types::observability::genai_operation_name, types::observability::genai_provider_name,
    types::provider::ModelTier,
};
use moa_memory_ingest::{IngestionVOClient, SessionTurn, ingestion_object_key, turn_transcript};
use moa_observability::record_llm_cost_cents;
use moa_providers::ProviderRegistry;
use restate_sdk::prelude::*;
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::services::narration::NarrateSessionRequest;
use crate::services::session_store::RestateSessionStoreClient;
use crate::workflows::errors::moa_error_to_handler_error;
use moa_observability::restate_observability::annotate_restate_handler_span;

/// Restate service surface for journaled LLM completions.
#[restate_sdk::service]
pub trait LLMGateway {
    /// Executes one buffered completion through the configured provider.
    async fn complete(
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError>;

    /// Produces at most one durable progress narration for a session.
    ///
    /// Invoked as a detached job by the per-session narration tick. Hosted on
    /// this service to reuse its provider registry and avoid a new Restate
    /// binding; the narration logic lives in [`crate::services::narration`].
    async fn narrate_session(request: Json<NarrateSessionRequest>) -> Result<(), HandlerError>;
}

/// Concrete Restate service implementation backed by configured providers.
#[derive(Clone)]
pub struct LLMGatewayImpl {
    providers: Arc<ProviderRegistry>,
    session_limits: Option<SessionLimitsConfig>,
}

impl LLMGatewayImpl {
    /// Creates a new Restate LLM gateway over a shared provider registry.
    #[must_use]
    pub fn new(providers: Arc<ProviderRegistry>) -> Self {
        Self {
            providers,
            session_limits: None,
        }
    }

    /// Supplies the session limits used by progress narration.
    #[must_use]
    pub fn with_session_limits(mut self, session_limits: SessionLimitsConfig) -> Self {
        self.session_limits = Some(session_limits);
        self
    }

    /// Executes one completion directly and buffers the full provider response.
    pub async fn complete_buffered(
        &self,
        request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionResponse> {
        let requested_model = request.model.as_ref().map(ModelId::as_str);
        let (provider_id, model) = self.providers.resolve_provider_id(requested_model)?;
        let resolved = self.providers.provider_for_id(provider_id, &model)?;
        let mut request = request;
        request.model = Some(resolved.model.clone());

        let stream = resolved.provider.complete(request).await?;
        stream.collect().await
    }
}

impl LLMGateway for LLMGatewayImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal workflow and eval-runner callers admit session or tenant access before requesting provider completion.
    async fn complete(
        &self,
        ctx: Context<'_>,
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError> {
        let request = request.into_inner();
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("LLMGateway", "complete");
        let (provider_id, _) = self
            .providers
            .resolve_provider_id(request.model.as_ref().map(ModelId::as_str))
            .map_err(moa_error_to_handler_error)?;
        let request_for_run = request.clone();
        let service = self.clone();
        let response = ctx
            .run(|| async move {
                service
                    .complete_buffered(request_for_run)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name("llm_complete")
            .retry_policy(llm_run_retry_policy())
            .await?
            .into_inner();
        let usage = response.token_usage();
        let cost_cents = compute_cost_cents(response.model.as_str(), usage);
        let finish_reason = match &response.stop_reason {
            moa_core::types::completion::StopReason::EndTurn => "end_turn",
            moa_core::types::completion::StopReason::MaxTokens => "max_tokens",
            moa_core::types::completion::StopReason::ToolUse => "tool_use",
            moa_core::types::completion::StopReason::Cancelled => "cancelled",
            moa_core::types::completion::StopReason::Other(_) => "other",
        };
        let provider_name = genai_provider_name(provider_id.as_str());
        let operation_name = genai_operation_name(provider_id.as_str());
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
        record_llm_cost_cents(
            provider_id.as_str(),
            response.model.as_str(),
            cost_cents as u64,
        );

        if !should_defer_brain_response(&request)
            && let Some(session_id) = session_id_from_request(&request)
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

            let turn_seq = crate::restate_identity::replay_safe_request(
                ctx.service_client::<RestateSessionStoreClient>()
                    .append_event(Json(AppendEventRequest {
                        session_id,
                        event,
                        dedupe_key: None,
                    })),
            )
            .call()
            .await?
            .into_inner()
            .sequence_num;

            if let Some(turn) =
                session_turn_from_completion_request(&request, session_id, turn_seq, Utc::now())
            {
                crate::restate_identity::replay_safe_request(
                    ctx.object_client::<IngestionVOClient>(ingestion_object_key(&turn))
                        .ingest_turn(Json(turn)),
                )
                .send();
            }
        }

        Ok(Json::from(response))
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
    RunRetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(30))
        .max_attempts(5)
}

fn session_id_from_request(request: &CompletionRequest) -> Option<SessionId> {
    let session_value = request.metadata.get("_moa.session_id")?;
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
    let tenant_id = uuid_metadata(request, "_moa.tenant_id").map(TenantId)?;
    let contact_id = uuid_metadata(request, "_moa.contact_id").map(ContactId)?;
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
    })
}

fn string_metadata<'a>(request: &'a CompletionRequest, key: &str) -> Option<&'a str> {
    match request.metadata.get(key)? {
        Value::String(raw) => Some(raw.as_str()),
        other => {
            tracing::warn!(metadata = %other, key, "ignoring non-string request metadata");
            None
        }
    }
}

fn uuid_metadata(request: &CompletionRequest, key: &str) -> Option<Uuid> {
    let raw = string_metadata(request, key)?;
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
        types::completion::CompletionRequest, types::contact::ContactId,
        types::context::ContextMessage, types::identifiers::SessionId,
        types::identifiers::TenantId,
    };
    use serde_json::json;

    use super::{USER_TURN_METADATA_KEY, session_turn_from_completion_request};

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

        let turn = session_turn_from_completion_request(&request, session_id, 42, finalized_at)
            .expect("request metadata should produce an ingestable turn");

        assert_eq!(turn.tenant_id, tenant_id);
        assert_eq!(turn.contact_id, Some(contact_id));
        assert_eq!(turn.session_id, session_id);
        assert_eq!(turn.turn_seq, 42);
        assert_eq!(turn.finalized_at, finalized_at);
        assert_eq!(turn.dominant_pii_class, "pii");
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
}
