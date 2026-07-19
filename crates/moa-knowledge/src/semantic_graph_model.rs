//! Model-backed semantic graph extraction for tenant knowledge chunks.
//!
//! This is the production extractor when a provider is configured. It asks a
//! configured [`LLMProvider`] for a schema-constrained knowledge graph over one
//! chunk: the request carries a strict JSON schema whose entity-kind and
//! relation-kind `enum` lists are derived from [`SemanticEntityKind::ALL`] and
//! [`SemanticRelationKind::ALL`], so the model may only emit the closed variant
//! sets the graph already understands. Output is then validated structurally --
//! entities must appear in the source text, relations must reference surviving
//! entities, and both are capped -- before it is shaped into the same
//! [`SemanticGraphExtraction`] the deterministic keyword ruleset produces.
//!
//! Extraction is per chunk and cached by the ingestion pipeline; the model is
//! called at most once per new or changed chunk. On any transport, parse, or
//! validation error the caller falls back to the deterministic extractor, so a
//! model failure never aborts ingestion.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    traits::LLMProvider, types::completion::CompletionRequest,
    types::completion::JsonResponseFormat, types::context::ContextMessage,
    types::identifiers::ModelId,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::domain::{KnowledgeChunk, KnowledgeObject};
use crate::error::{Error, Result};
use crate::semantic_graph::{
    SEMANTIC_GRAPH_MODEL, SEMANTIC_GRAPH_PROMPT_VERSION, SEMANTIC_GRAPH_SCHEMA_VERSION,
    SemanticEntity, SemanticEntityKind, SemanticGraphExtraction, SemanticRelation,
    SemanticRelationKind, canonical_slug, clean_entity_name,
};

/// Prompt version recorded for model-backed extractions.
///
/// This is part of the extraction cache key alongside the resolved model id, so
/// bumping it (or switching models) re-extracts every chunk instead of serving a
/// stale row produced by an older prompt.
pub const SEMANTIC_GRAPH_MODEL_PROMPT_VERSION: &str = "llm_structured_v1";

/// Maximum entities kept per chunk after validation.
///
/// A module constant, like the deterministic extractor's
/// `GENERIC_ENTITY_CHUNK_CAP`: it bounds graph fan-out, not an operator knob.
const MAX_ENTITIES_PER_CHUNK: usize = 24;

/// Maximum relations kept per chunk after validation.
const MAX_RELATIONS_PER_CHUNK: usize = 32;

/// Per-chunk extraction request timeout.
const MODEL_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Output token ceiling for one extraction call.
const MODEL_EXTRACTION_MAX_OUTPUT_TOKENS: usize = 4_096;

/// Confidence assigned when the model omits one for an entity.
const DEFAULT_ENTITY_CONFIDENCE: f64 = 0.7;

/// Confidence assigned when the model omits one for a relation.
const DEFAULT_RELATION_CONFIDENCE: f64 = 0.7;

/// Minimum cleaned entity-name length, matching the deterministic extractor.
const MIN_ENTITY_NAME_LEN: usize = 3;

/// Identity that keys one extractor's rows in the semantic graph cache.
///
/// The cache is unique on `(tenant, chunk_hash, schema_version, model,
/// prompt_version)`, so this triple determines which rows a lookup hits. The
/// deterministic and model-backed extractors carry distinct `(model,
/// prompt_version)` values, which makes switching between them re-extract rather
/// than serve the other extractor's cached output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticExtractionCacheIdentity<'a> {
    /// Schema version constraining entity and relation types.
    pub schema_version: &'a str,
    /// Extractor model or deterministic ruleset identifier.
    pub model: &'a str,
    /// Prompt or ruleset version.
    pub prompt_version: &'a str,
}

impl SemanticExtractionCacheIdentity<'static> {
    /// Returns the identity stamped by the deterministic keyword extractor.
    #[must_use]
    pub fn deterministic() -> Self {
        Self {
            schema_version: SEMANTIC_GRAPH_SCHEMA_VERSION,
            model: SEMANTIC_GRAPH_MODEL,
            prompt_version: SEMANTIC_GRAPH_PROMPT_VERSION,
        }
    }
}

/// Extracts a schema-constrained semantic graph for one chunk via an LLM.
///
/// Built at the composition root from a provider and model, matching how the
/// pipeline's other dependencies (parser, embedder, graph) are injected rather
/// than constructed from config inside this crate.
#[derive(Clone)]
pub struct ModelSemanticGraphExtractor {
    provider: Arc<dyn LLMProvider>,
    model: ModelId,
}

impl ModelSemanticGraphExtractor {
    /// Creates a model-backed extractor from an injected provider and model.
    #[must_use]
    pub fn new(provider: Arc<dyn LLMProvider>, model: ModelId) -> Self {
        Self { provider, model }
    }

    /// Returns the cache identity this extractor stamps on its output.
    #[must_use]
    pub fn cache_identity(&self) -> SemanticExtractionCacheIdentity<'_> {
        SemanticExtractionCacheIdentity {
            schema_version: SEMANTIC_GRAPH_SCHEMA_VERSION,
            model: self.model.as_str(),
            prompt_version: SEMANTIC_GRAPH_MODEL_PROMPT_VERSION,
        }
    }

    /// Extracts a validated, schema-constrained extraction for one chunk.
    ///
    /// Returns an error on any provider, timeout, or parse failure; the pipeline
    /// treats that as a signal to fall back to the deterministic extractor for
    /// this chunk, so ingestion never fails on model output alone.
    pub async fn extract(
        &self,
        object: &KnowledgeObject,
        chunk: &KnowledgeChunk,
    ) -> Result<SemanticGraphExtraction> {
        let response = self
            .complete(&system_prompt(), &user_prompt(object, chunk))
            .await?;
        let parsed = parse_model_extraction(&response)?;
        let grounding = grounding_haystack(object, chunk);
        let entities = validate_entities(parsed.entities, &grounding);
        let relations = validate_relations(parsed.relations, &entities);
        Ok(SemanticGraphExtraction {
            chunk_hash: chunk.chunk_hash.clone(),
            content_hash: chunk.chunk_hash.clone(),
            schema_version: SEMANTIC_GRAPH_SCHEMA_VERSION.to_string(),
            model: self.model.as_str().to_string(),
            prompt_version: SEMANTIC_GRAPH_MODEL_PROMPT_VERSION.to_string(),
            entities,
            relations,
        })
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "moa.knowledge.task".to_string(),
            json!("semantic_graph_extraction"),
        );
        let request = CompletionRequest {
            model: Some(self.model.clone()),
            messages: vec![ContextMessage::system(system), ContextMessage::user(user)],
            tools: Vec::new(),
            max_output_tokens: Some(MODEL_EXTRACTION_MAX_OUTPUT_TOKENS),
            temperature: Some(0.0),
            response_format: Some(response_format()),
            native_web_search: Default::default(),
            metadata,
        };

        let response = timeout(MODEL_EXTRACTION_TIMEOUT, async {
            let stream = self
                .provider
                .complete(request)
                .await
                .map_err(|error| Error::ModelExtraction(error.to_string()))?;
            stream
                .collect()
                .await
                .map_err(|error| Error::ModelExtraction(error.to_string()))
        })
        .await
        .map_err(|_| {
            Error::ModelExtraction(format!(
                "semantic graph model request timed out after {} ms",
                MODEL_EXTRACTION_TIMEOUT.as_millis()
            ))
        })??;

        if response.text.trim().is_empty() {
            return Err(Error::ModelExtraction(
                "semantic graph model returned empty text".to_string(),
            ));
        }
        Ok(response.text)
    }
}

/// Keeps only in-schema, text-grounded entities, merged by slug and capped.
fn validate_entities(parsed: Vec<ParsedModelEntity>, grounding: &str) -> Vec<SemanticEntity> {
    let mut by_slug = BTreeMap::<String, SemanticEntity>::new();
    for entity in parsed {
        let Some(kind) = parse_entity_kind(&entity.kind) else {
            continue;
        };
        let cleaned = clean_entity_name(&entity.name);
        if cleaned.len() < MIN_ENTITY_NAME_LEN {
            continue;
        }
        // Structural grounding: the entity must appear verbatim
        // (case-insensitively) in the source text shown to the model. This is a
        // containment check, not a semantic judgement, so it stays a
        // deterministic guard against hallucinated entities.
        if !grounding.contains(&cleaned.to_ascii_lowercase()) {
            continue;
        }
        let slug = canonical_slug(&cleaned);
        let confidence = clamp_confidence(entity.confidence.unwrap_or(DEFAULT_ENTITY_CONFIDENCE));
        let evidence = clean_evidence(&entity.evidence).unwrap_or_else(|| cleaned.clone());
        let alias = cleaned.clone();
        by_slug
            .entry(slug.clone())
            .and_modify(|existing| {
                if confidence > existing.confidence {
                    existing.kind = kind;
                    existing.confidence = confidence;
                    existing.evidence = evidence.clone();
                }
                if !existing.aliases.iter().any(|value| value == &alias) {
                    existing.aliases.push(alias.clone());
                    existing.aliases.sort();
                }
            })
            .or_insert_with(|| SemanticEntity {
                canonical_name: cleaned.clone(),
                canonical_slug: slug,
                kind,
                aliases: vec![alias],
                confidence,
                evidence,
            });
    }
    let mut entities = by_slug.into_values().collect::<Vec<_>>();
    entities.truncate(MAX_ENTITIES_PER_CHUNK);
    entities
}

/// Keeps only in-schema relations whose endpoints survived entity validation,
/// deduplicated by `(from, to, kind)` and capped.
fn validate_relations(
    parsed: Vec<ParsedModelRelation>,
    entities: &[SemanticEntity],
) -> Vec<SemanticRelation> {
    let known = entities
        .iter()
        .map(|entity| entity.canonical_slug.clone())
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::<(String, String, &'static str)>::new();
    let mut relations = Vec::new();
    for relation in parsed {
        if relations.len() >= MAX_RELATIONS_PER_CHUNK {
            break;
        }
        let Some(kind) = parse_relation_kind(&relation.kind) else {
            continue;
        };
        let from_slug = canonical_slug(&clean_entity_name(&relation.from));
        let to_slug = canonical_slug(&clean_entity_name(&relation.to));
        if from_slug == to_slug
            || !known.contains(&from_slug)
            || !known.contains(&to_slug)
            || !seen.insert((from_slug.clone(), to_slug.clone(), kind.as_str()))
        {
            continue;
        }
        let confidence =
            clamp_confidence(relation.confidence.unwrap_or(DEFAULT_RELATION_CONFIDENCE));
        let evidence = clean_evidence(&relation.evidence)
            .unwrap_or_else(|| format!("{from_slug} -> {to_slug}"));
        relations.push(SemanticRelation {
            from_slug,
            to_slug,
            kind,
            confidence,
            evidence,
        });
    }
    relations
}

/// Lowercased source text used to ground extracted entities: object title,
/// heading path, and chunk text -- everything the model was shown.
fn grounding_haystack(object: &KnowledgeObject, chunk: &KnowledgeChunk) -> String {
    let mut haystack = String::new();
    if let Some(title) = object.title.as_deref() {
        haystack.push_str(title);
        haystack.push('\n');
    }
    for heading in &chunk.heading_path {
        haystack.push_str(heading);
        haystack.push('\n');
    }
    haystack.push_str(&chunk.text);
    haystack.to_ascii_lowercase()
}

fn clamp_confidence(confidence: f64) -> f64 {
    if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Trims model-provided evidence, capping length; returns `None` when empty.
fn clean_evidence(evidence: &str) -> Option<String> {
    let trimmed = evidence.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .split_whitespace()
            .take(24)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn parse_entity_kind(value: &str) -> Option<SemanticEntityKind> {
    let value = value.trim();
    SemanticEntityKind::ALL
        .iter()
        .copied()
        .find(|kind| kind.as_str() == value)
}

fn parse_relation_kind(value: &str) -> Option<SemanticRelationKind> {
    let value = value.trim();
    SemanticRelationKind::ALL
        .iter()
        .copied()
        .find(|kind| kind.as_str() == value)
}

fn parse_model_extraction(response: &str) -> Result<ParsedModelExtraction> {
    let stripped = strip_json_code_fence(response);
    serde_json::from_str::<ParsedModelExtraction>(stripped).map_err(|error| {
        Error::ModelExtraction(format!("failed to parse model extraction JSON: {error}"))
    })
}

/// Strips a leading ```json fence and trailing ``` if the model wrapped its
/// output, matching the house model-extractor tolerance for fenced JSON.
fn strip_json_code_fence(response: &str) -> &str {
    let trimmed = response.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("json").unwrap_or(rest).trim_start();
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

#[derive(Debug, Deserialize)]
struct ParsedModelExtraction {
    #[serde(default)]
    entities: Vec<ParsedModelEntity>,
    #[serde(default)]
    relations: Vec<ParsedModelRelation>,
}

#[derive(Debug, Deserialize)]
struct ParsedModelEntity {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    evidence: String,
}

#[derive(Debug, Deserialize)]
struct ParsedModelRelation {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    evidence: String,
}

/// Builds the strict JSON schema constraining model output to the closed entity
/// and relation kind sets.
fn response_format() -> JsonResponseFormat {
    JsonResponseFormat::strict_json_schema(
        "semantic_graph_extraction",
        "Schema-constrained entities and relations grounded in the source chunk text.",
        extraction_schema(),
    )
}

fn extraction_schema() -> Value {
    let entity_kinds = SemanticEntityKind::ALL
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>();
    let relation_kinds = SemanticRelationKind::ALL
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["entities", "relations"],
        "properties": {
            "entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "kind", "confidence", "evidence"],
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Entity name copied verbatim from the source text."
                        },
                        "kind": {"type": "string", "enum": entity_kinds},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                        "evidence": {
                            "type": "string",
                            "description": "Short phrase from the source text supporting the entity."
                        }
                    }
                }
            },
            "relations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["from", "to", "kind", "confidence", "evidence"],
                    "properties": {
                        "from": {
                            "type": "string",
                            "description": "Name of the source entity, matching an entities[].name value."
                        },
                        "to": {
                            "type": "string",
                            "description": "Name of the target entity, matching an entities[].name value."
                        },
                        "kind": {"type": "string", "enum": relation_kinds},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                        "evidence": {
                            "type": "string",
                            "description": "Short phrase from the source text supporting the relation."
                        }
                    }
                }
            }
        }
    })
}

/// Builds the system prompt, listing the closed kind sets from the enum arrays.
fn system_prompt() -> String {
    let entity_lines = SemanticEntityKind::ALL
        .iter()
        .map(|kind| format!("- {}: {}", kind.as_str(), entity_kind_guidance(*kind)))
        .collect::<Vec<_>>()
        .join("\n");
    let relation_lines = SemanticRelationKind::ALL
        .iter()
        .map(|kind| format!("- {}: {}", kind.as_str(), relation_kind_guidance(*kind)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You extract a knowledge graph from one support-documentation chunk.\n\
         Return entities and the relations between them, using ONLY the kinds listed below.\n\
         Ground every entity in the source text: an entity name must be a phrase that appears \
         in the provided title, headings, or chunk text. Do not invent entities.\n\
         Each relation's `from` and `to` must exactly match an entity `name` you returned.\n\
         Prefer a few precise, well-typed entities over many vague ones.\n\n\
         Entity kinds:\n{entity_lines}\n\n\
         Relation kinds:\n{relation_lines}\n\n\
         Respond with a single JSON object matching the provided schema and nothing else."
    )
}

/// Builds the user prompt: object title, heading path, and chunk text.
fn user_prompt(object: &KnowledgeObject, chunk: &KnowledgeChunk) -> String {
    let mut prompt = String::new();
    if let Some(title) = object
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        prompt.push_str("DOCUMENT TITLE:\n");
        prompt.push_str(title);
        prompt.push_str("\n\n");
    }
    let headings = chunk
        .heading_path
        .iter()
        .map(|heading| heading.trim())
        .filter(|heading| !heading.is_empty())
        .collect::<Vec<_>>();
    if !headings.is_empty() {
        prompt.push_str("HEADING PATH:\n");
        prompt.push_str(&headings.join(" > "));
        prompt.push_str("\n\n");
    }
    prompt.push_str("CHUNK TEXT:\n");
    prompt.push_str(&chunk.text);
    prompt
}

fn entity_kind_guidance(kind: SemanticEntityKind) -> &'static str {
    match kind {
        SemanticEntityKind::Product => "a product or product family",
        SemanticEntityKind::Feature => "a user-facing feature or capability",
        SemanticEntityKind::Action => "an action a user performs",
        SemanticEntityKind::Procedure => "a procedure or multi-step workflow",
        SemanticEntityKind::Step => "a single step within a procedure",
        SemanticEntityKind::Requirement => "a requirement or prerequisite",
        SemanticEntityKind::Setting => "a configuration setting or value",
        SemanticEntityKind::Error => "an error state or error message",
        SemanticEntityKind::Plan => "a pricing or capability plan",
        SemanticEntityKind::Integration => "a first- or third-party integration",
        SemanticEntityKind::Policy => "a policy or rule",
        SemanticEntityKind::TroubleshootingSymptom => "a troubleshooting symptom",
    }
}

fn relation_kind_guidance(kind: SemanticRelationKind) -> &'static str {
    match kind {
        SemanticRelationKind::PartOf => "the source is part of the target",
        SemanticRelationKind::Answers => "the source answers a question about the target",
        SemanticRelationKind::Requires => "the source requires the target",
        SemanticRelationKind::AppliesTo => "the source applies to the target",
        SemanticRelationKind::Configures => "the source action configures the target",
        SemanticRelationKind::Troubleshoots => "the source action troubleshoots the target",
        SemanticRelationKind::Causes => "the source causes the target",
        SemanticRelationKind::NextStep => "the source step is followed by the target step",
        SemanticRelationKind::AlternativeTo => "the source is an alternative to the target",
        SemanticRelationKind::Mentions => "the source mentions the target",
        SemanticRelationKind::EvidencedBy => "the source is evidenced by the target",
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use moa_core::{
        types::completion::CompletionRequest, types::completion::CompletionResponse,
        types::completion::CompletionStream, types::completion::StopReason,
        types::completion::TokenUsage, types::model::ModelCapabilities, types::model::TokenPricing,
        types::model::ToolCallFormat,
    };
    use uuid::Uuid;

    use super::*;
    use crate::domain::{KnowledgeChunk, KnowledgeObject, ObjectStatus};

    struct ScriptedProvider {
        response: String,
    }

    #[async_trait]
    impl LLMProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted-semantic-graph"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: ModelId::new("gpt-5.4-mini"),
                context_window: 400_000,
                max_output: 128_000,
                supports_tools: true,
                supports_vision: false,
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
            _request: CompletionRequest,
        ) -> moa_core::error::Result<CompletionStream> {
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

    fn extractor_for(response: &str) -> ModelSemanticGraphExtractor {
        ModelSemanticGraphExtractor::new(
            Arc::new(ScriptedProvider {
                response: response.to_string(),
            }),
            ModelId::new("gpt-5.4-mini"),
        )
    }

    fn object_with_title(title: Option<&str>) -> KnowledgeObject {
        KnowledgeObject {
            object_uid: Uuid::from_u128(2),
            tenant_id: moa_core::types::identifiers::TenantId::from(Uuid::from_u128(1)),
            connection_uid: Uuid::from_u128(3),
            object_type: "page".to_string(),
            source_id: "domain".to_string(),
            parent_source_id: None,
            title: title.map(str::to_string),
            source_uri: None,
            change_token: None,
            source_updated_at: None,
            deleted_at: None,
            status: ObjectStatus::Active,
            metadata: json!({}),
        }
    }

    fn chunk_with_text(text: &str) -> KnowledgeChunk {
        KnowledgeChunk {
            chunk_uid: Uuid::from_u128(4),
            version_uid: Uuid::from_u128(5),
            graph_node_uid: None,
            chunk_hash: "chunk-a".to_string(),
            block_hashes: vec!["block-a".to_string()],
            heading_path: vec![],
            text: text.to_string(),
            ordinal: 0,
            token_count: 12,
            metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn model_extractor_maps_schema_constrained_entities_and_relations() {
        // Pins: valid model JSON becomes a typed extraction with enum kinds mapped
        // and the model-backed cache identity stamped (distinct from deterministic).
        let extractor = extractor_for(
            r#"```json
            {
              "entities": [
                {"name": "Custom domain", "kind": "feature", "confidence": 0.9, "evidence": "connect a custom domain"},
                {"name": "Premium plan", "kind": "plan", "confidence": 0.8, "evidence": "requires a premium plan"}
              ],
              "relations": [
                {"from": "Custom domain", "to": "Premium plan", "kind": "requires", "confidence": 0.85, "evidence": "custom domain requires a premium plan"}
              ]
            }
            ```"#,
        );
        let object = object_with_title(Some("Connect a custom domain"));
        let chunk = chunk_with_text("Connecting a custom domain requires a premium plan.");

        let extraction = extractor.extract(&object, &chunk).await.expect("extract");

        assert_eq!(extraction.model, "gpt-5.4-mini");
        assert_eq!(
            extraction.prompt_version,
            SEMANTIC_GRAPH_MODEL_PROMPT_VERSION
        );
        assert_eq!(extraction.schema_version, SEMANTIC_GRAPH_SCHEMA_VERSION);
        assert_ne!(extraction.model, SEMANTIC_GRAPH_MODEL);
        assert!(
            extraction
                .entities
                .iter()
                .any(|entity| entity.kind == SemanticEntityKind::Feature)
        );
        assert!(
            extraction
                .entities
                .iter()
                .any(|entity| entity.kind == SemanticEntityKind::Plan)
        );
        assert_eq!(extraction.relations.len(), 1);
        assert_eq!(extraction.relations[0].kind, SemanticRelationKind::Requires);
    }

    #[tokio::test]
    async fn model_extractor_drops_out_of_schema_and_ungrounded_output() {
        // Pins: entities with an unknown kind or a name absent from the source
        // text are dropped, and a relation referencing a dropped entity is dropped.
        let extractor = extractor_for(
            r#"{
              "entities": [
                {"name": "Custom domain", "kind": "feature", "confidence": 0.9, "evidence": "custom domain"},
                {"name": "Hallucinated Thing", "kind": "feature", "confidence": 0.9, "evidence": "n/a"},
                {"name": "Custom domain", "kind": "spaceship", "confidence": 0.9, "evidence": "custom domain"}
              ],
              "relations": [
                {"from": "Custom domain", "to": "Hallucinated Thing", "kind": "requires", "confidence": 0.8, "evidence": "x"},
                {"from": "Custom domain", "to": "Custom domain", "kind": "mentions", "confidence": 0.8, "evidence": "x"}
              ]
            }"#,
        );
        let object = object_with_title(None);
        let chunk = chunk_with_text("Connecting a custom domain is simple.");

        let extraction = extractor.extract(&object, &chunk).await.expect("extract");

        assert_eq!(
            extraction.entities.len(),
            1,
            "only the grounded, in-schema entity survives: {:?}",
            extraction.entities
        );
        assert_eq!(extraction.entities[0].canonical_name, "Custom domain");
        assert!(
            extraction.relations.is_empty(),
            "relations to dropped entities and self-relations are removed: {:?}",
            extraction.relations
        );
    }

    #[tokio::test]
    async fn model_extractor_errors_on_unparseable_response() {
        // Pins: an unparseable model response is an error, which is the pipeline's
        // signal to fall back to the deterministic extractor for the chunk.
        let extractor = extractor_for("not json at all");
        let object = object_with_title(None);
        let chunk = chunk_with_text("Connecting a custom domain is simple.");

        let error = extractor
            .extract(&object, &chunk)
            .await
            .expect_err("unparseable response should error");

        assert!(matches!(error, Error::ModelExtraction(_)), "{error:?}");
    }

    #[tokio::test]
    async fn model_extractor_caps_entities_per_chunk() {
        // Pins: validated entity output is bounded by the per-chunk cap even when
        // the model returns more grounded, in-schema entities.
        let count = MAX_ENTITIES_PER_CHUNK * 2;
        let entities = (0..count)
            .map(|index| {
                format!(
                    "{{\"name\": \"Entity{index}\", \"kind\": \"feature\", \"confidence\": 0.9, \"evidence\": \"entity{index}\"}}"
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let text = (0..count)
            .map(|index| format!("Entity{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let extractor = extractor_for(&format!(
            "{{\"entities\": [{entities}], \"relations\": []}}"
        ));
        let object = object_with_title(None);
        let chunk = chunk_with_text(&text);

        let extraction = extractor.extract(&object, &chunk).await.expect("extract");

        assert_eq!(extraction.entities.len(), MAX_ENTITIES_PER_CHUNK);
    }

    #[test]
    fn deterministic_and_model_identities_are_distinct() {
        // Pins: the two extractors carry distinct cache identities, so switching
        // between them re-extracts rather than serving the other's cached rows.
        let extractor = extractor_for("{}");
        let model = extractor.cache_identity();
        let deterministic = SemanticExtractionCacheIdentity::deterministic();

        assert_eq!(model.schema_version, deterministic.schema_version);
        assert_ne!(model.model, deterministic.model);
        assert_ne!(model.prompt_version, deterministic.prompt_version);
        assert_eq!(model.model, "gpt-5.4-mini");
    }

    #[test]
    fn extraction_schema_enumerates_every_kind() {
        // Pins: the strict schema's enum lists cover exactly the closed variant
        // sets, so no kind can be emitted that the graph does not understand.
        let schema = extraction_schema();
        let entity_enum = schema["properties"]["entities"]["items"]["properties"]["kind"]["enum"]
            .as_array()
            .expect("entity kind enum");
        let relation_enum =
            schema["properties"]["relations"]["items"]["properties"]["kind"]["enum"]
                .as_array()
                .expect("relation kind enum");

        assert_eq!(entity_enum.len(), SemanticEntityKind::ALL.len());
        assert_eq!(relation_enum.len(), SemanticRelationKind::ALL.len());
        for kind in SemanticEntityKind::ALL {
            assert!(
                entity_enum.iter().any(|value| value == kind.as_str()),
                "entity kind {} missing from schema",
                kind.as_str()
            );
        }
        for kind in SemanticRelationKind::ALL {
            assert!(
                relation_enum.iter().any(|value| value == kind.as_str()),
                "relation kind {} missing from schema",
                kind.as_str()
            );
        }
    }
}
