//! Recorded fact extraction replay for hermetic memory-eval lanes.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use moa_memory_types::{FactCategory, FactEdgeLabel};

use crate::{
    ExtractedFact, ExtractedFactScopeHint, FactExtractor, IngestError, Result, TurnChunk,
    fact_hash, fact_uid_from_hash,
    model_fact_extractor::{
        COMPATIBLE_PROMPT_VERSIONS, normalize_extracted_fact, should_keep_extracted_fact,
    },
};

/// One recorded extraction fixture keyed by raw chunk text hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionFixtureRecord {
    /// SHA-256 hex hash of the raw chunk text sent to the extractor.
    pub chunk_hash: String,
    /// Provider model that produced this fixture.
    pub model: String,
    /// Extraction prompt version that produced this fixture.
    pub prompt_version: String,
    /// Recorded facts for this chunk.
    pub facts: Vec<RecordedFact>,
}

/// One structured fact stored inside an extraction fixture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedFact {
    /// Subject text.
    pub subject: String,
    /// Predicate text.
    pub predicate: String,
    /// Object text.
    pub object: String,
    /// Concise fact summary.
    pub summary: String,
    /// Scope hint recorded by the extractor.
    #[serde(default)]
    pub scope_hint: ExtractedFactScopeHint,
    /// Optional extraction confidence.
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Coarse fact category recorded by the extractor. Absent in fixtures
    /// recorded before prompt v4, which replay as [`FactCategory::Other`].
    #[serde(default)]
    pub category: FactCategory,
    /// Fact-to-object edge label recorded by the extractor. Absent in fixtures
    /// recorded before prompt v4, which replay as [`FactEdgeLabel::RelatesTo`].
    #[serde(default)]
    pub edge_label: FactEdgeLabel,
    /// Whether the predicate is single-valued. Absent in fixtures recorded
    /// before prompt v4, which replay as `false`.
    #[serde(default)]
    pub functional: bool,
    /// Optional stated event time recorded by the extractor.
    #[serde(default)]
    pub event_time: Option<chrono::DateTime<chrono::Utc>>,
}

impl ExtractionFixtureRecord {
    /// Returns the fixture lookup key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.chunk_hash
    }

    /// Returns the fixture version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.prompt_version
    }
}

impl From<&ExtractedFact> for RecordedFact {
    fn from(fact: &ExtractedFact) -> Self {
        Self {
            subject: fact.subject.clone(),
            predicate: fact.predicate.clone(),
            object: fact.object.clone(),
            summary: fact.summary.clone(),
            scope_hint: fact.scope_hint,
            confidence: fact.confidence,
            category: fact.category,
            edge_label: fact.edge_label,
            functional: fact.functional,
            event_time: fact.event_time,
        }
    }
}

/// Lookup interface implemented by fixture stores that can back recorded extraction.
pub trait RecordedExtractionStore: Send + Sync {
    /// Returns the recorded fixture for a chunk hash, if it exists.
    fn get_optional(&self, key: &str) -> Option<&ExtractionFixtureRecord>;
}

/// Fact extractor that replays committed extraction fixtures.
#[derive(Debug, Clone)]
pub struct RecordedFactExtractor<S> {
    store: S,
    remediation_command: String,
}

impl<S> RecordedFactExtractor<S> {
    /// Creates a recorded extractor from a fixture store and remediation command.
    #[must_use]
    pub fn new(store: S, remediation_command: impl Into<String>) -> Self {
        Self {
            store,
            remediation_command: remediation_command.into(),
        }
    }
}

#[async_trait]
impl<S> FactExtractor for RecordedFactExtractor<S>
where
    S: RecordedExtractionStore,
{
    async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
        let mut missing = Vec::new();
        let mut facts = Vec::new();
        for chunk in chunks {
            let key = chunk_hash(&chunk.text);
            let Some(record) = self.store.get_optional(&key) else {
                missing.push(key);
                continue;
            };
            // v3 only adds the optional `event_time` key on top of v2, so v2
            // fixtures stay valid: their facts simply carry no event time.
            if !COMPATIBLE_PROMPT_VERSIONS.contains(&record.prompt_version.as_str()) {
                return Err(IngestError::Extraction(format!(
                    "recorded extraction fixture {} has prompt_version {}; expected one of {}",
                    record.chunk_hash,
                    record.prompt_version,
                    COMPATIBLE_PROMPT_VERSIONS.join(", ")
                )));
            }
            for recorded in &record.facts {
                let fact = normalize_extracted_fact(recorded.to_extracted(chunk.index)?);
                if should_keep_extracted_fact(&fact) {
                    facts.push(fact);
                }
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Err(IngestError::Extraction(format!(
                "recorded extraction fixtures are missing chunk hashes: {}. Regenerate with: {}",
                missing.join(", "),
                self.remediation_command
            )));
        }
        Ok(facts)
    }
}

impl RecordedFact {
    fn to_extracted(&self, source_chunk: usize) -> Result<ExtractedFact> {
        let mut fact = ExtractedFact {
            uid: Uuid::nil(),
            subject: self.subject.clone(),
            predicate: self.predicate.clone(),
            object: self.object.clone(),
            summary: self.summary.clone(),
            source_chunk,
            scope_hint: self.scope_hint,
            confidence: self.confidence.map(|value| value.clamp(0.0, 1.0)),
            event_time: self.event_time,
            category: self.category,
            edge_label: self.edge_label,
            functional: self.functional,
        };
        let hash = fact_hash(&fact)?;
        fact.uid = fact_uid_from_hash(&hash);
        Ok(fact)
    }
}

/// Returns the SHA-256 hex hash for raw chunk text.
#[must_use]
pub fn chunk_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    format!("{digest:x}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model_fact_extractor::EXTRACTION_PROMPT_VERSION;

    #[derive(Debug, Clone)]
    struct MapStore {
        records: BTreeMap<String, ExtractionFixtureRecord>,
    }

    impl RecordedExtractionStore for MapStore {
        fn get_optional(&self, key: &str) -> Option<&ExtractionFixtureRecord> {
            self.records.get(key)
        }
    }

    #[tokio::test]
    async fn recorded_extractor_replays_facts_with_recomputed_stable_uids() {
        // Pins: recorded extraction replays structured facts while deriving source_chunk and uid.
        let chunk = TurnChunk {
            index: 9,
            text: "user: auth uses JWT".to_string(),
            token_estimate: 4,
        };
        let key = chunk_hash(&chunk.text);
        let record = ExtractionFixtureRecord {
            chunk_hash: key.clone(),
            model: "command-test".to_string(),
            prompt_version: EXTRACTION_PROMPT_VERSION.to_string(),
            facts: vec![RecordedFact {
                subject: "auth".to_string(),
                predicate: "uses".to_string(),
                object: "JWT".to_string(),
                summary: "auth uses JWT".to_string(),
                scope_hint: ExtractedFactScopeHint::Tenant,
                confidence: Some(0.87),
                category: FactCategory::Other,
                edge_label: FactEdgeLabel::RelatesTo,
                functional: false,
                event_time: None,
            }],
        };
        let extractor = RecordedFactExtractor::new(
            MapStore {
                records: BTreeMap::from([(key, record)]),
            },
            "cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus target/memory-eval/pr-natural",
        );

        let facts = extractor.extract(&[chunk]).await.expect("replay facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].source_chunk, 9);
        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Tenant);
        assert_eq!(facts[0].confidence, Some(0.87));
        let hash = fact_hash(&facts[0]).expect("hash replayed fact");
        assert_eq!(facts[0].uid, fact_uid_from_hash(&hash));
    }

    #[tokio::test]
    async fn recorded_extractor_replays_structured_category_edge_and_functional() {
        // Pins: recorded structured semantics survive replay so re-recorded v4
        // fixtures drive digest ordering, edge typing, and the contradiction
        // sweep exactly as live extraction would.
        let chunk = TurnChunk {
            index: 3,
            text: "user: checkout-service depends on lib-auth".to_string(),
            token_estimate: 6,
        };
        let key = chunk_hash(&chunk.text);
        let record = ExtractionFixtureRecord {
            chunk_hash: key.clone(),
            model: "command-test".to_string(),
            prompt_version: EXTRACTION_PROMPT_VERSION.to_string(),
            facts: vec![RecordedFact {
                subject: "checkout-service".to_string(),
                predicate: "depends on".to_string(),
                object: "lib-auth".to_string(),
                summary: "checkout-service depends on lib-auth.".to_string(),
                scope_hint: ExtractedFactScopeHint::Tenant,
                confidence: Some(0.9),
                category: FactCategory::Relationship,
                edge_label: FactEdgeLabel::DependsOn,
                functional: false,
                event_time: None,
            }],
        };
        let extractor = RecordedFactExtractor::new(
            MapStore {
                records: BTreeMap::from([(key, record)]),
            },
            "cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus target/memory-eval/pr-natural",
        );

        let facts = extractor.extract(&[chunk]).await.expect("replay facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].category, FactCategory::Relationship);
        assert_eq!(facts[0].edge_label, FactEdgeLabel::DependsOn);
        assert!(!facts[0].functional);
    }

    #[tokio::test]
    async fn fixture_loader_rejects_v1_fixtures_after_version_bump() {
        // Pins: recorded replay fails closed when fixture prompt versions lag the extractor prompt.
        let chunk = TurnChunk {
            index: 0,
            text: "user: I prefer Linear.".to_string(),
            token_estimate: 5,
        };
        let key = chunk_hash(&chunk.text);
        let record = ExtractionFixtureRecord {
            chunk_hash: key.clone(),
            model: "command-test".to_string(),
            prompt_version: "v1".to_string(),
            facts: vec![RecordedFact {
                subject: "user".to_string(),
                predicate: "prefers".to_string(),
                object: "Linear".to_string(),
                summary: "The user prefers Linear.".to_string(),
                scope_hint: ExtractedFactScopeHint::Contact,
                confidence: Some(0.9),
                category: FactCategory::Other,
                edge_label: FactEdgeLabel::RelatesTo,
                functional: false,
                event_time: None,
            }],
        };
        let extractor = RecordedFactExtractor::new(
            MapStore {
                records: BTreeMap::from([(key, record)]),
            },
            "cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus target/memory-eval/pr-natural",
        );

        let error = extractor
            .extract(&[chunk])
            .await
            .expect_err("reject v1 fixture");

        assert!(
            error
                .to_string()
                .contains("prompt_version v1; expected one of v2, v3, v4"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn fixture_loader_accepts_v2_fixtures_because_v3_only_adds_event_time() {
        // Pins: recorded v2 fixtures stay replayable after the v3 prompt bump —
        // v3 only adds the optional event_time key, so v2 facts load with none.
        let chunk = TurnChunk {
            index: 2,
            text: "user: auth uses JWT everywhere".to_string(),
            token_estimate: 5,
        };
        let key = chunk_hash(&chunk.text);
        let record = ExtractionFixtureRecord {
            chunk_hash: key.clone(),
            model: "command-test".to_string(),
            prompt_version: "v2".to_string(),
            facts: vec![RecordedFact {
                subject: "auth".to_string(),
                predicate: "uses".to_string(),
                object: "JWT".to_string(),
                summary: "auth uses JWT".to_string(),
                scope_hint: ExtractedFactScopeHint::Tenant,
                confidence: Some(0.9),
                category: FactCategory::Other,
                edge_label: FactEdgeLabel::RelatesTo,
                functional: false,
                event_time: None,
            }],
        };
        let extractor = RecordedFactExtractor::new(
            MapStore {
                records: BTreeMap::from([(key, record)]),
            },
            "cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus target/memory-eval/pr-natural",
        );

        let facts = extractor
            .extract(&[chunk])
            .await
            .expect("v2 fixture replays");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].event_time, None);
    }

    #[tokio::test]
    async fn recorded_extractor_applies_model_durability_filter() {
        // Pins: recorded replay follows the same structural ingestion filter as live
        // model extraction — event-shaped predicates are dropped on replay too.
        let chunk = TurnChunk {
            index: 0,
            text: "The standardization occurred during last sprint.".to_string(),
            token_estimate: 12,
        };
        let key = chunk_hash(&chunk.text);
        let record = ExtractionFixtureRecord {
            chunk_hash: key.clone(),
            model: "command-test".to_string(),
            prompt_version: EXTRACTION_PROMPT_VERSION.to_string(),
            facts: vec![RecordedFact {
                subject: "standardization".to_string(),
                predicate: "occurred during".to_string(),
                object: "last sprint".to_string(),
                summary: "The standardization occurred during last sprint.".to_string(),
                scope_hint: ExtractedFactScopeHint::Tenant,
                confidence: Some(0.8),
                category: FactCategory::Other,
                edge_label: FactEdgeLabel::RelatesTo,
                functional: false,
                event_time: None,
            }],
        };
        let extractor = RecordedFactExtractor::new(
            MapStore {
                records: BTreeMap::from([(key, record)]),
            },
            "cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus target/memory-eval/pr-natural",
        );

        let facts = extractor.extract(&[chunk]).await.expect("replay facts");

        assert!(facts.is_empty());
    }

    #[tokio::test]
    async fn recorded_extractor_corrects_user_subject_scope() {
        // Pins: recorded replay applies the same user-scope correction as live extraction.
        let chunk = TurnChunk {
            index: 0,
            text: "For my work, User 04 should use repo/control-plane.".to_string(),
            token_estimate: 12,
        };
        let key = chunk_hash(&chunk.text);
        let record = ExtractionFixtureRecord {
            chunk_hash: key.clone(),
            model: "command-test".to_string(),
            prompt_version: EXTRACTION_PROMPT_VERSION.to_string(),
            facts: vec![RecordedFact {
                subject: "User 04".to_string(),
                predicate: "should use".to_string(),
                object: "repo/control-plane".to_string(),
                summary: "User 04 should use repo/control-plane.".to_string(),
                scope_hint: ExtractedFactScopeHint::Tenant,
                confidence: Some(0.8),
                category: FactCategory::Other,
                edge_label: FactEdgeLabel::RelatesTo,
                functional: false,
                event_time: None,
            }],
        };
        let extractor = RecordedFactExtractor::new(
            MapStore {
                records: BTreeMap::from([(key, record)]),
            },
            "cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus target/memory-eval/pr-natural",
        );

        let facts = extractor.extract(&[chunk]).await.expect("replay facts");

        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Contact);
    }
}
