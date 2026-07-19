//! Tenant knowledge-base connector, parser, sync, and observability settings.

use serde::{Deserialize, Serialize};

use crate::error::{MoaError, Result};

/// Tenant knowledge-base ingestion settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KnowledgeConfig {
    /// Linked-account provider settings.
    pub providers: KnowledgeProvidersConfig,
    /// Parser eligibility settings.
    pub parsers: KnowledgeParsersConfig,
    /// Default parser selection settings.
    pub parser: KnowledgeParserDefaultsConfig,
    /// Nango linked-account provider settings.
    pub nango: NangoKnowledgeProviderConfig,
    /// Merge linked-account provider settings.
    pub merge: MergeKnowledgeProviderConfig,
    /// LlamaParse parser settings.
    pub llamaparse: LlamaParseKnowledgeParserConfig,
    /// Unstructured parser settings.
    pub unstructured: UnstructuredKnowledgeParserConfig,
    /// Reducto parser settings.
    pub reducto: ReductoKnowledgeParserConfig,
    /// Sync-run controls.
    pub sync: KnowledgeSyncConfig,
    /// Chunking controls.
    pub chunking: KnowledgeChunkingConfig,
    /// Semantic graph extraction controls.
    pub semantic: KnowledgeSemanticConfig,
    /// Knowledge observability controls.
    pub observability: KnowledgeObservabilityConfig,
}

impl KnowledgeConfig {
    /// Loads the configured API key for a selected linked-account provider.
    pub fn selected_provider_api_key(&self, provider: &str) -> Result<String> {
        require_enabled("provider", provider, &self.providers.enabled)?;
        let api_key = match provider {
            "nango" => self.nango.api_key.as_str(),
            "merge" => self.merge.api_key.as_str(),
            other => {
                return Err(MoaError::ConfigError(format!(
                    "knowledge provider `{other}` is not configured"
                )));
            }
        };
        require_config_secret("knowledge provider", provider, api_key)
    }

    /// Loads the configured API key for a selected document parser.
    ///
    /// The native parser is local and does not require an API key.
    pub fn selected_parser_api_key(&self, parser: &str) -> Result<Option<String>> {
        require_enabled("parser", parser, &self.parsers.enabled)?;
        match parser {
            "native" => Ok(None),
            "llamaparse" => {
                require_config_secret("knowledge parser", parser, self.llamaparse.api_key.as_str())
                    .map(Some)
            }
            "unstructured" => require_config_secret(
                "knowledge parser",
                parser,
                self.unstructured.api_key.as_str(),
            )
            .map(Some),
            "reducto" => {
                require_config_secret("knowledge parser", parser, self.reducto.api_key.as_str())
                    .map(Some)
            }
            other => Err(MoaError::ConfigError(format!(
                "knowledge parser `{other}` is not configured"
            ))),
        }
    }
}

/// Enabled linked-account providers for tenant knowledge ingestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeProvidersConfig {
    /// Provider identifiers allowed for link and sync runs.
    pub enabled: Vec<String>,
}

impl Default for KnowledgeProvidersConfig {
    fn default() -> Self {
        Self {
            enabled: vec!["nango".to_string(), "merge".to_string()],
        }
    }
}

/// Enabled document parsers for tenant knowledge ingestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeParsersConfig {
    /// Parser identifiers allowed for request-time or sync-run selection.
    pub enabled: Vec<String>,
}

impl Default for KnowledgeParsersConfig {
    fn default() -> Self {
        Self {
            enabled: vec![
                "native".to_string(),
                "llamaparse".to_string(),
                "unstructured".to_string(),
                "reducto".to_string(),
            ],
        }
    }
}

/// Default parser choices for local and external-source objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeParserDefaultsConfig {
    /// Default parser for local or already-normalized content.
    pub default: String,
    /// Default parser for external synced records and files.
    pub external_default: String,
}

impl Default for KnowledgeParserDefaultsConfig {
    fn default() -> Self {
        Self {
            default: "native".to_string(),
            external_default: "llamaparse".to_string(),
        }
    }
}

/// Nango linked-account provider settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NangoKnowledgeProviderConfig {
    /// Nango API base URL.
    pub api_base_url: String,
    /// Nango API key loaded from runtime configuration.
    pub api_key: String,
    /// Nango webhook signing key loaded from runtime configuration.
    pub webhook_signing_key: String,
}

impl Default for NangoKnowledgeProviderConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://api.nango.dev".to_string(),
            api_key: String::new(),
            webhook_signing_key: String::new(),
        }
    }
}

/// Merge linked-account provider settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeKnowledgeProviderConfig {
    /// Merge API base URL.
    pub api_base_url: String,
    /// Merge API key loaded from runtime configuration.
    pub api_key: String,
    /// Merge webhook signature key loaded from runtime configuration.
    pub webhook_signature_key: String,
}

impl Default for MergeKnowledgeProviderConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://api.merge.dev".to_string(),
            api_key: String::new(),
            webhook_signature_key: String::new(),
        }
    }
}

/// LlamaParse parser settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlamaParseKnowledgeParserConfig {
    /// LlamaParse API base URL.
    pub api_base_url: String,
    /// LlamaParse API key loaded from runtime configuration.
    pub api_key: String,
    /// LlamaParse webhook signing key loaded from runtime configuration.
    pub webhook_signing_key: String,
    /// Optional custom header name required on LlamaParse webhooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_header_name: Option<String>,
    /// Optional custom header value required on LlamaParse webhooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_header_value: Option<String>,
    /// LlamaParse plan or routing tier.
    pub tier: String,
    /// Parse response expansions requested from LlamaParse.
    pub expand: Vec<String>,
}

impl Default for LlamaParseKnowledgeParserConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://api.cloud.llamaindex.ai".to_string(),
            api_key: String::new(),
            webhook_signing_key: String::new(),
            webhook_header_name: None,
            webhook_header_value: None,
            tier: "agentic".to_string(),
            expand: vec![
                "markdown".to_string(),
                "markdown_full".to_string(),
                "items".to_string(),
                "metadata".to_string(),
                "job_metadata".to_string(),
            ],
        }
    }
}

/// Unstructured parser settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UnstructuredKnowledgeParserConfig {
    /// Unstructured API base URL.
    pub api_base_url: String,
    /// Unstructured API key loaded from runtime configuration.
    pub api_key: String,
    /// Unstructured partition strategy.
    pub strategy: String,
    /// Unstructured chunking strategy.
    pub chunking_strategy: String,
}

impl Default for UnstructuredKnowledgeParserConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://api.unstructuredapp.io".to_string(),
            api_key: String::new(),
            strategy: "auto".to_string(),
            chunking_strategy: "by_title".to_string(),
        }
    }
}

/// Reducto parser settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReductoKnowledgeParserConfig {
    /// Reducto API base URL.
    pub api_base_url: String,
    /// Reducto API key loaded from runtime configuration.
    pub api_key: String,
    /// Reducto webhook signing key loaded from runtime configuration.
    pub webhook_signing_key: String,
    /// Optional custom header name required on Reducto webhooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_header_name: Option<String>,
    /// Optional custom header value required on Reducto webhooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_header_value: Option<String>,
    /// Reducto parse mode.
    pub parse_mode: String,
    /// Whether Reducto asynchronous parsing is enabled.
    pub async_enabled: bool,
    /// Reducto chunk mode.
    pub chunk_mode: String,
    /// Whether Reducto should force URL results.
    pub force_url_result: bool,
}

impl Default for ReductoKnowledgeParserConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://platform.reducto.ai".to_string(),
            api_key: String::new(),
            webhook_signing_key: String::new(),
            webhook_header_name: None,
            webhook_header_value: None,
            parse_mode: "standard".to_string(),
            async_enabled: true,
            chunk_mode: "variable".to_string(),
            force_url_result: true,
        }
    }
}

/// Tenant knowledge sync-run controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeSyncConfig {
    /// Default source page size for provider record listing.
    pub default_page_size: u32,
    /// Maximum records one sync run may process.
    pub max_records_per_run: u32,
}

impl Default for KnowledgeSyncConfig {
    fn default() -> Self {
        Self {
            default_page_size: 100,
            max_records_per_run: 10_000,
        }
    }
}

/// Tenant knowledge chunking controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeChunkingConfig {
    /// Target chunk size in tokens.
    pub target_tokens: usize,
    /// Maximum chunk size in tokens.
    pub max_tokens: usize,
    /// Minimum chunk size in tokens.
    pub min_tokens: usize,
}

impl Default for KnowledgeChunkingConfig {
    fn default() -> Self {
        Self {
            target_tokens: 700,
            max_tokens: 1_000,
            min_tokens: 120,
        }
    }
}

/// Tenant knowledge semantic graph extraction controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeSemanticConfig {
    /// Emit deterministic generic proper-noun entities when the domain-specific
    /// ruleset matches nothing in a chunk.
    ///
    /// Defaults to `true`: the fallback is purely lexical (no provider or LLM
    /// calls) and cheap, and without it the graph retrieval leg never fires on
    /// non-domain corpora because no entity nodes exist to seed traversal.
    pub generic_entities: bool,
    /// Model-backed semantic graph extraction settings.
    pub model_extraction: KnowledgeSemanticModelConfig,
}

impl Default for KnowledgeSemanticConfig {
    fn default() -> Self {
        Self {
            generic_entities: true,
            model_extraction: KnowledgeSemanticModelConfig::default(),
        }
    }
}

/// Model-backed tenant knowledge semantic graph extraction controls.
///
/// The surface is deliberately just provider selection: enable the path and pick
/// the model. Extraction bounds (per-chunk caps, request timeout) are constants
/// in the extractor, matching how the deterministic extractor bounds itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KnowledgeSemanticModelConfig {
    /// Whether the model-backed extractor is the production semantic extractor.
    ///
    /// Defaults to `false`: the deterministic keyword extractor is the sole
    /// extractor unless this is enabled and a provider resolves. When enabled the
    /// model becomes the production extractor for new or changed chunks and the
    /// keyword ruleset stays the per-chunk fallback for failed model calls.
    pub enabled: bool,
    /// Model id used for extraction; empty resolves the config's default model.
    pub model: String,
}

/// Tenant knowledge observability controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeObservabilityConfig {
    /// Maximum object preview characters kept for inspection surfaces.
    pub max_object_preview_chars: usize,
}

impl Default for KnowledgeObservabilityConfig {
    fn default() -> Self {
        Self {
            max_object_preview_chars: 4_000,
        }
    }
}

fn require_enabled(kind: &str, selected: &str, enabled: &[String]) -> Result<()> {
    if enabled.iter().any(|candidate| candidate == selected) {
        return Ok(());
    }
    Err(MoaError::ConfigError(format!(
        "knowledge {kind} `{selected}` is not enabled"
    )))
}

fn require_config_secret(kind: &str, selected: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(MoaError::ConfigError(format!(
            "{kind} `{selected}` requires an API key when selected"
        )));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knowledge_selected_provider_requires_key_only_when_selected() {
        // Pins: enabled providers do not require API keys until a sync/link request selects one.
        let config = KnowledgeConfig::default();

        assert_eq!(
            config
                .selected_provider_api_key("nango")
                .expect_err("selected provider without key should fail")
                .to_string(),
            "configuration error: knowledge provider `nango` requires an API key when selected"
        );
    }

    #[test]
    fn knowledge_disabled_provider_does_not_leak_key_requirement() {
        // Pins: disabled provider selection fails on enablement before checking credentials.
        let mut config = KnowledgeConfig::default();
        config.providers.enabled = vec!["merge".to_string()];

        assert_eq!(
            config
                .selected_provider_api_key("nango")
                .expect_err("disabled provider should fail")
                .to_string(),
            "configuration error: knowledge provider `nango` is not enabled"
        );
    }

    #[test]
    fn knowledge_native_parser_needs_no_key_but_external_parser_does() {
        // Pins: native parsing is local while external parser credentials are request-time requirements.
        let config = KnowledgeConfig::default();

        assert_eq!(
            config
                .selected_parser_api_key("native")
                .expect("native parser should be local"),
            None
        );
        assert_eq!(
            config
                .selected_parser_api_key("llamaparse")
                .expect_err("selected external parser without key should fail")
                .to_string(),
            "configuration error: knowledge parser `llamaparse` requires an API key when selected"
        );
    }

    #[test]
    fn unstructured_defaults_use_auto_partitioning_and_section_chunks() {
        // Pins: production Unstructured parsing defaults to automatic partitioning and section-preserving chunks.
        let config = UnstructuredKnowledgeParserConfig::default();

        assert_eq!(config.strategy, "auto");
        assert_eq!(config.chunking_strategy, "by_title");
    }
}
