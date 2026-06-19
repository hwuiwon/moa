//! Runtime registry for configured LLM provider families.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[cfg(feature = "scripted-provider")]
use moa_core::{CompletionContent, StopReason, ToolCallContent, ToolInvocation};
use moa_core::{
    LLMProvider, MoaConfig, MoaError, ModelCapabilities, ModelId, ModelTask, QueryRewriteConfig,
};
#[cfg(feature = "scripted-provider")]
use moa_core::{TokenPricing, ToolCallFormat as ScriptedToolCallFormat};
#[cfg(feature = "scripted-provider")]
use serde::Deserialize;
#[cfg(feature = "scripted-provider")]
use serde_json::Value;

use crate::routing::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_OPENAI_MODEL, ProviderKind,
    REWRITER_ANTHROPIC_MODEL, REWRITER_GOOGLE_MODEL, REWRITER_OPENAI_MODEL, infer_provider_kind,
    split_explicit_provider,
};
use crate::{AnthropicProvider, GeminiProvider, ModelRouter, OpenAIProvider};
#[cfg(feature = "scripted-provider")]
use crate::{ScriptedBlock, ScriptedProvider, ScriptedResponse};

#[derive(Clone)]
enum ProviderSource {
    Static(Arc<dyn LLMProvider>),
    Factory(ProviderFactory),
}

type ProviderFactory = Arc<dyn Fn(&str) -> moa_core::Result<Arc<dyn LLMProvider>> + Send + Sync>;
type ProviderCache = Arc<RwLock<HashMap<ProviderCacheKey, ResolvedProvider>>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderCacheKey {
    kind: ProviderKind,
    model: ModelId,
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

/// Provider instance resolved for one configured provider family and model.
#[derive(Clone)]
pub struct ResolvedProvider {
    /// Provider client instance.
    pub provider: Arc<dyn LLMProvider>,
    /// Model id that should be written onto the completion request.
    pub model: ModelId,
}

/// Runtime registry for configured provider families.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    anthropic: Option<RegisteredProvider>,
    openai: Option<RegisteredProvider>,
    google: Option<RegisteredProvider>,
    provider_cache: ProviderCache,
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
            provider_cache: ProviderCache::default(),
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
            provider_cache: ProviderCache::default(),
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
            provider_cache: ProviderCache::default(),
        }
    }

    /// Builds a deterministic scripted registry from a JSON fixture file.
    pub fn scripted(path: impl AsRef<std::path::Path>) -> moa_core::Result<Self> {
        #[cfg(not(feature = "scripted-provider"))]
        {
            let _ = path;
            Err(MoaError::ConfigError(
                "MOA_PROVIDERS_OVERRIDE=scripted requires the moa-providers/scripted-provider feature"
                    .to_string(),
            ))
        }

        #[cfg(feature = "scripted-provider")]
        {
            let path = path.as_ref();
            let body = std::fs::read_to_string(path).map_err(|error| {
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
    }

    /// Builds a deterministic mock registry with an unbounded fallback response.
    pub fn mock(seed: u64) -> moa_core::Result<Self> {
        #[cfg(not(feature = "scripted-provider"))]
        {
            let _ = seed;
            Err(MoaError::ConfigError(
                "MOA_PROVIDERS_OVERRIDE=mock requires the moa-providers/scripted-provider feature"
                    .to_string(),
            ))
        }

        #[cfg(feature = "scripted-provider")]
        {
            let response = ScriptedResponse::text(format!("OK mock response seed={seed}"));
            let provider = Arc::new(
                ScriptedProvider::new(scripted_capabilities("scripted-mock"))
                    .with_fallback_response(response),
            );
            Ok(Self::all_kinds_from_static(provider))
        }
    }

    #[cfg(feature = "scripted-provider")]
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
            .provider_for_kind(provider_kind, &model)?
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
            return Ok(Some(self.provider_for_kind(kind, &model)?.provider));
        }

        if self.anthropic.is_some() {
            let model = ModelId::new(REWRITER_ANTHROPIC_MODEL);
            return Ok(Some(
                self.provider_for_kind(ProviderKind::Anthropic, &model)?
                    .provider,
            ));
        }
        if self.openai.is_some() {
            let model = ModelId::new(REWRITER_OPENAI_MODEL);
            return Ok(Some(
                self.provider_for_kind(ProviderKind::OpenAI, &model)?
                    .provider,
            ));
        }
        if self.google.is_some() {
            let model = ModelId::new(REWRITER_GOOGLE_MODEL);
            return Ok(Some(
                self.provider_for_kind(ProviderKind::Google, &model)?
                    .provider,
            ));
        }

        Ok(None)
    }

    /// Builds a model-task router using this registry's resolved provider instances.
    pub fn model_router_for_config(&self, config: &MoaConfig) -> moa_core::Result<ModelRouter> {
        let main = self.provider_for_model(Some(config.model_for_task(ModelTask::MainLoop)))?;
        let auxiliary = config
            .models
            .auxiliary
            .as_deref()
            .map(|model| self.provider_for_model(Some(model)))
            .transpose()?;
        Ok(ModelRouter::new(main, auxiliary))
    }

    /// Resolves the provider instance that should serve a requested model.
    pub fn provider_for_model(
        &self,
        requested_model: Option<&str>,
    ) -> moa_core::Result<Arc<dyn LLMProvider>> {
        let (kind, model) = self.resolve_provider_kind(requested_model)?;
        Ok(self.provider_for_kind(kind, &model)?.provider)
    }

    /// Resolves a provider instance for an already-selected provider family and model.
    pub fn provider_for_kind(
        &self,
        kind: ProviderKind,
        model: &ModelId,
    ) -> moa_core::Result<ResolvedProvider> {
        let cache_key = ProviderCacheKey {
            kind,
            model: model.clone(),
        };
        if let Some(resolved) = self.cached_provider(&cache_key) {
            return Ok(resolved);
        }

        let provider = self
            .provider_entry(kind)
            .ok_or_else(|| {
                MoaError::ConfigError(format!("{} provider is not configured", kind.as_str()))
            })?
            .build(model.as_str())?;

        let resolved = ResolvedProvider {
            provider,
            model: model.clone(),
        };
        self.cache_provider(cache_key, resolved.clone());
        Ok(resolved)
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

    fn provider_entry(&self, kind: ProviderKind) -> Option<&RegisteredProvider> {
        match kind {
            ProviderKind::Anthropic => self.anthropic.as_ref(),
            ProviderKind::OpenAI => self.openai.as_ref(),
            ProviderKind::Google => self.google.as_ref(),
        }
    }

    fn cached_provider(&self, key: &ProviderCacheKey) -> Option<ResolvedProvider> {
        let cache = match self.provider_cache.read() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.get(key).cloned()
    }

    fn cache_provider(&self, key: ProviderCacheKey, provider: ResolvedProvider) {
        let mut cache = match self.provider_cache.write() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.entry(key).or_insert(provider);
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

#[cfg(feature = "scripted-provider")]
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ScriptedProviderFile {
    default: Option<ScriptedEntry>,
    responses: Vec<ScriptedEntry>,
}

#[cfg(feature = "scripted-provider")]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScriptedEntry {
    Wrapped { completion: ScriptedCompletion },
    Direct(ScriptedCompletion),
}

#[cfg(feature = "scripted-provider")]
impl ScriptedEntry {
    fn into_completion(self) -> ScriptedCompletion {
        match self {
            Self::Wrapped { completion } => completion,
            Self::Direct(completion) => completion,
        }
    }
}

#[cfg(feature = "scripted-provider")]
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

#[cfg(feature = "scripted-provider")]
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

#[cfg(feature = "scripted-provider")]
#[derive(Debug, Deserialize)]
struct ScriptedToolCall {
    name: String,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    id: Option<String>,
}

#[cfg(feature = "scripted-provider")]
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

#[cfg(feature = "scripted-provider")]
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
                CompletionContent::Text(text) => ScriptedBlock::text(text),
                CompletionContent::ToolCall(call) => ScriptedBlock::tool_call(
                    call.invocation.name,
                    call.invocation.input,
                    call.invocation
                        .id
                        .unwrap_or_else(|| "scripted-tool-call".to_string()),
                ),
                CompletionContent::ProviderToolResult { tool_name, summary } => {
                    ScriptedBlock::provider_tool_result(tool_name, summary)
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

#[cfg(feature = "scripted-provider")]
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

#[cfg(feature = "scripted-provider")]
fn scripted_capabilities(model_id: &str) -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new(model_id),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
        cache_ttl: Some(std::time::Duration::from_secs(300)),
        tool_call_format: ScriptedToolCallFormat::Anthropic,
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use moa_core::{
        CompletionRequest, CompletionResponse, CompletionStream, LLMProvider, ModelCapabilities,
        ModelId, StopReason, TokenPricing, TokenUsage, ToolCallFormat,
    };

    use super::{
        ProviderCache, ProviderFactory, ProviderKind, ProviderRegistry, ProviderSource,
        RegisteredProvider,
    };

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

    struct CacheTestProvider {
        model: String,
    }

    #[async_trait]
    impl LLMProvider for CacheTestProvider {
        fn name(&self) -> &str {
            "cache-test"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: ModelId::new(self.model.clone()),
                context_window: 32_000,
                max_output: 1_024,
                supports_tools: false,
                supports_vision: false,
                supports_prefix_caching: false,
                cache_ttl: None,
                tool_call_format: ToolCallFormat::OpenAiCompatible,
                pricing: TokenPricing {
                    input_per_mtok: 0.0,
                    output_per_mtok: 0.0,
                    cached_input_per_mtok: None,
                    cache_write_5m_per_mtok: None,
                    cache_write_1h_per_mtok: None,
                },
                native_tools: Vec::new(),
            }
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> moa_core::Result<CompletionStream> {
            Ok(CompletionStream::from_response(CompletionResponse {
                text: "ok".to_string(),
                content: Vec::new(),
                stop_reason: StopReason::EndTurn,
                model: ModelId::new(self.model.clone()),
                usage: TokenUsage::default(),
                duration_ms: 1,
                thought_signature: None,
            }))
        }
    }

    fn provider(model: &'static str) -> Arc<dyn LLMProvider> {
        Arc::new(CacheTestProvider {
            model: model.to_string(),
        })
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
    fn provider_registry_reuses_factory_provider_for_same_model() {
        // Pins: repeated provider resolution does not rebuild factory-backed clients.
        let builds = Arc::new(AtomicUsize::new(0));
        let builds_for_factory = builds.clone();
        let factory: ProviderFactory = Arc::new(move |model| {
            builds_for_factory.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(CacheTestProvider {
                model: model.to_string(),
            }))
        });
        let registry = ProviderRegistry {
            anthropic: None,
            openai: Some(RegisteredProvider {
                default_model: "gpt-cache".to_string(),
                source: ProviderSource::Factory(factory),
            }),
            google: None,
            provider_cache: ProviderCache::default(),
        };

        let first = registry
            .provider_for_model(Some("openai:gpt-cache"))
            .expect("explicit provider model resolves");
        let second = registry
            .provider_for_model(Some("gpt-cache"))
            .expect("default provider model resolves through same cache key");
        let third = registry
            .provider_for_model(Some("openai:gpt-cache-other"))
            .expect("different model resolves");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn resolves_requested_model_by_family_and_explicit_prefix() {
        // Pins: provider routing accepts family inference and explicit provider prefixes.
        let registry = ProviderRegistry::with_static_providers(
            Some(provider("claude-sonnet-4-6")),
            Some(provider("gpt-5.4")),
            Some(provider("gemini-3-flash-preview")),
        );

        let (kind, model) = registry
            .resolve_provider_kind(Some("claude-sonnet-4-6"))
            .expect("claude model should resolve");
        assert_eq!(kind, ProviderKind::Anthropic);
        assert_eq!(model, ModelId::new("claude-sonnet-4-6"));

        let (kind, model) = registry
            .resolve_provider_kind(Some("gpt-5.4"))
            .expect("gpt model should resolve");
        assert_eq!(kind, ProviderKind::OpenAI);
        assert_eq!(model, ModelId::new("gpt-5.4"));

        let (kind, model) = registry
            .resolve_provider_kind(Some("google:gemini-3-flash-preview"))
            .expect("prefixed google model should resolve");
        assert_eq!(kind, ProviderKind::Google);
        assert_eq!(model, ModelId::new("gemini-3-flash-preview"));
    }

    #[test]
    fn resolves_static_default_model_without_family_prefix() {
        // Pins: a configured static provider can route custom model ids by default-model match.
        let registry = ProviderRegistry::with_static_providers(
            None,
            Some(provider("scripted-loadtest")),
            None,
        );

        let (kind, model) = registry
            .resolve_provider_kind(Some("scripted-loadtest"))
            .expect("configured static default model should resolve");

        assert_eq!(kind, ProviderKind::OpenAI);
        assert_eq!(model, ModelId::new("scripted-loadtest"));
    }

    #[test]
    fn missing_provider_returns_configuration_error() {
        // Pins: inferred model families still require that provider family to be configured.
        let registry =
            ProviderRegistry::with_static_providers(None, Some(provider("gpt-5.4")), None);

        let error = registry
            .resolve_provider_kind(Some("claude-sonnet-4-6"))
            .expect_err("unconfigured Anthropic provider should fail");

        assert!(
            error
                .to_string()
                .contains("anthropic provider is not configured"),
            "unexpected error: {error}"
        );
    }
}
