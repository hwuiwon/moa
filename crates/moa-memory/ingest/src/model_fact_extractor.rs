//! Model-backed fact extraction through the existing ingestion extractor seam.

use async_trait::async_trait;
use moa_core::config::MoaConfig;
use moa_memory_types::{FactCategory, FactEdgeLabel};
use serde::Deserialize;
use uuid::Uuid;

use crate::model_client::{ModelCallObserver, ModelTextClient, resolved_extraction_config};
use crate::{
    ExtractedFact, ExtractedFactScopeHint, FactExtractor, IngestError, Result, TurnChunk,
    fact_hash, fact_uid_from_hash,
};

/// Version for the LLM extraction prompt and recorded extraction fixtures.
pub const EXTRACTION_PROMPT_VERSION: &str = "v4";

/// Prompt versions whose recorded fixtures remain valid for replay.
///
/// Each version only adds optional output keys on top of the previous one, so
/// older fixtures replay correctly — their facts simply carry the conservative
/// defaults for the keys they predate. v3 added the optional `event_time` key
/// over v2; v4 added the `category`, `edge_label`, and `functional` structured
/// keys over v3. v2/v3 fixtures therefore load with `category = other`,
/// `edge_label = relates_to`, and `functional = false` until re-recorded.
pub const COMPATIBLE_PROMPT_VERSIONS: &[&str] = &["v2", "v3", "v4"];

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You extract durable, declarative facts from transcripts.
Skip questions, requests, speculation, small talk, transient scheduling commentary, and provenance-only details like "last sprint" or "per the platform decision".
Do not split one durable preference/value into multiple facts only because the object contains punctuation.
For each fact return one JSON object with keys:
- subject: concise noun phrase
- predicate: concise relation phrase
- object: concise noun phrase or value
- summary: one sentence restating the fact
- scope: "contact" or "tenant" using the scope rubric below
- confidence: number from 0.0 to 1.0
- category: one of "preference", "identity", "relationship", "event", "other" using the category rubric below
- edge_label: one of "depends_on", "owned_by", "relates_to" using the edge rubric below
- functional: true when the predicate holds at most one value per subject at a time (a newer value replaces the older one), false when the subject can hold several values at once
- event_time: OPTIONAL; include only when the transcript states when the fact became true, as an ISO 8601 date or timestamp (e.g. "2025-08-01"). Omit the key when the transcript gives no time.
Use this scope rubric:
scope = "contact" when the fact is about the speaker personally: preferences ("I prefer", "my setup", "for my work"), personal state, individual habits, or anything phrased in first person about themselves.
scope = "tenant" when the fact is about shared systems or team agreements: "we decided", "the team", "our service", infrastructure, ownership, processes that apply to everyone inside the tenant.
When genuinely ambiguous, choose "contact".
Use this category rubric:
category = "preference" for a standing choice, setting, default, or working/response style.
category = "identity" for a durable attribute of who or what the subject is (role, location, type, configuration value).
category = "relationship" for a link between entities such as ownership, dependency, or membership.
category = "event" for a time-bound occurrence.
category = "other" when none of the above fit.
Use this edge rubric (it describes how the subject relates to the object):
edge_label = "depends_on" when the subject depends on, uses, requires, calls, or is built on the object.
edge_label = "owned_by" when the subject is owned, maintained, or stewarded by the object.
edge_label = "relates_to" for any other relationship, including when the subject owns the object.
Few-shot examples:
Transcript: user: I prefer Linear for bug triage.
Fact: {"subject":"contact","predicate":"prefers","object":"Linear for bug triage","summary":"The contact prefers Linear for bug triage.","scope":"contact","confidence":0.95,"category":"preference","edge_label":"relates_to","functional":false}
Transcript: user: For my work, repo/control-plane is my default repo.
Fact: {"subject":"contact","predicate":"uses as default repository","object":"repo/control-plane","summary":"The contact uses repo/control-plane as their default repository.","scope":"contact","confidence":0.92,"category":"preference","edge_label":"depends_on","functional":true}
Transcript: user: We decided the API gateway runs on port 8443.
Fact: {"subject":"API gateway","predicate":"runs on port","object":"8443","summary":"The API gateway runs on port 8443.","scope":"tenant","confidence":0.94,"category":"identity","edge_label":"relates_to","functional":true}
Transcript: user: Our team owns the billing reconciler service.
Fact: {"subject":"team","predicate":"owns","object":"billing reconciler service","summary":"The team owns the billing reconciler service.","scope":"tenant","confidence":0.93,"category":"relationship","edge_label":"relates_to","functional":false}
Transcript: user: checkout-service depends on lib-auth.
Fact: {"subject":"checkout-service","predicate":"depends on","object":"lib-auth","summary":"checkout-service depends on lib-auth.","scope":"tenant","confidence":0.9,"category":"relationship","edge_label":"depends_on","functional":false}
Transcript (sent 2026-03-10): user: I moved to Denver last August.
Fact: {"subject":"contact","predicate":"lives in","object":"Denver","summary":"The contact lives in Denver.","scope":"contact","confidence":0.9,"category":"identity","edge_label":"relates_to","functional":true,"event_time":"2025-08-01"}
Return a JSON array and nothing else."#;

/// Fact extractor backed by the configured provider model.
#[derive(Clone)]
pub struct ModelFactExtractor {
    client: ModelFactExtractionClient,
}

impl ModelFactExtractor {
    /// Creates a model extractor from the runtime config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let extraction = resolved_extraction_config(config).ok_or_else(|| {
            IngestError::ModelInference("memory.extraction.enabled is false".to_string())
        })?;
        let client = ModelTextClient::from_config(config, &extraction)?;
        Ok(Self::new(client, extraction.max_facts_per_chunk))
    }

    /// Creates a configured model extractor with a provider-call observer.
    pub fn from_config_with_observer(
        config: &MoaConfig,
        observer: std::sync::Arc<dyn ModelCallObserver>,
    ) -> Result<Self> {
        let extraction = resolved_extraction_config(config).ok_or_else(|| {
            IngestError::ModelInference("memory.extraction.enabled is false".to_string())
        })?;
        let client = ModelTextClient::from_config_with_observer(config, &extraction, observer)?;
        Ok(Self::new(client, extraction.max_facts_per_chunk))
    }

    /// Creates a model extractor from an already configured model client.
    #[must_use]
    pub(crate) fn new(client: ModelTextClient, max_facts_per_chunk: usize) -> Self {
        Self {
            client: ModelFactExtractionClient::new(client, max_facts_per_chunk),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelFactExtractionChunk {
    index: usize,
    text: String,
}

#[derive(Debug, Clone, PartialEq)]
struct ModelExtractedFact {
    subject: String,
    predicate: String,
    object: String,
    summary: String,
    scope: String,
    confidence: Option<f64>,
    category: FactCategory,
    edge_label: FactEdgeLabel,
    functional: bool,
    event_time: Option<String>,
    source_chunk: usize,
}

#[derive(Clone)]
struct ModelFactExtractionClient {
    client: ModelTextClient,
    max_facts_per_chunk: usize,
}

impl ModelFactExtractionClient {
    fn new(client: ModelTextClient, max_facts_per_chunk: usize) -> Self {
        Self {
            client,
            max_facts_per_chunk,
        }
    }

    async fn extract(
        &self,
        chunks: &[ModelFactExtractionChunk],
    ) -> Result<Vec<ModelExtractedFact>> {
        let mut facts = Vec::new();
        for chunk in chunks {
            let response = self
                .client
                .complete_text(EXTRACTION_SYSTEM_PROMPT, &self.user_prompt(chunk))
                .await?;
            let parsed = parse_extraction_response(&response)?;
            facts.extend(
                parsed
                    .into_iter()
                    .take(self.max_facts_per_chunk)
                    .map(|fact| ModelExtractedFact {
                        subject: fact.subject,
                        predicate: fact.predicate,
                        object: fact.object,
                        summary: fact.summary,
                        scope: fact.scope,
                        confidence: fact.confidence,
                        category: parse_structured_enum(&fact.category),
                        edge_label: parse_structured_enum(&fact.edge_label),
                        functional: fact.functional,
                        event_time: fact.event_time,
                        source_chunk: chunk.index,
                    }),
            );
        }
        Ok(facts)
    }

    fn user_prompt(&self, chunk: &ModelFactExtractionChunk) -> String {
        format!(
            "prompt_version: {EXTRACTION_PROMPT_VERSION}\nmax_facts_per_chunk: {}\nchunk_index: {}\n\nTRANSCRIPT:\n{}",
            self.max_facts_per_chunk, chunk.index, chunk.text
        )
    }
}

fn parse_extraction_response(response: &str) -> Result<Vec<ParsedModelExtractedFact>> {
    let stripped = strip_json_code_fence(response);
    serde_json::from_str::<Vec<ParsedModelExtractedFact>>(stripped).map_err(|error| {
        IngestError::Extraction(format!(
            "failed to parse model extraction JSON array: {error}"
        ))
    })
}

fn strip_json_code_fence(response: &str) -> &str {
    let trimmed = response.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("json").unwrap_or(rest).trim_start();
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

#[derive(Debug, Clone, Deserialize)]
struct ParsedModelExtractedFact {
    subject: String,
    predicate: String,
    object: String,
    summary: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    confidence: Option<f64>,
    // Structured semantics emitted by the v4 prompt. `category` and `edge_label`
    // deserialize as raw strings (like `scope`) rather than straight into their
    // enums so an absent or unrecognized value degrades to the conservative
    // default instead of failing the whole chunk parse and dropping every fact.
    #[serde(default)]
    category: String,
    #[serde(default)]
    edge_label: String,
    #[serde(default)]
    functional: bool,
    #[serde(default)]
    event_time: Option<String>,
}

#[async_trait]
impl FactExtractor for ModelFactExtractor {
    async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
        validate_chunks(chunks)?;
        let provider_chunks = chunks
            .iter()
            .map(|chunk| ModelFactExtractionChunk {
                index: chunk.index,
                text: chunk.text.clone(),
            })
            .collect::<Vec<_>>();
        let extracted = self
            .client
            .extract(&provider_chunks)
            .await?
            .into_iter()
            .map(|fact| provider_fact_into_extracted(fact).map(normalize_extracted_fact))
            .collect::<Result<Vec<_>>>()?;
        Ok(extracted
            .into_iter()
            .filter(should_keep_extracted_fact)
            .collect())
    }
}

fn validate_chunks(chunks: &[TurnChunk]) -> Result<()> {
    for chunk in chunks {
        let actual_chars = chunk.text.chars().count();
        if actual_chars > crate::extract::MAX_EXTRACT_CHUNK_CHARS {
            return Err(IngestError::ChunkTooLarge {
                index: chunk.index,
                actual_chars,
                max_chars: crate::extract::MAX_EXTRACT_CHUNK_CHARS,
            });
        }
    }
    Ok(())
}

fn provider_fact_into_extracted(fact: ModelExtractedFact) -> Result<ExtractedFact> {
    let scope_hint = match fact.scope.trim().to_ascii_lowercase().as_str() {
        "tenant" => ExtractedFactScopeHint::Tenant,
        _ => ExtractedFactScopeHint::Contact,
    };
    let confidence = fact.confidence.map(clamp_confidence);
    let mut extracted = ExtractedFact {
        uid: Uuid::nil(),
        subject: fact.subject.trim().to_string(),
        predicate: fact.predicate.trim().to_string(),
        object: fact.object.trim().to_string(),
        summary: fact.summary.trim().to_string(),
        source_chunk: fact.source_chunk,
        scope_hint,
        confidence,
        event_time: parse_event_time(fact.event_time.as_deref()),
        category: fact.category,
        edge_label: fact.edge_label,
        functional: fact.functional,
    };
    let hash = fact_hash(&extracted)?;
    extracted.uid = fact_uid_from_hash(&hash);
    Ok(extracted)
}

fn clamp_confidence(confidence: f64) -> f64 {
    confidence.clamp(0.0, 1.0)
}

/// Maps a model-emitted controlled-vocabulary string onto its structured enum,
/// degrading to the type's conservative default when the value is empty or not
/// a recognized variant.
///
/// This parses a fixed enum vocabulary the model is instructed to emit; it is
/// not prose keyword matching. Deserializing through the enum's own serde
/// mapping keeps the accepted spellings in one place — the enum definition.
fn parse_structured_enum<T>(raw: &str) -> T
where
    T: Default + serde::de::DeserializeOwned,
{
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return T::default();
    }
    serde_json::from_value(serde_json::Value::String(normalized)).unwrap_or_default()
}

/// Parses a model-provided event time as an RFC 3339 timestamp or plain date.
///
/// Unparseable values degrade to `None` rather than failing the extraction:
/// event time only refines `valid_from`, so a malformed value must never cost
/// the fact itself.
pub(crate) fn parse_event_time(value: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(instant) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(instant.with_timezone(&chrono::Utc));
    }
    chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc())
}

pub(crate) fn normalize_extracted_fact(mut fact: ExtractedFact) -> ExtractedFact {
    if subject_is_requesting_individual(&fact) {
        fact.scope_hint = ExtractedFactScopeHint::Contact;
    }
    fact
}

/// Returns whether an extracted fact is durable enough to store.
///
/// Only structural rules belong here: predicate shapes that describe one-off
/// events rather than standing facts. Content-specific phrase filters are
/// forbidden — an earlier version hardcoded eval-corpus phrases, which gamed
/// the extraction-precision metric while silently dropping any production fact
/// containing those substrings. Salience guidance for specific content belongs
/// in the extraction prompt, where it generalizes.
pub(crate) fn should_keep_extracted_fact(fact: &ExtractedFact) -> bool {
    let predicate = normalize_filter_text(&fact.predicate);

    // Event-shaped predicates describe something that happened once, not a
    // standing fact about the user or tenant; they churn every session and do
    // not answer later retrieval queries.
    !(predicate.contains("occurred")
        || predicate.contains("completed")
        || predicate.contains("determined by")
        || predicate.contains("based on")
        || predicate.contains("defined by")
        || predicate == "follows")
}

/// Structural scope backstop: a fact whose subject is the requesting individual
/// is contact-scoped by construction, whatever scope the model returned — a
/// statement about one person cannot be tenant-shared memory.
///
/// This is deliberately the ONLY scope correction. It keys on subject identity
/// alone, never on predicate/object/summary phrasing. An earlier version instead
/// matched a whitelist of predicate phrases ("contact email", "response style",
/// "prefers", "switched to", ...) tuned to the eval corpus; it silently
/// overrode the model's structured scope and mis-scoped any production fact that
/// happened to contain those substrings. The model's `scope` is now trusted for
/// everything except this structural case.
fn subject_is_requesting_individual(fact: &ExtractedFact) -> bool {
    let subject = normalize_filter_text(&fact.subject);
    subject == "user" || subject == "contact" || subject.starts_with("user ")
}

fn normalize_filter_text(text: &str) -> String {
    let mut normalized = String::new();
    let mut previous_was_space = true;
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            normalized.push(character);
            previous_was_space = false;
        } else if !previous_was_space {
            normalized.push(' ');
            previous_was_space = true;
        }
    }
    normalized.trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use moa_core::{
        traits::LLMProvider, types::completion::CompletionRequest,
        types::completion::CompletionResponse, types::completion::CompletionStream,
        types::completion::StopReason, types::completion::TokenUsage, types::identifiers::ModelId,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
    };

    use super::*;

    struct StaticProvider {
        response: String,
    }

    #[async_trait]
    impl LLMProvider for StaticProvider {
        fn name(&self) -> &str {
            "static"
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

    fn chunk() -> TurnChunk {
        TurnChunk {
            index: 7,
            text: "user: I prefer Linear for bug triage.".to_string(),
            token_estimate: 10,
        }
    }

    #[test]
    fn extraction_prompt_contains_few_shot_pairs_contact_default_and_event_time() {
        // Pins: the extraction prompt makes ambiguous scope privacy-preserving,
        // keeps the few-shot examples explicit, asks for an optional event_time
        // only when the transcript states one, and requests the structured
        // category/edge_label/functional keys downstream now consumes.
        assert_eq!(EXTRACTION_PROMPT_VERSION, "v4");
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("When genuinely ambiguous, choose \"contact\"."));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("I prefer Linear for bug triage"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("For my work, repo/control-plane"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("We decided the API gateway runs on port 8443"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("Our team owns the billing reconciler service"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("event_time: OPTIONAL"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("I moved to Denver last August"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("Use this category rubric:"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("Use this edge rubric"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("\"category\":\"preference\""));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("\"edge_label\":\"depends_on\""));
    }

    #[test]
    fn event_time_parses_dates_and_timestamps_and_degrades_on_garbage() {
        // Pins: stated event times parse from RFC 3339 timestamps or plain dates,
        // and malformed values degrade to None instead of failing extraction.
        let date = parse_event_time(Some("2025-08-01")).expect("plain date parses");
        assert_eq!(date.to_rfc3339(), "2025-08-01T00:00:00+00:00");
        let instant =
            parse_event_time(Some("2025-08-01T12:30:00-04:00")).expect("timestamp parses");
        assert_eq!(instant.to_rfc3339(), "2025-08-01T16:30:00+00:00");
        assert_eq!(parse_event_time(Some("last summer")), None);
        assert_eq!(parse_event_time(Some("  ")), None);
        assert_eq!(parse_event_time(None), None);
    }

    fn extractor_for_response(response: &str, max_facts: usize) -> ModelFactExtractor {
        let client = ModelTextClient::new(
            Arc::new(StaticProvider {
                response: response.to_string(),
            }),
            ModelId::new("gpt-5.4-mini"),
            1_000,
        )
        .expect("model client should build");
        ModelFactExtractor::new(client, max_facts)
    }

    #[tokio::test]
    async fn model_extractor_parses_json_array_and_strips_code_fences() {
        // Pins: the model extractor accepts fenced JSON arrays and preserves structured fields.
        let extractor = extractor_for_response(
            r#"```json
[
  {
    "subject": "contact",
    "predicate": "prefers",
    "object": "Linear for bug triage",
    "summary": "The contact prefers Linear for bug triage.",
    "scope": "contact",
    "confidence": 0.91
  }
]
```"#,
            12,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "contact");
        assert_eq!(facts[0].predicate, "prefers");
        assert_eq!(facts[0].object, "Linear for bug triage");
        assert_eq!(facts[0].source_chunk, 7);
        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Contact);
        assert_eq!(facts[0].confidence, Some(0.91));
        assert_eq!(facts[0].event_time, None);
        // Structured keys absent from this response degrade to conservative
        // defaults rather than failing the parse.
        assert_eq!(facts[0].category, FactCategory::Other);
        assert_eq!(facts[0].edge_label, FactEdgeLabel::RelatesTo);
        assert!(!facts[0].functional);
    }

    #[tokio::test]
    async fn model_extractor_parses_structured_category_edge_and_functional() {
        // Pins: the model's structured category/edge_label/functional keys reach
        // ExtractedFact through their own enum vocabulary.
        let extractor = extractor_for_response(
            r#"[{"subject":"checkout-service","predicate":"depends on","object":"lib-auth","summary":"checkout-service depends on lib-auth.","scope":"tenant","confidence":0.9,"category":"relationship","edge_label":"depends_on","functional":false}]"#,
            12,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts[0].category, FactCategory::Relationship);
        assert_eq!(facts[0].edge_label, FactEdgeLabel::DependsOn);
        assert!(!facts[0].functional);
    }

    #[tokio::test]
    async fn model_extractor_degrades_unknown_structured_values_to_defaults() {
        // Pins: an unrecognized category/edge_label value degrades to the
        // conservative default instead of failing the whole chunk parse and
        // dropping the fact.
        let extractor = extractor_for_response(
            r#"[{"subject":"api gateway","predicate":"runs on port","object":"8443","summary":"api gateway runs on port 8443.","scope":"tenant","confidence":0.9,"category":"bogus","edge_label":"sideways","functional":true}]"#,
            12,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].category, FactCategory::Other);
        assert_eq!(facts[0].edge_label, FactEdgeLabel::RelatesTo);
        assert!(facts[0].functional);
    }

    #[tokio::test]
    async fn model_extractor_trusts_model_tenant_scope_despite_preference_wording() {
        // Pins: the removed phrase whitelist no longer overrides scope. A
        // tenant-scoped fact whose predicate contains "prefers" stays tenant
        // because its subject is not the requesting individual.
        let extractor = extractor_for_response(
            r#"[{"subject":"platform team","predicate":"prefers","object":"blue-green deploys","summary":"The platform team prefers blue-green deploys.","scope":"tenant","confidence":0.9,"category":"preference","edge_label":"relates_to","functional":false}]"#,
            12,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Tenant);
    }

    #[tokio::test]
    async fn model_extractor_carries_stated_event_time_into_extracted_fact() {
        // Pins: an event_time key in the model response reaches ExtractedFact as
        // a parsed UTC instant, and its presence does not change the fact uid.
        let response = r#"[
{"subject":"contact","predicate":"lives in","object":"Denver","summary":"The contact lives in Denver.","scope":"contact","confidence":0.9,"event_time":"2025-08-01"}
]"#;
        let dated = extractor_for_response(response, 12)
            .extract(&[chunk()])
            .await
            .expect("extract dated fact");
        let undated =
            extractor_for_response(&response.replace(",\"event_time\":\"2025-08-01\"", ""), 12)
                .extract(&[chunk()])
                .await
                .expect("extract undated fact");

        assert_eq!(
            dated[0]
                .event_time
                .expect("event time should parse")
                .to_rfc3339(),
            "2025-08-01T00:00:00+00:00"
        );
        assert_eq!(undated[0].event_time, None);
        assert_eq!(
            dated[0].uid, undated[0].uid,
            "event time must not change fact identity"
        );
    }

    #[tokio::test]
    async fn model_extractor_clamps_to_max_facts_per_chunk() {
        // Pins: configured fact cap bounds model output per chunk.
        let extractor = extractor_for_response(
            r#"[
{"subject":"a","predicate":"uses","object":"b","summary":"a uses b","scope":"tenant","confidence":0.8},
{"subject":"c","predicate":"uses","object":"d","summary":"c uses d","scope":"tenant","confidence":0.8}
]"#,
            1,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].summary, "a uses b");
    }

    #[tokio::test]
    async fn model_extractor_maps_unknown_scope_to_contact() {
        // Pins: unknown model scope values fail closed to contact-scoped memory.
        let extractor = extractor_for_response(
            r#"[{"subject":"a","predicate":"uses","object":"b","summary":"a uses b","scope":"planet","confidence":2.0}]"#,
            12,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Contact);
        assert_eq!(facts[0].confidence, Some(1.0));
    }

    #[tokio::test]
    async fn model_extractor_corrects_user_subject_scope() {
        // Pins: model tenant scope is corrected for user-subject preference facts.
        let extractor = extractor_for_response(
            r#"[{"subject":"User 04","predicate":"should use","object":"repo/control-plane","summary":"User 04 should use repo/control-plane.","scope":"tenant","confidence":0.9}]"#,
            12,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Contact);
    }

    #[tokio::test]
    async fn model_extractor_filters_event_shaped_predicates_only() {
        // Pins: the durability filter is structural (event-shaped predicates), never
        // content-phrase matching — content salience is the extraction prompt's job.
        // A previous phrase filter was tuned to the eval corpus and silently dropped
        // real facts, so content the model chose to extract must survive this filter.
        let extractor = extractor_for_response(
            r#"[
{"subject":"week","predicate":"was busy","object":"user","summary":"The user had a busy week.","scope":"user","confidence":0.8},
{"subject":"standardization","predicate":"occurred during","object":"last sprint","summary":"The standardization occurred during last sprint.","scope":"tenant","confidence":0.8},
{"subject":"auth","predicate":"uses","object":"JWT","summary":"auth uses JWT.","scope":"tenant","confidence":0.9}
]"#,
            12,
        );

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].subject, "week");
        assert_eq!(facts[1].subject, "auth");
        assert!(
            !facts.iter().any(|fact| fact.predicate.contains("occurred")),
            "event-shaped predicates must not reach ingestion"
        );
    }
}
