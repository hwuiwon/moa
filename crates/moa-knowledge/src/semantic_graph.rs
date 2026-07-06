//! Schema-constrained semantic graph extraction for tenant knowledge chunks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::{KnowledgeChunk, KnowledgeObject};

/// Current semantic graph schema used for support knowledge ingestion.
pub const SEMANTIC_GRAPH_SCHEMA_VERSION: &str = "wix_support_v1";

/// Current prompt or ruleset version used to produce semantic graph facts.
pub const SEMANTIC_GRAPH_PROMPT_VERSION: &str = "deterministic_rules_v1";

/// Stable model identifier for deterministic semantic graph extraction.
pub const SEMANTIC_GRAPH_MODEL: &str = "moa-deterministic-support-v1";

/// One cached semantic graph extraction for a chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticGraphExtraction {
    /// Chunk content hash the extraction was produced from.
    pub chunk_hash: String,
    /// Content hash used for cache invalidation.
    pub content_hash: String,
    /// Schema version constraining entity and relation types.
    pub schema_version: String,
    /// Extractor model or deterministic ruleset identifier.
    pub model: String,
    /// Prompt or ruleset version.
    pub prompt_version: String,
    /// Extracted schema-constrained entities.
    #[serde(default)]
    pub entities: Vec<SemanticEntity>,
    /// Extracted schema-constrained relations.
    #[serde(default)]
    pub relations: Vec<SemanticRelation>,
}

/// One schema-constrained entity mention in a chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticEntity {
    /// Canonical display name.
    pub canonical_name: String,
    /// Stable lowercase slug.
    pub canonical_slug: String,
    /// Support-domain entity type.
    pub kind: SemanticEntityKind,
    /// Known aliases observed in the chunk.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Extractor confidence in `[0, 1]`.
    pub confidence: f64,
    /// Short evidence phrase from the source chunk.
    pub evidence: String,
}

/// Support-domain entity kinds for Wix-style knowledge bases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticEntityKind {
    /// Product or product family.
    Product,
    /// User-facing feature.
    Feature,
    /// User action.
    Action,
    /// Procedure or workflow.
    Procedure,
    /// Step in a procedure.
    Step,
    /// Requirement or prerequisite.
    Requirement,
    /// Configuration setting.
    Setting,
    /// Error state.
    Error,
    /// Pricing or capability plan.
    Plan,
    /// Third-party or first-party integration.
    Integration,
    /// Policy or rule.
    Policy,
    /// Troubleshooting symptom.
    TroubleshootingSymptom,
}

impl SemanticEntityKind {
    /// Returns the stable identifier used in graph properties.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Product => "product",
            Self::Feature => "feature",
            Self::Action => "action",
            Self::Procedure => "procedure",
            Self::Step => "step",
            Self::Requirement => "requirement",
            Self::Setting => "setting",
            Self::Error => "error",
            Self::Plan => "plan",
            Self::Integration => "integration",
            Self::Policy => "policy",
            Self::TroubleshootingSymptom => "troubleshooting_symptom",
        }
    }
}

/// One schema-constrained semantic relation in a chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticRelation {
    /// Source entity slug.
    pub from_slug: String,
    /// Target entity slug.
    pub to_slug: String,
    /// Relation type.
    pub kind: SemanticRelationKind,
    /// Extractor confidence in `[0, 1]`.
    pub confidence: f64,
    /// Short evidence phrase from the source chunk.
    pub evidence: String,
}

/// Support-domain relation kinds for Wix-style knowledge bases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticRelationKind {
    /// Entity is part of another entity.
    PartOf,
    /// Entity answers a user question about another entity.
    Answers,
    /// Entity requires another entity.
    Requires,
    /// Entity applies to another entity.
    AppliesTo,
    /// Action configures a feature or setting.
    Configures,
    /// Action troubleshoots an error or symptom.
    Troubleshoots,
    /// Entity causes another entity.
    Causes,
    /// Step follows another step.
    NextStep,
    /// Entity is an alternative to another entity.
    AlternativeTo,
    /// Entity mentions another entity.
    Mentions,
    /// Entity is evidenced by another entity.
    EvidencedBy,
}

impl SemanticRelationKind {
    /// Returns the stable identifier used in graph properties.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PartOf => "part_of",
            Self::Answers => "answers",
            Self::Requires => "requires",
            Self::AppliesTo => "applies_to",
            Self::Configures => "configures",
            Self::Troubleshoots => "troubleshoots",
            Self::Causes => "causes",
            Self::NextStep => "next_step",
            Self::AlternativeTo => "alternative_to",
            Self::Mentions => "mentions",
            Self::EvidencedBy => "evidenced_by",
        }
    }

    /// Returns the existing graph edge relationship used for this relation.
    #[must_use]
    pub const fn graph_relationship(self) -> &'static str {
        match self {
            Self::Requires => "DEPENDS_ON",
            Self::AppliesTo => "APPLIES_TO",
            Self::Causes => "CAUSED",
            Self::EvidencedBy => "DERIVED_FROM",
            Self::PartOf
            | Self::Answers
            | Self::Configures
            | Self::Troubleshoots
            | Self::NextStep
            | Self::AlternativeTo
            | Self::Mentions => "RELATES_TO",
        }
    }
}

/// Builds a deterministic support-domain extraction for one chunk.
#[must_use]
pub fn extract_chunk_semantics(
    object: &KnowledgeObject,
    chunk: &KnowledgeChunk,
) -> SemanticGraphExtraction {
    let mut entities = BTreeMap::<String, SemanticEntity>::new();
    if let Some(title) = object
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        insert_entity(
            &mut entities,
            title,
            classify_entity(title),
            0.78,
            "object_title",
        );
    }
    for heading in &chunk.heading_path {
        let heading = heading.trim();
        if !heading.is_empty() {
            insert_entity(
                &mut entities,
                heading,
                classify_entity(heading),
                0.82,
                "heading",
            );
        }
    }

    let lower = chunk.text.to_ascii_lowercase();
    for (phrase, kind, confidence) in support_phrase_entities(&lower) {
        insert_entity(&mut entities, phrase, kind, confidence, phrase);
    }
    for requirement in requirement_phrases(&chunk.text) {
        insert_entity(
            &mut entities,
            &requirement,
            SemanticEntityKind::Requirement,
            0.86,
            &requirement,
        );
    }

    let ordered_entities = entities.values().cloned().collect::<Vec<_>>();
    let relations = semantic_relations(&ordered_entities, &lower);
    SemanticGraphExtraction {
        chunk_hash: chunk.chunk_hash.clone(),
        content_hash: chunk.chunk_hash.clone(),
        schema_version: SEMANTIC_GRAPH_SCHEMA_VERSION.to_string(),
        model: SEMANTIC_GRAPH_MODEL.to_string(),
        prompt_version: SEMANTIC_GRAPH_PROMPT_VERSION.to_string(),
        entities: ordered_entities,
        relations,
    }
}

/// Returns a stable lowercase slug for semantic graph names.
#[must_use]
pub fn canonical_slug(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let collapsed = normalized
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        blake3::hash(value.as_bytes()).to_hex()[0..16].to_string()
    } else {
        collapsed
    }
}

fn insert_entity(
    entities: &mut BTreeMap<String, SemanticEntity>,
    name: &str,
    kind: SemanticEntityKind,
    confidence: f64,
    evidence: &str,
) {
    let cleaned = clean_entity_name(name);
    if cleaned.len() < 3 {
        return;
    }
    let slug = canonical_slug(&cleaned);
    let alias = cleaned.clone();
    entities
        .entry(slug.clone())
        .and_modify(|entity| {
            if confidence > entity.confidence {
                entity.kind = kind;
                entity.confidence = confidence;
                entity.evidence = evidence.to_string();
            }
            if !entity.aliases.iter().any(|existing| existing == &alias) {
                entity.aliases.push(alias.clone());
                entity.aliases.sort();
            }
        })
        .or_insert_with(|| SemanticEntity {
            canonical_name: cleaned,
            canonical_slug: slug,
            kind,
            aliases: vec![alias],
            confidence,
            evidence: evidence.to_string(),
        });
}

fn clean_entity_name(name: &str) -> String {
    name.trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation())
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ")
}

fn classify_entity(value: &str) -> SemanticEntityKind {
    let lower = value.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("not working") {
        SemanticEntityKind::Error
    } else if lower.contains("plan") || lower.contains("premium") {
        SemanticEntityKind::Plan
    } else if lower.contains("policy") || lower.contains("rule") {
        SemanticEntityKind::Policy
    } else if lower.contains("step") {
        SemanticEntityKind::Step
    } else if lower.contains("connect")
        || lower.contains("configure")
        || lower.contains("set up")
        || lower.contains("add ")
    {
        SemanticEntityKind::Action
    } else {
        SemanticEntityKind::Feature
    }
}

fn support_phrase_entities(lower: &str) -> Vec<(&'static str, SemanticEntityKind, f64)> {
    const PHRASES: &[(&str, SemanticEntityKind, f64)] = &[
        ("wix", SemanticEntityKind::Product, 0.75),
        ("custom domain", SemanticEntityKind::Feature, 0.88),
        ("domain", SemanticEntityKind::Feature, 0.74),
        ("dns", SemanticEntityKind::Setting, 0.82),
        ("premium plan", SemanticEntityKind::Plan, 0.88),
        ("business plan", SemanticEntityKind::Plan, 0.86),
        ("enterprise plan", SemanticEntityKind::Plan, 0.86),
        ("pricing plan", SemanticEntityKind::Plan, 0.84),
        ("checkout", SemanticEntityKind::Feature, 0.8),
        ("payment", SemanticEntityKind::Feature, 0.78),
        ("booking", SemanticEntityKind::Feature, 0.78),
        ("member", SemanticEntityKind::Feature, 0.74),
        ("seo", SemanticEntityKind::Feature, 0.8),
        ("app market", SemanticEntityKind::Integration, 0.82),
        ("google workspace", SemanticEntityKind::Integration, 0.84),
        ("error", SemanticEntityKind::Error, 0.72),
        (
            "not working",
            SemanticEntityKind::TroubleshootingSymptom,
            0.78,
        ),
        ("connect", SemanticEntityKind::Action, 0.72),
        ("configure", SemanticEntityKind::Action, 0.72),
        ("set up", SemanticEntityKind::Action, 0.72),
        ("troubleshoot", SemanticEntityKind::Action, 0.78),
    ];
    PHRASES
        .iter()
        .filter(|(phrase, _, _)| lower.contains(*phrase))
        .copied()
        .collect()
}

fn requirement_phrases(text: &str) -> Vec<String> {
    let mut phrases = Vec::new();
    for keyword in ["requires", "require", "must", "need to", "needs to"] {
        let lower = text.to_ascii_lowercase();
        let Some(index) = lower.find(keyword) else {
            continue;
        };
        let after_keyword = &text[index + keyword.len()..];
        let phrase = after_keyword
            .split(['.', ';', '\n'])
            .next()
            .map(str::trim)
            .unwrap_or_default()
            .split_whitespace()
            .take(8)
            .collect::<Vec<_>>()
            .join(" ");
        if phrase.len() >= 3 {
            phrases.push(phrase);
        }
    }
    phrases.sort();
    phrases.dedup();
    phrases
}

fn semantic_relations(entities: &[SemanticEntity], lower_text: &str) -> Vec<SemanticRelation> {
    let mut relations = BTreeMap::<(String, String, &'static str), SemanticRelation>::new();
    let primary = entities
        .iter()
        .find(|entity| {
            matches!(
                entity.kind,
                SemanticEntityKind::Feature
                    | SemanticEntityKind::Procedure
                    | SemanticEntityKind::Action
                    | SemanticEntityKind::Product
            )
        })
        .or_else(|| entities.first());

    if let Some(primary) = primary {
        for requirement in entities
            .iter()
            .filter(|entity| entity.kind == SemanticEntityKind::Requirement)
        {
            push_relation(
                &mut relations,
                primary,
                requirement,
                SemanticRelationKind::Requires,
                0.84,
            );
        }
        for plan in entities
            .iter()
            .filter(|entity| entity.kind == SemanticEntityKind::Plan)
        {
            push_relation(
                &mut relations,
                primary,
                plan,
                SemanticRelationKind::AppliesTo,
                0.78,
            );
        }
    }

    let actions = entities
        .iter()
        .filter(|entity| entity.kind == SemanticEntityKind::Action)
        .collect::<Vec<_>>();
    for action in actions {
        for target in entities.iter().filter(|entity| {
            matches!(
                entity.kind,
                SemanticEntityKind::Feature
                    | SemanticEntityKind::Setting
                    | SemanticEntityKind::Integration
            )
        }) {
            if action.canonical_slug != target.canonical_slug {
                push_relation(
                    &mut relations,
                    action,
                    target,
                    SemanticRelationKind::Configures,
                    0.76,
                );
            }
        }
        for symptom in entities.iter().filter(|entity| {
            matches!(
                entity.kind,
                SemanticEntityKind::Error | SemanticEntityKind::TroubleshootingSymptom
            )
        }) {
            if lower_text.contains("troubleshoot") || lower_text.contains("not working") {
                push_relation(
                    &mut relations,
                    action,
                    symptom,
                    SemanticRelationKind::Troubleshoots,
                    0.8,
                );
            }
        }
    }

    relations.into_values().collect()
}

fn push_relation(
    relations: &mut BTreeMap<(String, String, &'static str), SemanticRelation>,
    from: &SemanticEntity,
    to: &SemanticEntity,
    kind: SemanticRelationKind,
    confidence: f64,
) {
    if from.canonical_slug == to.canonical_slug {
        return;
    }
    let key = (
        from.canonical_slug.clone(),
        to.canonical_slug.clone(),
        kind.as_str(),
    );
    relations.entry(key).or_insert_with(|| SemanticRelation {
        from_slug: from.canonical_slug.clone(),
        to_slug: to.canonical_slug.clone(),
        kind,
        confidence,
        evidence: format!("{} -> {}", from.canonical_name, to.canonical_name),
    });
}

#[cfg(test)]
mod tests {
    use moa_core::TenantId;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::domain::{KnowledgeChunk, KnowledgeObject, ObjectStatus};

    #[test]
    fn extract_chunk_semantics_is_schema_constrained_and_stable() {
        // Pins: deterministic extraction emits schema-constrained support
        // entities and relations without provider calls.
        let tenant_id = TenantId::from(Uuid::from_u128(1));
        let object = KnowledgeObject {
            object_uid: Uuid::from_u128(2),
            tenant_id,
            connection_uid: Uuid::from_u128(3),
            object_type: "page".to_string(),
            source_id: "domain".to_string(),
            parent_source_id: None,
            title: Some("Connect a custom domain".to_string()),
            source_uri: None,
            change_token: None,
            source_updated_at: None,
            deleted_at: None,
            status: ObjectStatus::Active,
            metadata: json!({}),
        };
        let chunk = KnowledgeChunk {
            chunk_uid: Uuid::from_u128(4),
            version_uid: Uuid::from_u128(5),
            graph_node_uid: None,
            chunk_hash: "chunk-a".to_string(),
            block_hashes: vec!["block-a".to_string()],
            heading_path: vec!["Custom domain DNS records".to_string()],
            text: "Connecting a custom domain requires a premium plan and DNS records.".to_string(),
            ordinal: 0,
            token_count: 12,
            metadata: json!({}),
        };

        let extraction = extract_chunk_semantics(&object, &chunk);

        assert_eq!(extraction.schema_version, SEMANTIC_GRAPH_SCHEMA_VERSION);
        assert!(
            extraction
                .entities
                .iter()
                .any(|entity| entity.kind == SemanticEntityKind::Plan)
        );
        assert!(
            extraction
                .relations
                .iter()
                .any(|relation| relation.kind == SemanticRelationKind::Requires)
        );
    }
}
