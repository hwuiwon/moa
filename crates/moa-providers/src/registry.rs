//! Runtime registry for configured LLM provider families.

use std::collections::{BTreeMap, HashMap};
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

use crate::ModelRouter;
use crate::routing::{
    PROVIDER_DESCRIPTORS, ProviderDescriptor, ProviderId, infer_provider_id,
    provider_descriptor_by_name, split_explicit_provider,
};
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
    id: ProviderId,
    model: ModelId,
}

#[derive(Clone)]
struct RegisteredProvider {
    descriptor: &'static ProviderDescriptor,
    default_model: String,
    source: ProviderSource,
}

impl RegisteredProvider {
    fn from_factory(descriptor: &'static ProviderDescriptor, factory: ProviderFactory) -> Self {
        Self {
            descriptor,
            default_model: descriptor.default_model.to_string(),
            source: ProviderSource::Factory(factory),
        }
    }

    fn from_static(
        descriptor: &'static ProviderDescriptor,
        provider: Arc<dyn LLMProvider>,
    ) -> Self {
        Self {
            descriptor,
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
    providers: BTreeMap<ProviderId, RegisteredProvider>,
    provider_cache: ProviderCache,
}

impl ProviderRegistry {
    /// Builds a registry from the provider API keys available in standard provider env vars.
    #[must_use]
    pub fn from_env() -> Self {
        let mut registry = Self::default();
        for descriptor in PROVIDER_DESCRIPTORS {
            if configured_env(descriptor.default_api_key_env) {
                registry.register_factory(
                    descriptor,
                    Arc::new(move |model| (descriptor.build_from_env)(model)),
                );
            }
        }
        registry
    }

    /// Builds a registry from configured provider API keys.
    #[must_use]
    pub fn from_config(config: &MoaConfig) -> Self {
        let config = Arc::new(config.clone());
        let mut registry = Self::default();
        for descriptor in PROVIDER_DESCRIPTORS {
            if configured_secret((descriptor.api_key)(&config)) {
                let config = config.clone();
                registry.register_factory(
                    descriptor,
                    Arc::new(move |model| (descriptor.build_from_config)(&config, model)),
                );
            }
        }
        registry
    }

    /// Builds a registry from preconstructed provider instances.
    #[must_use]
    pub fn with_static_providers(
        anthropic: Option<Arc<dyn LLMProvider>>,
        openai: Option<Arc<dyn LLMProvider>>,
        google: Option<Arc<dyn LLMProvider>>,
    ) -> Self {
        let mut registry = Self::default();
        if let Some(provider) = openai {
            registry.register_static(ProviderId::OpenAI, provider);
        }
        if let Some(provider) = anthropic {
            registry.register_static(ProviderId::Anthropic, provider);
        }
        if let Some(provider) = google {
            registry.register_static(ProviderId::Google, provider);
        }
        registry
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

    /// Resolves the configured provider/model selection without constructing a provider.
    pub fn resolve_selection_from_config(
        config: &MoaConfig,
        model_override: Option<&str>,
    ) -> moa_core::Result<(ProviderId, ModelId)> {
        let requested = model_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(config.models.main.as_str());

        if requested.contains('/') {
            return Err(MoaError::ConfigError(
                "vendor-prefixed model ids are not supported; use direct model ids for anthropic, openai, or google".to_string(),
            ));
        }

        if let Some((provider_id, model_id)) = split_explicit_provider(requested) {
            return Ok((provider_id, ModelId::new(model_id)));
        }

        let provider_id = match infer_provider_id(requested) {
            Some(provider_id) => provider_id,
            None => default_provider_id(config.general.default_provider.trim())?,
        };

        Ok((provider_id, ModelId::new(requested.trim())))
    }

    #[cfg(feature = "scripted-provider")]
    fn all_kinds_from_static(provider: Arc<dyn LLMProvider>) -> Self {
        let mut registry = Self::default();
        for descriptor in PROVIDER_DESCRIPTORS {
            registry.register_static(descriptor.id, provider.clone());
        }
        registry
    }

    /// Resolves which provider id should serve the requested model.
    pub fn resolve_provider_id(
        &self,
        requested_model: Option<&str>,
    ) -> moa_core::Result<(ProviderId, ModelId)> {
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
        let (provider_id, model) = self.resolve_provider_id(requested_model)?;
        Ok(self
            .provider_for_id(provider_id, &model)?
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
            let (id, model) = self.resolve_provider_id(Some(model))?;
            return Ok(Some(self.provider_for_id(id, &model)?.provider));
        }

        if let Some((id, provider)) = self
            .providers
            .iter()
            .min_by_key(|(_, provider)| provider.descriptor.rewriter_priority)
        {
            let model = ModelId::new(provider.descriptor.rewriter_default_model);
            return Ok(Some(self.provider_for_id(*id, &model)?.provider));
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
        let (id, model) = self.resolve_provider_id(requested_model)?;
        Ok(self.provider_for_id(id, &model)?.provider)
    }

    /// Resolves a provider instance for an already-selected provider id and model.
    pub fn provider_for_id(
        &self,
        id: ProviderId,
        model: &ModelId,
    ) -> moa_core::Result<ResolvedProvider> {
        let cache_key = ProviderCacheKey {
            id,
            model: model.clone(),
        };
        if let Some(resolved) = self.cached_provider(&cache_key) {
            return Ok(resolved);
        }

        let provider = self
            .provider_entry(id)
            .ok_or_else(|| {
                MoaError::ConfigError(format!("{} provider is not configured", id.as_str()))
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
    ) -> moa_core::Result<(ProviderId, ModelId)> {
        let trimmed = requested_model.trim();
        if trimmed.is_empty() {
            return self.resolve_default_model();
        }

        if let Some((provider_id, model_id)) = split_explicit_provider(trimmed) {
            self.provider_entry(provider_id).ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "{} provider is not configured",
                    provider_id.as_str()
                ))
            })?;
            return Ok((provider_id, ModelId::new(model_id)));
        }

        if let Some(provider_id) = self.provider_id_for_default_model(trimmed) {
            return Ok((provider_id, ModelId::new(trimmed)));
        }

        let provider_id = infer_provider_id(trimmed).ok_or_else(|| {
            MoaError::ConfigError(format!(
                "could not infer a configured provider for model `{trimmed}`"
            ))
        })?;
        self.provider_entry(provider_id).ok_or_else(|| {
            MoaError::ConfigError(format!(
                "{} provider is not configured",
                provider_id.as_str()
            ))
        })?;

        Ok((provider_id, ModelId::new(trimmed)))
    }

    fn provider_id_for_default_model(&self, model: &str) -> Option<ProviderId> {
        self.providers
            .iter()
            .find(|(_, provider)| provider.default_model == model)
            .map(|(id, _)| *id)
    }

    fn resolve_default_model(&self) -> moa_core::Result<(ProviderId, ModelId)> {
        if let Some((id, provider)) = self
            .providers
            .iter()
            .min_by_key(|(_, provider)| provider.descriptor.default_priority)
        {
            return Ok((*id, provider.default_model()));
        }

        Err(MoaError::ConfigError(
            "LLMGateway has no configured providers".to_string(),
        ))
    }

    fn provider_entry(&self, id: ProviderId) -> Option<&RegisteredProvider> {
        self.providers.get(&id)
    }

    fn register_factory(
        &mut self,
        descriptor: &'static ProviderDescriptor,
        factory: ProviderFactory,
    ) {
        self.providers.insert(
            descriptor.id,
            RegisteredProvider::from_factory(descriptor, factory),
        );
    }

    fn register_static(&mut self, id: ProviderId, provider: Arc<dyn LLMProvider>) {
        let descriptor = id.descriptor();
        self.providers
            .insert(id, RegisteredProvider::from_static(descriptor, provider));
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

fn configured_secret(value: &str) -> bool {
    !value.trim().is_empty()
}

fn default_provider_id(provider_name: &str) -> moa_core::Result<ProviderId> {
    provider_descriptor_by_name(provider_name)
        .map(|descriptor| descriptor.id)
        .ok_or_else(|| MoaError::ConfigError(format!("unsupported provider '{provider_name}'")))
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use moa_core::{
        CompletionRequest, CompletionResponse, CompletionStream, LLMProvider, ModelCapabilities,
        ModelId, QueryRewriteConfig, StopReason, TokenPricing, TokenUsage, ToolCallFormat,
    };

    use super::{ProviderFactory, ProviderId, ProviderRegistry};

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

    fn model_factory(builds: Arc<AtomicUsize>) -> ProviderFactory {
        Arc::new(move |model| {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(CacheTestProvider {
                model: model.to_string(),
            }))
        })
    }

    #[test]
    fn from_config_uses_configured_api_key() {
        // Pins: provider registry availability follows direct MoaConfig provider API keys.
        let mut config = moa_core::MoaConfig::default();
        config.providers.openai.api_key = "test-key".to_string();

        let registry = ProviderRegistry::from_config(&config);
        let (id, model) = registry
            .resolve_provider_id(Some("openai:gpt-5.4-mini"))
            .expect("configured custom OpenAI env should enable provider");

        assert_eq!(id, ProviderId::OpenAI);
        assert_eq!(model, ModelId::new("gpt-5.4-mini"));
    }

    #[test]
    fn default_model_resolution_prefers_configured_openai_for_main_loop() {
        // Pins: when several families are configured, main-loop routing uses provider priority,
        // not BTreeMap insertion order or a hard-coded provider slot.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            ProviderId::Anthropic.descriptor(),
            model_factory(builds.clone()),
        );
        registry.register_factory(
            ProviderId::OpenAI.descriptor(),
            model_factory(builds.clone()),
        );
        registry.register_factory(
            ProviderId::Google.descriptor(),
            model_factory(builds.clone()),
        );

        let provider = registry
            .provider_for_model(None)
            .expect("default provider should resolve");

        assert_eq!(provider.capabilities().model_id, ModelId::new("gpt-5.4"));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_registry_reuses_factory_provider_for_same_model() {
        // Pins: repeated provider resolution does not rebuild factory-backed clients.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            ProviderId::OpenAI.descriptor(),
            model_factory(builds.clone()),
        );

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
    fn rewriter_resolution_prefers_anthropic_small_model_when_available() {
        // Pins: query rewrite selects by rewriter priority and builds the provider's
        // rewriter model, not the main-loop default model.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            ProviderId::OpenAI.descriptor(),
            model_factory(builds.clone()),
        );
        registry.register_factory(
            ProviderId::Anthropic.descriptor(),
            model_factory(builds.clone()),
        );

        let provider = registry
            .resolve_rewriter_provider(&QueryRewriteConfig {
                enabled: true,
                model: None,
                ..QueryRewriteConfig::default()
            })
            .expect("rewriter resolution should succeed")
            .expect("enabled query rewrite should return a provider");

        assert_eq!(
            provider.capabilities().model_id,
            ModelId::new("claude-haiku-4-5")
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn resolves_requested_model_by_family_and_explicit_prefix() {
        // Pins: provider routing accepts family inference and explicit provider prefixes.
        let registry = ProviderRegistry::with_static_providers(
            Some(provider("claude-sonnet-4-6")),
            Some(provider("gpt-5.4")),
            Some(provider("gemini-3-flash-preview")),
        );

        let (id, model) = registry
            .resolve_provider_id(Some("claude-sonnet-4-6"))
            .expect("claude model should resolve");
        assert_eq!(id, ProviderId::Anthropic);
        assert_eq!(model, ModelId::new("claude-sonnet-4-6"));

        let (id, model) = registry
            .resolve_provider_id(Some("gpt-5.4"))
            .expect("gpt model should resolve");
        assert_eq!(id, ProviderId::OpenAI);
        assert_eq!(model, ModelId::new("gpt-5.4"));

        let (id, model) = registry
            .resolve_provider_id(Some("google:gemini-3-flash-preview"))
            .expect("prefixed google model should resolve");
        assert_eq!(id, ProviderId::Google);
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

        let (id, model) = registry
            .resolve_provider_id(Some("scripted-loadtest"))
            .expect("configured static default model should resolve");

        assert_eq!(id, ProviderId::OpenAI);
        assert_eq!(model, ModelId::new("scripted-loadtest"));
    }

    #[test]
    fn missing_provider_returns_configuration_error() {
        // Pins: inferred model families still require that provider family to be configured.
        let registry =
            ProviderRegistry::with_static_providers(None, Some(provider("gpt-5.4")), None);

        let error = registry
            .resolve_provider_id(Some("claude-sonnet-4-6"))
            .expect_err("unconfigured Anthropic provider should fail");

        assert!(
            error
                .to_string()
                .contains("anthropic provider is not configured"),
            "unexpected error: {error}"
        );
    }
}
