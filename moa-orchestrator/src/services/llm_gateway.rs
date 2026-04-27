//! Durable Restate façade over the workspace LLM providers.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use moa_core::{
    CompletionRequest, CompletionResponse, Event, LLMProvider, MoaError, ModelCapabilities,
    ModelId, ModelTier, QueryRewriteConfig, SessionId, TokenPricing, TokenUsage, UserId,
    WorkspaceId, record_llm_cost_cents,
};
use moa_memory_ingest::SessionTurn;
use moa_providers::{AnthropicProvider, GeminiProvider, OpenAIProvider};
use restate_sdk::prelude::*;
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::ingestion_vo::{IngestionVOClient, ingestion_object_key, turn_transcript};
use crate::observability::annotate_restate_handler_span;
use crate::services::session_store::{AppendEventRequest, SessionStoreClient};

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4";
const DEFAULT_GOOGLE_MODEL: &str = "gemini-3.1-pro-preview";
const REWRITER_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
const REWRITER_OPENAI_MODEL: &str = "gpt-5.4-mini";
const REWRITER_GOOGLE_MODEL: &str = "gemini-3.1-flash-lite-preview";

/// Restate service surface for journaled LLM completions.
#[restate_sdk::service]
pub trait LLMGateway {
    /// Executes one buffered completion through the configured provider.
    async fn complete(
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionResponse>, HandlerError>;

    /// Starts a streamed completion and returns a polling handle.
    async fn stream_complete(
        request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionStreamHandle>, HandlerError>;

    /// Polls an existing streamed completion handle for the next chunk.
    async fn poll_stream(
        handle: Json<CompletionStreamHandle>,
    ) -> Result<Json<StreamPoll>, HandlerError>;
}

/// Opaque handle for a streamed completion session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompletionStreamHandle {
    /// Stable stream identifier.
    pub id: Uuid,
    /// Expiration timestamp for the stream handle.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Polled streaming state returned by `poll_stream`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StreamPoll {
    /// One streamed text chunk.
    Chunk {
        /// Newly available text.
        text: String,
        /// Tokens emitted so far.
        partial_tokens: usize,
    },
    /// Final buffered response.
    Done {
        /// Completed buffered response.
        full_response: CompletionResponse,
    },
    /// Terminal stream failure.
    Error {
        /// Human-readable failure message.
        message: String,
    },
}

/// Provider family selected for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Anthropic Claude models.
    Anthropic,
    /// OpenAI GPT/o-series models.
    OpenAI,
    /// Google Gemini models.
    Google,
}

impl ProviderKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Google => "google",
        }
    }
}

#[derive(Clone)]
enum ProviderSource {
    Static(Arc<dyn LLMProvider>),
    Factory(fn(&str) -> moa_core::Result<Arc<dyn LLMProvider>>),
}

#[derive(Clone)]
struct RegisteredProvider {
    default_model: String,
    source: ProviderSource,
}

impl RegisteredProvider {
    fn from_static(provider: Arc<dyn LLMProvider>) -> Self {
        Self {
            default_model: provider.capabilities().model_id.to_string(),
            source: ProviderSource::Static(provider),
        }
    }

    fn default_model(&self) -> ModelId {
        match &self.source {
            ProviderSource::Static(provider) => provider.capabilities().model_id,
            ProviderSource::Factory(_) => ModelId::new(self.default_model.clone()),
        }
    }

    fn build(&self, model: &str) -> moa_core::Result<Arc<dyn LLMProvider>> {
        match &self.source {
            ProviderSource::Static(provider) => Ok(provider.clone()),
            ProviderSource::Factory(factory) => factory(model),
        }
    }
}

/// Runtime registry for configured provider families.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    anthropic: Option<RegisteredProvider>,
    openai: Option<RegisteredProvider>,
    google: Option<RegisteredProvider>,
}

impl ProviderRegistry {
    /// Builds a registry from the provider API keys available in the environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            anthropic: configured_env("ANTHROPIC_API_KEY").then_some(RegisteredProvider {
                default_model: DEFAULT_ANTHROPIC_MODEL.to_string(),
                source: ProviderSource::Factory(build_anthropic_provider),
            }),
            openai: configured_env("OPENAI_API_KEY").then_some(RegisteredProvider {
                default_model: DEFAULT_OPENAI_MODEL.to_string(),
                source: ProviderSource::Factory(build_openai_provider),
            }),
            google: configured_env("GOOGLE_API_KEY").then_some(RegisteredProvider {
                default_model: DEFAULT_GOOGLE_MODEL.to_string(),
                source: ProviderSource::Factory(build_google_provider),
            }),
        }
    }

    /// Builds a registry from preconstructed provider instances.
    #[must_use]
    pub fn with_static_providers(
        anthropic: Option<Arc<dyn LLMProvider>>,
        openai: Option<Arc<dyn LLMProvider>>,
        google: Option<Arc<dyn LLMProvider>>,
    ) -> Self {
        Self {
            anthropic: anthropic.map(RegisteredProvider::from_static),
            openai: openai.map(RegisteredProvider::from_static),
            google: google.map(RegisteredProvider::from_static),
        }
    }

    /// Resolves which provider family should serve the requested model.
    pub fn resolve_provider_kind(
        &self,
        requested_model: Option<&str>,
    ) -> moa_core::Result<(ProviderKind, ModelId)> {
        match requested_model {
            Some(requested_model) => self.resolve_requested_model(requested_model),
            None => self.resolve_default_model(),
        }
    }

    /// Resolves model capabilities for the requested model using the configured provider family.
    pub fn capabilities_for_model(
        &self,
        requested_model: Option<&str>,
    ) -> moa_core::Result<ModelCapabilities> {
        let (provider_kind, model) = self.resolve_provider_kind(requested_model)?;
        Ok(self
            .provider_for(provider_kind, &model)?
            .provider
            .capabilities())
    }

    /// Resolves the provider instance that should serve query-rewriting calls.
    pub fn resolve_rewriter_provider(
        &self,
        config: &QueryRewriteConfig,
    ) -> moa_core::Result<Option<Arc<dyn LLMProvider>>> {
        if !config.enabled {
            return Ok(None);
        }

        if let Some(model) = config.model.as_deref() {
            let (kind, model) = self.resolve_provider_kind(Some(model))?;
            return Ok(Some(self.provider_for(kind, &model)?.provider));
        }

        if let Some(provider) = self.anthropic.as_ref() {
            return Ok(Some(provider.build(REWRITER_ANTHROPIC_MODEL)?));
        }
        if let Some(provider) = self.openai.as_ref() {
            return Ok(Some(provider.build(REWRITER_OPENAI_MODEL)?));
        }
        if let Some(provider) = self.google.as_ref() {
            return Ok(Some(provider.build(REWRITER_GOOGLE_MODEL)?));
        }

        Ok(None)
    }

    fn resolve_requested_model(
        &self,
        requested_model: &str,
    ) -> moa_core::Result<(ProviderKind, ModelId)> {
        let trimmed = requested_model.trim();
        if trimmed.is_empty() {
            return self.resolve_default_model();
        }

        if let Some((provider_kind, model_id)) = split_explicit_provider(trimmed) {
            self.provider_entry(provider_kind).ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "{} provider is not configured",
                    provider_kind.as_str()
                ))
            })?;
            return Ok((provider_kind, ModelId::new(model_id)));
        }

        let provider_kind = infer_provider_kind(trimmed).ok_or_else(|| {
            MoaError::ConfigError(format!(
                "could not infer a configured provider for model `{trimmed}`"
            ))
        })?;
        self.provider_entry(provider_kind).ok_or_else(|| {
            MoaError::ConfigError(format!(
                "{} provider is not configured",
                provider_kind.as_str()
            ))
        })?;

        Ok((provider_kind, ModelId::new(trimmed)))
    }

    fn resolve_default_model(&self) -> moa_core::Result<(ProviderKind, ModelId)> {
        if let Some(provider) = &self.openai {
            return Ok((ProviderKind::OpenAI, provider.default_model()));
        }
        if let Some(provider) = &self.anthropic {
            return Ok((ProviderKind::Anthropic, provider.default_model()));
        }
        if let Some(provider) = &self.google {
            return Ok((ProviderKind::Google, provider.default_model()));
        }

        Err(MoaError::ConfigError(
            "LLMGateway has no configured providers".to_string(),
        ))
    }

    fn provider_for(
        &self,
        kind: ProviderKind,
        model: &ModelId,
    ) -> moa_core::Result<ResolvedProvider> {
        let provider = self
            .provider_entry(kind)
            .ok_or_else(|| {
                MoaError::ConfigError(format!("{} provider is not configured", kind.as_str()))
            })?
            .build(model.as_str())?;

        Ok(ResolvedProvider {
            provider,
            model: model.clone(),
        })
    }

    fn provider_entry(&self, kind: ProviderKind) -> Option<&RegisteredProvider> {
        match kind {
            ProviderKind::Anthropic => self.anthropic.as_ref(),
            ProviderKind::OpenAI => self.openai.as_ref(),
            ProviderKind::Google => self.google.as_ref(),
        }
    }
}

struct ResolvedProvider {
    provider: Arc<dyn LLMProvider>,
    model: ModelId,
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
        let resolved = self.providers.provider_for(provider_kind, &model)?;
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
                .service_client::<SessionStoreClient>()
                .append_event(Json(AppendEventRequest { session_id, event }))
                .call()
                .await?;

            if let Some((workspace_id, user_id)) = turn_scope_from_request(&request) {
                let transcript = turn_transcript(&request.messages, &response.text);
                if !transcript.trim().is_empty() {
                    let turn = SessionTurn {
                        workspace_id,
                        user_id,
                        session_id,
                        turn_seq,
                        dominant_pii_class: dominant_pii_class_hint(&transcript).to_string(),
                        transcript,
                        finalized_at: Utc::now(),
                    };
                    ctx.object_client::<IngestionVOClient>(ingestion_object_key(&turn))
                        .ingest_turn(Json(turn))
                        .send();
                }
            }
        }

        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, _ctx, _request))]
    async fn stream_complete(
        &self,
        _ctx: Context<'_>,
        _request: Json<CompletionRequest>,
    ) -> Result<Json<CompletionStreamHandle>, HandlerError> {
        annotate_restate_handler_span("LLMGateway", "stream_complete");
        Err(TerminalError::new("stream_complete not yet implemented").into())
    }

    #[tracing::instrument(skip(self, _ctx, _handle))]
    async fn poll_stream(
        &self,
        _ctx: Context<'_>,
        _handle: Json<CompletionStreamHandle>,
    ) -> Result<Json<StreamPoll>, HandlerError> {
        annotate_restate_handler_span("LLMGateway", "poll_stream");
        Err(TerminalError::new("poll_stream not yet implemented").into())
    }
}

/// Computes the normalized completion cost in cents for one model response.
#[must_use]
pub fn compute_cost_cents(model: &str, usage: TokenUsage) -> u32 {
    let pricing = pricing_for_model(model);
    let input_cost = usage.input_tokens_uncached as f64 / 1_000_000.0 * pricing.input_per_mtok;
    let cache_write_cost =
        usage.input_tokens_cache_write as f64 / 1_000_000.0 * pricing.input_per_mtok;
    let cache_read_cost = usage.input_tokens_cache_read as f64 / 1_000_000.0
        * pricing
            .cached_input_per_mtok
            .unwrap_or(pricing.input_per_mtok);
    let output_cost = usage.output_tokens as f64 / 1_000_000.0 * pricing.output_per_mtok;

    ((input_cost + cache_write_cost + cache_read_cost + output_cost) * 100.0).round() as u32
}

fn configured_env(key: &str) -> bool {
    std::env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn build_anthropic_provider(model: &str) -> moa_core::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(AnthropicProvider::from_env(model)?))
}

fn build_openai_provider(model: &str) -> moa_core::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(OpenAIProvider::from_env(model)?))
}

fn build_google_provider(model: &str) -> moa_core::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(GeminiProvider::from_env(model)?))
}

fn split_explicit_provider(model: &str) -> Option<(ProviderKind, &str)> {
    let (provider, model_id) = model.split_once(':')?;
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }

    let kind = match provider.trim() {
        "anthropic" => ProviderKind::Anthropic,
        "openai" => ProviderKind::OpenAI,
        "google" => ProviderKind::Google,
        _ => return None,
    };

    Some((kind, model_id))
}

fn infer_provider_kind(model: &str) -> Option<ProviderKind> {
    if model.starts_with("claude-") {
        return Some(ProviderKind::Anthropic);
    }
    if model.starts_with("gemini-") {
        return Some(ProviderKind::Google);
    }
    if model.starts_with("gpt-")
        || model.starts_with("chatgpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        return Some(ProviderKind::OpenAI);
    }

    None
}

fn pricing_for_model(model: &str) -> TokenPricing {
    if model.starts_with("claude-haiku-4-5") {
        return TokenPricing {
            input_per_mtok: 0.8,
            output_per_mtok: 4.0,
            cached_input_per_mtok: Some(0.08),
        };
    }
    if model.starts_with("claude-opus-4-6") {
        return TokenPricing {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cached_input_per_mtok: Some(0.5),
        };
    }
    if model.starts_with("claude-sonnet-4-6") {
        return TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
        };
    }
    if model.starts_with("gpt-5.4-mini") {
        return TokenPricing {
            input_per_mtok: 0.75,
            output_per_mtok: 4.50,
            cached_input_per_mtok: Some(0.075),
        };
    }
    if model.starts_with("gpt-5.4-nano") {
        return TokenPricing {
            input_per_mtok: 0.20,
            output_per_mtok: 1.25,
            cached_input_per_mtok: Some(0.02),
        };
    }
    if model.starts_with("gpt-5.4") {
        return TokenPricing {
            input_per_mtok: 2.50,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.25),
        };
    }
    if model.starts_with("gpt-5-mini") {
        return TokenPricing {
            input_per_mtok: 0.25,
            output_per_mtok: 2.0,
            cached_input_per_mtok: Some(0.025),
        };
    }
    if model.starts_with("gpt-5-nano") {
        return TokenPricing {
            input_per_mtok: 0.05,
            output_per_mtok: 0.40,
            cached_input_per_mtok: Some(0.005),
        };
    }
    if model.starts_with("gemini-3.1-pro-preview") {
        return TokenPricing {
            input_per_mtok: 2.0,
            output_per_mtok: 8.0,
            cached_input_per_mtok: Some(0.2),
        };
    }
    if model.starts_with("gemini-3.1-flash-lite-preview") {
        return TokenPricing {
            input_per_mtok: 0.25,
            output_per_mtok: 1.0,
            cached_input_per_mtok: Some(0.025),
        };
    }
    if model.starts_with("gemini-3-flash-preview") {
        return TokenPricing {
            input_per_mtok: 0.5,
            output_per_mtok: 2.0,
            cached_input_per_mtok: Some(0.05),
        };
    }
    if model.starts_with("gemini-2.5-pro") {
        return TokenPricing {
            input_per_mtok: 1.25,
            output_per_mtok: 10.0,
            cached_input_per_mtok: Some(0.125),
        };
    }
    if model.starts_with("gemini-2.5-flash") {
        return TokenPricing {
            input_per_mtok: 0.3,
            output_per_mtok: 2.5,
            cached_input_per_mtok: Some(0.03),
        };
    }

    TokenPricing {
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
        cached_input_per_mtok: None,
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
