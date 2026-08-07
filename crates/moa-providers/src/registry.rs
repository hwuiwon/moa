//! Runtime registry for configured LLM provider families.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};

use moa_config::MoaConfig;
use moa_config::QueryRewriteConfig;
use moa_core::types::provider::ProviderId;
use moa_core::{
    error::MoaError,
    traits::{LLMProvider, RuntimeCacheStore},
    types::identifiers::ModelId,
    types::model::ModelCapabilities,
    types::provider::ModelTask,
};
#[cfg(feature = "scripted-provider")]
use moa_core::{
    types::completion::CompletionContent, types::completion::StopReason,
    types::completion::ToolCallContent, types::completion::ToolInvocation,
};
#[cfg(feature = "scripted-provider")]
use moa_core::{
    types::model::TokenPricing, types::model::ToolCallFormat as ScriptedToolCallFormat,
};
#[cfg(feature = "scripted-provider")]
use serde::Deserialize;
#[cfg(feature = "scripted-provider")]
use serde_json::Value;

use moa_memory_pii::PiiClassifier;

use crate::ModelRouter;
use crate::core::concurrency_factory::ProviderCoordination;
use crate::core::models::find_model;
use crate::governance::{CachingPiiClassifier, GovernedLLMProvider};
use crate::provider_policy::{
    DeploymentProviderPolicy, ProviderCapabilities, provider_capabilities,
};
use crate::routing::{
    PROVIDER_DESCRIPTORS, ProviderDescriptor, build_provider_with_coordination, infer_provider_id,
    provider_descriptor, provider_descriptor_by_name, split_explicit_provider,
};
#[cfg(feature = "scripted-provider")]
use crate::{ScriptedBlock, ScriptedFault, ScriptedProvider, ScriptedResponse};

#[cfg(feature = "scripted-provider")]
const SCRIPTED_PROVIDER_REQUEST_LOG_ENV: &str = "MOA_SCRIPTED_PROVIDER_REQUEST_LOG";

#[derive(Clone)]
enum ProviderSource {
    Static(Arc<dyn LLMProvider>),
    Factory(ProviderFactory),
}

type ProviderFactory =
    Arc<dyn Fn(&str) -> moa_core::error::Result<Arc<dyn LLMProvider>> + Send + Sync>;
type ProviderCache = Arc<Mutex<ProviderCacheState>>;

/// Maximum number of inactive provider/model instances retained by one registry.
///
/// The cache is process-local and providers may be selected by extensible model
/// ids, so a fixed capacity bounds credential-backed client retention without
/// imposing a model catalog on routing. Overflow identity is tracked weakly
/// while callers remain active, so it cannot retain an evicted client by itself.
const PROVIDER_CACHE_CAPACITY: usize = 128;

/// Maximum number of live raw clients tracked weakly beyond the strong cache.
///
/// A weak entry does not retain a provider, but bounding the identity index keeps
/// adversarial model-id churn from growing process metadata without limit.
const PROVIDER_OVERFLOW_CAPACITY: usize = 128;

/// Maximum weak entries inspected during one cache miss under overflow pressure.
const PROVIDER_OVERFLOW_RECLAIM_BATCH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderCacheKey {
    id: ProviderId,
    model: ModelId,
}

/// Explicit bounded LRU state for resolved provider instances.
#[derive(Default)]
struct ProviderCacheState {
    entries: HashMap<ProviderCacheKey, ResolvedProvider>,
    recency: VecDeque<ProviderCacheKey>,
    /// Live providers that could not enter the strong cache because every
    /// retained entry was active.
    overflow: HashMap<ProviderCacheKey, Weak<dyn LLMProvider>>,
    /// Least-to-most-recently accessed overflow identities.
    overflow_recency: VecDeque<ProviderCacheKey>,
    /// Next overflow identity considered by bounded dead-entry cleanup.
    overflow_cleanup_cursor: usize,
}

impl ProviderCacheState {
    fn get(&mut self, key: &ProviderCacheKey) -> Option<ResolvedProvider> {
        let provider = self.entries.get(key).cloned();
        if provider.is_some() {
            self.touch(key);
            return provider;
        }

        let provider = self.overflow.get(key).and_then(Weak::upgrade);
        match provider {
            Some(provider) => {
                self.touch_overflow(key);
                Some(ResolvedProvider {
                    provider,
                    model: key.model.clone(),
                })
            }
            None => {
                if self.overflow.contains_key(key) {
                    self.remove_overflow(key);
                }
                None
            }
        }
    }

    /// Verifies that a previously unseen raw key can be admitted before its
    /// provider client is constructed.
    fn admit_new_key(&mut self) -> moa_core::error::Result<()> {
        if self.entries.len() < PROVIDER_CACHE_CAPACITY
            || self
                .entries
                .values()
                .any(|resolved| Arc::strong_count(&resolved.provider) == 1)
        {
            return Ok(());
        }

        if self.overflow.len() >= PROVIDER_OVERFLOW_CAPACITY {
            self.reclaim_dead_overflow(PROVIDER_OVERFLOW_RECLAIM_BATCH);
        }
        if self.overflow.len() < PROVIDER_OVERFLOW_CAPACITY {
            return Ok(());
        }

        Err(provider_cache_saturated())
    }

    fn insert_or_get(
        &mut self,
        key: ProviderCacheKey,
        provider: ResolvedProvider,
    ) -> moa_core::error::Result<ResolvedProvider> {
        if let Some(existing) = self.get(&key) {
            return Ok(existing);
        }

        if self.entries.len() >= PROVIDER_CACHE_CAPACITY && !self.evict_inactive() {
            if self.overflow.len() >= PROVIDER_OVERFLOW_CAPACITY {
                self.reclaim_dead_overflow(PROVIDER_OVERFLOW_RECLAIM_BATCH);
            }
            if self.overflow.len() >= PROVIDER_OVERFLOW_CAPACITY {
                return Err(provider_cache_saturated());
            }

            // Every strongly retained provider is active. Track this raw client
            // weakly so repeated overflow lookups preserve its pacing, cooldown,
            // and retry identity without retaining it after the caller drops.
            self.overflow_recency.push_back(key.clone());
            self.overflow
                .insert(key, Arc::downgrade(&provider.provider));
            return Ok(provider);
        }

        self.recency.push_back(key.clone());
        let returned = provider.clone();
        self.entries.insert(key, provider);
        Ok(returned)
    }

    fn reclaim_dead_overflow(&mut self, budget: usize) {
        let inspections = budget.min(self.overflow_recency.len());
        for _ in 0..inspections {
            if self.overflow_recency.is_empty() {
                self.overflow_cleanup_cursor = 0;
                return;
            }
            if self.overflow_cleanup_cursor >= self.overflow_recency.len() {
                self.overflow_cleanup_cursor = 0;
            }

            let Some(key) = self
                .overflow_recency
                .get(self.overflow_cleanup_cursor)
                .cloned()
            else {
                return;
            };
            let dead = self
                .overflow
                .get(&key)
                .is_none_or(|provider| provider.strong_count() == 0);
            if dead {
                self.overflow.remove(&key);
                self.overflow_recency.remove(self.overflow_cleanup_cursor);
            } else {
                self.overflow_cleanup_cursor += 1;
            }
        }
        if self.overflow_cleanup_cursor >= self.overflow_recency.len() {
            self.overflow_cleanup_cursor = 0;
        }
    }

    fn touch_overflow(&mut self, key: &ProviderCacheKey) {
        let Some(position) = self
            .overflow_recency
            .iter()
            .position(|candidate| candidate == key)
        else {
            return;
        };
        let Some(key) = self.overflow_recency.remove(position) else {
            return;
        };
        self.adjust_overflow_cursor_after_remove(position);
        self.overflow_recency.push_back(key);
    }

    fn remove_overflow(&mut self, key: &ProviderCacheKey) {
        self.overflow.remove(key);
        if let Some(position) = self
            .overflow_recency
            .iter()
            .position(|candidate| candidate == key)
        {
            self.overflow_recency.remove(position);
            self.adjust_overflow_cursor_after_remove(position);
        }
    }

    fn adjust_overflow_cursor_after_remove(&mut self, position: usize) {
        if position < self.overflow_cleanup_cursor {
            self.overflow_cleanup_cursor -= 1;
        }
        if self.overflow_cleanup_cursor >= self.overflow_recency.len() {
            self.overflow_cleanup_cursor = 0;
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.recency.clear();
        self.overflow.clear();
        self.overflow_recency.clear();
        self.overflow_cleanup_cursor = 0;
    }

    fn touch(&mut self, key: &ProviderCacheKey) {
        if let Some(position) = self.recency.iter().position(|candidate| candidate == key) {
            self.recency.remove(position);
        }
        self.recency.push_back(key.clone());
    }

    fn evict_inactive(&mut self) -> bool {
        let Some(position) = self.recency.iter().position(|candidate| {
            self.entries
                .get(candidate)
                .is_some_and(|resolved| Arc::strong_count(&resolved.provider) == 1)
        }) else {
            return false;
        };
        let Some(key) = self.recency.remove(position) else {
            return false;
        };
        self.entries.remove(&key).is_some()
    }
}

fn provider_cache_saturated() -> MoaError {
    MoaError::RateLimited {
        retries: 0,
        message: format!(
            "raw provider cache saturated with {PROVIDER_CACHE_CAPACITY} strongly retained and \
             {PROVIDER_OVERFLOW_CAPACITY} weakly indexed live clients"
        ),
    }
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

    fn build(&self, model: &str) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
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

/// Egress DLP governance attached to a registry.
///
/// When present, every provider the registry hands out is wrapped with a
/// [`GovernedLLMProvider`] so restricted spans are tokenized before egress and
/// detokenized on the response. Cloneable so the registry stays `Clone`.
#[derive(Clone)]
struct LlmDlpState {
    /// Detector that produces the spans fed to the tokenizer. Wrapped in a
    /// [`CachingPiiClassifier`] so frozen history is classified once, and shared
    /// across every resolved provider so the cache persists across turns.
    classifier: Arc<dyn PiiClassifier>,
}

/// Deployment-wide provider-routing state attached to a registry.
///
/// Bundles the active policy with the effective capabilities of each configured
/// provider so the single routing gate ([`ProviderRegistry::provider_for_id`])
/// can decide compliance without consulting config again. `None` on the registry
/// means no deployment policy (a cheap early return, unchanged routing).
#[derive(Clone)]
struct DeploymentPolicyState {
    /// The deployment-wide requirement every routed provider must satisfy.
    policy: DeploymentProviderPolicy,
    /// Effective capabilities per configured provider (conservative when unset).
    capabilities: BTreeMap<ProviderId, ProviderCapabilities>,
}

/// Runtime registry for configured provider families.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<ProviderId, RegisteredProvider>,
    provider_cache: ProviderCache,
    /// Optional egress DLP governance applied to every resolved provider.
    llm_dlp: Option<LlmDlpState>,
    /// Optional deployment provider routing policy. When set (active), the single
    /// routing gate fails closed before building a non-compliant provider.
    /// `None` = unchanged routing, zero overhead.
    deployment_policy: Option<DeploymentPolicyState>,
}

impl ProviderRegistry {
    /// Builds a registry from configured provider API keys and an optional
    /// runtime coordination cache.
    ///
    /// Per-provider deployment capabilities are read from each provider's
    /// credential config (conservative by default), and an active
    /// `[providers.routing_policy]` policy is attached so routing is constrained to
    /// compliant providers and fails closed. An inactive policy leaves routing
    /// unchanged with zero overhead. Pass `None` only for deliberately local
    /// construction such as isolated tests and eval tools.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when global provider coordination is
    /// configured without a usable runtime cache.
    pub fn from_config(
        config: &MoaConfig,
        runtime_cache: Option<Arc<dyn RuntimeCacheStore>>,
    ) -> moa_core::error::Result<Self> {
        let coordination = ProviderCoordination::from_config(config, runtime_cache)?;
        let config = Arc::new(config.clone());
        let mut registry = Self::default();
        let mut capabilities = BTreeMap::new();
        for descriptor in PROVIDER_DESCRIPTORS {
            if configured_secret((descriptor.api_key)(&config)) {
                capabilities.insert(descriptor.id, provider_capabilities(&config, descriptor.id));
                let build_config = Arc::clone(&config);
                let coordination = coordination.clone();
                registry.register_factory(
                    descriptor,
                    Arc::new(move |model| {
                        build_provider_with_coordination(
                            descriptor.id,
                            &build_config,
                            &coordination,
                            model,
                        )
                    }),
                );
            }
        }

        let policy = DeploymentProviderPolicy::from_config(&config.providers.routing_policy);
        if policy.is_active() {
            tracing::info!(
                "deployment provider routing policy active; routing to compliant providers only"
            );
            registry.deployment_policy = Some(DeploymentPolicyState {
                policy,
                capabilities,
            });
        }
        Ok(registry)
    }

    /// Attaches egress DLP governance so every provider this registry resolves is
    /// wrapped with a [`GovernedLLMProvider`].
    ///
    /// This is the governed-provider composition point: the runtime calls it after building
    /// the registry when `[llm_dlp].tokenize_enabled` is set. When it is not called,
    /// providers are resolved directly with zero added overhead. Resolved clients stay
    /// unwrapped in the cache; governance is applied at each public provider boundary so
    /// a main failover chain can place one governor around the whole chain.
    #[must_use]
    pub fn with_llm_dlp(
        mut self,
        classifier: Arc<dyn PiiClassifier>,
        classifier_namespace: impl Into<String>,
        classifier_model: impl Into<String>,
    ) -> Self {
        // Memoize the classifier once here so frozen history is classified a
        // single time; the shared wrapper threads across every resolved provider
        // so the cache persists across turns.
        let classifier = Arc::new(CachingPiiClassifier::new(
            classifier,
            classifier_namespace,
            classifier_model,
        )) as Arc<dyn PiiClassifier>;
        self.llm_dlp = Some(LlmDlpState { classifier });
        // Clear resolution state so every subsequent boundary applies the newly
        // attached governance wrapper.
        let mut cache = self
            .provider_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.clear();
        drop(cache);
        self
    }

    /// Wraps `provider` with egress governance when it is configured, otherwise
    /// returns it unchanged (zero overhead).
    fn govern(&self, provider: Arc<dyn LLMProvider>) -> Arc<dyn LLMProvider> {
        match &self.llm_dlp {
            Some(governor) => Arc::new(GovernedLLMProvider::new(
                provider,
                governor.classifier.clone(),
            )),
            None => provider,
        }
    }

    /// Attaches a deployment-wide provider-routing policy so routing is constrained
    /// to compliant providers and fails closed rather than falling back to a
    /// non-compliant one.
    ///
    /// This is the composition point for constraining a restricted tenant to
    /// zero-retention / private / self-hosted providers. When the policy is
    /// inactive, or this is never called, routing is unchanged with zero
    /// overhead. `[providers.routing_policy]` config is applied automatically by
    /// [`from_config`](Self::from_config); this builder is for callers that
    /// resolve a per-deployment policy themselves (or in tests). Providers start at
    /// the conservative capability baseline; assert compliance with
    /// [`with_provider_capabilities`](Self::with_provider_capabilities).
    #[must_use]
    pub fn with_deployment_policy(mut self, policy: DeploymentProviderPolicy) -> Self {
        self.deployment_policy = policy.is_active().then_some(DeploymentPolicyState {
            policy,
            capabilities: BTreeMap::new(),
        });
        self
    }

    /// Asserts the effective deployment capabilities for one provider family, e.g.
    /// to mark a self-hosted/static provider compliant.
    ///
    /// [`from_config`](Self::from_config) already derives capabilities from
    /// credential config; this builder is for static/scripted registries and
    /// tests. It is a no-op when no active policy has been attached (nothing to
    /// govern).
    #[must_use]
    pub fn with_provider_capabilities(
        mut self,
        id: ProviderId,
        capabilities: ProviderCapabilities,
    ) -> Self {
        if let Some(governor) = self.deployment_policy.as_mut() {
            governor.capabilities.insert(id, capabilities);
        }
        self
    }

    /// The single routing gate: fails closed if an active deployment policy
    /// excludes `id`.
    ///
    /// Every path that builds a concrete provider — main, auxiliary, rewriter, and
    /// each failover fallback — passes through
    /// [`provider_for_id`](Self::provider_for_id), which calls this before doing
    /// any work, so a non-compliant provider can never be constructed or handed
    /// out. Returns `Ok(())` when there is no active policy or the provider is
    /// compliant.
    fn enforce_deployment_policy(&self, id: ProviderId) -> moa_core::error::Result<()> {
        let Some(governor) = &self.deployment_policy else {
            return Ok(());
        };
        // Defer to the existing "provider is not configured" path when the family
        // is absent; deployment policy only speaks to configured providers.
        if !self.providers.contains_key(&id) {
            return Ok(());
        }
        let capabilities = governor.capabilities.get(&id).cloned().unwrap_or_default();
        if let Err(exclusion) = governor.policy.evaluate(id, &capabilities) {
            tracing::warn!(
                provider = id.as_str(),
                reason = %exclusion,
                "provider excluded by deployment provider policy; failing closed",
            );
            return Err(exclusion.into());
        }
        Ok(())
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
    pub fn scripted(path: impl AsRef<std::path::Path>) -> moa_core::error::Result<Self> {
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
    pub fn mock(seed: u64) -> moa_core::error::Result<Self> {
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
                configure_scripted_request_journal(ScriptedProvider::new(scripted_capabilities(
                    "scripted-mock",
                )))
                .with_fallback_response(response),
            );
            Ok(Self::all_kinds_from_static(provider))
        }
    }

    /// Resolves the configured provider/model selection without constructing a provider.
    pub fn resolve_selection_from_config(
        config: &MoaConfig,
        model_override: Option<&str>,
    ) -> moa_core::error::Result<(ProviderId, ModelId)> {
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
    ) -> moa_core::error::Result<(ProviderId, ModelId)> {
        match requested_model {
            Some(requested_model) => self.resolve_requested_model(requested_model),
            None => self.resolve_default_model(),
        }
    }

    /// Resolves model capabilities for the requested model using the configured provider family.
    pub fn capabilities_for_model(
        &self,
        requested_model: Option<&str>,
    ) -> moa_core::error::Result<ModelCapabilities> {
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
    ) -> moa_core::error::Result<Option<Arc<dyn LLMProvider>>> {
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
    ///
    /// The main-loop provider is wrapped with the configured LLM failover chain
    /// (`models.fallback_models`) so a rate-limited primary transparently fails
    /// over; the auxiliary provider is left unwrapped.
    pub fn model_router_for_config(
        &self,
        config: &MoaConfig,
    ) -> moa_core::error::Result<ModelRouter> {
        let main_model = config.model_for_task(ModelTask::MainLoop);
        let (main_provider_id, main_model_id) = self.resolve_provider_id(Some(main_model))?;
        let main = self.main_provider_with_failover(config, main_provider_id, &main_model_id)?;
        let auxiliary = config
            .models
            .auxiliary
            .as_deref()
            .map(|model| self.provider_for_model(Some(model)))
            .transpose()?;
        Ok(ModelRouter::new(main, auxiliary))
    }

    /// Builds one governed main-loop provider from a raw failover chain.
    ///
    /// Every main-provider factory and router path calls this helper. Raw cached
    /// providers form the chain first, then DLP governance is applied exactly
    /// once around the chain so failover replays one effective transformed view.
    ///
    /// A fallback is a hard config error (fails startup, so operators find out at
    /// boot rather than at failover time) when it is not in the model catalog, or
    /// when its capability tier is more than one tier from the primary's (so an
    /// operator cannot, e.g., silently fall a flagship model over to a fast one).
    /// A fallback that validates but whose provider is not configured (missing API
    /// key) is skipped with a warning rather than failing startup.
    pub(crate) fn main_provider_with_failover(
        &self,
        config: &MoaConfig,
        provider_id: ProviderId,
        primary_model: &ModelId,
    ) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
        let main = self
            .provider_for_id_raw(provider_id, primary_model)?
            .provider;
        let chain = self.build_raw_failover_chain(config, primary_model.as_str(), main)?;
        Ok(self.govern(chain))
    }

    /// Builds the failover chain exclusively from raw cached provider clients.
    fn build_raw_failover_chain(
        &self,
        config: &MoaConfig,
        primary_model: &str,
        main: Arc<dyn LLMProvider>,
    ) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
        if config.models.fallback_models.is_empty() {
            return Ok(main);
        }

        let primary_model_id = strip_provider_prefix(primary_model);
        let primary_tier = find_model(primary_model_id).map(|model| model.tier);

        let mut fallbacks = Vec::new();
        for entry in &config.models.fallback_models {
            let fallback_model_id = strip_provider_prefix(entry.trim());
            let catalog_entry = find_model(fallback_model_id).ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "LLM fallback model '{fallback_model_id}' (models.fallback_models entry '{entry}') is not in the model catalog"
                ))
            })?;

            if let Some(primary_tier) = primary_tier {
                let distance = primary_tier.distance(catalog_entry.tier);
                if distance > 1 {
                    return Err(MoaError::ConfigError(format!(
                        "LLM fallback '{fallback_model_id}' (tier {}) is {distance} capability tiers from primary '{primary_model_id}' (tier {}); configure a fallback within one tier",
                        catalog_entry.tier.as_str(),
                        primary_tier.as_str()
                    )));
                }
            }

            // Catalog/tier shape is valid; a fallback whose provider is not
            // configured (missing key) is skipped rather than failing boot.
            match self
                .resolve_provider_id(Some(entry))
                .and_then(|(id, model)| {
                    let resolved = self.provider_for_id_raw(id, &model)?;
                    Ok((resolved.provider, model))
                }) {
                Ok(pair) => fallbacks.push(pair),
                Err(error) => tracing::warn!(
                    fallback = %entry,
                    %error,
                    "skipping LLM fallback whose provider is unavailable"
                ),
            }
        }

        Ok(crate::FailoverLLMProvider::wrap(main, fallbacks))
    }

    /// Resolves the provider instance that should serve a requested model.
    pub fn provider_for_model(
        &self,
        requested_model: Option<&str>,
    ) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
        let provider = self.provider_for_model_raw(requested_model)?;
        Ok(self.govern(provider))
    }

    fn provider_for_model_raw(
        &self,
        requested_model: Option<&str>,
    ) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
        let (id, model) = self.resolve_provider_id(requested_model)?;
        Ok(self.provider_for_id_raw(id, &model)?.provider)
    }

    /// Resolves a provider instance for an already-selected provider id and model.
    pub fn provider_for_id(
        &self,
        id: ProviderId,
        model: &ModelId,
    ) -> moa_core::error::Result<ResolvedProvider> {
        let resolved = self.provider_for_id_raw(id, model)?;
        Ok(ResolvedProvider {
            provider: self.govern(resolved.provider),
            model: resolved.model,
        })
    }

    fn provider_for_id_raw(
        &self,
        id: ProviderId,
        model: &ModelId,
    ) -> moa_core::error::Result<ResolvedProvider> {
        // Fail closed before any work: an active deployment policy must never let
        // a non-compliant provider be built or cached. The cache stores raw clients
        // so a failover chain can be governed once at its outer boundary.
        self.enforce_deployment_policy(id)?;

        let cache_key = ProviderCacheKey {
            id,
            model: model.clone(),
        };
        if let Some(resolved) = self.cached_provider_or_admit(&cache_key)? {
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
        self.cache_provider(cache_key, resolved)
    }

    fn resolve_requested_model(
        &self,
        requested_model: &str,
    ) -> moa_core::error::Result<(ProviderId, ModelId)> {
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

    fn resolve_default_model(&self) -> moa_core::error::Result<(ProviderId, ModelId)> {
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
        let descriptor = provider_descriptor(id);
        self.providers
            .insert(id, RegisteredProvider::from_static(descriptor, provider));
    }

    fn cached_provider_or_admit(
        &self,
        key: &ProviderCacheKey,
    ) -> moa_core::error::Result<Option<ResolvedProvider>> {
        let mut cache = self
            .provider_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(provider) = cache.get(key) {
            return Ok(Some(provider));
        }
        cache.admit_new_key()?;
        Ok(None)
    }

    fn cache_provider(
        &self,
        key: ProviderCacheKey,
        provider: ResolvedProvider,
    ) -> moa_core::error::Result<ResolvedProvider> {
        let mut cache = match self.provider_cache.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.insert_or_get(key, provider)
    }
}

fn configured_secret(value: &str) -> bool {
    !value.trim().is_empty()
}

/// Strips a leading `provider:` prefix from a model selector, returning the model id.
fn strip_provider_prefix(model: &str) -> &str {
    model.split_once(':').map_or(model, |(_, model)| model)
}

fn default_provider_id(provider_name: &str) -> moa_core::error::Result<ProviderId> {
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
    keyed: Vec<KeyedEntry>,
}

/// One reusable request-matched scripted completion keyed by a message substring.
#[cfg(feature = "scripted-provider")]
#[derive(Debug, Deserialize)]
struct KeyedEntry {
    #[serde(rename = "match")]
    match_: String,
    completion: ScriptedCompletion,
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
    /// Simulated total call latency; the provider actually sleeps this long.
    latency_ms: Option<u64>,
    /// Simulated time-to-first-block; defaults to `latency_ms` (single burst).
    ttft_ms: Option<u64>,
    /// Optional deterministic fault plan.
    fault: Option<ScriptedFaultSpec>,
}

/// JSON fault plan for one scripted completion.
#[cfg(feature = "scripted-provider")]
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ScriptedFaultSpec {
    /// Fail the first N matching requests before succeeding.
    fail_first_n: u32,
    /// Modeled provider status (429 becomes a rate-limit error).
    status: Option<u16>,
    /// Optional retry-after hint carried in the error.
    retry_after_ms: Option<u64>,
    /// Abort every stream after the first block.
    abort_mid_stream: bool,
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
            latency_ms: None,
            ttft_ms: None,
            fault: None,
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
fn scripted_registry_from_file(
    file: ScriptedProviderFile,
) -> moa_core::error::Result<ProviderRegistry> {
    let mut provider = configure_scripted_request_journal(ScriptedProvider::new(
        scripted_capabilities("scripted-loadtest"),
    ));
    if let Some(default) = file.default {
        provider = provider.with_fallback_response(scripted_response(default)?);
    }
    for response in file.responses {
        provider = provider.push_response(scripted_response(response)?);
    }
    for entry in file.keyed {
        provider = provider.push_keyed(
            entry.match_,
            scripted_response(ScriptedEntry::Direct(entry.completion))?,
        );
    }
    Ok(ProviderRegistry::all_kinds_from_static(Arc::new(provider)))
}

#[cfg(feature = "scripted-provider")]
fn configure_scripted_request_journal(provider: ScriptedProvider) -> ScriptedProvider {
    match std::env::var_os(SCRIPTED_PROVIDER_REQUEST_LOG_ENV) {
        Some(path) if !path.is_empty() => provider.with_request_journal(path.into()),
        Some(_) | None => provider,
    }
}

#[cfg(feature = "scripted-provider")]
fn scripted_response(entry: ScriptedEntry) -> moa_core::error::Result<ScriptedResponse> {
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
    if let Some(latency_ms) = completion.latency_ms {
        let total = std::time::Duration::from_millis(latency_ms);
        let ttft = std::time::Duration::from_millis(completion.ttft_ms.unwrap_or(latency_ms));
        response = response.with_timing(ttft.min(total), total);
    }
    if let Some(fault) = completion.fault {
        let mut plan = ScriptedFault::fail_first(
            fault.fail_first_n,
            fault.status.unwrap_or(500),
            fault.retry_after_ms.map(std::time::Duration::from_millis),
        );
        plan.abort_mid_stream = fault.abort_mid_stream;
        response = response.with_fault(plan);
    }
    Ok(response)
}

#[cfg(feature = "scripted-provider")]
fn parse_scripted_stop_reason(raw: &str) -> moa_core::error::Result<StopReason> {
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
    use moa_config::QueryRewriteConfig;
    use moa_core::{
        error::MoaError, traits::LLMProvider, types::completion::CompletionResponse,
        types::completion::CompletionStream, types::completion::SharedCompletionRequest,
        types::completion::StopReason, types::completion::TokenUsage, types::identifiers::ModelId,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
    };

    use moa_config::DeploymentProviderPolicyConfig;

    use super::{ProviderFactory, ProviderId, ProviderRegistry, provider_descriptor};
    use crate::provider_policy::{DeploymentProviderPolicy, ProviderCapabilities};

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
            _request: SharedCompletionRequest,
        ) -> moa_core::error::Result<CompletionStream> {
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

    fn cache_len(registry: &ProviderRegistry) -> usize {
        registry
            .provider_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .len()
    }

    fn overflow_len(registry: &ProviderRegistry) -> usize {
        registry
            .provider_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .overflow
            .len()
    }

    fn overflow_recency_len(registry: &ProviderRegistry) -> usize {
        registry
            .provider_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .overflow_recency
            .len()
    }

    fn fill_live_strong_cache(
        registry: &ProviderRegistry,
        prefix: &str,
    ) -> Vec<Arc<dyn LLMProvider>> {
        (0..super::PROVIDER_CACHE_CAPACITY)
            .map(|index| {
                registry
                    .provider_for_model(Some(&format!("gpt-{prefix}-strong-{index}")))
                    .expect("strong-cache fixture should resolve")
            })
            .collect()
    }

    #[test]
    fn from_config_uses_configured_api_key() {
        // Pins: provider registry availability follows direct MoaConfig provider API keys.
        let mut config = moa_config::MoaConfig::default();
        config.providers.openai.api_key = "test-key".to_string();

        let registry = ProviderRegistry::from_config(&config, None)
            .expect("local provider registry should build");
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
            provider_descriptor(ProviderId::Anthropic),
            model_factory(builds.clone()),
        );
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds.clone()),
        );
        registry.register_factory(
            provider_descriptor(ProviderId::Google),
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
            provider_descriptor(ProviderId::OpenAI),
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
    fn provider_registry_reuses_hot_model_concurrently() {
        // Pins: a hot model remains one shared client for concurrent registry
        // callers, even though provider construction itself is synchronous.
        const CALLERS: usize = 8;
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds.clone()),
        );
        let hot = registry
            .provider_for_model(Some("gpt-hot-concurrent"))
            .expect("the hot model should build once");
        let registry = Arc::new(registry);
        let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
        let mut handles = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                registry
                    .provider_for_model(Some("gpt-hot-concurrent"))
                    .expect("the hot model should remain routable")
            }));
        }

        for handle in handles {
            let resolved = handle
                .join()
                .expect("concurrent provider lookup should not panic");
            assert!(
                Arc::ptr_eq(&hot, &resolved),
                "hot-key lookups must reuse the cached provider instance"
            );
        }
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "a cached hot model must not be rebuilt by concurrent readers"
        );
    }

    #[test]
    fn provider_cache_reclaims_inactive_models_and_keeps_active_clients() {
        // Pins: model-id extensibility does not create an unbounded cache, while
        // an active provider Arc prevents its cache entry from being evicted.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds.clone()),
        );

        let active = registry
            .provider_for_model(Some("gpt-active-cache-client"))
            .expect("the active model should build");
        let inactive = registry
            .provider_for_model(Some("gpt-inactive-cache-client"))
            .expect("the inactive model should build");
        drop(inactive);

        for index in 0..super::PROVIDER_CACHE_CAPACITY {
            let model = format!("gpt-extensible-{index}");
            let _ = registry
                .provider_for_model(Some(&model))
                .expect("extensible OpenAI model ids must remain routable");
        }
        assert!(
            cache_len(&registry) <= super::PROVIDER_CACHE_CAPACITY,
            "provider cache must stay within its configured capacity"
        );

        let active_again = registry
            .provider_for_model(Some("gpt-active-cache-client"))
            .expect("the active model must remain routable");
        assert!(
            Arc::ptr_eq(&active, &active_again),
            "an active provider Arc must not be evicted from the cache"
        );

        let builds_before_reclaimed_lookup = builds.load(Ordering::SeqCst);
        let _recreated = registry
            .provider_for_model(Some("gpt-inactive-cache-client"))
            .expect("an evicted inactive model must remain routable");
        assert!(
            builds.load(Ordering::SeqCst) > builds_before_reclaimed_lookup,
            "an inactive model evicted by capacity must be rebuilt on a later lookup"
        );
    }

    #[test]
    fn full_cache_overflow_reuses_one_live_client_and_keeps_strong_cache_bounded() {
        // Pins: when every bounded-cache entry has an active caller, repeated
        // and concurrent lookups of one overflow model share its live client
        // and pacing identity without increasing strong cache retention.
        const CALLERS: usize = 8;
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds.clone()),
        );
        let mut active = Vec::with_capacity(super::PROVIDER_CACHE_CAPACITY);
        for index in 0..super::PROVIDER_CACHE_CAPACITY {
            active.push(
                registry
                    .provider_for_model(Some(&format!("gpt-active-overflow-{index}")))
                    .expect("each active model should resolve"),
            );
        }
        assert_eq!(cache_len(&registry), super::PROVIDER_CACHE_CAPACITY);

        let overflow = registry
            .provider_for_model(Some("gpt-overflow-shared"))
            .expect("overflow model should resolve");
        let builds_after_overflow = builds.load(Ordering::SeqCst);
        let registry = Arc::new(registry);
        let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
        let mut handles = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                registry
                    .provider_for_model(Some("gpt-overflow-shared"))
                    .expect("concurrent overflow lookup should resolve")
            }));
        }
        for handle in handles {
            let resolved = handle
                .join()
                .expect("overflow lookup thread should not panic");
            assert!(
                Arc::ptr_eq(&overflow, &resolved),
                "all live overflow lookups must share one client identity"
            );
        }
        assert_eq!(builds.load(Ordering::SeqCst), builds_after_overflow);
        assert_eq!(cache_len(&registry), super::PROVIDER_CACHE_CAPACITY);
        assert_eq!(overflow_len(&registry), 1);

        drop(overflow);
        let builds_before_recreate = builds.load(Ordering::SeqCst);
        let recreated = registry
            .provider_for_model(Some("gpt-overflow-shared"))
            .expect("expired weak overflow entry should be rebuilt");
        assert_eq!(
            builds.load(Ordering::SeqCst),
            builds_before_recreate + 1,
            "a dead overflow identity must not retain its client"
        );
        assert_eq!(cache_len(&registry), super::PROVIDER_CACHE_CAPACITY);
        assert_eq!(overflow_len(&registry), 1);
        drop(recreated);
        drop(active);
    }

    #[test]
    fn strong_cache_hit_does_not_reclaim_dead_overflow() {
        // Pins: the hot strong-cache path never pays for weak-overflow cleanup;
        // dead weak metadata is reclaimed only by a weak hit or a pressured miss.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds),
        );
        let active = fill_live_strong_cache(&registry, "hot-hit");
        let overflow = registry
            .provider_for_model(Some("gpt-hot-hit-dead-overflow"))
            .expect("overflow fixture should resolve");
        drop(overflow);
        assert_eq!(overflow_len(&registry), 1);

        let hot_again = registry
            .provider_for_model(Some("gpt-hot-hit-strong-0"))
            .expect("hot strong key should resolve");
        assert!(Arc::ptr_eq(&active[0], &hot_again));
        assert_eq!(
            overflow_len(&registry),
            1,
            "a strong hit must leave weak-overflow cleanup untouched"
        );
        assert_eq!(overflow_recency_len(&registry), 1);
    }

    #[test]
    fn saturated_live_overflow_returns_typed_error_and_preserves_identity() {
        // Pins: fully live strong and weak capacities reject a new raw key with
        // a typed transient error, while an existing overflow key keeps exactly
        // the same provider identity and is never evicted under pressure.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds.clone()),
        );
        let strong = fill_live_strong_cache(&registry, "saturated");
        let overflow: Vec<_> = (0..super::PROVIDER_OVERFLOW_CAPACITY)
            .map(|index| {
                registry
                    .provider_for_model(Some(&format!("gpt-saturated-overflow-{index}")))
                    .expect("live overflow fixture should resolve")
            })
            .collect();
        assert_eq!(cache_len(&registry), super::PROVIDER_CACHE_CAPACITY);
        assert_eq!(overflow_len(&registry), super::PROVIDER_OVERFLOW_CAPACITY);
        let builds_at_capacity = builds.load(Ordering::SeqCst);

        let Err(error) = registry.provider_for_model(Some("gpt-saturated-new-key")) else {
            panic!("a new raw key must fail while both capacities are fully live");
        };
        assert!(
            matches!(
                error,
                MoaError::RateLimited { retries: 0, ref message }
                    if message.contains("raw provider cache saturated")
            ),
            "saturation must use the typed transient provider admission error: {error}"
        );
        assert_eq!(
            builds.load(Ordering::SeqCst),
            builds_at_capacity,
            "known-live saturation must fail before constructing another raw client"
        );

        let existing = registry
            .provider_for_model(Some("gpt-saturated-overflow-0"))
            .expect("an existing overflow identity remains routable at capacity");
        assert!(
            Arc::ptr_eq(&overflow[0], &existing),
            "capacity pressure must not replace a live weak raw identity"
        );
        assert_eq!(overflow_len(&registry), super::PROVIDER_OVERFLOW_CAPACITY);
        assert_eq!(
            overflow_recency_len(&registry),
            super::PROVIDER_OVERFLOW_CAPACITY
        );
        drop(existing);
        drop(overflow);
        drop(strong);
    }

    #[test]
    fn pressured_overflow_reclaims_dead_entries_incrementally() {
        // Pins: one pressured miss performs only the fixed cleanup batch, admits
        // the new key, and leaves the remaining dead weak metadata for later
        // amortized passes instead of scanning the complete overflow map.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds),
        );
        let strong = fill_live_strong_cache(&registry, "incremental");
        let overflow: Vec<_> = (0..super::PROVIDER_OVERFLOW_CAPACITY)
            .map(|index| {
                registry
                    .provider_for_model(Some(&format!("gpt-incremental-overflow-{index}")))
                    .expect("overflow fixture should resolve")
            })
            .collect();
        drop(overflow);
        assert_eq!(overflow_len(&registry), super::PROVIDER_OVERFLOW_CAPACITY);

        let admitted = registry
            .provider_for_model(Some("gpt-incremental-admitted"))
            .expect("bounded dead-entry cleanup should make room");
        let expected =
            super::PROVIDER_OVERFLOW_CAPACITY - super::PROVIDER_OVERFLOW_RECLAIM_BATCH + 1;
        assert_eq!(
            overflow_len(&registry),
            expected,
            "one miss must reclaim exactly one bounded batch before admission"
        );
        assert_eq!(overflow_recency_len(&registry), expected);
        assert!(
            overflow_len(&registry) > 1,
            "incremental cleanup must leave uninspected dead entries for later misses"
        );
        drop(admitted);
        drop(strong);
    }

    #[test]
    fn rewriter_resolution_prefers_openai_nano_model_when_available() {
        // Pins: query rewrite selects by rewriter priority and builds the provider's
        // rewriter model, not the main-loop default model.
        let builds = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::default();
        registry.register_factory(
            provider_descriptor(ProviderId::OpenAI),
            model_factory(builds.clone()),
        );
        registry.register_factory(
            provider_descriptor(ProviderId::Anthropic),
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
            ModelId::new("gpt-5.4-nano")
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
    fn failover_accepts_adjacent_tier_fallback() {
        // Pins: a within-one-tier fallback (fable-5 Frontier → opus-4-8 Flagship)
        // is accepted and wraps the primary provider.
        let mut config = moa_config::MoaConfig::default();
        config.models.main = "claude-fable-5".to_string();
        config.models.fallback_models = vec!["claude-opus-4-8".to_string()];
        let main = provider("claude-fable-5");
        let registry = ProviderRegistry::with_static_providers(
            Some(main.clone()),
            Some(provider("gpt-5.4")),
            None,
        );

        let wrapped = registry
            .main_provider_with_failover(
                &config,
                ProviderId::Anthropic,
                &ModelId::new("claude-fable-5"),
            )
            .expect("an adjacent-tier fallback should be accepted");

        assert!(
            !Arc::ptr_eq(&wrapped, &main),
            "an accepted fallback should wrap the primary in a failover provider"
        );
    }

    #[test]
    fn failover_rejects_two_tier_gap_naming_both_tiers() {
        // Pins: a fallback more than one tier from the primary (gpt-5.4 Flagship →
        // claude-haiku-4-5 Fast) is a hard config error at build time.
        let mut config = moa_config::MoaConfig::default();
        config.models.main = "gpt-5.4".to_string();
        config.models.fallback_models = vec!["claude-haiku-4-5".to_string()];
        let registry = ProviderRegistry::with_static_providers(
            Some(provider("claude-fable-5")),
            Some(provider("gpt-5.4")),
            None,
        );

        let Err(error) = registry.main_provider_with_failover(
            &config,
            ProviderId::OpenAI,
            &ModelId::new("gpt-5.4"),
        ) else {
            panic!("a two-tier fallback gap must be rejected");
        };
        let message = error.to_string();

        assert!(
            message.contains("flagship"),
            "names the primary tier: {message}"
        );
        assert!(
            message.contains("fast"),
            "names the fallback tier: {message}"
        );
        assert!(
            message.contains("claude-haiku-4-5"),
            "names the rejected fallback model: {message}"
        );
    }

    #[test]
    fn failover_rejects_fallback_absent_from_catalog() {
        // Pins: a fallback model that is not catalogued is a hard config error.
        let mut config = moa_config::MoaConfig::default();
        config.models.main = "gpt-5.4".to_string();
        config.models.fallback_models = vec!["claude-imaginary-9".to_string()];
        let registry =
            ProviderRegistry::with_static_providers(None, Some(provider("gpt-5.4")), None);

        let Err(error) = registry.main_provider_with_failover(
            &config,
            ProviderId::OpenAI,
            &ModelId::new("gpt-5.4"),
        ) else {
            panic!("an uncatalogued fallback must be rejected");
        };

        assert!(
            error.to_string().contains("not in the model catalog"),
            "{error}"
        );
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

    /// Effective capabilities marking a provider as zero-retention compliant.
    fn zero_retention() -> ProviderCapabilities {
        ProviderCapabilities {
            zero_retention: true,
            ..ProviderCapabilities::default()
        }
    }

    /// A policy that requires zero-retention providers.
    fn require_zero_retention_policy() -> DeploymentProviderPolicy {
        DeploymentProviderPolicy::from_config(&DeploymentProviderPolicyConfig {
            require_zero_retention: true,
            ..DeploymentProviderPolicyConfig::default()
        })
    }

    #[test]
    fn deployment_policy_routes_a_compliant_provider_and_blocks_the_rest() {
        // Pins: under a zero-retention policy the single routing gate lets the
        // compliant provider through and fails closed for the non-compliant one —
        // a compliant provider is reachable when one exists, the other is blocked.
        let registry = ProviderRegistry::with_static_providers(
            Some(provider("claude-sonnet-4-6")),
            Some(provider("gpt-5.4")),
            None,
        )
        .with_deployment_policy(require_zero_retention_policy())
        .with_provider_capabilities(ProviderId::Anthropic, zero_retention());

        let resolved = registry
            .provider_for_model(Some("claude-sonnet-4-6"))
            .expect("the compliant provider must be reachable");
        assert_eq!(
            resolved.capabilities().model_id,
            ModelId::new("claude-sonnet-4-6")
        );

        let Err(error) = registry.provider_for_model(Some("gpt-5.4")) else {
            panic!("the non-compliant provider must fail closed, not be rerouted");
        };
        assert!(error.to_string().contains("zero-retention"), "{error}");
    }

    #[test]
    fn deployment_policy_fails_closed_when_the_selected_provider_is_non_compliant() {
        // Pins: with only non-compliant providers, resolution fails closed with a
        // governance error rather than silently handing back a data-retaining
        // provider.
        let registry = ProviderRegistry::with_static_providers(
            Some(provider("claude-sonnet-4-6")),
            Some(provider("gpt-5.4")),
            None,
        )
        .with_deployment_policy(require_zero_retention_policy());

        // Both an explicit request and the default selection fail closed.
        let Err(explicit) = registry.provider_for_model(Some("gpt-5.4")) else {
            panic!("an explicit non-compliant model must fail closed");
        };
        assert!(
            explicit.to_string().contains("zero-retention"),
            "{explicit}"
        );

        let Err(default) = registry.provider_for_model(None) else {
            panic!("the default non-compliant provider must fail closed, not fall back");
        };
        assert!(default.to_string().contains("excluded"), "{default}");
    }

    #[test]
    fn no_deployment_policy_routing_is_unchanged() {
        // Pins: with no deployment policy, default resolution keeps existing
        // behavior (OpenAI wins on default priority) with zero overhead.
        let registry = ProviderRegistry::with_static_providers(
            Some(provider("claude-sonnet-4-6")),
            Some(provider("gpt-5.4")),
            None,
        );

        let resolved = registry
            .provider_for_model(None)
            .expect("ungoverned routing resolves normally");
        assert_eq!(resolved.capabilities().model_id, ModelId::new("gpt-5.4"));
    }

    #[test]
    fn deployment_policy_allowlist_fails_closed_for_an_unlisted_provider() {
        // Pins: an allowlist fails closed for a provider that is not on it, while
        // the allowlisted provider routes.
        let policy = DeploymentProviderPolicy::from_config(&DeploymentProviderPolicyConfig {
            allowed_providers: vec![ProviderId::Anthropic],
            ..DeploymentProviderPolicyConfig::default()
        });
        let registry = ProviderRegistry::with_static_providers(
            Some(provider("claude-sonnet-4-6")),
            Some(provider("gpt-5.4")),
            None,
        )
        .with_deployment_policy(policy);

        let Err(error) = registry.provider_for_model(Some("gpt-5.4")) else {
            panic!("an unlisted provider must be excluded");
        };
        assert!(error.to_string().contains("allowlist"), "{error}");

        let resolved = registry
            .provider_for_model(Some("claude-sonnet-4-6"))
            .expect("the allowlisted provider routes");
        assert_eq!(
            resolved.capabilities().model_id,
            ModelId::new("claude-sonnet-4-6")
        );
    }

    #[test]
    fn from_config_reads_capability_assertions_and_activates_policy() {
        // Pins: an operator's zero-retention assertion on a credential plus an
        // active policy in config produce a governed registry whose single gate
        // fails closed for the non-compliant provider but admits the compliant one.
        let mut config = moa_config::MoaConfig::default();
        config.providers.openai.api_key = "openai-key".to_string();
        config.providers.anthropic.api_key = "anthropic-key".to_string();
        config.providers.anthropic.capabilities.zero_retention = true;
        config.providers.routing_policy.require_zero_retention = true;

        let registry = ProviderRegistry::from_config(&config, None)
            .expect("local provider registry should build");

        // The compliant Anthropic provider passes the gate and builds.
        registry
            .provider_for_id(ProviderId::Anthropic, &ModelId::new("claude-sonnet-4-6"))
            .expect("the zero-retention Anthropic provider must be reachable");

        // The non-compliant OpenAI provider fails closed at the gate.
        let Err(error) = registry.provider_for_id(ProviderId::OpenAI, &ModelId::new("gpt-5.4"))
        else {
            panic!("the non-compliant configured provider must fail closed");
        };
        assert!(error.to_string().contains("zero-retention"), "{error}");
    }
}
