//! Provider construction honours the fleet-coordination policy.
//!
//! The unit tests in `moa-providers` pin what each distributed control does once
//! it has a coordination store. These pin the wiring in front of them: that the
//! real, public construction paths — chat registry, embedder factory, reranker
//! factory — actually resolve coordination from config, so a deployment that
//! declares a fleet-wide scope cannot end up with per-replica controls because
//! one builder forgot to ask.

use std::sync::Arc;

use moa_config::{ConcurrencyScope, CoordinationFailurePolicy, MoaConfig};
use moa_core::traits::RuntimeCacheStore;
use moa_providers::{
    AnthropicProvider, EmbedderConstructionRole, ProviderRegistry, build_embedder_from_config,
    build_reranker_from_config,
};
use moa_runtime_store::MemoryRuntimeCacheStore;

/// A config that wants fleet-wide pacing and refuses to run without it.
fn fail_closed_global_config() -> MoaConfig {
    let mut config = MoaConfig::default();
    config.providers.concurrency.scope = ConcurrencyScope::Global;
    config.providers.pacing.scope = ConcurrencyScope::Global;
    config.providers.concurrency.on_coordination_failure = CoordinationFailurePolicy::FailClosed;
    config.providers.anthropic.api_key = "test-anthropic-key".to_string();
    config.models.main = "claude-sonnet-4-6".to_string();
    config.providers.cohere.api_key = "test-cohere-key".to_string();
    config.memory.vector.embedder.name = "cohere:embed-v4.0".to_string();
    config.memory.vector.embedder.output_dim = 1_024;
    config.memory.retrieval.reranker_model = "cohere:rerank-v3.5".to_string();
    config
}

fn runtime_store() -> Arc<dyn RuntimeCacheStore> {
    Arc::new(MemoryRuntimeCacheStore::new())
}

#[test]
fn chat_provider_construction_fails_closed_without_a_coordination_store_offline() {
    // Pins: a chat provider built from a fail-closed global config refuses to
    // construct when no coordination store was injected, instead of silently
    // enforcing a per-replica ceiling the fleet would multiply.
    let config = fail_closed_global_config();
    let error = match AnthropicProvider::from_config(&config) {
        Ok(_) => {
            panic!("a fail-closed global config with no coordination store must not construct")
        }
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("composition root"),
        "the error must name the injection point: {error}"
    );

    ProviderRegistry::from_config(&config, Some(runtime_store()))
        .expect("registry constructs with an explicit runtime cache")
        .provider_for_model(Some("claude-sonnet-4-6"))
        .expect("the real model client constructs from the shared coordination runtime");
}

#[test]
fn embedder_construction_fails_closed_without_a_coordination_store_offline() {
    // Pins: the embedding factory resolves coordination on the real construction
    // path, so an embedder cannot be built with uncoordinated pacing under a
    // fail-closed global config.
    let config = fail_closed_global_config();
    let error = match build_embedder_from_config(&config, None, EmbedderConstructionRole::Retrieval)
    {
        Ok(_) => panic!("the embedder factory must apply the coordination-failure policy"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("composition root"),
        "the error must name the injection point: {error}"
    );

    build_embedder_from_config(
        &config,
        Some(runtime_store()),
        EmbedderConstructionRole::Retrieval,
    )
    .map(|_| ())
    .expect("the embedder builds once the coordination store is injected");
}

#[test]
fn reranker_construction_fails_closed_without_a_coordination_store_offline() {
    // Pins: the rerank factory resolves coordination too — rerank shares the same
    // credential quota as embed on one Cohere key, so it cannot opt out.
    let config = fail_closed_global_config();
    let error = match build_reranker_from_config(&config, None) {
        Ok(_) => panic!("the rerank factory must apply the coordination-failure policy"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("composition root"),
        "the error must name the injection point: {error}"
    );

    build_reranker_from_config(&config, Some(runtime_store()))
        .map(|_| ())
        .expect("the reranker builds once the coordination store is injected");
}

#[test]
fn a_local_scope_deployment_needs_no_coordination_store_offline() {
    // Pins: the negative control. A deliberate process-local deployment is
    // configuration, not a degraded state, so every builder constructs cleanly
    // with no store even under the strictest failure policy. Without this, the
    // tests above could pass simply because construction always failed.
    let mut config = fail_closed_global_config();
    config.providers.concurrency.scope = ConcurrencyScope::Local;
    config.providers.pacing.scope = ConcurrencyScope::Local;

    AnthropicProvider::from_config(&config)
        .map(|_| ())
        .expect("local scope needs no coordination store");
    build_embedder_from_config(&config, None, EmbedderConstructionRole::Retrieval)
        .map(|_| ())
        .expect("local scope needs no coordination store");
    build_reranker_from_config(&config, None)
        .map(|_| ())
        .expect("local scope needs no coordination store");
}

#[test]
fn bounded_degraded_keeps_building_without_a_coordination_store_offline() {
    // Pins: the other half of the policy. Under bounded_degraded the same
    // store-less global config still constructs — availability is preserved, with
    // the degradation reported through the coordination metrics and warning
    // rather than by refusing to start.
    let mut config = fail_closed_global_config();
    config.providers.concurrency.on_coordination_failure =
        CoordinationFailurePolicy::BoundedDegraded;

    AnthropicProvider::from_config(&config)
        .map(|_| ())
        .expect("bounded_degraded must not block startup");
    build_embedder_from_config(&config, None, EmbedderConstructionRole::Retrieval)
        .map(|_| ())
        .expect("bounded_degraded must not block startup");
    build_reranker_from_config(&config, None)
        .map(|_| ())
        .expect("bounded_degraded must not block startup");
}

#[test]
fn runtime_wiring_is_not_part_of_serializable_configuration_offline() {
    // Pins: live service handles belong to composition, not the MoaConfig data
    // model. This catches a future service-locator field being smuggled back in.
    let serialized = serde_json::to_value(MoaConfig::default()).expect("serialize config");
    assert!(
        serialized.get("runtime_coordination").is_none(),
        "a live provider coordination handle must not appear as a config key"
    );
}
