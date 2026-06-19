//! Durable Restate façade over the workspace LLM providers.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::wire::AppendEventRequest;
use moa_core::{
    CompletionRequest, CompletionResponse, Event, MoaError, ModelId, ModelTier, SessionId,
    TokenPricing, TokenUsage, UserId, WorkspaceId, record_llm_cost_cents,
};
use moa_memory_ingest::{IngestionVOClient, SessionTurn, ingestion_object_key, turn_transcript};
use moa_providers::ProviderRegistry;
use restate_sdk::prelude::*;
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::services::session_store::RestateSessionStoreClient;
use moa_core::restate_observability::annotate_restate_handler_span;

/// Restate service surface for journaled LLM completions.
#[restate_sdk::service]
pub trait LLMGateway {
    /// Executes one buffered completion through the configured provider.
    async fn complete(
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError>;
}

/// Concrete Restate service implementation backed by workspace providers.
#[derive(Clone)]
pub struct LLMGatewayImpl {
    providers: Arc<ProviderRegistry>,
}

impl LLMGatewayImpl {
    /// Creates a new Restate LLM gateway over a shared provider registry.
    #[must_use]
    pub fn new(providers: Arc<ProviderRegistry>) -> Self {
        Self { providers }
    }

    /// Executes one completion directly and buffers the full provider response.
    pub async fn complete_buffered(
        &self,
        request: CompletionRequest,
    ) -> moa_core::Result<CompletionResponse> {
        let requested_model = request.model.as_ref().map(ModelId::as_str);
        let (provider_kind, model) = self.providers.resolve_provider_kind(requested_model)?;
        let resolved = self.providers.provider_for_kind(provider_kind, &model)?;
        let mut request = request;
        request.model = Some(resolved.model.clone());

        let stream = resolved.provider.complete(request).await?;
        stream.collect().await
    }
}

impl LLMGateway for LLMGatewayImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn complete(
        &self,
        ctx: Context<'_>,
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError> {
        let request = request.into_inner();
        annotate_restate_handler_span("LLMGateway", "complete");
        let (provider_kind, _) = self
            .providers
            .resolve_provider_kind(request.model.as_ref().map(ModelId::as_str))
            .map_err(to_handler_error)?;
        let request_for_run = request.clone();
        let service = self.clone();
        let response = ctx
            .run(|| async move {
                service
                    .complete_buffered(request_for_run)
                    .await
                    .map(Json::from)
                    .map_err(to_handler_error)
            })
            .name("llm_complete")
            .retry_policy(llm_run_retry_policy())
            .await?
            .into_inner();
        let usage = response.token_usage();
        let cost_cents = compute_cost_cents(response.model.as_str(), usage);
        let finish_reason = match &response.stop_reason {
            moa_core::StopReason::EndTurn => "end_turn",
            moa_core::StopReason::MaxTokens => "max_tokens",
            moa_core::StopReason::ToolUse => "tool_use",
            moa_core::StopReason::Cancelled => "cancelled",
            moa_core::StopReason::Other(_) => "other",
        };
        let span = tracing::Span::current();
        span.set_attribute("gen_ai.system", provider_kind.as_str().to_string());
        span.set_attribute("gen_ai.request.model", response.model.to_string());
        span.set_attribute("gen_ai.response.model", response.model.to_string());
        span.set_attribute("gen_ai.response.finish_reasons", finish_reason.to_string());
        span.set_attribute(
            "gen_ai.usage.input_tokens",
            usage.input_tokens_uncached as i64,
        );
        span.set_attribute("gen_ai.usage.output_tokens", usage.output_tokens as i64);
        record_llm_cost_cents(
            provider_kind.as_str(),
            response.model.as_str(),
            cost_cents as u64,
        );

        if let Some(session_id) = session_id_from_request(&request) {
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
            };

            let turn_seq = ctx
                .service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest { session_id, event }))
                .call()
                .await?;

            if let Some(turn) = session_turn_from_completion_request(
                &request,
                &response.text,
                session_id,
                turn_seq,
                Utc::now(),
            ) {
                ctx.object_client::<IngestionVOClient>(ingestion_object_key(&turn))
                    .ingest_turn(Json(turn))
                    .send();
            }
        }

        Ok(Json::from(response))
    }
}

/// Computes the normalized completion cost in cents for one model response.
#[must_use]
pub fn compute_cost_cents(model: &str, usage: TokenUsage) -> u32 {
    let pricing = moa_providers::pricing_for_model(model).unwrap_or_else(zero_token_pricing);
    let input_cost = usage.input_tokens_uncached as f64 / 1_000_000.0 * pricing.input_per_mtok;
    let cache_write_cost =
        usage.input_tokens_cache_write as f64 / 1_000_000.0 * pricing.cache_write_per_mtok();
    let cache_read_cost = usage.input_tokens_cache_read as f64 / 1_000_000.0
        * pricing
            .cached_input_per_mtok
            .unwrap_or(pricing.input_per_mtok);
    let output_cost = usage.output_tokens as f64 / 1_000_000.0 * pricing.output_per_mtok;

    ((input_cost + cache_write_cost + cache_read_cost + output_cost) * 100.0).round() as u32
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

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
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

fn turn_scope_from_request(request: &CompletionRequest) -> Option<(WorkspaceId, UserId)> {
    let workspace_id = string_metadata(request, "_moa.workspace_id").map(WorkspaceId::new)?;
    let user_id = string_metadata(request, "_moa.user_id").map(UserId::new)?;
    Some((workspace_id, user_id))
}

fn session_turn_from_completion_request(
    request: &CompletionRequest,
    response_text: &str,
    session_id: SessionId,
    turn_seq: u64,
    finalized_at: DateTime<Utc>,
) -> Option<SessionTurn> {
    let (workspace_id, user_id) = turn_scope_from_request(request)?;
    validate_turn_author_scope(request, &user_id);
    let transcript = turn_transcript(&request.messages, response_text);
    if transcript.trim().is_empty() {
        return None;
    }
    Some(SessionTurn {
        workspace_id,
        user_id,
        session_id,
        turn_seq,
        dominant_pii_class: dominant_pii_class_hint(&transcript).to_string(),
        transcript,
        finalized_at,
    })
}

fn validate_turn_author_scope(request: &CompletionRequest, turn_user_id: &UserId) {
    let Some(author_user_id) = string_metadata(request, "_moa.user_id").map(UserId::new) else {
        return;
    };
    if author_user_id != *turn_user_id {
        debug_assert_eq!(
            author_user_id, *turn_user_id,
            "SessionTurn.user_id must match the current turn author"
        );
        tracing::warn!(
            author_user_id = %author_user_id,
            turn_user_id = %turn_user_id,
            "SessionTurn user attribution mismatch"
        );
    }
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
    use moa_core::{CompletionRequest, ContextMessage, SessionId, UserId, WorkspaceId};
    use serde_json::json;

    use super::session_turn_from_completion_request;

    #[test]
    fn session_turn_user_id_matches_turn_author() {
        // Pins: finalized LLM turns stamp memory ingestion with the current request's user metadata.
        let session_id = SessionId::new();
        let finalized_at = DateTime::parse_from_rfc3339("2026-05-07T12:00:00Z")
            .expect("test timestamp parses")
            .with_timezone(&Utc);
        let mut metadata = HashMap::new();
        metadata.insert("_moa.session_id".to_string(), json!(session_id.to_string()));
        metadata.insert("_moa.workspace_id".to_string(), json!("workspace-alpha"));
        metadata.insert("_moa.user_id".to_string(), json!("user-alpha"));
        let request = CompletionRequest {
            model: None,
            messages: vec![
                ContextMessage::system("system policy"),
                ContextMessage::assistant("prior assistant answer"),
                ContextMessage::user("My email is user-alpha@example.com."),
            ],
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            response_format: None,
            metadata,
        };

        let turn = session_turn_from_completion_request(
            &request,
            "Stored that.",
            session_id,
            42,
            finalized_at,
        )
        .expect("request metadata should produce an ingestable turn");

        assert_eq!(turn.workspace_id, WorkspaceId::new("workspace-alpha"));
        assert_eq!(turn.user_id, UserId::new("user-alpha"));
        assert_eq!(turn.session_id, session_id);
        assert_eq!(turn.turn_seq, 42);
        assert_eq!(turn.finalized_at, finalized_at);
        assert_eq!(turn.dominant_pii_class, "pii");
        assert_eq!(
            turn.transcript,
            "user: My email is user-alpha@example.com.\nassistant: Stored that."
        );
        assert!(!turn.transcript.contains("system policy"));
        assert!(!turn.transcript.contains("prior assistant answer"));
    }
}
