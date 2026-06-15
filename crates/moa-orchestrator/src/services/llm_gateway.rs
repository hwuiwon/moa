//! Durable Restate façade over the workspace LLM providers.

use std::sync::Arc;
use std::time::Duration;
use std::{fs, path::Path};

use chrono::{DateTime, Utc};
use moa_core::wire::AppendEventRequest;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, Event, LLMProvider, MoaConfig,
    MoaError, ModelCapabilities, ModelId, ModelTier, QueryRewriteConfig, SessionId, StopReason,
    TokenPricing, TokenUsage, ToolCallContent, ToolCallFormat, ToolInvocation, UserId, WorkspaceId,
    record_llm_cost_cents,
};
use moa_memory_ingest::{IngestionVOClient, SessionTurn, ingestion_object_key, turn_transcript};
use moa_providers::{
    AnthropicProvider, GeminiProvider, OpenAIProvider, ScriptedProvider, ScriptedResponse,
};
use restate_sdk::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::services::session_store::RestateSessionStoreClient;
use moa_core::restate_observability::annotate_restate_handler_span;

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4";
const DEFAULT_GOOGLE_MODEL: &str = "gemini-3-flash-preview";
const REWRITER_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
const REWRITER_OPENAI_MODEL: &str = "gpt-5.4-mini";
const REWRITER_GOOGLE_MODEL: &str = "gemini-3-flash-preview";

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
    Factory(ProviderFactory),
}

type ProviderFactory = Arc<dyn Fn(&str) -> moa_core::Result<Arc<dyn LLMProvider>> + Send + Sync>;

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
                source: ProviderSource::Factory(Arc::new(build_anthropic_provider)),
            }),
            openai: configured_env("OPENAI_API_KEY").then_some(RegisteredProvider {
                default_model: DEFAULT_OPENAI_MODEL.to_string(),
                source: ProviderSource::Factory(Arc::new(build_openai_provider)),
            }),
            google: configured_env("GOOGLE_API_KEY").then_some(RegisteredProvider {
                default_model: DEFAULT_GOOGLE_MODEL.to_string(),
                source: ProviderSource::Factory(Arc::new(build_google_provider)),
            }),
        }
    }

    /// Builds a registry from configured provider API key environment names.
    #[must_use]
    pub fn from_config(config: &MoaConfig) -> Self {
        let config = Arc::new(config.clone());
        Self {
            anthropic: configured_env(&config.providers.anthropic.api_key_env).then(|| {
                let config = config.clone();
                RegisteredProvider {
                    default_model: DEFAULT_ANTHROPIC_MODEL.to_string(),
                    source: ProviderSource::Factory(Arc::new(move |model| {
                        build_anthropic_provider_from_config(&config, model)
                    })),
                }
            }),
            openai: configured_env(&config.providers.openai.api_key_env).then(|| {
                let config = config.clone();
                RegisteredProvider {
                    default_model: DEFAULT_OPENAI_MODEL.to_string(),
                    source: ProviderSource::Factory(Arc::new(move |model| {
                        build_openai_provider_from_config(&config, model)
                    })),
                }
            }),
            google: configured_env(&config.providers.google.api_key_env).then(|| {
                let config = config.clone();
                RegisteredProvider {
                    default_model: DEFAULT_GOOGLE_MODEL.to_string(),
                    source: ProviderSource::Factory(Arc::new(move |model| {
                        build_google_provider_from_config(&config, model)
                    })),
                }
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

    /// Builds a deterministic scripted registry from a JSON fixture file.
    pub fn scripted(path: impl AsRef<Path>) -> moa_core::Result<Self> {
        let path = path.as_ref();
        let body = fs::read_to_string(path).map_err(|error| {
            MoaError::ConfigError(format!(
                "failed to read scripted provider fixture {}: {error}",
                path.display()
            ))
        })?;
        let file: ScriptedProviderFile = serde_json::from_str(&body).map_err(|error| {
            MoaError::ConfigError(format!(
                "failed to parse scripted provider fixture {}: {error}",
                path.display()
            ))
        })?;
        scripted_registry_from_file(file)
    }

    /// Builds a deterministic mock registry with an unbounded fallback response.
    #[must_use]
    pub fn mock(seed: u64) -> Self {
        let response = ScriptedResponse::text(format!("OK mock response seed={seed}"));
        let provider = Arc::new(
            ScriptedProvider::new(scripted_capabilities("scripted-mock"))
                .with_fallback_response(response),
        );
        Self::all_kinds_from_static(provider)
    }

    fn all_kinds_from_static(provider: Arc<dyn LLMProvider>) -> Self {
        Self::with_static_providers(
            Some(provider.clone()),
            Some(provider.clone()),
            Some(provider),
        )
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

        if let Some(provider_kind) = self.provider_kind_for_default_model(trimmed) {
            return Ok((provider_kind, ModelId::new(trimmed)));
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

    fn provider_kind_for_default_model(&self, model: &str) -> Option<ProviderKind> {
        if self
            .openai
            .as_ref()
            .is_some_and(|provider| provider.default_model == model)
        {
            return Some(ProviderKind::OpenAI);
        }
        if self
            .anthropic
            .as_ref()
            .is_some_and(|provider| provider.default_model == model)
        {
            return Some(ProviderKind::Anthropic);
        }
        if self
            .google
            .as_ref()
            .is_some_and(|provider| provider.default_model == model)
        {
            return Some(ProviderKind::Google);
        }
        None
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

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ScriptedProviderFile {
    default: Option<ScriptedEntry>,
    responses: Vec<ScriptedEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScriptedEntry {
    Wrapped { completion: ScriptedCompletion },
    Direct(ScriptedCompletion),
}

impl ScriptedEntry {
    fn into_completion(self) -> ScriptedCompletion {
        match self {
            Self::Wrapped { completion } => completion,
            Self::Direct(completion) => completion,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ScriptedCompletion {
    content: String,
    tool_calls: Vec<ScriptedToolCall>,
    duration_ms: u64,
    input_tokens: usize,
    cached_input_tokens: usize,
    cache_write_input_tokens: usize,
    stop_reason: Option<String>,
}

impl Default for ScriptedCompletion {
    fn default() -> Self {
        Self {
            content: "OK".to_string(),
            tool_calls: Vec::new(),
            duration_ms: 1,
            input_tokens: 64,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            stop_reason: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ScriptedToolCall {
    name: String,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    id: Option<String>,
}

fn scripted_registry_from_file(file: ScriptedProviderFile) -> moa_core::Result<ProviderRegistry> {
    let mut provider = ScriptedProvider::new(scripted_capabilities("scripted-loadtest"));
    if let Some(default) = file.default {
        provider = provider.with_fallback_response(scripted_response(default)?);
    }
    for response in file.responses {
        provider = provider.push_response(scripted_response(response)?);
    }
    Ok(ProviderRegistry::all_kinds_from_static(Arc::new(provider)))
}

fn scripted_response(entry: ScriptedEntry) -> moa_core::Result<ScriptedResponse> {
    let completion = entry.into_completion();
    let mut blocks = Vec::new();
    if !completion.content.is_empty() {
        blocks.push(CompletionContent::Text(completion.content));
    }
    for (index, call) in completion.tool_calls.into_iter().enumerate() {
        blocks.push(CompletionContent::ToolCall(ToolCallContent {
            invocation: ToolInvocation {
                id: Some(
                    call.id
                        .unwrap_or_else(|| format!("scripted-tool-call-{}", index + 1)),
                ),
                name: call.name,
                input: call.input,
            },
            provider_metadata: None,
        }));
    }

    let mut response = ScriptedResponse::from_blocks(
        blocks
            .into_iter()
            .map(|block| match block {
                CompletionContent::Text(text) => moa_providers::ScriptedBlock::text(text),
                CompletionContent::ToolCall(call) => moa_providers::ScriptedBlock::tool_call(
                    call.invocation.name,
                    call.invocation.input,
                    call.invocation
                        .id
                        .unwrap_or_else(|| "scripted-tool-call".to_string()),
                ),
                CompletionContent::ProviderToolResult { tool_name, summary } => {
                    moa_providers::ScriptedBlock::provider_tool_result(tool_name, summary)
                }
            })
            .collect(),
    );
    response.duration_ms = completion.duration_ms;
    response.input_tokens = completion.input_tokens;
    response.cached_input_tokens = completion.cached_input_tokens;
    response.cache_write_input_tokens = completion.cache_write_input_tokens;
    if let Some(stop_reason) = completion.stop_reason {
        response.stop_reason = parse_scripted_stop_reason(&stop_reason)?;
    }
    Ok(response)
}

fn parse_scripted_stop_reason(raw: &str) -> moa_core::Result<StopReason> {
    match raw {
        "end_turn" => Ok(StopReason::EndTurn),
        "max_tokens" => Ok(StopReason::MaxTokens),
        "tool_use" => Ok(StopReason::ToolUse),
        "cancelled" => Ok(StopReason::Cancelled),
        other if !other.trim().is_empty() => Ok(StopReason::Other(other.to_string())),
        _ => Err(MoaError::ConfigError(
            "scripted stop_reason must be non-empty".to_string(),
        )),
    }
}

fn scripted_capabilities(model_id: &str) -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new(model_id),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
        cache_ttl: Some(Duration::from_secs(300)),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: Some(0.0),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        native_tools: Vec::new(),
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

fn build_anthropic_provider_from_config(
    config: &MoaConfig,
    model: &str,
) -> moa_core::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(AnthropicProvider::from_config_with_model(
        config, model,
    )?))
}

fn build_openai_provider_from_config(
    config: &MoaConfig,
    model: &str,
) -> moa_core::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(OpenAIProvider::from_config_with_model(
        config, model,
    )?))
}

fn build_google_provider_from_config(
    config: &MoaConfig,
    model: &str,
) -> moa_core::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(GeminiProvider::from_config_with_model(
        config, model,
    )?))
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
    use std::sync::Mutex;

    use chrono::{DateTime, Utc};
    use moa_core::{CompletionRequest, ContextMessage, ModelId, SessionId, UserId, WorkspaceId};
    use serde_json::json;

    use super::{ProviderKind, ProviderRegistry, session_turn_from_completion_request};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        key: &'static str,
        value: Option<String>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key,
                value: original,
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = &self.value {
                    std::env::set_var(self.key, value);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn from_config_uses_configured_api_key_env_name() {
        // Pins: provider registry availability follows MoaConfig provider env-var names.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::set("MOA_TEST_OPENAI_API_KEY", "test-key");
        let mut config = moa_core::MoaConfig::default();
        config.providers.openai.api_key_env = "MOA_TEST_OPENAI_API_KEY".to_string();

        let registry = ProviderRegistry::from_config(&config);
        let (kind, model) = registry
            .resolve_provider_kind(Some("openai:gpt-5.4-mini"))
            .expect("configured custom OpenAI env should enable provider");

        assert_eq!(kind, ProviderKind::OpenAI);
        assert_eq!(model, ModelId::new("gpt-5.4-mini"));
    }

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
