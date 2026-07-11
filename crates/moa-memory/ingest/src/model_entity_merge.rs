//! Model-backed entity merge verification and recorded replay support.

use async_trait::async_trait;
use moa_core::config::MoaConfig;
use moa_memory_graph::NodeIndexRow;
use moa_memory_types::normalize_entity_name;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model_client::{ModelCallObserver, ModelTextClient, resolved_extraction_config};
use crate::{EntityMergeVerifier, IngestError, Result};

/// Merge-verifier prompt version used for recorded fixtures.
pub const MERGE_PROMPT_VERSION: &str = "v1";

const MERGE_SYSTEM_PROMPT: &str = r#"You decide whether two extracted entity mentions refer to the same real entity in graph memory.
Answer with exactly one lowercase word: yes or no.
Say yes only when the mention is a paraphrase, abbreviation, casing variant, or punctuation variant of the candidate.
Say no when the terms could name different services, repositories, people, teams, credentials, or documents.
When ambiguous, answer no."#;

/// Model-backed entity merge verifier that uses the configured provider model.
#[derive(Clone)]
pub struct ModelEntityMergeVerifier {
    client: ModelEntityMergeClient,
}

impl ModelEntityMergeVerifier {
    /// Creates a merge verifier from the shared memory model client.
    #[must_use]
    pub(crate) fn new(client: ModelTextClient) -> Self {
        Self {
            client: ModelEntityMergeClient::new(client),
        }
    }

    /// Creates a merge verifier from runtime config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let extraction = resolved_extraction_config(config).ok_or_else(|| {
            IngestError::ModelInference("memory.extraction.enabled is false".to_string())
        })?;
        Ok(Self::new(ModelTextClient::from_config(
            config,
            &extraction,
        )?))
    }

    /// Creates a configured merge verifier with a provider-call observer.
    pub fn from_config_with_observer(
        config: &MoaConfig,
        observer: std::sync::Arc<dyn ModelCallObserver>,
    ) -> Result<Self> {
        let extraction = resolved_extraction_config(config).ok_or_else(|| {
            IngestError::ModelInference("memory.extraction.enabled is false".to_string())
        })?;
        Ok(Self::new(ModelTextClient::from_config_with_observer(
            config,
            &extraction,
            observer,
        )?))
    }
}

#[derive(Clone)]
struct ModelEntityMergeClient {
    client: ModelTextClient,
}

impl ModelEntityMergeClient {
    fn new(client: ModelTextClient) -> Self {
        Self { client }
    }

    async fn should_merge(
        &self,
        mention: &str,
        candidate_name: &str,
        normalized_candidate_name: &str,
    ) -> Result<bool> {
        let user = format!(
            "Mention: {}\nCandidate: {}\nCandidate normalized name: {}\n",
            mention.trim(),
            candidate_name.trim(),
            normalized_candidate_name
        );
        let answer = self
            .client
            .complete_text(MERGE_SYSTEM_PROMPT, &user)
            .await?;
        Ok(parse_merge_answer(&answer))
    }
}

fn parse_merge_answer(answer: &str) -> bool {
    match answer.trim().to_ascii_lowercase().as_str() {
        "yes" => true,
        "no" => false,
        _ => false,
    }
}

#[async_trait]
impl EntityMergeVerifier for ModelEntityMergeVerifier {
    async fn should_merge(&self, mention: &str, candidate: &NodeIndexRow) -> Result<bool> {
        Ok(self
            .client
            .should_merge(
                mention,
                &candidate.name,
                &normalize_entity_name(&candidate.name),
            )
            .await?)
    }
}

/// One recorded merge-verifier decision keyed by mention and candidate name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityMergeFixtureRecord {
    /// Stable SHA-256 hex key over normalized mention then normalized candidate.
    pub key: String,
    /// Merge prompt version used to produce this fixture.
    pub prompt_version: String,
    /// Raw mention sent to the verifier.
    pub mention: String,
    /// Candidate entity name sent to the verifier.
    pub candidate: String,
    /// Recorded verifier decision.
    pub should_merge: bool,
}

impl EntityMergeFixtureRecord {
    /// Returns the fixture lookup key.
    #[must_use]
    pub fn fixture_key(&self) -> &str {
        &self.key
    }

    /// Returns the fixture prompt version.
    #[must_use]
    pub fn fixture_version(&self) -> &str {
        &self.prompt_version
    }
}

/// Lookup interface implemented by fixture stores that can back recorded merge verification.
pub trait RecordedEntityMergeStore: Send + Sync {
    /// Returns the recorded merge fixture for a key, if it exists.
    fn get_optional(&self, key: &str) -> Option<&EntityMergeFixtureRecord>;
}

/// Merge verifier that replays committed fixtures and never calls the network.
#[derive(Debug, Clone)]
pub struct RecordedEntityMergeVerifier<S> {
    store: S,
    remediation_command: String,
}

impl<S> RecordedEntityMergeVerifier<S> {
    /// Creates a recorded verifier from a fixture store and remediation command.
    #[must_use]
    pub fn new(store: S, remediation_command: impl Into<String>) -> Self {
        Self {
            store,
            remediation_command: remediation_command.into(),
        }
    }
}

#[async_trait]
impl<S> EntityMergeVerifier for RecordedEntityMergeVerifier<S>
where
    S: RecordedEntityMergeStore,
{
    async fn should_merge(&self, mention: &str, candidate: &NodeIndexRow) -> Result<bool> {
        let key = merge_fixture_key(mention, &candidate.name);
        let Some(record) = self.store.get_optional(&key) else {
            return Err(IngestError::EntityResolution(format!(
                "recorded merge fixture is missing key {key}. Regenerate with: {}",
                self.remediation_command
            )));
        };
        if record.prompt_version != MERGE_PROMPT_VERSION {
            return Err(IngestError::EntityResolution(format!(
                "recorded merge fixture {} has prompt_version {}; expected {}",
                record.key, record.prompt_version, MERGE_PROMPT_VERSION
            )));
        }
        Ok(record.should_merge)
    }
}

/// Returns the recorded-fixture key for a mention-candidate verifier call.
#[must_use]
pub fn merge_fixture_key(mention: &str, candidate: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_entity_name(mention).as_bytes());
    hasher.update([0]);
    hasher.update(normalize_entity_name(candidate).as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use moa_core::{
        traits::LLMProvider, types::completion::CompletionRequest,
        types::completion::CompletionResponse, types::completion::CompletionStream,
        types::completion::StopReason, types::completion::TokenUsage, types::identifiers::ModelId,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
    };

    use super::*;

    struct CapturingProvider {
        request: Mutex<Option<CompletionRequest>>,
        response: String,
    }

    #[async_trait]
    impl LLMProvider for CapturingProvider {
        fn name(&self) -> &str {
            "capturing"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: ModelId::new("gpt-5.4-mini"),
                context_window: 400_000,
                max_output: 128_000,
                supports_tools: true,
                supports_vision: true,
                supports_prefix_caching: true,
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
            request: CompletionRequest,
        ) -> moa_core::error::Result<CompletionStream> {
            *self.request.lock().expect("capture request") = Some(request);
            Ok(CompletionStream::from_response(CompletionResponse {
                text: self.response.clone(),
                content: Vec::new(),
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("gpt-5.4-mini"),
                usage: TokenUsage::default(),
                duration_ms: 1,
                thought_signature: None,
            }))
        }
    }

    #[derive(Debug, Clone)]
    struct MapStore {
        records: BTreeMap<String, EntityMergeFixtureRecord>,
    }

    impl RecordedEntityMergeStore for MapStore {
        fn get_optional(&self, key: &str) -> Option<&EntityMergeFixtureRecord> {
            self.records.get(key)
        }
    }

    #[test]
    fn merge_verifier_false_or_malformed_means_no_merge() {
        // Pins: ambiguous merge-verifier output is fail-closed to avoid corrupting entity links.
        assert!(parse_merge_answer("yes"));
        assert!(!parse_merge_answer("no"));
        assert!(!parse_merge_answer("maybe"));
        assert!(!parse_merge_answer("yes, probably"));
    }

    #[test]
    fn merge_fixture_key_is_order_dependent_on_mention_then_candidate() {
        // Pins: recorded merge keys distinguish the live verifier's mention/candidate call order.
        let first = merge_fixture_key("the checkout service", "CheckoutSvc");
        let reverse = merge_fixture_key("CheckoutSvc", "the checkout service");

        assert_ne!(first, reverse);
    }

    #[tokio::test]
    async fn recorded_merge_verifier_errors_on_missing_key_with_remediation() {
        // Pins: replay cannot silently skip missing merge fixtures.
        let verifier = RecordedEntityMergeVerifier::new(
            MapStore {
                records: BTreeMap::new(),
            },
            "cargo run -p xtask --features eval-tools -- record-memory-merges --corpus target/memory-eval/pr-natural",
        );
        let candidate = test_candidate("checkout-service");

        let error = verifier
            .should_merge("the checkout service", &candidate)
            .await
            .expect_err("missing fixture should fail");

        assert!(error.to_string().contains("record-memory-merges"));
        assert!(error.to_string().contains(&merge_fixture_key(
            "the checkout service",
            "checkout-service"
        )));
    }

    #[tokio::test]
    async fn model_merge_verifier_sends_prompt_and_parses_yes() {
        // Pins: model-backed merge verification uses the standard provider
        // request shape while preserving the yes/no merge decision.
        let provider = Arc::new(CapturingProvider {
            request: Mutex::new(None),
            response: "yes".to_string(),
        });
        let client = ModelTextClient::new(provider.clone(), ModelId::new("gpt-5.4-mini"), 1_000)
            .expect("model client should build");
        let verifier = ModelEntityMergeVerifier::new(client);

        let should_merge = verifier
            .should_merge("checkout service", &test_candidate("CheckoutSvc"))
            .await
            .expect("model verifier should decide merge");

        assert!(should_merge);
        let request = provider
            .request
            .lock()
            .expect("capture request")
            .clone()
            .expect("request captured");
        assert_eq!(request.model, Some(ModelId::new("gpt-5.4-mini")));
        assert_eq!(request.messages[0].content, MERGE_SYSTEM_PROMPT);
        assert!(
            request.messages[1]
                .content
                .contains("Mention: checkout service")
        );
        assert!(
            request.messages[1]
                .content
                .contains("Candidate normalized name: checkoutsvc")
        );
    }

    fn test_candidate(name: &str) -> NodeIndexRow {
        NodeIndexRow {
            uid: uuid::Uuid::now_v7(),
            label: moa_memory_graph::NodeLabel::Entity,
            storage_partition_id: Some("storage-partition-a".to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: name.to_string(),
            pii_class: moa_memory_graph::PiiClass::None,
            valid_to: None,
            valid_from: chrono::Utc::now(),
            properties_summary: Some(serde_json::json!({})),
            last_accessed_at: chrono::Utc::now(),
            quality_score: 0.5,
        }
    }
}
