//! LLM-backed entity merge verification and recorded replay support.

use async_trait::async_trait;
use moa_memory_graph::NodeIndexRow;
use moa_providers::{LlmChatClient, LlmChatError, LlmEntityMergeClient};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{EntityMergeVerifier, IngestError, Result, entity_resolution::normalize_entity_name};

pub use moa_providers::MERGE_PROMPT_VERSION;

/// LLM-backed entity merge verifier that uses the shared memory chat client.
#[derive(Clone)]
pub struct LlmEntityMergeVerifier {
    client: LlmEntityMergeClient,
}

impl LlmEntityMergeVerifier {
    /// Creates a merge verifier from the shared chat transport.
    #[must_use]
    pub fn new(client: LlmChatClient) -> Self {
        Self {
            client: LlmEntityMergeClient::new(client),
        }
    }

    /// Creates a merge verifier from an API-key environment variable and model settings.
    pub fn from_env(api_key_env: &str, model: &str, timeout_ms: u64) -> Result<Self> {
        let api_key = std::env::var(api_key_env).map_err(|_| LlmChatError::Auth {
            message: format!("missing Cohere API key env var {api_key_env}"),
        })?;
        if api_key.trim().is_empty() {
            return Err(LlmChatError::Auth {
                message: format!("empty Cohere API key env var {api_key_env}"),
            }
            .into());
        }
        Ok(Self::new(LlmChatClient::from_api_key(
            secrecy::SecretString::from(api_key),
            model,
            timeout_ms,
        )))
    }

    /// Creates a merge verifier from a direct API key and model settings.
    #[must_use]
    pub fn from_api_key(api_key: String, model: &str, timeout_ms: u64) -> Self {
        Self::new(LlmChatClient::from_api_key(
            secrecy::SecretString::from(api_key),
            model,
            timeout_ms,
        ))
    }
}

#[async_trait]
impl EntityMergeVerifier for LlmEntityMergeVerifier {
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

    use super::*;

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
            "cargo run -p xtask -- record-memory-merges --corpus target/memory-eval/pr-natural",
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
