//! Explicit, hashable memory-formation configuration contracts.

use std::path::PathBuf;

use moa_core::canonical_json::canonical_json_bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ExternalMemoryError, Result};

/// Schema version of resolved formation configuration.
pub const FORMATION_SCHEMA_VERSION: u32 = 1;
const FORMATION_HASH_DOMAIN: &[u8] = b"moa.external-memory.formation.v1\0";

/// Explicit formation execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormationMode {
    /// Deterministic local heuristic components.
    Heuristic,
    /// Versioned extraction and merge fixtures.
    Recorded,
    /// Explicit paid extractor and merge-verifier selectors.
    Live,
}

/// Resolved implementation/model/prompt identity for one formation component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentConfig {
    /// Concrete implementation identifier.
    pub implementation: String,
    /// Provider/model selector, when model-backed.
    pub model: Option<String>,
    /// Prompt schema/version, when prompted.
    pub prompt_version: Option<String>,
}

/// Resolved embedding identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingConfig {
    /// Embedding provider.
    pub provider: String,
    /// Embedding model.
    pub model: String,
    /// Provider model version persisted with vectors.
    pub version: i32,
    /// Vector dimensions.
    pub dimensions: usize,
}

/// Resolved entity blocking configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityBlockingConfig {
    /// Whether embedding candidate blocking is enabled.
    pub enabled: bool,
    /// Minimum cosine score for embedding-blocked candidates.
    pub cosine_threshold: String,
}

/// Complete consolidation settings that can affect formed memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidationSettings {
    /// Idle days before confidence decay.
    pub decay_idle_days: i64,
    /// Confidence decay half-life in days.
    pub decay_half_life_days: String,
    /// Minimum retained confidence.
    pub decay_floor: String,
    /// Idle days before floor-bound facts expire.
    pub expire_idle_days: i64,
    /// Whether standing digests are enabled.
    pub digest_enabled: bool,
    /// Maximum standing-digest tokens.
    pub digest_max_tokens: usize,
    /// Minimum hours between digest rebuilds.
    pub digest_rebuild_min_interval_hours: i64,
}

/// Fully resolved, reproducible memory formation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedFormationConfig {
    /// Configuration schema version.
    pub schema_version: u32,
    /// Explicit execution mode.
    pub mode: FormationMode,
    /// Extractor identity.
    pub extractor: ComponentConfig,
    /// Merge implementation/verifier identity.
    pub merge: ComponentConfig,
    /// Embedding identity.
    pub embedding: EmbeddingConfig,
    /// Entity candidate blocking settings.
    pub entity_blocking: EntityBlockingConfig,
    /// PII classifier identity.
    pub pii_classifier: ComponentConfig,
    /// Contradiction detector identity.
    pub contradiction_detector: ComponentConfig,
    /// Complete consolidation settings.
    pub consolidation: ConsolidationSettings,
}

impl ResolvedFormationConfig {
    /// Validates the resolved configuration and selected mode's required fields.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != FORMATION_SCHEMA_VERSION {
            return Err(ExternalMemoryError::InvalidConfig(format!(
                "unsupported formation schema version {}",
                self.schema_version
            )));
        }
        for (name, value) in [
            (
                "extractor implementation",
                self.extractor.implementation.as_str(),
            ),
            ("merge implementation", self.merge.implementation.as_str()),
            ("embedding provider", self.embedding.provider.as_str()),
            ("embedding model", self.embedding.model.as_str()),
            (
                "PII classifier",
                self.pii_classifier.implementation.as_str(),
            ),
            (
                "contradiction detector",
                self.contradiction_detector.implementation.as_str(),
            ),
        ] {
            if value.trim().is_empty() {
                return Err(ExternalMemoryError::InvalidConfig(format!(
                    "{name} is required"
                )));
            }
        }
        if self.embedding.dimensions == 0 {
            return Err(ExternalMemoryError::InvalidConfig(
                "embedding dimensions must be positive".to_string(),
            ));
        }
        let entity_threshold = parse_decimal(
            "entity blocking cosine threshold",
            &self.entity_blocking.cosine_threshold,
        )?;
        if !(0.0..=1.0).contains(&entity_threshold) {
            return Err(ExternalMemoryError::InvalidConfig(
                "entity blocking cosine threshold must be finite and in [0, 1]".to_string(),
            ));
        }
        for (name, raw) in [
            (
                "consolidation decay half-life",
                self.consolidation.decay_half_life_days.as_str(),
            ),
            (
                "consolidation decay floor",
                self.consolidation.decay_floor.as_str(),
            ),
        ] {
            if parse_decimal(name, raw)? < 0.0 {
                return Err(ExternalMemoryError::InvalidConfig(format!(
                    "{name} must be finite and non-negative"
                )));
            }
        }
        if self.mode == FormationMode::Live
            && (self.extractor.model.as_deref().is_none_or(str::is_empty)
                || self.merge.model.as_deref().is_none_or(str::is_empty)
                || self
                    .extractor
                    .prompt_version
                    .as_deref()
                    .is_none_or(str::is_empty)
                || self
                    .merge
                    .prompt_version
                    .as_deref()
                    .is_none_or(str::is_empty))
        {
            return Err(ExternalMemoryError::InvalidConfig(
                "live formation requires extractor and merge-verifier model selectors and prompt versions"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Returns the lowercase schema-v1 domain-separated canonical SHA-256 digest.
    pub fn canonical_hash(&self) -> Result<String> {
        self.validate()?;
        let value = serde_json::to_value(self)?;
        Self::canonical_hash_value(&value)
    }

    /// Hashes a JSON representation using the schema-v1 canonical contract.
    pub fn canonical_hash_value(value: &serde_json::Value) -> Result<String> {
        let config: Self = serde_json::from_value(value.clone())?;
        config.validate()?;
        let mut hasher = Sha256::new();
        hasher.update(FORMATION_HASH_DOMAIN);
        hasher.update(canonical_json_bytes(value)?);
        Ok(format!("{:x}", hasher.finalize()))
    }
}

fn parse_decimal(name: &str, raw: &str) -> Result<f64> {
    let value = raw.parse::<f64>().map_err(|error| {
        ExternalMemoryError::InvalidConfig(format!("invalid {name} `{raw}`: {error}"))
    })?;
    if !value.is_finite() {
        return Err(ExternalMemoryError::InvalidConfig(format!(
            "{name} must be finite"
        )));
    }
    Ok(value)
}

/// Versioned recorded-mode fixture manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedFormationManifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Extraction fixture path.
    pub extraction_fixture_path: PathBuf,
    /// Extraction fixture SHA-256.
    pub extraction_fixture_sha256: String,
    /// Merge fixture path.
    pub merge_fixture_path: PathBuf,
    /// Merge fixture SHA-256.
    pub merge_fixture_sha256: String,
}

impl RecordedFormationManifest {
    /// Validates versioned, separate extraction and merge fixtures.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(ExternalMemoryError::InvalidConfig(format!(
                "unsupported recorded formation manifest version {}",
                self.schema_version
            )));
        }
        if self.extraction_fixture_path == self.merge_fixture_path {
            return Err(ExternalMemoryError::InvalidConfig(
                "recorded formation requires separate extraction and merge fixtures".to_string(),
            ));
        }
        validate_digest(&self.extraction_fixture_sha256)?;
        validate_digest(&self.merge_fixture_sha256)?;
        Ok(())
    }
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ExternalMemoryError::InvalidConfig(
            "fixture SHA-256 must be lowercase hexadecimal".to_string(),
        ));
    }
    Ok(())
}
