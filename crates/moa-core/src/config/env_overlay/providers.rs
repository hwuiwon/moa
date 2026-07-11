//! Model, memory, knowledge, and sandbox provider overlay handling.

use super::*;

pub(super) fn optional_section_seed(path: &[&str]) -> Option<Value> {
    match path {
        ["cloud", "hands"] => Some(json!({
            "default_provider": null,
            "fallback_providers": [],
            "allow_local_provider": false,
            "daytona_api_key": null,
            "daytona_api_url": null,
            "daytona_default_image": null,
            "e2b_api_key": null,
            "e2b_api_url": null,
            "e2b_domain": null,
            "e2b_template": null,
        })),
        _ => None,
    }
}

pub(super) fn exact_overlay_path(field: &str) -> Option<Vec<String>> {
    let path = match field {
        "models_fallback_models" => &["models", "fallback_models"][..],
        "knowledge_external_parser_default" => &["knowledge", "parser", "external_default"],
        "llamaparse_api_url" => &["knowledge", "llamaparse", "api_base_url"],
        "unstructured_api_url" => &["knowledge", "unstructured", "api_base_url"],
        "reducto_api_url" => &["knowledge", "reducto", "api_base_url"],
        "turbopuffer_baa" => &["memory", "vector", "turbopuffer", "baa_enabled"],
        "cloud_hands_allow_local" => &["cloud", "hands", "allow_local_provider"],
        "providers_concurrency_scope" => &["providers", "concurrency", "scope"],
        "providers_concurrency_default_max_in_flight" => {
            &["providers", "concurrency", "default_max_in_flight"]
        }
        "providers_concurrency_block_threshold_ms" => {
            &["providers", "concurrency", "block_threshold_ms"]
        }
        "providers_concurrency_lease_ttl_ms" => &["providers", "concurrency", "lease_ttl_ms"],
        _ => return None,
    };
    Some(strings(path))
}

pub(super) fn deserialize_optional_list<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(Some(split_list(raw)))
}

fn split_list(value: String) -> Vec<String> {
    value
        .split(',')
        .filter_map(|item| {
            let item = item.trim();
            (!item.is_empty()).then(|| item.to_string())
        })
        .collect()
}

pub(super) fn validate_urls(overlay: &MoaEnvOverlay) -> Result<()> {
    validate_url("MOA_TURBOPUFFER_BASE_URL", &overlay.turbopuffer_base_url)?;
    validate_url("MOA_NANGO_API_BASE_URL", &overlay.nango_api_base_url)?;
    validate_url("MOA_MERGE_API_BASE_URL", &overlay.merge_api_base_url)?;
    validate_url("MOA_LLAMAPARSE_API_URL", &overlay.llamaparse_api_url)?;
    validate_url("MOA_UNSTRUCTURED_API_URL", &overlay.unstructured_api_url)?;
    validate_url("MOA_REDUCTO_API_URL", &overlay.reducto_api_url)?;
    validate_url(
        "MOA_CLOUD_HANDS_DAYTONA_API_URL",
        &overlay.cloud_hands_daytona_api_url,
    )?;
    validate_url(
        "MOA_CLOUD_HANDS_E2B_API_URL",
        &overlay.cloud_hands_e2b_api_url,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knowledge_overlay_applies_flat_provider_and_parser_settings() {
        // Pins: tenant knowledge overlay updates non-secret runtime config without adding secret indirection knobs.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_KNOWLEDGE_PROVIDERS_ENABLED", "nango"),
            ("MOA_KNOWLEDGE_PARSERS_ENABLED", "native,llamaparse"),
            ("MOA_KNOWLEDGE_PARSER_DEFAULT", "native"),
            ("MOA_KNOWLEDGE_EXTERNAL_PARSER_DEFAULT", "llamaparse"),
            ("MOA_NANGO_API_BASE_URL", "https://nango.example"),
            ("MOA_NANGO_API_KEY", "nango-key"),
            ("MOA_NANGO_WEBHOOK_SIGNING_KEY", "nango-signing-key"),
            ("MOA_MERGE_API_BASE_URL", "https://merge.example"),
            ("MOA_MERGE_API_KEY", "merge-key"),
            ("MOA_MERGE_WEBHOOK_SIGNATURE_KEY", "merge-signature-key"),
            ("MOA_LLAMAPARSE_API_URL", "https://llamaparse.example"),
            ("MOA_LLAMAPARSE_API_KEY", "llamaparse-key"),
            (
                "MOA_LLAMAPARSE_WEBHOOK_SIGNING_KEY",
                "llamaparse-signing-key",
            ),
            ("MOA_LLAMAPARSE_WEBHOOK_HEADER_NAME", "x-llama-secret"),
            ("MOA_LLAMAPARSE_WEBHOOK_HEADER_VALUE", "llama-header-secret"),
            ("MOA_LLAMAPARSE_TIER", "agentic"),
            ("MOA_UNSTRUCTURED_API_URL", "https://unstructured.example"),
            ("MOA_UNSTRUCTURED_API_KEY", "unstructured-key"),
            ("MOA_UNSTRUCTURED_STRATEGY", "fast"),
            ("MOA_UNSTRUCTURED_CHUNKING_STRATEGY", "basic"),
            ("MOA_REDUCTO_API_URL", "https://reducto.example"),
            ("MOA_REDUCTO_API_KEY", "reducto-key"),
            ("MOA_REDUCTO_WEBHOOK_SIGNING_KEY", "reducto-signing-key"),
            ("MOA_REDUCTO_WEBHOOK_HEADER_NAME", "x-reducto-secret"),
            ("MOA_REDUCTO_WEBHOOK_HEADER_VALUE", "reducto-header-secret"),
            ("MOA_REDUCTO_PARSE_MODE", "ocr"),
            ("MOA_REDUCTO_ASYNC_ENABLED", "false"),
            ("MOA_REDUCTO_CHUNK_MODE", "page"),
        ]))
        .expect("knowledge overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("knowledge overlay should apply");

        assert_eq!(config.knowledge.providers.enabled, ["nango"]);
        assert_eq!(config.knowledge.parsers.enabled, ["native", "llamaparse"]);
        assert_eq!(config.knowledge.parser.default, "native");
        assert_eq!(config.knowledge.parser.external_default, "llamaparse");
        assert_eq!(config.knowledge.nango.api_base_url, "https://nango.example");
        assert_eq!(config.knowledge.nango.api_key, "nango-key");
        assert_eq!(
            config.knowledge.nango.webhook_signing_key,
            "nango-signing-key"
        );
        assert_eq!(config.knowledge.merge.api_base_url, "https://merge.example");
        assert_eq!(config.knowledge.merge.api_key, "merge-key");
        assert_eq!(
            config.knowledge.merge.webhook_signature_key,
            "merge-signature-key"
        );
        assert_eq!(
            config.knowledge.llamaparse.api_base_url,
            "https://llamaparse.example"
        );
        assert_eq!(config.knowledge.llamaparse.api_key, "llamaparse-key");
        assert_eq!(
            config.knowledge.llamaparse.webhook_signing_key,
            "llamaparse-signing-key"
        );
        assert_eq!(
            config.knowledge.llamaparse.webhook_header_name.as_deref(),
            Some("x-llama-secret")
        );
        assert_eq!(
            config.knowledge.llamaparse.webhook_header_value.as_deref(),
            Some("llama-header-secret")
        );
        assert_eq!(config.knowledge.llamaparse.tier, "agentic");
        assert_eq!(
            config.knowledge.unstructured.api_base_url,
            "https://unstructured.example"
        );
        assert_eq!(config.knowledge.unstructured.api_key, "unstructured-key");
        assert_eq!(config.knowledge.unstructured.strategy, "fast");
        assert_eq!(config.knowledge.unstructured.chunking_strategy, "basic");
        assert_eq!(
            config.knowledge.reducto.api_base_url,
            "https://reducto.example"
        );
        assert_eq!(config.knowledge.reducto.api_key, "reducto-key");
        assert_eq!(
            config.knowledge.reducto.webhook_signing_key,
            "reducto-signing-key"
        );
        assert_eq!(
            config.knowledge.reducto.webhook_header_name.as_deref(),
            Some("x-reducto-secret")
        );
        assert_eq!(
            config.knowledge.reducto.webhook_header_value.as_deref(),
            Some("reducto-header-secret")
        );
        assert_eq!(config.knowledge.reducto.parse_mode, "ocr");
        assert!(!config.knowledge.reducto.async_enabled);
        assert_eq!(config.knowledge.reducto.chunk_mode, "page");
    }

    #[test]
    fn empty_default_provider_env_is_rejected_not_clobbered() {
        // Pins: an empty MOA_GENERAL_DEFAULT_PROVIDER must not silently clobber the
        // populated "openai" default (the known mock/empty gotcha); apply_to fails
        // closed naming the offending field rather than yielding an empty provider.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_GENERAL_DEFAULT_PROVIDER", ""),
        ]))
        .expect("overlay should deserialize");

        let mut config = MoaConfig::default();
        assert_eq!(config.general.default_provider, "openai");

        assert_config_error_contains(overlay.apply_to(&mut config), "general.default_provider");
    }

    #[test]
    fn empty_models_main_env_is_rejected_not_clobbered() {
        // Pins: an empty MOA_MODELS_MAIN must not clobber the populated main-model
        // default; validation fails closed naming models.main.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_MODELS_MAIN", ""),
        ]))
        .expect("overlay should deserialize");

        let mut config = MoaConfig::default();
        assert_ne!(config.models.main, "");

        assert_config_error_contains(overlay.apply_to(&mut config), "models.main");
    }

    #[test]
    fn fallback_models_env_overrides_model_failover_chain() {
        // Pins: flat Kubernetes env can configure the main-loop failover chain,
        // not just the primary and auxiliary models.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            (
                "MOA_MODELS_FALLBACK_MODELS",
                "openai:gpt-5.4, anthropic:claude-haiku-4-5",
            ),
        ]))
        .expect("overlay should deserialize");
        let mut config = MoaConfig::default();

        overlay.apply_to(&mut config).expect("overlay should apply");

        assert_eq!(
            config.models.fallback_models,
            vec![
                "openai:gpt-5.4".to_string(),
                "anthropic:claude-haiku-4-5".to_string()
            ]
        );
    }

    #[test]
    fn cloud_hands_fallback_providers_env_overrides_route_chain() {
        // Pins: cloud hand fallback can be configured from flat Kubernetes env.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_CLOUD_HANDS_DEFAULT_PROVIDER", "daytona"),
            ("MOA_CLOUD_HANDS_FALLBACK_PROVIDERS", "e2b"),
            ("MOA_CLOUD_HANDS_ALLOW_LOCAL", "true"),
        ]))
        .expect("overlay should deserialize");
        let mut config = MoaConfig::default();

        overlay.apply_to(&mut config).expect("overlay should apply");

        let hands = config.cloud.hands.expect("cloud hands config");
        assert_eq!(hands.default_provider.as_deref(), Some("daytona"));
        assert_eq!(hands.fallback_providers, vec!["e2b".to_string()]);
        assert!(hands.allow_local_provider);
    }
}
